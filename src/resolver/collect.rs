//! Pass 1: top-level declaration collection.
//!
//! Walks the program's items once, registers every declaration with
//! the symbol table, validates layouts, and assembles the
//! cross-module use / import tables. Methods are kept thin —
//! each item form has its own dedicated `collect_<kind>` that
//! pushes its symbol(s) onto `table` and defers body resolution
//! to Pass 2.
//!
//! Houses `collect_top_level_items` (the dispatcher) + every
//! per-item-form helper, the `validate_layouts` / `validate_layout`
//! pair, the `check_compiler_builtin_attr` gate, the two name
//! helpers `type_expr_name` / `is_trait_assoc_fn_name`, and the
//! big `collect_import` body that drives cross-module visibility
//! resolution against `ProgramTree`.
//!
//! Lives in a sibling `impl<'a> super::Resolver<'a>` block.

use std::collections::HashSet;

use crate::ast::*;
use crate::edit_distance::suggest_similar;
use crate::module::{self};

use super::{
    module_exposes_name, module_item_visibility, module_top_level_names, visibility_allows_access,
    ResolveError, ResolveErrorKind, ScopeId, Symbol, SymbolId, SymbolKind, TextEdit,
    VariantSymbolKind,
};

impl<'a> super::Resolver<'a> {
    // ── Pass 1: Top-level declaration collection ────────────────

    pub(crate) fn collect_top_level_items(&mut self) {
        for item in &self.program.items {
            match item {
                Item::Function(f) => self.collect_function(f),
                Item::StructDef(s) => self.collect_struct(s),
                Item::EnumDef(e) => self.collect_enum(e),
                Item::TraitDef(t) => self.collect_trait(t),
                Item::TraitAlias(t) => self.collect_trait_alias(t),
                Item::MarkerTrait(t) => self.collect_marker_trait(t),
                Item::ImplBlock(i) => self.collect_impl(i),
                Item::EffectResource(e) => self.collect_effect_resource(e),
                Item::EffectGroup(e) => self.collect_effect_group(e),
                Item::EffectVerbDecl(e) => self.collect_effect_verb(e),
                Item::ConstDecl(c) => self.collect_const(c),
                Item::TypeAlias(t) => self.collect_type_alias(t),
                Item::UseDecl(u) => self.collect_use(u),
                Item::Import(i) => self.collect_import(i),
                Item::ExternFunction(e) => self.collect_extern_function(e),
                Item::ExternBlock(b) => {
                    for it in &b.items {
                        match it {
                            ExternItem::Function(f) => self.collect_extern_function(f),
                            ExternItem::OpaqueType(o) => self.collect_opaque_foreign_type(o),
                        }
                    }
                }
                Item::DistinctType(d) => self.collect_distinct_type(d),
                Item::LayoutDef(_) | Item::AliasDecl(_) | Item::IndependentDecl(_) => {}
            }
        }
    }

