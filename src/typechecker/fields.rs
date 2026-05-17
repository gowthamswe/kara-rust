//! Field access and struct-literal inference.
//!
//! Houses `infer_field_access` (with primitive-type associated-constant
//! lookup), `check_cross_module_field_access` (CR-24 slice 6b
//! private-field visibility check), `infer_imported_field_access`
//! (imported-struct field-access surface), and `infer_struct_literal`
//! (positional/named field literal inference, shared-struct
//! recognition).

use crate::ast::*;
use crate::token::Span;
use std::collections::HashSet;

use super::const_eval::primitive_const_type;
use super::types::Type;
use super::{find_struct_def, TypeErrorKind};

impl<'a> super::TypeChecker<'a> {
    pub(super) fn infer_field_access(&mut self, object: &Expr, field: &str, span: &Span) -> Type {
        // Primitive-type associated constants — `i64.MAX`, `f64.INFINITY`,
        // `usize.MAX`, etc. The parser emits these as
        // `FieldAccess { object: Identifier("<primitive>"), field: "<NAME>" }`.
        // Intercept here before `infer_expr(object)` would silently return
        // `Type::Error` for the bare primitive identifier. The lookup
        // returns the const's typed surface so downstream code (`let x =
        // i64.MAX;`) sees the right `Type::Int(I64)` etc.
        if let ExprKind::Identifier(name) = &object.kind {
            if let Some(cv) = crate::prelude::lookup_primitive_const(name, field) {
                return primitive_const_type(cv);
            }
        }
        let obj_ty = self.infer_expr(object);
        if obj_ty == Type::Error {
            return Type::Error;
        }

        // Slice 1b: opaque foreign types (`unsafe extern { type Foo; }`)
        // have no fields visible to Kāra — the C side owns the layout, so
        // even `r.field` through a `ref Foo` / `mut ref Foo` has no
        // meaningful resolution. The bare `Type::Named` arm is a defensive
        // belt for typecheck-error-recovery flows; the by-value binding
        // itself would already have fired `E_OPAQUE_TYPE_REQUIRES_INDIRECTION`
        // upstream.
        let opaque_receiver_name = match &obj_ty {
            Type::Ref(inner) | Type::MutRef(inner) => match inner.as_ref() {
                Type::Named { name, .. } if self.env.opaque_foreign_types.contains(name) => {
                    Some(name.clone())
                }
                _ => None,
            },
            Type::Named { name, .. } if self.env.opaque_foreign_types.contains(name) => {
                Some(name.clone())
            }
            _ => None,
        };
        if let Some(name) = opaque_receiver_name {
            self.type_error(
                format!(
                    "error[E_OPAQUE_TYPE_NO_FIELDS]: opaque foreign type '{name}' \
                     has no fields visible to Kāra; the C side owns the layout. \
                     Field access through `ref {name}` / `mut ref {name}` is not \
                     supported — pass the reference to a foreign function declared \
                     in the same `unsafe extern {{ }}` block instead"
                ),
                span.clone(),
                TypeErrorKind::TypeMismatch,
            );
            return Type::Error;
        }

        let type_name = match &obj_ty {
            Type::Named { name, .. } => name.clone(),
            // Shared-struct receivers (`Type::Shared(name)` — a `shared
            // struct N { ... }`'s value type) carry the same struct
            // definition lookup as a bare `Type::Named { name, args: [] }`.
            // Without this arm, `node.field` on a pattern-bound shared
            // handle falls through to `Type::Error` and silently breaks
            // every downstream consumer (match scrutinee inference,
            // method dispatch, pattern-binding type recording).
            Type::Shared(name) => name.clone(),
            _ => return Type::Error,
        };

        if let Some(struct_info) = self.env.structs.get(&type_name) {
            let struct_info = struct_info.clone();
            for (fname, ftype, is_pub) in &struct_info.fields {
                if fname == field {
                    // CR-18 field-access half: reject non-`pub` field access
                    // on an imported struct from outside the defining module.
                    if !is_pub {
                        self.check_cross_module_field_access(&type_name, field, span);
                    }
                    return ftype.clone();
                }
            }
            let available: Vec<&str> = struct_info
                .fields
                .iter()
                .map(|(n, _, _)| n.as_str())
                .collect();
            self.type_error(
                format!(
                    "no field '{}' on struct '{}', available fields: {}",
                    field,
                    type_name,
                    available.join(", ")
                ),
                span.clone(),
                TypeErrorKind::UndefinedField,
            );
            Type::Error
        } else {
            // Not in the local env, but may be an imported struct — probe the
            // origin module directly so cross-module field access can still
            // be validated for CR-18.
            self.infer_imported_field_access(&type_name, field, span)
        }
    }

