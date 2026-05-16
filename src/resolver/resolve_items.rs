//! Pass 2: per-item resolution.
//!
//! After `collect_top_level_items` registers every declaration in
//! the symbol table, this pass revisits the items and resolves
//! references inside their bodies — function bodies, trait method
//! defaults, impl method bodies, const initializers, type alias
//! targets, effect-group references, etc.
//!
//! Houses the dispatcher `resolve_items` + every per-item-form
//! resolver (`resolve_function`, `resolve_struct_def`, `resolve_enum_def`,
//! `resolve_enum_variants`, `resolve_trait_def`, `resolve_impl_block`,
//! `resolve_const_decl`, `resolve_type_alias_def`,
//! `resolve_extern_function`, `resolve_effect_group_def`) plus the
//! three trait-restriction checks (`check_into_trait_restriction`,
//! `check_impl_level_effect_vars`, `check_operator_trait_restriction`)
//! that fire during `resolve_impl_block` / `resolve_trait_def`.
//!
//! Lives in a sibling `impl<'a> super::Resolver<'a>` block.

use std::collections::HashMap;

use crate::ast::*;

use super::{ResolveError, ResolveErrorKind, ScopeKind, SymbolId, SymbolKind};

impl<'a> super::Resolver<'a> {
    // ── Pass 2: Resolve all items ───────────────────────────────

    pub(crate) fn resolve_items(&mut self) {
        // Clone items to avoid borrow conflict
        let items: Vec<Item> = self.program.items.clone();
        for item in &items {
            match item {
                Item::Function(f) => self.resolve_function(f),
                Item::StructDef(s) => self.resolve_struct_def(s),
                Item::EnumDef(e) => self.resolve_enum_def(e),
                Item::TraitDef(t) => self.resolve_trait_def(t),
                Item::ImplBlock(i) => self.resolve_impl_block(i),
                Item::ConstDecl(c) => self.resolve_const_decl(c),
                Item::TypeAlias(t) => self.resolve_type_alias_def(t),
                Item::ExternFunction(e) => self.resolve_extern_function(e),
                Item::ExternBlock(b) => {
                    for it in &b.items {
                        match it {
                            ExternItem::Function(f) => self.resolve_extern_function(f),
                            // Opaque foreign type declarations have no
                            // body to resolve — the name was registered
                            // in the collection pass; nothing else to do.
                            ExternItem::OpaqueType(_) => {}
                        }
                    }
                }
                Item::EffectGroup(g) => self.resolve_effect_group_def(g),
                _ => {}
            }
        }
    }

    fn resolve_function(&mut self, f: &Function) {
        self.table.push_scope(ScopeKind::Function);

        // Register generic type params (with inline trait bounds). Where-clause
        // bounds, if any, are merged into the same per-param bound list below.
        let params_by_name = if let Some(ref generics) = f.generic_params {
            self.define_generic_params(generics)
        } else {
            HashMap::new()
        };
        if let Some(ref wc) = f.where_clause {
            self.resolve_where_clause(wc, &params_by_name);
        }

        // Register self if present
        if f.self_param.is_some() {
            let _ = self.table.define(
                "self".to_string(),
                SymbolKind::SelfValue,
                f.span.clone(),
                false,
            );
        }

        // Register parameters
        for param in &f.params {
            self.define_pattern_bindings(&param.pattern, false);
            self.resolve_type_expr(&param.ty);
        }

        // Resolve return type
        if let Some(ref ret_ty) = f.return_type {
            self.resolve_type_expr(ret_ty);
        }

        // Resolve effect annotations
        if let Some(ref effects) = f.effects {
            self.resolve_effect_list(effects);
        }

        // Resolve body
        self.resolve_block(&f.body);

        self.table.pop_scope();
    }

