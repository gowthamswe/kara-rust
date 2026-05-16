//! Validation for `#[derive(...)]` attributes.
//!
//! Houses `type_supports_*` predicates (per-trait field-shape checks)
//! and the `validate_derive_*` driver methods that enforce the
//! derive rules on struct / enum definitions. Lives in a sibling
//! `impl<'a> super::TypeChecker<'a>` block so the methods stay on
//! `TypeChecker` (preserving the `self.validate_derive_*` call shape
//! from `super::check()`).

use crate::ast::*;

use super::types::{is_numeric, type_display, Type, VariantTypeInfo};
use super::{extract_derived_traits, TypeErrorKind};

impl<'a> super::TypeChecker<'a> {
    /// Check whether a type supports `==` / `!=` (PartialEq).
    /// All primitives including floats support PartialEq.
    /// Named types (structs/enums) require `#[derive(Eq)]` or `#[derive(PartialEq)]`.
    pub(super) fn type_supports_partial_eq(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_)
            | Type::UInt(_)
            | Type::Float(_)
            | Type::Bool
            | Type::Char
            | Type::Str
            | Type::Unit => true,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_partial_eq(e)),
            Type::Array { element, .. } => self.type_supports_partial_eq(element),
            Type::Slice { element, .. } => self.type_supports_partial_eq(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_partial_eq(inner),
            Type::Named { name, args } => {
                // A user-provided `impl Eq for Name` is sufficient — the
                // lowering pass dispatches `==`/`!=` through it. Falls back
                // to `#[derive(Eq)]`/`#[derive(PartialEq)]` when no impl is
                // registered (e.g. for compiler-provided structural eq on
                // built-in enums like `Option`/`Result`).
                if self.env.has_impl("Eq", name, args) {
                    return true;
                }
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Eq") || info.derived_traits.contains("PartialEq")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Eq") || info.derived_traits.contains("PartialEq")
                } else {
                    true
                }
            }
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_partial_eq(inner),
            Type::Shared(name) => {
                if self.env.has_impl("Eq", name, &[]) {
                    return true;
                }
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Eq") || info.derived_traits.contains("PartialEq")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Eq") || info.derived_traits.contains("PartialEq")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    /// Check whether a type supports full `Eq` (required for Map/Set keys, etc.).
    /// Floats (f32/f64) do NOT support Eq due to IEEE 754 NaN != NaN.
    /// Named types require `#[derive(Eq)]`.
    pub(super) fn type_supports_eq(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_) | Type::UInt(_) | Type::Bool | Type::Char | Type::Str | Type::Unit => true,
            // f32/f64 follow IEEE 754: NaN != NaN, so they don't implement Eq
            Type::Float(_) => false,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_eq(e)),
            Type::Array { element, .. } => self.type_supports_eq(element),
            Type::Slice { element, .. } => self.type_supports_eq(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_eq(inner),
            Type::Named { name, .. } => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Eq")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Eq")
                } else {
                    // Unknown type — permissive to avoid cascading errors
                    // when the resolver has already flagged it.
                    true
                }
            }
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_eq(inner),
            Type::Shared(name) => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Eq")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Eq")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    /// Check whether a type supports `Hash`. Floats do not — NaN-as-key would
    /// break the hash/eq contract. Named types require `#[derive(Hash)]`.
    pub(super) fn type_supports_hash(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_) | Type::UInt(_) | Type::Bool | Type::Char | Type::Str | Type::Unit => true,
            Type::Float(_) => false,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_hash(e)),
            Type::Array { element, .. } => self.type_supports_hash(element),
            Type::Slice { element, .. } => self.type_supports_hash(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_hash(inner),
            Type::Named { name, .. } => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Hash")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Hash")
                } else {
                    true
                }
            }
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_hash(inner),
            Type::Shared(name) => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Hash")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Hash")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    /// Check whether a type supports total `Ord`. Floats do not (see Eq).
    pub(super) fn type_supports_ord(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_) | Type::UInt(_) | Type::Bool | Type::Char | Type::Str | Type::Unit => true,
            Type::Float(_) => false,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_ord(e)),
            Type::Array { element, .. } => self.type_supports_ord(element),
            Type::Slice { element, .. } => self.type_supports_ord(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_ord(inner),
            Type::Named { name, .. } => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Ord")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Ord")
                } else {
                    true
                }
            }
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_ord(inner),
            Type::Shared(name) => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Ord")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Ord")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    /// Check whether a type implements `Display`.
    /// All primitives support Display. Built-in containers (Vec, Map, SortedSet,
    /// Option, Result) support Display when their type arguments do.
    /// Named user types require `#[derive(Display)]`.
    pub(super) fn type_supports_display(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_)
            | Type::UInt(_)
            | Type::Float(_)
            | Type::Bool
            | Type::Char
            | Type::Str
            | Type::Unit => true,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_display(e)),
            Type::Array { element, .. } => self.type_supports_display(element),
            Type::Slice { element, .. } => self.type_supports_display(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_display(inner),
            Type::Named { name, args } => match name.as_str() {
                "Vec" | "Option" | "SortedSet" | "Set" if args.len() == 1 => {
                    self.type_supports_display(&args[0])
                }
                "Map" | "Result" if args.len() == 2 => {
                    self.type_supports_display(&args[0]) && self.type_supports_display(&args[1])
                }
                _ => {
                    if self.env.has_impl("Display", name, args) {
                        return true;
                    }
                    if let Some(info) = self.env.structs.get(name) {
                        info.derived_traits.contains("Display")
                    } else if let Some(info) = self.env.enums.get(name) {
                        info.derived_traits.contains("Display")
                    } else {
                        true
                    }
                }
            },
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_display(inner),
            Type::Shared(name) => {
                if self.env.has_impl("Display", name, &[]) {
                    return true;
                }
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Display")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Display")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }

    /// Check whether a type supports `PartialOrd` (admits NaN for floats).
    pub(super) fn type_supports_partial_ord(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_)
            | Type::UInt(_)
            | Type::Float(_)
            | Type::Bool
            | Type::Char
            | Type::Str
            | Type::Unit => true,
            Type::Tuple(elems) => elems.iter().all(|e| self.type_supports_partial_ord(e)),
            Type::Array { element, .. } => self.type_supports_partial_ord(element),
            Type::Slice { element, .. } => self.type_supports_partial_ord(element),
            Type::Ref(inner) | Type::MutRef(inner) => self.type_supports_partial_ord(inner),
            Type::Named { name, .. } => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("PartialOrd")
                        || info.derived_traits.contains("Ord")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("PartialOrd")
                        || info.derived_traits.contains("Ord")
                } else {
                    true
                }
            }
            Type::Rc(inner) | Type::Arc(inner) => self.type_supports_partial_ord(inner),
            Type::Shared(name) => {
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("PartialOrd")
                        || info.derived_traits.contains("Ord")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("PartialOrd")
                        || info.derived_traits.contains("Ord")
                } else {
                    true
                }
            }
            Type::TypeParam(_) | Type::TypeVar(_) | Type::AssocProjection { .. } | Type::Error => {
                true
            }
            Type::Never => true,
            Type::Function { .. }
            | Type::OnceFunction { .. }
            | Type::Pointer { .. }
            | Type::Weak(_) => false,
        }
    }
    /// Returns `true` when `ty` is a distinct type that derives `Arithmetic`.
    pub(super) fn distinct_type_has_arithmetic(&self, ty: &Type) -> bool {
        if let Type::Named { name, args } = ty {
            if args.is_empty() {
                return self
                    .env
                    .distinct_types
                    .get(name)
                    .is_some_and(|t| t.contains("Arithmetic"));
            }
        }
        false
    }

    /// Check whether a type is Copy (primitive or derives Copy).
    pub(super) fn is_type_copy(&self, ty: &Type) -> bool {
        match ty {
            Type::Int(_)
            | Type::UInt(_)
            | Type::Float(_)
            | Type::Bool
            | Type::Char
            | Type::Unit
            | Type::Never
            | Type::Error => true,
            Type::Tuple(types) => types.iter().all(|t| self.is_type_copy(t)),
            // Array[T, N] is Copy iff T is Copy.
            Type::Array { element, .. } => self.is_type_copy(element),
            // Slice[T] is unconditionally Copy; mut Slice[T] is not.
            Type::Slice { mutable, .. } => !mutable,
            Type::Named { name, args } => {
                // Option[T] / Result[T, E] are Copy when all type args are Copy.
                if matches!(name.as_str(), "Option" | "Result") {
                    return args.iter().all(|a| self.is_type_copy(a));
                }
                if let Some(info) = self.env.structs.get(name) {
                    info.derived_traits.contains("Copy")
                } else if let Some(info) = self.env.enums.get(name) {
                    info.derived_traits.contains("Copy")
                } else if let Some(traits) = self.env.distinct_types.get(name) {
                    traits.contains("Copy")
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Validate that #[derive(Copy)] structs/enums have all-Copy fields, and
    /// that distinct types with #[derive(Copy)] have a Copy base type.
    pub(super) fn validate_derive_copy(&mut self) {
        self.validate_derived_trait("Copy", |this, ty| this.is_type_copy(ty));
        // Check distinct types: base type must be Copy.
        let distinct_items: Vec<_> = self
            .program
            .items
            .iter()
            .filter_map(|item| {
                if let Item::DistinctType(d) = item {
                    let traits = extract_derived_traits(&d.attributes);
                    if traits.contains("Copy") {
                        return Some((d.name.clone(), d.span.clone(), d.base_type.clone()));
                    }
                }
                None
            })
            .collect();
        for (name, span, base_ty_expr) in distinct_items {
            let base_ty = self.lower_type_expr(&base_ty_expr, &[]);
            if !self.is_type_copy(&base_ty) {
                self.type_error(
                    format!(
                        "distinct type '{}' derives Copy but its base type '{}' is not Copy",
                        name,
                        type_display(&base_ty)
                    ),
                    span,
                    TypeErrorKind::TypeMismatch,
                );
            }
        }
    }

    /// Validate that every type deriving Copy also derives Clone.
    pub(super) fn validate_copy_implies_clone(&mut self) {
        let items: Vec<_> = self.program.items.clone();
        for item in &items {
            match item {
                Item::StructDef(s) => {
                    let traits = extract_derived_traits(&s.attributes);
                    if traits.contains("Copy") && !traits.contains("Clone") {
                        self.type_error(
                            format!(
                                "struct '{}' derives Copy but not Clone; Copy requires Clone",
                                s.name
                            ),
                            s.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
                Item::EnumDef(e) => {
                    let traits = extract_derived_traits(&e.attributes);
                    if traits.contains("Copy") && !traits.contains("Clone") {
                        self.type_error(
                            format!(
                                "enum '{}' derives Copy but not Clone; Copy requires Clone",
                                e.name
                            ),
                            e.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
                Item::DistinctType(d) => {
                    let traits = extract_derived_traits(&d.attributes);
                    if traits.contains("Copy") && !traits.contains("Clone") {
                        self.type_error(
                            format!(
                                "distinct type '{}' derives Copy but not Clone; Copy requires Clone",
                                d.name
                            ),
                            d.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Validate that every `#[derive(Trait)]` on a struct/enum implies all
    /// fields recursively support `Trait`. Reports one diagnostic per
    /// offending field. `Copy` is handled separately via `validate_derive_copy`
    /// so the message can reference `is_type_copy`'s broader rules.
    pub(super) fn validate_derived_traits_recursive(&mut self) {
        self.validate_derived_trait("Eq", |this, ty| this.type_supports_eq(ty));
        self.validate_derived_trait("PartialEq", |this, ty| this.type_supports_partial_eq(ty));
        self.validate_derived_trait("Hash", |this, ty| this.type_supports_hash(ty));
        self.validate_derived_trait("Ord", |this, ty| this.type_supports_ord(ty));
        self.validate_derived_trait("PartialOrd", |this, ty| this.type_supports_partial_ord(ty));
        self.validate_derive_display_on_enums();
    }

    /// Compound-payload enum codegen (Slice CP, CP5 carve-out) —
    /// reject enum variants whose payload field type is itself a
    /// (non-shared) user enum. v1 ships single-level enum-payload
    /// nesting only; the layout pass cannot size a recursively-nested
    /// payload area without an infinite-recursion guard, and the
    /// canonical workaround is to wrap the inner enum in `Vec`,
    /// `shared` (RC pointer), or a `Box`-style indirection. Recursion
    /// through `Vec[T]`, `Slice[T]`, tuples, or `shared` enums is
    /// fine — those layers stop the size recursion at one indirection.
    pub(super) fn validate_enum_payload_no_nested_enum(&mut self) {
        // Collect enum names for the carve-out check. `shared` enums
        // are heap-allocated via RC, so a payload field of type
        // `SharedFoo` is a single pointer word and is allowed.
        let value_enum_names: std::collections::HashSet<String> = self
            .program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::EnumDef(e) if !e.is_shared => Some(e.name.clone()),
                _ => None,
            })
            .collect();

        // Walk every enum variant and inspect its payload field types.
        // The payload field's `TypeExpr` -> head segment is the
        // user-visible type name; if that name is a value enum, emit
        // the diagnostic. We only flag the *direct* head; recursion
        // through `Vec[Inner]` etc. is intentionally allowed
        // (CP5 carve-out is about size-recursion, not name presence).
        let items: Vec<_> = self.program.items.clone();
        for item in &items {
            if let Item::EnumDef(e) = item {
                if e.is_shared {
                    continue;
                }
                for variant in &e.variants {
                    let field_tys: Vec<&TypeExpr> = match &variant.kind {
                        VariantKind::Unit => Vec::new(),
                        VariantKind::Tuple(tys) => tys.iter().collect(),
                        VariantKind::Struct(fields) => fields.iter().map(|f| &f.ty).collect(),
                    };
                    for ty in field_tys {
                        if let TypeKind::Path(path) = &ty.kind {
                            if let Some(head) = path.segments.first() {
                                if value_enum_names.contains(head) {
                                    self.type_error(
                                        format!(
                                            "error[E_ENUM_NESTED_ENUM_PAYLOAD]: enum variant \
                                             '{}.{}' has a payload of nested enum type '{}' — \
                                             v1 only supports up to one level of enum nesting; \
                                             either flatten the variant, mark the inner enum as \
                                             `shared` (RC pointer), or wrap it in a `Vec` / \
                                             collection layer",
                                            e.name, variant.name, head
                                        ),
                                        variant.span.clone(),
                                        TypeErrorKind::TypeMismatch,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// `#[derive(Display)]` on enums only works for all-unit-variant enums.
    /// Reject any enum that has a tuple or struct variant.
    pub(super) fn validate_derive_display_on_enums(&mut self) {
        let display_enums: Vec<_> = self
            .env
            .enums
            .iter()
            .filter(|(_, info)| info.derived_traits.contains("Display"))
            .map(|(name, info)| (name.clone(), info.clone()))
            .collect();

        for (name, info) in display_enums {
            let enum_span = self.program.items.iter().find_map(|item| {
                if let Item::EnumDef(e) = item {
                    if e.name == name {
                        return Some(e.span.clone());
                    }
                }
                None
            });
            let Some(span) = enum_span else {
                continue;
            };
            for (variant_name, variant_info) in &info.variants {
                if !matches!(variant_info, VariantTypeInfo::Unit) {
                    self.type_error(
                        format!(
                            "enum '{}' derives Display but variant '{}' is not a unit variant; \
                             #[derive(Display)] only works on all-unit-variant enums — \
                             implement Display manually for enums with data variants",
                            name, variant_name
                        ),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
            }
        }
    }

    /// Validate `#[derive(Arithmetic)]` usage:
    /// - Reject on struct/enum (must use manual impls).
    /// - Reject on distinct types whose base type is non-numeric.
    pub(super) fn validate_derive_arithmetic(&mut self) {
        let items: Vec<_> = self.program.items.clone();
        for item in &items {
            match item {
                Item::StructDef(s)
                    if extract_derived_traits(&s.attributes).contains("Arithmetic") =>
                {
                    self.type_error(
                        format!(
                            "#[derive(Arithmetic)] is only valid on `distinct type`, not on \
                             struct '{}'; use manual trait impls for structs",
                            s.name
                        ),
                        s.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                Item::EnumDef(e)
                    if extract_derived_traits(&e.attributes).contains("Arithmetic") =>
                {
                    self.type_error(
                        format!(
                            "#[derive(Arithmetic)] is only valid on `distinct type`, not on \
                             enum '{}'; use manual trait impls for enums",
                            e.name
                        ),
                        e.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                Item::DistinctType(d)
                    if extract_derived_traits(&d.attributes).contains("Arithmetic") =>
                {
                    let base_ty = self.lower_type_expr(&d.base_type, &[]);
                    if !is_numeric(&base_ty) {
                        self.type_error(
                            format!(
                                "distinct type '{}' derives Arithmetic but its base type \
                                 '{}' is not numeric",
                                d.name,
                                type_display(&base_ty)
                            ),
                            d.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Walk every struct/enum that derives `trait_name`; emit a diagnostic for
    /// each field whose type fails `supports`. Skips types that aren't in the
    /// program AST — those are compiler-provided built-ins (`F32`, `F64`,
    /// `Ordering`, `MemoryOrdering`) whose derived-trait bundles are
    /// hand-verified.
    pub(super) fn validate_derived_trait(
        &mut self,
        trait_name: &str,
        supports: impl Fn(&Self, &Type) -> bool,
    ) {
        let structs: Vec<_> = self
            .env
            .structs
            .iter()
            .filter(|(_, info)| info.derived_traits.contains(trait_name))
            .map(|(name, info)| (name.clone(), info.clone()))
            .collect();

        for (name, info) in structs {
            let struct_span = self.program.items.iter().find_map(|item| {
                if let Item::StructDef(s) = item {
                    if s.name == name {
                        return Some(s.span.clone());
                    }
                }
                None
            });
            let Some(struct_span) = struct_span else {
                continue;
            };
            for (field_name, field_ty, _) in &info.fields {
                if !supports(self, field_ty) {
                    let span = struct_span.clone();
                    self.type_error(
                        format!(
                            "struct '{}' derives {} but field '{}' has non-{} type '{}'",
                            name,
                            trait_name,
                            field_name,
                            trait_name,
                            type_display(field_ty)
                        ),
                        span,
                        TypeErrorKind::TypeMismatch,
                    );
                }
            }
        }

        let enums: Vec<_> = self
            .env
            .enums
            .iter()
            .filter(|(_, info)| info.derived_traits.contains(trait_name))
            .map(|(name, info)| (name.clone(), info.clone()))
            .collect();

        for (name, info) in enums {
            let enum_span = self.program.items.iter().find_map(|item| {
                if let Item::EnumDef(e) = item {
                    if e.name == name {
                        return Some(e.span.clone());
                    }
                }
                None
            });
            let Some(enum_span) = enum_span else {
                continue;
            };
            for (variant_name, variant_info) in &info.variants {
                let bad_fields: Vec<(String, Type)> = match variant_info {
                    VariantTypeInfo::Unit => Vec::new(),
                    VariantTypeInfo::Tuple(types) => types
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| !supports(self, t))
                        .map(|(i, t)| (i.to_string(), t.clone()))
                        .collect(),
                    VariantTypeInfo::Struct(fields) => fields
                        .iter()
                        .filter(|(_, t)| !supports(self, t))
                        .map(|(n, t)| (n.clone(), t.clone()))
                        .collect(),
                };
                for (field_ref, field_ty) in bad_fields {
                    self.type_error(
                        format!(
                            "enum '{}' derives {} but variant '{}' field '{}' has non-{} type '{}'",
                            name,
                            trait_name,
                            variant_name,
                            field_ref,
                            trait_name,
                            type_display(&field_ty)
                        ),
                        enum_span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
            }
        }
    }
}
