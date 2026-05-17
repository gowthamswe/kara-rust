//! Operator + identifier + path / offset_of / pipe / question
//! expression inference.
//!
//! Houses six per-shape inference rules that sit between the big
//! `infer_expr_inner` dispatch and the lower-level type / impl
//! helpers:
//!
//! - `infer_offset_of` — `offset_of[T](field.path)` per design.md
//!   § Field Offsets.
//! - `resolve_identifier_type` — bare-identifier resolution
//!   (locals / params / functions / constants / enum variants).
//! - `resolve_path_type` — `Foo.Bar` / `Foo.method` path resolution
//!   in expression position.
//! - `infer_binary` — typecheck binary operator expressions
//!   (arithmetic / comparison / bitwise / shift / `+` overloads).
//! - `infer_unary` — typecheck unary operator expressions
//!   (`-x`, `!b`, `~i`, deref).
//! - `infer_pipe` — `a |> f` / `a |> f(args)` desugaring inference.
//! - `infer_question` — `?` operator typechecking + `From`
//!   conversion recording.
//!
//! Lives in a sibling `impl<'a> super::TypeChecker<'a>` block.

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::token::Span;

use super::types::{
    is_integer, is_numeric, is_prelude_type_or_module_name, type_display, types_compatible, Type,
    UIntSize, VariantTypeInfo,
};
use super::TypeErrorKind;