    fn resolve_struct_def(&mut self, s: &StructDef) {
        if let Some(ref generics) = s.generic_params {
            self.table.push_scope(ScopeKind::Block);
            let params_by_name = self.define_generic_params(generics);
            if let Some(ref wc) = s.where_clause {
                self.resolve_where_clause(wc, &params_by_name);
            }
            for field in &s.fields {
                self.resolve_type_expr(&field.ty);
            }
            self.table.pop_scope();
        } else {
            for field in &s.fields {
                self.resolve_type_expr(&field.ty);
            }
        }
    }

    fn resolve_enum_def(&mut self, e: &EnumDef) {
        if let Some(ref generics) = e.generic_params {
            self.table.push_scope(ScopeKind::Block);
            let params_by_name = self.define_generic_params(generics);
            if let Some(ref wc) = e.where_clause {
                self.resolve_where_clause(wc, &params_by_name);
            }
            self.resolve_enum_variants(&e.variants);
            self.table.pop_scope();
        } else {
            self.resolve_enum_variants(&e.variants);
        }
    }

    fn resolve_enum_variants(&mut self, variants: &[Variant]) {
        for variant in variants {
            match &variant.kind {
                VariantKind::Tuple(types) => {
                    for ty in types {
                        self.resolve_type_expr(ty);
                    }
                }
                VariantKind::Struct(fields) => {
                    for field in fields {
                        self.resolve_type_expr(&field.ty);
                    }
                }
                VariantKind::Unit => {}
            }
        }
    }

    fn resolve_trait_def(&mut self, t: &TraitDef) {
        // Push a trait-level scope that exposes `Self` (and any trait-level
        // generic params) to every method signature and default body.
        self.table.push_scope(ScopeKind::Block);
        let self_id = self
            .table
            .define(
                "Self".to_string(),
                SymbolKind::TypeParam,
                t.span.clone(),
                false,
            )
            .ok();
        // Supertrait constraints (`trait Foo: Bar + Baz`) are bounds on `Self`
        // — every `Self` value is also a `Bar` and a `Baz`. Recording them
        // here lets the typechecker dispatch `Self.method()` calls to
        // supertrait methods and bare `method()` calls in default bodies via
        // the same trait-bound machinery.
        if let Some(id) = self_id {
            self.table.record_generic_bounds(id, &t.supertraits);
        }
        for bound in &t.supertraits {
            self.resolve_trait_bound(bound);
        }
        let mut params_by_name: HashMap<String, SymbolId> = HashMap::new();
        if let Some(ref generics) = t.generic_params {
            params_by_name = self.define_generic_params(generics);
        }
        if let Some(ref wc) = t.where_clause {
            self.resolve_where_clause(wc, &params_by_name);
        }

        for item in &t.items {
            match item {
                TraitItem::Method(method) => {
                    self.table.push_scope(ScopeKind::Function);

                    // Method-level generic params + where clause.
                    let method_params = if let Some(ref mg) = method.generic_params {
                        self.define_generic_params(mg)
                    } else {
                        HashMap::new()
                    };
                    if let Some(ref wc) = method.where_clause {
                        self.resolve_where_clause(wc, &method_params);
                    }

                    // Register self if present
                    if method.self_param.is_some() {
                        let _ = self.table.define(
                            "self".to_string(),
                            SymbolKind::SelfValue,
                            method.span.clone(),
                            false,
                        );
                    }

                    for param in &method.params {
                        self.define_pattern_bindings(&param.pattern, false);
                        self.resolve_type_expr(&param.ty);
                    }

                    if let Some(ref ret_ty) = method.return_type {
                        self.resolve_type_expr(ret_ty);
                    }

                    if let Some(ref effects) = method.effects {
                        self.resolve_effect_list(effects);
                    }

                    // Resolve default method body if present
                    if let Some(ref body) = method.body {
                        self.resolve_block_no_scope(body);
                    }

                    self.table.pop_scope();
                }
                TraitItem::AssocType(_assoc) => {
                    // Associated type declarations are collected but not resolved here.
                    // Bounds on associated types are checked during type checking.
                }
            }
        }

        self.table.pop_scope();
    }