    pub(crate) fn validate_layouts(&mut self) {
        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            if let Item::LayoutDef(layout) = item {
                self.validate_layout(layout);
            }
        }
    }

    fn validate_layout(&mut self, layout: &LayoutDef) {
        // Extract the element type name from the collection type.
        // e.g., Vec[Entity] → "Entity", Array[Entity, 100] → "Entity"
        let struct_name = match &layout.collection_type.kind {
            TypeKind::Path(path) => {
                let coll_name = path.segments.first().map(|s| s.as_str()).unwrap_or("");
                if coll_name != "Vec" && coll_name != "Array" {
                    self.errors.push(ResolveError {
                        message: format!(
                            "layout collection type must be Vec[T] or Array[T, N], found '{}'",
                            coll_name
                        ),
                        span: layout.span.clone(),
                        kind: ResolveErrorKind::UndefinedType,
                        suggestion: None,
                        replacement: None,
                    });
                    return;
                }
                // Extract the element type from generic args.
                match &path.generic_args {
                    Some(args) if !args.is_empty() => {
                        if let GenericArg::Type(te) = &args[0] {
                            if let TypeKind::Path(inner) = &te.kind {
                                inner.segments.first().cloned()
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        let struct_name = match struct_name {
            Some(n) => n,
            None => {
                self.errors.push(ResolveError {
                    message:
                        "layout collection type must specify an element type (e.g., Vec[Entity])"
                            .to_string(),
                    span: layout.span.clone(),
                    kind: ResolveErrorKind::UndefinedType,
                    suggestion: None,
                    replacement: None,
                });
                return;
            }
        };

        // Look up the struct's field names.
        let struct_fields: Vec<String> = if let Some(sym) =
            self.table.lookup_in_scope(ScopeId(0), &struct_name)
        {
            if let SymbolKind::Struct { field_names } = &sym.kind {
                field_names.clone()
            } else if let SymbolKind::Enum { .. } = &sym.kind {
                // Validate split_by_variant for enums.
                for item in &layout.items {
                    if let LayoutItem::Group { name, span, .. } = item {
                        self.errors.push(ResolveError {
                                message: format!(
                                    "layout group '{}' is not allowed for enum types; use split_by_variant",
                                    name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::UndefinedField,
                                suggestion: Some("use split_by_variant instead of group".to_string()),
                                replacement: None,
                            });
                    }
                }
                return;
            } else {
                self.errors.push(ResolveError {
                    message: format!("'{}' is not a struct", struct_name),
                    span: layout.span.clone(),
                    kind: ResolveErrorKind::UndefinedType,
                    suggestion: None,
                    replacement: None,
                });
                return;
            }
        } else {
            self.errors.push(ResolveError {
                message: format!("undefined struct '{}' in layout definition", struct_name),
                span: layout.span.clone(),
                kind: ResolveErrorKind::UndefinedType,
                suggestion: None,
                replacement: None,
            });
            return;
        };

        // Validate layout items: field existence, uniqueness, cold constraints, align(N).
        let mut assigned: HashSet<String> = HashSet::new();
        let mut cold_count = 0usize;
        for item in &layout.items {
            match item {
                LayoutItem::Group {
                    name,
                    fields,
                    align,
                    span,
                } => {
                    for field in fields {
                        if !struct_fields.contains(field) {
                            self.errors.push(ResolveError {
                                message: format!(
                                    "field '{}' does not exist on struct '{}' (in group '{}')",
                                    field, struct_name, name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::UndefinedField,
                                suggestion: None,
                                replacement: None,
                            });
                        } else if !assigned.insert(field.clone()) {
                            self.errors.push(ResolveError {
                                message: format!(
                                    "field '{}' appears in multiple sections in layout '{}'",
                                    field, layout.name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::DuplicateDefinition,
                                suggestion: None,
                                replacement: None,
                            });
                        }
                    }
                    if let Some(n) = align {
                        if *n == 0 || (*n & (*n - 1)) != 0 {
                            self.errors.push(ResolveError {
                                message: format!(
                                    "align({}) is not a power of two in layout '{}' group '{}'",
                                    n, layout.name, name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::UndefinedField,
                                suggestion: Some(
                                    "common values: 8, 16, 32, 64 (cache line), 128 (Apple Silicon cache line)".to_string(),
                                ),
                                replacement: None,
                            });
                        }
                    }
                }
                LayoutItem::Cold { fields, span } => {
                    cold_count += 1;
                    if cold_count > 1 {
                        self.errors.push(ResolveError {
                            message: format!(
                                "layout '{}' has more than one cold section; at most one is allowed",
                                layout.name
                            ),
                            span: span.clone(),
                            kind: ResolveErrorKind::DuplicateDefinition,
                            suggestion: None,
                            replacement: None,
                        });
                    }
                    for field in fields {
                        if !struct_fields.contains(field) {
                            self.errors.push(ResolveError {
                                message: format!(
                                    "field '{}' does not exist on struct '{}' (in cold section)",
                                    field, struct_name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::UndefinedField,
                                suggestion: None,
                                replacement: None,
                            });
                        } else if !assigned.insert(field.clone()) {
                            self.errors.push(ResolveError {
                                message: format!(
                                    "field '{}' appears in multiple sections in layout '{}'",
                                    field, layout.name
                                ),
                                span: span.clone(),
                                kind: ResolveErrorKind::DuplicateDefinition,
                                suggestion: None,
                                replacement: None,
                            });
                        }
                    }
                }
                LayoutItem::SplitByVariant(span) => {
                    self.errors.push(ResolveError {
                        message: "split_by_variant is only valid for enum layout blocks"
                            .to_string(),
                        span: span.clone(),
                        kind: ResolveErrorKind::UndefinedField,
                        suggestion: None,
                        replacement: None,
                    });
                }
            }
        }

        // Warn about unassigned fields (fields not in any group or cold section).
        let unassigned: Vec<&String> = struct_fields
            .iter()
            .filter(|f| !assigned.contains(*f))
            .collect();
        if !unassigned.is_empty() {
            // TODO: Implement proper warning severity and #[allow(layout_unassigned_fields)].
            let field_list = unassigned
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            self.errors.push(ResolveError {
                message: format!(
                    "layout '{}': fields not assigned to any group or cold section: {}. These will be placed in an implicit trailing hot group.",
                    layout.name, field_list
                ),
                span: layout.span.clone(),
                kind: ResolveErrorKind::UndefinedField,
                suggestion: Some("assign all fields to groups, or suppress with #[allow(layout_unassigned_fields)]".to_string()),
                replacement: None,
            });
        }
    }

    /// Reject `#[deprecated]` placed on an `impl` block per design.md
    /// § `#[deprecated]` for Item Deprecation > "Where it cannot
    /// appear" — impl-level deprecation would be ambiguous (which
    /// methods does it cover?); the user should deprecate the
    /// individual methods instead.
    fn reject_deprecated_on_impl(&mut self, attrs: &[Attribute]) {
        for attr in attrs {
            if attr.name == "deprecated" {
                self.errors.push(ResolveError {
                    message: "error[E_DEPRECATED_ON_IMPL]: \
                              `#[deprecated]` is not valid on an `impl` \
                              block — deprecate the underlying methods \
                              individually instead. See design.md § \
                              `#[deprecated]` for Item Deprecation."
                        .to_string(),
                    span: attr.span.clone(),
                    kind: ResolveErrorKind::DeprecatedOnImpl,
                    suggestion: None,
                    replacement: None,
                });
            }
        }
    }

    /// Reject `#[deprecated]` placed on a struct field. Field-level
    /// deprecation is post-v1 — use-site detection for field
    /// reads/writes is non-trivial and is bundled with the post-v1
    /// lint expansion. Per design.md § `#[deprecated]` for Item
    /// Deprecation > "Where it cannot appear".
    fn reject_deprecated_on_field(&mut self, attrs: &[Attribute]) {
        for attr in attrs {
            if attr.name == "deprecated" {
                self.errors.push(ResolveError {
                    message: "error[E_DEPRECATED_ON_FIELD]: \
                              `#[deprecated]` on individual struct fields \
                              is post-v1; only item-level deprecation is \
                              supported. Use-site detection for field \
                              reads/writes is bundled with the post-v1 \
                              lint expansion."
                        .to_string(),
                    span: attr.span.clone(),
                    kind: ResolveErrorKind::DeprecatedOnField,
                    suggestion: None,
                    replacement: None,
                });
            }
        }
    }

    /// Reject `#[track_caller]` placed on an item kind that is not a
    /// `fn` declaration. Per design.md § Error Handling > "Stdlib
    /// panic-emitters report the caller's source location", the
    /// attribute redirects the panic-site source location and only
    /// makes sense on functions. Trait method declarations are *also*
    /// legal targets per the spec (last-writer-wins propagation to
    /// impls), but trait-method attribute support is a separate
    /// enabling change — `TraitMethod` has no `attributes` field
    /// today, so the attribute cannot attach there and this helper
    /// covers every other site (struct, enum, trait, marker trait,
    /// trait alias, impl block, struct field).
    ///
    /// `target_kind` is the human-readable role name surfaced in the
    /// diagnostic message. Function and impl-method callers skip this
    /// helper entirely — the attribute is legal at those sites.
    fn reject_track_caller_attr(&mut self, attrs: &[Attribute], target_kind: &str) {
        for attr in attrs {
            if attr.name == "track_caller" {
                self.errors.push(ResolveError {
                    message: format!(
                        "error[E_TRACK_CALLER_INVALID_TARGET]: \
                         `#[track_caller]` is not valid on {target_kind}; \
                         the attribute redirects the panic-site source \
                         location and only applies to `fn` declarations \
                         — see design.md § Error Handling > \"Stdlib \
                         panic-emitters report the caller's source location\"",
                    ),
                    span: attr.span.clone(),
                    kind: ResolveErrorKind::TrackCallerInvalidTarget,
                    suggestion: None,
                    replacement: None,
                });
            }
        }
    }

    /// Reject `#[non_exhaustive]` placed on an item kind that does
    /// not support it. Per design.md § `#[non_exhaustive]` for
    /// Evolvable Public Types, the attribute is meaningful only on
    /// `pub struct` and `pub enum` declarations — no cross-package
    /// boundary exists for private types, traits / fns / impls have
    /// no field or variant surface to evolve, and individual enum
    /// variants are v1-out-of-scope (Rust accepts variant-level; we
    /// ship type-level only, additive later if real use surfaces).
    ///
    /// `target_kind` is the human-readable role name surfaced in the
    /// diagnostic message — `"trait"`, `"function"`, `"impl block"`,
    /// `"private struct"`, etc. The two callers that *do* accept the
    /// attribute (`collect_struct` / `collect_enum` on `pub` types)
    /// skip this helper entirely; everyone else calls through with
    /// the kind name that fits their item.
    fn reject_non_exhaustive_attr(&mut self, attrs: &[Attribute], target_kind: &str) {
        for attr in attrs {
            if attr.name == "non_exhaustive" {
                self.errors.push(ResolveError {
                    message: format!(
                        "error[E_NON_EXHAUSTIVE_INVALID_TARGET]: \
                         `#[non_exhaustive]` is not valid on {target_kind}; \
                         the attribute applies only to `pub struct` and \
                         `pub enum` declarations — see design.md § \
                         `#[non_exhaustive]` for Evolvable Public Types",
                    ),
                    span: attr.span.clone(),
                    kind: ResolveErrorKind::NonExhaustiveInvalidTarget,
                    suggestion: None,
                    replacement: None,
                });
            }
        }
    }

    fn check_compiler_builtin_attr(&mut self, attrs: &[Attribute], item_stdlib_origin: bool) {
        // The gate bypasses (a) when the whole resolver session is in
        // stdlib-source mode (CR-202 slice 1's `with_stdlib_source(true)`
        // builder), or (b) when the individual item carries the per-item
        // stdlib-origin tag (CR-202 slice 3b). The per-item tag is what
        // 3c uses to flip baked stdlib items spliced into a user-mode
        // program tree — the resolver session for that tree stays
        // user-mode, but individual baked items get an exemption.
        if self.is_stdlib_source || item_stdlib_origin {
            return;
        }
        for attr in attrs {
            if attr.name == "compiler_builtin" {
                self.errors.push(ResolveError {
                    message: "`#[compiler_builtin]` is reserved for stdlib source baked into the compiler binary"
                        .to_string(),
                    span: attr.span.clone(),
                    kind: ResolveErrorKind::CompilerBuiltinReserved,
                    suggestion: None,
                    replacement: None,
                });
            }
        }
    }

    fn collect_function(&mut self, f: &Function) {
        self.check_compiler_builtin_attr(&f.attributes, f.stdlib_origin);
        self.reject_non_exhaustive_attr(&f.attributes, "function");
        let param_names: Vec<String> = f
            .params
            .iter()
            .flat_map(|p| p.pattern.binding_names())
            .collect();
        if let Err(e) = self.table.define(
            f.name.clone(),
            SymbolKind::Function { param_names },
            f.span.clone(),
            f.is_pub,
        ) {
            self.errors.push(e);
        }
    }

    fn collect_struct(&mut self, s: &StructDef) {
        self.check_compiler_builtin_attr(&s.attributes, s.stdlib_origin);
        // `#[non_exhaustive]` is only meaningful at the cross-package
        // boundary — a private struct has no consumers outside its own
        // package, so the attribute is rejected with the kind-named
        // diagnostic. `pub struct` consumes the attribute via the
        // parser-set `is_non_exhaustive` flag and no rejection fires.
        if s.is_non_exhaustive && !s.is_pub {
            self.reject_non_exhaustive_attr(&s.attributes, "private struct");
        }
        self.reject_track_caller_attr(&s.attributes, "struct");
        // Field-level `#[non_exhaustive]` is post-v1 (Rust accepts it
        // on fields too; we ship type-level only). Reject so users get
        // a focused message instead of a silent acceptance that does
        // nothing — the attribute presence on a field would otherwise
        // be ignored, which is worse than the diagnostic.
        for field in &s.fields {
            self.reject_non_exhaustive_attr(&field.attributes, "struct field");
            self.reject_track_caller_attr(&field.attributes, "struct field");
            self.reject_deprecated_on_field(&field.attributes);
        }
        let field_names: Vec<String> = s.fields.iter().map(|f| f.name.clone()).collect();
        if let Err(e) = self.table.define(
            s.name.clone(),
            SymbolKind::Struct { field_names },
            s.span.clone(),
            s.is_pub,
        ) {
            self.errors.push(e);
        }
    }

    fn collect_enum(&mut self, e: &EnumDef) {
        self.check_compiler_builtin_attr(&e.attributes, e.stdlib_origin);
        if e.is_non_exhaustive && !e.is_pub {
            self.reject_non_exhaustive_attr(&e.attributes, "private enum");
        }
        self.reject_track_caller_attr(&e.attributes, "enum");
        let variant_names: Vec<String> = e.variants.iter().map(|v| v.name.clone()).collect();
        let enum_id = match self.table.define(
            e.name.clone(),
            SymbolKind::Enum { variant_names },
            e.span.clone(),
            e.is_pub,
        ) {
            Ok(id) => id,
            Err(err) => {
                self.errors.push(err);
                return;
            }
        };

        // Register each variant in global scope
        for variant in &e.variants {
            let variant_kind = match &variant.kind {
                VariantKind::Unit => VariantSymbolKind::Unit,
                VariantKind::Tuple(types) => VariantSymbolKind::Tuple(types.len()),
                VariantKind::Struct(fields) => {
                    VariantSymbolKind::Struct(fields.iter().map(|f| f.name.clone()).collect())
                }
            };
            // Try to register variant name directly; if collision, that's ok —
            // user must use qualified path
            let _ = self.table.define(
                variant.name.clone(),
                SymbolKind::EnumVariant {
                    parent_enum: enum_id,
                    variant_kind,
                },
                variant.span.clone(),
                e.is_pub,
            );
        }
    }

    fn collect_trait(&mut self, t: &TraitDef) {
        self.check_compiler_builtin_attr(&t.attributes, t.stdlib_origin);
        self.reject_non_exhaustive_attr(&t.attributes, "trait");
        self.reject_track_caller_attr(&t.attributes, "trait");
        let method_names: Vec<String> = t
            .items
            .iter()
            .filter_map(|item| match item {
                TraitItem::Method(m) => Some(m.name.clone()),
                TraitItem::AssocType(_) => None,
            })
            .collect();
        if let Err(e) = self.table.define(
            t.name.clone(),
            SymbolKind::Trait { method_names },
            t.span.clone(),
            t.is_pub,
        ) {
            self.errors.push(e);
        }
    }

    fn collect_trait_alias(&mut self, t: &TraitAliasDef) {
        self.reject_non_exhaustive_attr(&t.attributes, "trait alias");
        self.reject_track_caller_attr(&t.attributes, "trait alias");
        if let Err(e) = self.table.define(
            t.name.clone(),
            SymbolKind::TraitAlias,
            t.span.clone(),
            t.is_pub,
        ) {
            self.errors.push(e);
        }
    }

    fn collect_marker_trait(&mut self, t: &MarkerTraitDef) {
        self.reject_non_exhaustive_attr(&t.attributes, "marker trait");
        self.reject_track_caller_attr(&t.attributes, "marker trait");
        // Marker traits register in the trait namespace alongside ordinary
        // traits; no methods to track, so the symbol carries an empty
        // method list. Trait-bound resolution and impl coherence treat
        // markers identically to ordinary traits — the marker-ness is a
        // definition-site property, not a use-site property.
        if let Err(e) = self.table.define(
            t.name.clone(),
            SymbolKind::Trait {
                method_names: Vec::new(),
            },
            t.span.clone(),
            t.is_pub,
        ) {
            self.errors.push(e);
        }
    }

    fn collect_impl(&mut self, imp: &ImplBlock) {
        // `#[compiler_builtin]` is reserved for stdlib source — user code
        // is rejected (E0237). The other `collect_*` callers gate top-level
        // items; impl blocks need the same gate at two levels: the block
        // itself, and each method. ImplBlock has no `stdlib_origin` field
        // because baked stdlib impls live in `STDLIB_PROGRAMS` and are
        // walked by the typechecker/interpreter directly — they never
        // reach this resolver path. So `false` here is correct: any impl
        // block walked by `collect_impl` is user-authored, and a
        // session-wide stdlib-source resolver bypasses via `is_stdlib_source`.
        // Methods carry their own `stdlib_origin` (inherited from the
        // `Function` field) and pass it through for the per-item exemption.
        self.check_compiler_builtin_attr(&imp.attributes, false);
        self.reject_non_exhaustive_attr(&imp.attributes, "impl block");
        self.reject_track_caller_attr(&imp.attributes, "impl block");
        self.reject_deprecated_on_impl(&imp.attributes);
        // Methods are registered in type_methods, not global scope.
        // We need the type name from the target_type.
        let type_name = self.type_expr_name(&imp.target_type);
        if let Some(type_name) = type_name {
            for item in &imp.items {
                let method = match item {
                    ImplItem::Method(m) => m,
                    ImplItem::AssocType(_) => continue,
                };
                self.check_compiler_builtin_attr(&method.attributes, method.stdlib_origin);
                self.reject_non_exhaustive_attr(&method.attributes, "impl method");
                let param_names: Vec<String> = method
                    .params
                    .iter()
                    .flat_map(|p| p.pattern.binding_names())
                    .collect();
                let method_id = SymbolId(self.table.symbols.len());
                self.table.symbols.push(Symbol {
                    id: method_id,
                    name: method.name.clone(),
                    kind: SymbolKind::Function { param_names },
                    span: method.span.clone(),
                    is_pub: method.is_pub,
                    scope: self.table.current_scope,
                });
                self.table.register_method(&type_name, method_id);
            }
        }
    }

    fn collect_effect_resource(&mut self, e: &EffectResourceDecl) {
        const RESERVED: &[&str] = &["CompileTimeEnv", "CompileTimeHeap"];
        if RESERVED.contains(&e.name.as_str()) {
            self.errors.push(ResolveError {
                message: format!(
                    "resource name '{}' is reserved for the deferred comptime feature; \
                     see deferred.md § Comptime Effect Defaults",
                    e.name
                ),
                span: e.span.clone(),
                kind: ResolveErrorKind::ReservedEffectResource,
                suggestion: None,
                replacement: None,
            });
            return;
        }
        if let Err(err) = self.table.define(
            e.name.clone(),
            SymbolKind::EffectResource,
            e.span.clone(),
            true, // effect resources are always accessible
        ) {
            self.errors.push(err);
        }
    }

    fn collect_effect_group(&mut self, e: &EffectGroupDecl) {
        if let Err(err) = self.table.define(
            e.name.clone(),
            SymbolKind::EffectGroup,
            e.span.clone(),
            e.is_pub,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_effect_verb(&mut self, e: &EffectVerbDecl) {
        if let Err(err) = self.table.define(
            e.verb_name.clone(),
            SymbolKind::EffectVerb,
            e.span.clone(),
            true,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_const(&mut self, c: &ConstDecl) {
        if let Err(err) = self.table.define(
            c.name.clone(),
            SymbolKind::Constant,
            c.span.clone(),
            c.is_pub,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_type_alias(&mut self, t: &TypeAliasDef) {
        if let Err(err) = self.table.define(
            t.name.clone(),
            SymbolKind::TypeAlias,
            t.span.clone(),
            t.is_pub,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_distinct_type(&mut self, d: &crate::ast::DistinctTypeDef) {
        if let Err(err) = self.table.define(
            d.name.clone(),
            SymbolKind::DistinctType,
            d.span.clone(),
            d.is_pub,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_use(&mut self, u: &UseDecl) {
        if let Some(last) = u.path.last() {
            if let Err(err) = self.table.define(
                last.clone(),
                SymbolKind::Import {
                    path: u.path.clone(),
                },
                u.span.clone(),
                u.is_pub,
            ) {
                self.errors.push(err);
            }
        }
    }

    /// Resolve a CR-24 `import` declaration against the `ProgramTree`:
    ///
    /// 1. Ensure the dotted prefix in `imp.path` names a module in the graph
    ///    (`E0224 UnknownModule` on miss, with a Levenshtein suggestion over
    ///    all known module paths).
    /// 2. For each brace-listed item, bind `alias.or(name)` in the current
    ///    scope as `SymbolKind::Import { path }`. If the `path + name` is
    ///    itself a submodule, the binding is a module reference; otherwise
    ///    the target module must expose a matching top-level item
    ///    (`E0225 UnknownItemInModule` on miss).
    ///
    /// Single-file mode (no tree attached) skips cross-module validation and
    /// just registers the symbol so downstream passes see the name in scope.
    /// This keeps `karac run file.kara` unchanged.
    fn collect_import(&mut self, imp: &ImportDecl) {
        let Some(tree) = self.tree else {
            // Single-file mode — bind without validation.
            for item in &imp.items {
                let bound = item.alias.clone().unwrap_or_else(|| item.name.clone());
                let mut full = imp.path.clone();
                full.push(item.name.clone());
                if let Err(e) = self.table.define(
                    bound,
                    SymbolKind::Import { path: full },
                    item.span.clone(),
                    imp.is_pub,
                ) {
                    self.errors.push(e);
                }
            }
            return;
        };

        // Validate the dotted prefix exists.
        if !tree.graph.by_path.contains_key(&imp.path) && !imp.path.is_empty() {
            let wanted = imp.path.join(".");
            let all_paths: Vec<String> = tree
                .graph
                .by_path
                .keys()
                .map(|p| {
                    if p.is_empty() {
                        "<crate>".to_string()
                    } else {
                        p.join(".")
                    }
                })
                .collect();
            let candidates: Vec<&str> = all_paths.iter().map(|s| s.as_str()).collect();
            let suggestion = suggest_similar(&wanted, &candidates);
            let mut message = format!("unknown module `{wanted}`");
            if let Some(ref s) = suggestion {
                message.push_str(&format!(", did you mean `{s}`?"));
            }
            // The replacement covers exactly the dotted prefix tokens in
            // source — `imp.path_spans` is the per-segment span vector
            // from the parser, paired with `imp.path`. A non-empty
            // `imp.path` (guarded above) therefore implies a non-empty
            // `path_spans`, and the contiguous span runs from the first
            // segment's offset to the last segment's end.
            let replacement = suggestion.as_ref().and_then(|s| {
                let first = imp.path_spans.first()?;
                let last = imp.path_spans.last()?;
                let offset = first.offset;
                let length = (last.offset + last.length).saturating_sub(offset);
                Some(Box::new(TextEdit {
                    offset,
                    length,
                    replacement: s.clone(),
                }))
            });
            self.errors.push(ResolveError {
                message,
                span: imp.span.clone(),
                kind: ResolveErrorKind::UnknownModule,
                suggestion,
                replacement,
            });
            // Still register names locally so downstream passes do not
            // compound with cascading UndefinedName errors.
            for item in &imp.items {
                let bound = item.alias.clone().unwrap_or_else(|| item.name.clone());
                let mut full = imp.path.clone();
                full.push(item.name.clone());
                if let Err(e) = self.table.define(
                    bound,
                    SymbolKind::Import { path: full },
                    item.span.clone(),
                    imp.is_pub,
                ) {
                    self.errors.push(e);
                }
            }
            return;
        }

        // Prefix exists. Look up each brace-listed item.
        let importer_path = self
            .current_module
            .map(|id| tree.module(id).path.clone())
            .unwrap_or_default();
        for item in &imp.items {
            let mut full = imp.path.clone();
            full.push(item.name.clone());

            let binds_submodule = tree.graph.by_path.contains_key(&full);
            let binds_item = !binds_submodule && module_exposes_name(tree, &imp.path, &item.name);

            if !binds_submodule && !binds_item {
                // Look at target module's top-level items for suggestions,
                // plus any submodule siblings.
                let mut candidates_owned: Vec<String> = module_top_level_names(tree, &imp.path);
                for p in tree.graph.by_path.keys() {
                    if p.len() == imp.path.len() + 1 && p.starts_with(&imp.path) {
                        candidates_owned.push(p.last().cloned().unwrap_or_default());
                    }
                }
                let candidates: Vec<&str> = candidates_owned.iter().map(|s| s.as_str()).collect();
                let suggestion = suggest_similar(&item.name, &candidates);

                let module_label = if imp.path.is_empty() {
                    "<crate root>".to_string()
                } else {
                    imp.path.join(".")
                };
                let mut message =
                    format!("unknown item `{}` in module `{module_label}`", item.name);
                if let Some(ref s) = suggestion {
                    message.push_str(&format!(", did you mean `{s}`?"));
                } else if candidates_owned.len() <= 10 && !candidates_owned.is_empty() {
                    // Design.md § Path resolution algorithm — for small
                    // modules, list the available exports.
                    message.push_str(&format!("; available: {}", candidates_owned.join(", ")));
                }
                let replacement = suggestion.as_ref().map(|s| {
                    Box::new(TextEdit {
                        offset: item.span.offset,
                        length: item.span.length,
                        replacement: s.clone(),
                    })
                });
                self.errors.push(ResolveError {
                    message,
                    span: item.span.clone(),
                    kind: ResolveErrorKind::UnknownItemInModule,
                    suggestion,
                    replacement,
                });
            } else if binds_item {
                // Slice 6 + 7: enforce three-level visibility against the
                // canonical defining item. Following the `pub import` chain
                // ensures re-exports are transparent — E0222 fires based on
                // where the item is really defined, not the re-exporter's
                // location. Submodule bindings (`binds_submodule`) have no
                // item visibility to check.
                let (def_path, def_name) =
                    match module::canonical_origin(tree, &imp.path, &item.name) {
                        Some(p) => p,
                        None => (imp.path.clone(), item.name.clone()),
                    };
                if let Some(vis) = module_item_visibility(tree, &imp.path, &item.name) {
                    if !visibility_allows_access(vis, &def_path, &importer_path) {
                        let def_label = if def_path.is_empty() {
                            "<crate root>".to_string()
                        } else {
                            def_path.join(".")
                        };
                        let message = format!(
                            "`{}` in module `{}` is `private` — visible only to files in the same directory",
                            def_name, def_label,
                        );
                        self.errors.push(ResolveError {
                            message,
                            span: item.span.clone(),
                            kind: ResolveErrorKind::PrivateItemAccess,
                            suggestion: Some(format!(
                                "mark `{}` as `pub` or move the caller into the same directory",
                                def_name
                            )),
                            replacement: None,
                        });
                    }
                }
            }

            // Slice 7: the bound symbol records the *canonical* path so that
            // re-exports are transparent to downstream phases — method
            // resolution, trait coherence, and typechecker cross-module
            // lookups all see a single identity regardless of which alias
            // the name travelled through.
            let canonical_full = if binds_item {
                module::canonical_origin(tree, &imp.path, &item.name)
                    .map(|(mut path, name)| {
                        path.push(name);
                        path
                    })
                    .unwrap_or_else(|| full.clone())
            } else {
                full.clone()
            };
            let bound = item.alias.clone().unwrap_or_else(|| item.name.clone());
            if let Err(e) = self.table.define(
                bound,
                SymbolKind::Import {
                    path: canonical_full,
                },
                item.span.clone(),
                imp.is_pub,
            ) {
                self.errors.push(e);
            }
        }
    }

    fn collect_extern_function(&mut self, e: &ExternFunction) {
        if let Err(err) = self.table.define(
            e.name.clone(),
            SymbolKind::ExternFunction,
            e.span.clone(),
            true,
        ) {
            self.errors.push(err);
        }
    }

    fn collect_opaque_foreign_type(&mut self, o: &OpaqueTypeDecl) {
        if let Err(err) = self.table.define(
            o.name.clone(),
            SymbolKind::OpaqueForeignType,
            o.span.clone(),
            true,
        ) {
            self.errors.push(err);
        }
    }

    /// Extract a simple name from a type expression (for impl block target types).
    pub(crate) fn type_expr_name(&self, ty: &TypeExpr) -> Option<String> {
        match &ty.kind {
            TypeKind::Path(path) => path.segments.last().cloned(),
            _ => None,
        }
    }

    /// Whether `name` is declared as an associated function on at least one
    /// visible trait. Used to suppress the undefined-name error for bare
    /// identifier callees that the typechecker can dispatch via expected-type
    /// inference (e.g. `let x: T = default()` where `T: Default`).
    pub(crate) fn is_trait_assoc_fn_name(&self, name: &str) -> bool {
        for item in &self.program.items {
            if let Item::TraitDef(t) = item {
                for ti in &t.items {
                    if let TraitItem::Method(m) = ti {
                        if m.name == name && m.self_param.is_none() {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}