impl<'a> super::TypeChecker<'a> {
    /// Type-check `offset_of[T](field.path)`. Per `design.md § Field
    /// Offsets`, the target type must be a struct (concrete or
    /// generic-with-fully-resolved args); opaque foreign types and
    /// generic type parameters are rejected at the first segment.
    /// Each path segment must name a field of the type at the previous
    /// segment's resolved type. Returns `usize` (also `Type::Error` on
    /// failure for downstream tolerance).
    pub(super) fn infer_offset_of(
        &mut self,
        ty: &TypeExpr,
        field_path: &[String],
        span: &Span,
    ) -> Type {
        let usize_ty = Type::UInt(UIntSize::Usize);
        // Lower the target with `parent_is_ref = true` so the slice-1b
        // walker doesn't fire E_OPAQUE_TYPE_REQUIRES_INDIRECTION; this
        // intrinsic emits E_OFFSET_OF_OPAQUE_TYPE instead.
        let resolved = self.lower_type_expr_inner(ty, &[], true);
        let (mut current_struct_name, _initial_args) = match &resolved {
            Type::Named { name, args } => (name.clone(), args.clone()),
            // Per design.md, generic type-parameter targets are rejected:
            // the typechecker can't see a layout without a concrete
            // instantiation. `Type::TypeParam` and other non-Named
            // shapes route here.
            Type::TypeParam(name) => {
                self.type_error(
                    format!(
                        "error[E_OFFSET_OF_GENERIC_PARAM]: offset_of requires a \
                         concrete type; the type parameter '{name}' is not \
                         resolvable to a layout at this call site"
                    ),
                    ty.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            }
            _ => {
                self.type_error(
                    format!(
                        "error[E_OFFSET_OF_NON_STRUCT_TARGET]: offset_of requires a \
                         struct target; got '{}'",
                        type_display(&resolved)
                    ),
                    ty.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            }
        };
        if self.env.opaque_foreign_types.contains(&current_struct_name) {
            self.type_error(
                format!(
                    "error[E_OFFSET_OF_OPAQUE_TYPE]: offset_of cannot be applied to \
                     opaque foreign type '{current_struct_name}'; the type's layout \
                     is unknown to Kāra"
                ),
                ty.span.clone(),
                TypeErrorKind::TypeMismatch,
            );
            return Type::Error;
        }
        if field_path.is_empty() {
            self.type_error(
                "error[E_OFFSET_OF_INVALID_PATH]: offset_of requires at least \
                 one field-name segment"
                    .to_string(),
                span.clone(),
                TypeErrorKind::WrongNumberOfArgs,
            );
            return Type::Error;
        }
        // Walk each segment, validating membership in the current struct's
        // declared field set and chasing the field's type for the next
        // segment. At each segment, the current struct is looked up by
        // name in `env.structs`; if absent (e.g., the surface type is an
        // enum or a primitive), `E_OFFSET_OF_NON_STRUCT_TARGET` fires.
        for (segment_idx, segment_name) in field_path.iter().enumerate() {
            let Some(info) = self.env.structs.get(&current_struct_name).cloned() else {
                self.type_error(
                    format!(
                        "error[E_OFFSET_OF_NON_STRUCT_TARGET]: offset_of cannot \
                         walk into '{current_struct_name}'; only struct types \
                         have field offsets"
                    ),
                    ty.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            };
            let mut found = None;
            for (fname, ftype, is_pub) in &info.fields {
                if fname == segment_name {
                    found = Some((ftype.clone(), *is_pub));
                    break;
                }
            }
            let Some((field_ty, is_pub)) = found else {
                let available: Vec<&str> = info.fields.iter().map(|(n, _, _)| n.as_str()).collect();
                self.type_error(
                    format!(
                        "error[E_OFFSET_OF_UNKNOWN_FIELD]: type '{current_struct_name}' \
                         has no field '{segment_name}'; available fields are: {}",
                        available.join(", ")
                    ),
                    span.clone(),
                    TypeErrorKind::UndefinedField,
                );
                return Type::Error;
            };
            if !is_pub {
                self.check_cross_module_field_access(&current_struct_name, segment_name, span);
            }
            // If this is the last segment, we're done — return usize.
            if segment_idx + 1 == field_path.len() {
                return usize_ty;
            }
            // Otherwise, the field's type must itself be a struct so the
            // next segment can walk into it.
            current_struct_name = match field_ty {
                Type::Named { name, .. } => name,
                _ => {
                    self.type_error(
                        format!(
                            "error[E_OFFSET_OF_NON_STRUCT_TARGET]: field \
                             '{segment_name}' is not a struct type; cannot walk \
                             further into the offset_of path"
                        ),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    return Type::Error;
                }
            };
        }
        usize_ty
    }

    // ── Identifier Resolution ───────────────────────────────────

    pub(super) fn resolve_identifier_type(&mut self, name: &str, span: &Span) -> Type {
        // Check local scope first
        if let Some(ty) = self.local_scope.lookup(name) {
            return ty.clone();
        }
        // Check functions
        if let Some((params, return_type)) = self
            .env
            .functions
            .get(name)
            .map(|sig| (sig.params.clone(), sig.return_type.clone()))
        {
            // `#[deprecated]` slice 4 — emit the deprecation warning
            // BEFORE returning so the cascade has the enclosing fn /
            // impl scope on the stack (the fn body that contains this
            // identifier reference). The lookup queries the resolver's
            // symbol table by name to find the deprecation payload.
            self.check_deprecated_use_at(span, name);
            return Type::Function {
                params,
                return_type: Box::new(return_type),
            };
        }
        // Check constants
        if let Some(ty) = self.env.constants.get(name).cloned() {
            self.check_deprecated_use_at(span, name);
            return ty;
        }
        // Check enum variants (unit variants used as values; tuple variants
        // as constructor functions). Generic enums thread their declared
        // type parameters through the return type's `args` so call-site
        // inference can solve them (see `infer_call`).
        //
        // **Variant-name shadow rule (Slice F).** Skip variants whose
        // bare name collides with a primitive type name (`String`,
        // `Array`, `Map`, `Set`, etc.) — those identifiers are
        // overwhelmingly used as type/module aliases at the call-site
        // (`String.from(...)`, `Map.new()`, `Vec.new()`), not as
        // variant constructors. Without this skip, declaring an enum
        // like `Json.String(String)` retroactively breaks every
        // pre-existing `String.from("...")` call by routing it through
        // the variant-as-function dispatch instead of the impl
        // resolution. Variants are still reachable through the
        // qualified path form (`Json.String(...)`) — `resolve_path_type`
        // above runs before this fallback and finds them by enum name.
        for (enum_name, enum_info) in &self.env.enums {
            for (variant_name, variant_type) in &enum_info.variants {
                if variant_name == name {
                    if is_prelude_type_or_module_name(name) {
                        continue;
                    }
                    let return_args: Vec<Type> = enum_info
                        .generic_params
                        .iter()
                        .map(|p| Type::TypeParam(p.clone()))
                        .collect();
                    let return_ty = Type::Named {
                        name: enum_name.clone(),
                        args: return_args,
                    };
                    match variant_type {
                        VariantTypeInfo::Unit => return return_ty,
                        VariantTypeInfo::Tuple(fields) => {
                            return Type::Function {
                                params: fields.clone(),
                                return_type: Box::new(return_ty),
                            };
                        }
                        _ => {}
                    }
                }
            }
        }
        // Fallback — likely a name the resolver already handled
        // Return Error silently (resolver already reported it)
        let _ = span;
        Type::Error
    }

    pub(super) fn resolve_path_type(&mut self, segments: &[String], span: &Span) -> Type {
        if segments.len() == 2 {
            let type_name = &segments[0];
            let member = &segments[1];

            // Check for enum variant. Generic enums thread their declared
            // type parameters through the return type's `args` so call-site
            // inference can solve them (see `infer_call`).
            if let Some(enum_info) = self.env.enums.get(type_name).cloned() {
                for (variant_name, variant_type) in &enum_info.variants {
                    if variant_name == member {
                        let return_args: Vec<Type> = enum_info
                            .generic_params
                            .iter()
                            .map(|p| Type::TypeParam(p.clone()))
                            .collect();
                        let return_ty = Type::Named {
                            name: type_name.clone(),
                            args: return_args,
                        };
                        match variant_type {
                            VariantTypeInfo::Unit => return return_ty,
                            VariantTypeInfo::Tuple(fields) => {
                                return Type::Function {
                                    params: fields.clone(),
                                    return_type: Box::new(return_ty),
                                };
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Check for associated function (from impl). No call-site args
            // context — type_name comes from a Path expression without
            // generic args. Theme-4 conservative: only generic-on-name
            // impls participate; specialized impls (`impl Foo for
            // Bar[i32]`) need an args-aware path-expr lookup that this
            // site doesn't carry.
            for imp in &self.env.impls.clone() {
                if imp.target_type == *type_name && imp.target_args.is_empty() {
                    if let Some(sig) = imp.methods.get(member) {
                        return Type::Function {
                            params: sig.params.clone(),
                            return_type: Box::new(sig.return_type.clone()),
                        };
                    }
                }
            }

            // Module-path free functions registered as "module.fn" in the
            // function table — `process.exit`, `env.args`, `env.var`. The
            // ambient effect-resource methods (`Stdin.read_line`,
            // `FileSystem.write`, …) used to land here too, but the slice-1
            // through slice-3 migration moved every `Type.method` entry into
            // `env.impls` via baked source, so this fallback now only serves
            // module-path free functions.
            let dotted = format!("{}.{}", type_name, member);
            if let Some(sig) = self.env.functions.get(&dotted) {
                return Type::Function {
                    params: sig.params.clone(),
                    return_type: Box::new(sig.return_type.clone()),
                };
            }
        }
        // First segment as identifier
        if let Some(first) = segments.first() {
            return self.resolve_identifier_type(first, span);
        }
        Type::Error
    }

    // ── Binary / Unary Operators ────────────────────────────────

    pub(super) fn infer_binary(
        &mut self,
        op: &BinOp,
        left: &Expr,
        right: &Expr,
        span: &Span,
    ) -> Type {
        let left_ty = self.infer_expr(left);
        let right_ty = self.infer_expr(right);

        if left_ty == Type::Error || right_ty == Type::Error {
            return Type::Error;
        }

        // Q4 literal promotion: for arithmetic, comparison, and equality ops,
        // when one operand is a suffix-free numeric literal and the other is a
        // concrete numeric type T, re-record the literal's span with type T so
        // the lowering pass sees a homogeneous pair. `effective_ty` tracks the
        // canonical type for the whole expression after promotion.
        let is_promotable_op = matches!(
            op,
            BinOp::Add
                | BinOp::Sub
                | BinOp::Mul
                | BinOp::Div
                | BinOp::Mod
                | BinOp::Lt
                | BinOp::LtEq
                | BinOp::Gt
                | BinOp::GtEq
                | BinOp::Eq
                | BinOp::NotEq
        );
        // After promotion these hold the effective operand types seen by the
        // match arms below. Initialised to the inferred types; overwritten when
        // promotion fires.
        let (eff_left_ty, eff_right_ty) = if is_promotable_op {
            let left_is_unsuffixed = matches!(
                &left.kind,
                ExprKind::Integer(_, None) | ExprKind::Float(_, None)
            );
            let right_is_unsuffixed = matches!(
                &right.kind,
                ExprKind::Integer(_, None) | ExprKind::Float(_, None)
            );
            if right_is_unsuffixed && !left_is_unsuffixed && is_numeric(&left_ty) {
                // Float literal cannot be promoted to an integer type.
                let can_promote = !(matches!(&right.kind, ExprKind::Float(_, None))
                    && matches!(left_ty, Type::Int(_) | Type::UInt(_)));
                if can_promote {
                    self.record_expr_type(&right.span, &left_ty);
                    (left_ty.clone(), left_ty.clone())
                } else {
                    (left_ty.clone(), right_ty.clone())
                }
            } else if left_is_unsuffixed && !right_is_unsuffixed && is_numeric(&right_ty) {
                let can_promote = !(matches!(&left.kind, ExprKind::Float(_, None))
                    && matches!(right_ty, Type::Int(_) | Type::UInt(_)));
                if can_promote {
                    self.record_expr_type(&left.span, &right_ty);
                    (right_ty.clone(), right_ty.clone())
                } else {
                    (left_ty.clone(), right_ty.clone())
                }
            } else {
                (left_ty.clone(), right_ty.clone())
            }
        } else {
            (left_ty.clone(), right_ty.clone())
        };
        let left_ty = eff_left_ty;
        let right_ty = eff_right_ty;

        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                if is_numeric(&left_ty) {
                    if !types_compatible(&left_ty, &right_ty) {
                        self.type_error(
                            format!(
                                "expected '{}', found '{}'",
                                type_display(&left_ty),
                                type_display(&right_ty)
                            ),
                            right.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                    left_ty
                } else if self.distinct_type_has_arithmetic(&left_ty) {
                    // Arithmetic on a distinct type: both operands must be the same type.
                    if left_ty != right_ty {
                        self.type_error(
                            format!(
                                "arithmetic on distinct type '{}' requires both operands to have \
                                 the same type, found '{}'",
                                type_display(&left_ty),
                                type_display(&right_ty)
                            ),
                            right.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                    left_ty
                } else {
                    self.type_error(
                        format!(
                            "arithmetic operator requires numeric type, found '{}'",
                            type_display(&left_ty)
                        ),
                        left.span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                    Type::Error
                }
            }
            BinOp::Eq | BinOp::NotEq => {
                if !types_compatible(&left_ty, &right_ty) {
                    self.type_error(
                        format!(
                            "cannot compare '{}' and '{}'",
                            type_display(&left_ty),
                            type_display(&right_ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                } else if !self.type_supports_partial_eq(&left_ty) {
                    self.type_error(
                        format!(
                            "type '{}' does not implement Eq; add #[derive(Eq)] to use == or !=",
                            type_display(&left_ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                }
                Type::Bool
            }
            BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                if !types_compatible(&left_ty, &right_ty) {
                    self.type_error(
                        format!(
                            "cannot compare '{}' and '{}'",
                            type_display(&left_ty),
                            type_display(&right_ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                }
                Type::Bool
            }
            BinOp::And | BinOp::Or => {
                if left_ty != Type::Bool {
                    self.type_error(
                        format!(
                            "logical operator requires 'bool', found '{}'",
                            type_display(&left_ty)
                        ),
                        left.span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                }
                if right_ty != Type::Bool {
                    self.type_error(
                        format!(
                            "logical operator requires 'bool', found '{}'",
                            type_display(&right_ty)
                        ),
                        right.span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                }
                Type::Bool
            }
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                if !is_integer(&left_ty) {
                    self.type_error(
                        format!(
                            "bitwise operator requires integer type, found '{}'",
                            type_display(&left_ty)
                        ),
                        left.span.clone(),
                        TypeErrorKind::InvalidBinaryOp,
                    );
                    return Type::Error;
                }
                if !types_compatible(&left_ty, &right_ty) {
                    self.type_error(
                        format!(
                            "expected '{}', found '{}'",
                            type_display(&left_ty),
                            type_display(&right_ty)
                        ),
                        right.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                left_ty
            }
            BinOp::Range | BinOp::RangeInclusive => {
                if !types_compatible(&left_ty, &right_ty) {
                    self.type_error(
                        "range bounds must have same type".to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                Type::Named {
                    name: "Range".to_string(),
                    args: vec![left_ty],
                }
            }
        }
    }

    pub(super) fn infer_unary(&mut self, op: &UnaryOp, operand: &Expr, span: &Span) -> Type {
        let ty = self.infer_expr(operand);
        if ty == Type::Error {
            return Type::Error;
        }

        match op {
            UnaryOp::Neg => {
                if !is_numeric(&ty) && !self.distinct_type_has_arithmetic(&ty) {
                    self.type_error(
                        format!(
                            "unary '-' requires numeric type, found '{}'",
                            type_display(&ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidUnaryOp,
                    );
                    Type::Error
                } else {
                    ty
                }
            }
            UnaryOp::Not => {
                if ty != Type::Bool {
                    self.type_error(
                        format!("unary '!' requires 'bool', found '{}'", type_display(&ty)),
                        span.clone(),
                        TypeErrorKind::InvalidUnaryOp,
                    );
                    Type::Error
                } else {
                    Type::Bool
                }
            }
            UnaryOp::BitNot => {
                if !is_integer(&ty) {
                    self.type_error(
                        format!(
                            "unary '~' requires integer type, found '{}'",
                            type_display(&ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidUnaryOp,
                    );
                    Type::Error
                } else {
                    ty
                }
            }
            UnaryOp::Deref => match ty {
                Type::Ref(inner) | Type::MutRef(inner) => *inner,
                // Raw-pointer dereference (`*const T` / `*mut T`) typechecks
                // to the pointee type. The operation itself is *unsafe* — the
                // `unsafe_op_in_unsafe_fn` lint (`src/unsafe_lint.rs`) rejects
                // it outside an `unsafe { }` block. Soundness lives at the
                // lint layer, not the type layer, so callers can still reason
                // about the deref's result type.
                Type::Pointer { inner, .. } => *inner,
                _ => {
                    self.type_error(
                        format!(
                            "unary '*' requires 'ref T', 'mut ref T', or a raw pointer \
                             ('*const T' / '*mut T'), found '{}'",
                            type_display(&ty)
                        ),
                        span.clone(),
                        TypeErrorKind::InvalidUnaryOp,
                    );
                    Type::Error
                }
            },
        }
    }

    // ── Pipe Desugaring ──────────────────────────────────────────

    pub(super) fn infer_pipe(&mut self, left: &Expr, right: &Expr, span: &Span) -> Type {
        match &right.kind {
            // a |> f => f(a)
            ExprKind::Identifier(_) | ExprKind::Path { .. } => {
                let synthetic_arg = CallArg {
                    label: None,
                    mut_marker: false,
                    value: left.clone(),
                    span: left.span.clone(),
                };
                self.infer_call(right, &[synthetic_arg], span)
            }

            // a |> f(args...) => f(a, args...) or f(args with _ replaced)
            ExprKind::Call { callee, args } => {
                // Count _ placeholders in args
                let placeholder_count = args
                    .iter()
                    .filter(|arg| matches!(arg.value.kind, ExprKind::PipePlaceholder))
                    .count();

                if placeholder_count > 1 {
                    self.type_error(
                        "at most one '_' placeholder allowed per pipe stage".to_string(),
                        right.span.clone(),
                        TypeErrorKind::InvalidPipePlaceholder,
                    );
                    self.infer_expr(callee);
                    for arg in args {
                        if !matches!(arg.value.kind, ExprKind::PipePlaceholder) {
                            self.infer_expr(&arg.value);
                        }
                    }
                    return Type::Error;
                }

                // Build the desugared argument list
                let desugared_args: Vec<CallArg> = if placeholder_count == 1 {
                    // Replace _ with the left-hand value
                    args.iter()
                        .map(|arg| {
                            if matches!(arg.value.kind, ExprKind::PipePlaceholder) {
                                CallArg {
                                    label: arg.label.clone(),
                                    mut_marker: arg.mut_marker,
                                    value: left.clone(),
                                    span: left.span.clone(),
                                }
                            } else {
                                arg.clone()
                            }
                        })
                        .collect()
                } else {
                    // No placeholder — prepend left as first argument
                    let mut new_args = vec![CallArg {
                        label: None,
                        mut_marker: false,
                        value: left.clone(),
                        span: left.span.clone(),
                    }];
                    new_args.extend(args.iter().cloned());
                    new_args
                };

                self.infer_call(callee, &desugared_args, span)
            }

            _ => {
                self.type_error(
                    "right-hand side of pipe must be a function name or function call".to_string(),
                    right.span.clone(),
                    TypeErrorKind::NotCallable,
                );
                self.infer_expr(right);
                Type::Error
            }
        }
    }

    // ── ? operator ──────────────────────────────────────────────

    /// Type-check `inner?`: validate that the operand is `Result[T, E1]` or
    /// `Option[T]`, that the enclosing function returns a compatible variant,
    /// and (for Result) that error types match exactly or convert via `From`.
    /// Returns the unwrapped success type (`T`).
    pub(super) fn infer_question(&mut self, inner: &Expr, span: &Span) -> Type {
        let inner_ty = self.infer_expr(inner);
        if inner_ty == Type::Error {
            return Type::Error;
        }

        let (inner_name, inner_args) = match &inner_ty {
            Type::Named { name, args } => (name.clone(), args.clone()),
            _ => {
                self.type_error(
                    format!(
                        "'?' operator requires `Result` or `Option`, found '{}'",
                        type_display(&inner_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            }
        };

        let return_ty = match self.current_return_type.clone() {
            Some(t) => t,
            None => {
                self.type_error(
                    "'?' operator used outside a function body".to_string(),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            }
        };
        let (ret_name, ret_args) = match &return_ty {
            Type::Named { name, args } => (name.clone(), args.clone()),
            _ => {
                self.type_error(
                    format!(
                        "'?' requires the enclosing function to return `Result` or `Option`, found '{}'",
                        type_display(&return_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                return Type::Error;
            }
        };

        match (inner_name.as_str(), ret_name.as_str()) {
            ("Option", "Option") if inner_args.len() == 1 && ret_args.len() == 1 => {
                inner_args[0].clone()
            }
            ("Result", "Result") if inner_args.len() == 2 && ret_args.len() == 2 => {
                let inner_err = &inner_args[1];
                let ret_err = &ret_args[1];
                if inner_err == ret_err {
                    return inner_args[0].clone();
                }
                // Cross-error type: require `impl From[InnerErr] for RetErr`.
                let target_name = match ret_err {
                    Type::Named { name, .. } => name.clone(),
                    _ => {
                        self.type_error(
                            format!(
                                "'?' cannot propagate error '{}' as '{}': target is not a named type",
                                type_display(inner_err),
                                type_display(ret_err)
                            ),
                            span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        return Type::Error;
                    }
                };
                if self
                    .env
                    .find_from_impl(inner_err, &target_name, &[])
                    .is_some()
                {
                    self.question_conversions
                        .insert(SpanKey::from_span(span), target_name.clone());
                    return inner_args[0].clone();
                }
                self.type_error(
                    format!(
                        "'?' cannot convert error '{}' to '{}': no `impl From[{}] for {}` in scope",
                        type_display(inner_err),
                        type_display(ret_err),
                        type_display(inner_err),
                        target_name
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                Type::Error
            }
            ("Result", "Option") | ("Option", "Result") => {
                self.type_error(
                    format!(
                        "'?' cannot mix `Result` and `Option`: operand is '{}', function returns '{}'",
                        type_display(&inner_ty),
                        type_display(&return_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                Type::Error
            }
            _ => {
                self.type_error(
                    format!(
                        "'?' requires operand and return type to be `Result` or `Option`, found '{}' and '{}'",
                        type_display(&inner_ty),
                        type_display(&return_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                Type::Error
            }
        }
    }
}