    /// Emit `E0221 PrivateTypeInPublicSignature` when a non-`pub` field is
    /// accessed on an imported struct from outside its defining module. For
    /// local structs (and when no `ProgramTree` is attached), silently
    /// accepts the access — slice 6b treats same-module field access as
    /// always allowed.
    pub(super) fn check_cross_module_field_access(
        &mut self,
        struct_name: &str,
        field: &str,
        span: &Span,
    ) {
        let Some(tree) = self.tree else { return };
        let Some(current_id) = self.current_module else {
            return;
        };
        let current_path = tree.module(current_id).path.clone();

        // Find the defining module. For a local struct, origin == current.
        let origin_path: Vec<String> = match self.type_origins.get(struct_name) {
            Some((path, _, _)) => path.clone(),
            None => current_path.clone(),
        };
        if origin_path == current_path {
            // Same-module access — non-pub fields are always reachable to
            // sibling code.
            return;
        }
        self.type_error(
            format!(
                "private field '{}' of struct '{}' is not visible outside its defining module",
                field, struct_name,
            ),
            span.clone(),
            TypeErrorKind::PrivateTypeInPublicSignature,
        );
    }

    /// Access a field on a struct that isn't registered in the local env —
    /// typically an imported struct from another module. Consults the
    /// `ProgramTree` so we can (a) return the field type and (b) enforce
    /// the cross-module field-visibility rule.
    fn infer_imported_field_access(&mut self, struct_name: &str, field: &str, span: &Span) -> Type {
        let Some(tree) = self.tree else {
            return Type::Error;
        };
        let Some((origin_path, canonical_name, _vis)) = self.type_origins.get(struct_name).cloned()
        else {
            return Type::Error;
        };
        let Some(&origin_id) = tree.graph.by_path.get::<[String]>(&origin_path) else {
            return Type::Error;
        };
        let origin_module = tree.module(origin_id);
        // Look up by the canonical name — `struct_name` here may be an
        // import alias (`import db.Connection as Conn` binds `Conn` but the
        // struct is defined as `Connection`). The canonical name survives
        // the chain walked in `collect_import_origins`.
        let Some(struct_def) = find_struct_def(origin_module, &canonical_name) else {
            return Type::Error;
        };
        let field_def = match struct_def.fields.iter().find(|f| f.name == field) {
            Some(f) => f,
            None => {
                let available: Vec<&str> =
                    struct_def.fields.iter().map(|f| f.name.as_str()).collect();
                self.type_error(
                    format!(
                        "no field '{}' on struct '{}', available fields: {}",
                        field,
                        struct_name,
                        available.join(", ")
                    ),
                    span.clone(),
                    TypeErrorKind::UndefinedField,
                );
                return Type::Error;
            }
        };

        if !field_def.is_pub {
            // `origin_path` is guaranteed to differ from `current_module`'s
            // path because `type_origins` only holds cross-module entries.
            self.type_error(
                format!(
                    "private field '{}' of struct '{}' is not visible outside its defining module",
                    field, struct_name,
                ),
                span.clone(),
                TypeErrorKind::PrivateTypeInPublicSignature,
            );
        }

        // Return the field's declared type. We lower the TypeExpr with an
        // empty generic scope — the origin module's generics are not in
        // scope here, and that's OK for slice-6b's coarse cross-module type
        // surface.
        self.lower_type_expr(&field_def.ty, &[])
    }

    // ── Struct Literals ─────────────────────────────────────────