    /// Reject `impl <OperatorTrait> for <UserType>` in v1 — operator traits
    /// (Add/Sub/Eq/etc.) are stdlib-only. Lifting the restriction is a
    /// one-line edit (remove or shrink `OPERATOR_TRAIT_NAMES`).
    /// `From`/`Into` are NOT operator traits — user impls are required for
    /// `?` cross-error propagation and stay allowed.
    /// Reject `impl Into[T] for S` and `impl TryInto[T] for S`. The design
    /// models these as blanket impls derived from `From` / `TryFrom`; a direct
    /// impl would conflict with the blanket and break the `x.into()` lowering.
    /// User must write `impl From[S] for T` (or `impl TryFrom[S] for T`) instead.
    fn check_into_trait_restriction(&mut self, trait_path: &PathExpr) {
        let trait_name = match trait_path.segments.last() {
            Some(name) => name.as_str(),
            None => return,
        };
        let source_trait = match trait_name {
            "Into" => "From",
            "TryInto" => "TryFrom",
            _ => return,
        };
        self.errors.push(ResolveError {
            message: format!(
                "user-defined `impl {trait_name} for T` is not allowed; \
                 `{trait_name}` is derived from `{source_trait}` via a blanket impl"
            ),
            span: trait_path.span.clone(),
            kind: ResolveErrorKind::IntoTraitImplNotAllowed,
            suggestion: Some(format!(
                "implement `{source_trait}` instead; `x.into()` will dispatch through it"
            )),
            replacement: None,
        });
    }

    /// Reject `impl[T, U, with E] Trait[U] for T { ... }` and any other
    /// impl block that binds a named effect variable at the impl level.
    /// Effect polymorphism on trait methods is expressed by declaring the
    /// method `with _` on the trait; impl-level binding would imply a
    /// per-monomorphization rewrite that the language does not model.
    fn check_impl_level_effect_vars(&mut self, imp: &ImplBlock) {
        let generics = match &imp.generic_params {
            Some(g) if !g.effect_params.is_empty() => g,
            _ => return,
        };
        let var_list = generics
            .effect_params
            .iter()
            .map(|n| format!("`{}`", n))
            .collect::<Vec<_>>()
            .join(", ");
        self.errors.push(ResolveError {
            message: format!(
                "impl-level effect variables ({var_list}) are not supported; \
                 use `with _` on the trait method instead"
            ),
            span: generics.span.clone(),
            kind: ResolveErrorKind::ImplLevelEffectVarNotAllowed,
            suggestion: Some(
                "remove the `with E` from the impl's generic parameters and declare the \
                 trait method `with _` so impls may carry any effects"
                    .to_string(),
            ),
            replacement: None,
        });
    }

    fn check_operator_trait_restriction(&mut self, trait_path: &PathExpr, target: &TypeExpr) {
        const OPERATOR_TRAIT_NAMES: &[&str] = &[
            "Add", "Sub", "Mul", "Div", "Rem", "Neg", "Eq", "Ord", "BitAnd", "BitOr", "BitXor",
            "Shl", "Shr", "Not", "Index", "IndexMut", "Display",
        ];
        const STDLIB_ALLOWLIST: &[&str] = &[
            "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "usize", "f32", "f64", "bool",
            "char", "String", "F32", "F64",
        ];

        let trait_name = match trait_path.segments.last() {
            Some(name) => name.as_str(),
            None => return,
        };
        if !OPERATOR_TRAIT_NAMES.contains(&trait_name) {
            return;
        }

        let target_name = self.type_expr_name(target).unwrap_or_default();
        if STDLIB_ALLOWLIST.contains(&target_name.as_str()) {
            return;
        }

        // Carve-out: relational operator traits (`Eq`, `Ord`) may be implemented
        // on user-defined types. User types routinely need custom equality and
        // ordering (map keys, domain-model invariants); the general stdlib-only
        // restriction is too strict. Arithmetic, bitwise, and indexing traits
        // stay restricted until the "heterogeneous Rhs / Output" design lands.
        const RELATIONAL_TRAITS: &[&str] = &["Eq", "Ord"];
        if RELATIONAL_TRAITS.contains(&trait_name) {
            return;
        }

        // Vec[T] gets a tailored hint pointing at the explicit alternatives.
        let (message, suggestion) = if trait_name == "Add" && target_name == "Vec" {
            (
                "`impl Add for Vec[T]` is not supported by design".to_string(),
                Some("use `.concat(other)` for concatenation or `.extend(other)` for in-place append".to_string()),
            )
        } else {
            (
                format!(
                    "user-defined `impl {trait_name} for {target_name}` is not supported in v1; \
                     operator traits are stdlib-only"
                ),
                Some(format!(
                    "remove the impl block; arithmetic and comparison are dispatched through stdlib `{trait_name}` impls"
                )),
            )
        };
        self.errors.push(ResolveError {
            message,
            span: trait_path.span.clone(),
            kind: ResolveErrorKind::OperatorTraitImplRestricted,
            suggestion,
            replacement: None,
        });
    }

    fn resolve_impl_block(&mut self, imp: &ImplBlock) {
        let type_name = self.type_expr_name(&imp.target_type).unwrap_or_default();

        self.current_impl_type = Some(type_name.clone());
        self.table.push_scope(ScopeKind::Impl {
            target_type: type_name,
        });

        // Register impl-level generic params before resolving target/trait types
        let params_by_name = if let Some(ref generics) = imp.generic_params {
            self.define_generic_params(generics)
        } else {
            HashMap::new()
        };
        if let Some(ref wc) = imp.where_clause {
            self.resolve_where_clause(wc, &params_by_name);
        }

        // Resolve target type (may reference impl generic params)
        self.resolve_type_expr(&imp.target_type);

        // Resolve trait name
        if let Some(ref trait_path) = imp.trait_name {
            self.resolve_path_expr(trait_path);
            self.check_operator_trait_restriction(trait_path, &imp.target_type);
            self.check_into_trait_restriction(trait_path);
        }
        self.check_impl_level_effect_vars(imp);

        // Register Self as a type
        let _ = self.table.define(
            "Self".to_string(),
            SymbolKind::TypeParam,
            imp.span.clone(),
            false,
        );

        for item in &imp.items {
            match item {
                ImplItem::Method(method) => self.resolve_function(method),
                ImplItem::AssocType(binding) => {
                    self.resolve_type_expr(&binding.ty);
                }
            }
        }

        self.table.pop_scope();
        self.current_impl_type = None;
    }

    fn resolve_const_decl(&mut self, c: &ConstDecl) {
        self.resolve_type_expr(&c.ty);
        self.resolve_expr(&c.value);
    }

    fn resolve_type_alias_def(&mut self, t: &TypeAliasDef) {
        // Register generic params in a temp scope
        if let Some(ref generics) = t.generic_params {
            self.table.push_scope(ScopeKind::Block);
            self.define_generic_params(generics);
            self.resolve_type_expr(&t.ty);
            self.table.pop_scope();
        } else {
            self.resolve_type_expr(&t.ty);
        }
    }

    fn resolve_extern_function(&mut self, e: &ExternFunction) {
        self.table.push_scope(ScopeKind::Function);
        for param in &e.params {
            self.define_pattern_bindings(&param.pattern, false);
            self.resolve_type_expr(&param.ty);
        }
        if let Some(ref ret_ty) = e.return_type {
            self.resolve_type_expr(ret_ty);
        }
        if let Some(ref effects) = e.effects {
            self.resolve_effect_list(effects);
        }
        self.table.pop_scope();
    }

    fn resolve_effect_group_def(&mut self, g: &EffectGroupDecl) {
        for term in &g.body {
            match term {
                EffectGroupTerm::Verb(verb) => {
                    self.resolve_effect_verb(verb);
                }
                EffectGroupTerm::GroupRef(name) => {
                    if self.table.lookup(name).is_none() {
                        self.error_undefined_name(name, g.span.clone());
                    }
                }
            }
        }
    }
}