    pub(super) fn infer_struct_literal(
        &mut self,
        path: &[String],
        fields: &[FieldInit],
        span: &Span,
    ) -> Type {
        let struct_name = path.last().cloned().unwrap_or_default();

        let struct_info = match self.env.structs.get(&struct_name) {
            Some(info) => info.clone(),
            None => {
                // Type-check field values anyway
                for f in fields {
                    self.infer_expr(&f.value);
                }
                self.type_error(
                    format!("'{}' is not a struct", struct_name),
                    span.clone(),
                    TypeErrorKind::NotAStruct,
                );
                return Type::Error;
            }
        };

        // `#[deprecated]` slice 4 — emit the deprecation warning when
        // the struct's defining decl carries a `Deprecation` payload.
        // Surface here is struct-literal construction (`Foo { ... }`);
        // the type-position reference (`var: Foo`) is covered by the
        // `lower_path_type` site, and the pattern reference is covered
        // by `check_pattern_against`'s struct-pattern arm.
        self.check_deprecated_use_at(span, &struct_name);

        // `#[non_exhaustive]` slice 4 — cross-package struct-literal
        // enforcement. A `pub struct` marked `#[non_exhaustive]` may
        // grow additional fields without breaking source compatibility;
        // outside-package consumers therefore cannot enumerate the
        // current field set in a literal (an added field would silently
        // become a missing-field error at the consumer). The defining
        // package retains exhaustive literal use because it owns the
        // shape. Today the only inter-package boundary the compiler
        // tracks is stdlib-vs-user (`stdlib_origin`); when richer
        // per-package boundaries land, the comparison below shifts at
        // this site without touching the `is_non_exhaustive` plumbing.
        if struct_info.is_non_exhaustive
            && struct_info.defining_stdlib_origin
            && !self.current_fn_stdlib_origin
        {
            self.type_error(
                format!(
                    "error[E_NON_EXHAUSTIVE_CROSS_PACKAGE_LITERAL]: \
                     cannot construct `{name}` with a struct literal — \
                     `{name}` is `#[non_exhaustive]` and defined in another \
                     package, so its field set may grow. Construct through \
                     the type's public constructor (commonly `{name}.new(...)`) \
                     instead. See design.md § `#[non_exhaustive]` for \
                     Evolvable Public Types.",
                    name = struct_name
                ),
                span.clone(),
                TypeErrorKind::NonExhaustiveCrossPackageLiteral,
            );
        }

        let expected_fields: HashSet<&str> = struct_info
            .fields
            .iter()
            .map(|(n, _, _)| n.as_str())
            .collect();
        let provided_fields: HashSet<&str> = fields.iter().map(|f| f.name.as_str()).collect();

        // Check for missing fields
        for (fname, _, _) in &struct_info.fields {
            if !provided_fields.contains(fname.as_str()) {
                self.type_error(
                    format!("missing field '{}' in struct '{}'", fname, struct_name),
                    span.clone(),
                    TypeErrorKind::MissingField,
                );
            }
        }

        // Check for extra fields
        for f in fields {
            if !expected_fields.contains(f.name.as_str()) {
                self.type_error(
                    format!("unknown field '{}' in struct '{}'", f.name, struct_name),
                    f.span.clone(),
                    TypeErrorKind::ExtraField,
                );
            }
        }

        // Type-check field values. Use `check_expr` against the field's
        // declared type when known so check-mode coercions (empty
        // `Vec[]` / `Set[]` / `Array[]`, `Into` / `TryInto`, closure
        // pushdown, etc.) fire at struct-field initializer positions.
        // Fall back to synthesis when the field is not declared on the
        // struct (already diagnosed above as an extra field).
        for f in fields {
            if let Some((_, expected_ty, _)) =
                struct_info.fields.iter().find(|(n, _, _)| n == &f.name)
            {
                self.check_expr(&f.value, &expected_ty.clone());
            } else {
                self.infer_expr(&f.value);
            }
        }

        // Shared-struct literals lower to Type::Shared so the literal's
        // type matches an annotated `let s: S = S { ... }` shape and the
        // method-resolution deref step (sub-item 3a) sees a consistent
        // receiver type. Sub-item 2's `lower_path_type` intercept handles
        // the annotation side; this is its construction-site twin.
        if struct_info.is_shared {
            Type::Shared(struct_name)
        } else {
            Type::Named {
                name: struct_name,
                args: Vec::new(),
            }
        }
    }
}
