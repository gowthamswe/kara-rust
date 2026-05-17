//! Pattern checking, refutability, exhaustiveness, and the
//! pattern-aware control-flow forms (`if let`, `match`).
//!
//! Houses `check_pattern_against` (structural pattern checker with
//! scrutinee-mode propagation), `bind_pattern_types` (binding
//! collector), `is_irrefutable_pattern` / `is_irrefutable_param_pattern`
//! / `check_param_irrefutable` (refutability checks),
//! `check_exhaustiveness` (the match-arm coverage algorithm), and the
//! top-level `check_if_against` / `check_if_let_against` /
//! `check_match_against` / `infer_match` drivers.

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::token::Span;
use std::collections::HashMap;

use super::inference::{resolve_type_vars, substitute_type_params};
use super::types::{
    type_display, ConstArg, ConstVarId, FloatSize, IntSize, ScrutineeMode, SubstValue, Type,
    TypeVarId, UIntSize, VariantTypeInfo,
};
use super::TypeErrorKind;

impl<'a> super::TypeChecker<'a> {
    /// Check-mode form of `if`. Threads `expected` into both branches so
    /// closures and other check-sensitive shapes inside arms see the
    /// target type. The condition is still synthesized + asserted Bool.
    /// When the `else` branch is missing, the expected type must accept
    /// `Unit` (the synth path's behavior); we delegate to
    /// `check_assignable` for that diagnostic so the message is uniform
    /// with non-branching cases.
    pub(super) fn check_if_against(
        &mut self,
        condition: &Expr,
        then_block: &Block,
        else_branch: Option<&Expr>,
        expected: &Type,
        span: &Span,
    ) -> Type {
        let cond_ty = self.infer_expr(condition);
        if cond_ty != Type::Bool && cond_ty != Type::Error {
            self.type_error(
                format!(
                    "condition must be 'bool', found '{}'",
                    type_display(&cond_ty)
                ),
                condition.span.clone(),
                TypeErrorKind::ConditionNotBool,
            );
        }
        let then_ty = self.check_block_against(then_block, expected);
        if let Some(else_expr) = else_branch {
            let else_ty = self.check_expr(else_expr, expected);
            // Each branch's check_expr already reported a TypeMismatch
            // against `expected` if it didn't comply; no need to re-check
            // cross-branch compatibility (it's transitive through expected).
            // Pick a non-Never type as the recorded result.
            let result_ty = if then_ty != Type::Never {
                then_ty
            } else {
                else_ty
            };
            self.record_expr_type(span, &result_ty);
            result_ty
        } else {
            // No else: the if returns Unit. Surface the standard
            // assignability diagnostic if the caller expected non-Unit.
            self.check_assignable(expected, &Type::Unit, span.clone());
            self.record_expr_type(span, &Type::Unit);
            Type::Unit
        }
    }

    /// Check-mode form of `if let`. Same shape as `check_if_against`
    /// but binds the pattern's variables in the then-block scope before
    /// checking it. Pattern type-checking against the value's type is
    /// deferred to the synthesis-mode `infer_expr` arm — we mirror its
    /// current behavior (synth value, no pattern type check) so this
    /// slice doesn't change diagnostics around irrefutable-let.
    pub(super) fn check_if_let_against(
        &mut self,
        pattern: &Pattern,
        value: &Expr,
        then_block: &Block,
        else_branch: Option<&Expr>,
        expected: &Type,
        span: &Span,
    ) -> Type {
        let scrut_ty = self.infer_expr(value);
        // Mirror `infer_if_let`'s pattern binding so the then-block sees
        // the pattern's bindings with their scrutinee-derived types.
        let (mode, dispatch_ty) = ScrutineeMode::classify(&scrut_ty);
        let dispatch_ty = dispatch_ty.clone();
        self.local_scope.push();
        self.check_pattern_against(pattern, &dispatch_ty, mode);
        let then_ty = self.check_block_against(then_block, expected);
        self.local_scope.pop();
        if let Some(else_expr) = else_branch {
            let else_ty = self.check_expr(else_expr, expected);
            let result_ty = if then_ty != Type::Never {
                then_ty
            } else {
                else_ty
            };
            self.record_expr_type(span, &result_ty);
            result_ty
        } else {
            self.check_assignable(expected, &Type::Unit, span.clone());
            self.record_expr_type(span, &Type::Unit);
            Type::Unit
        }
    }

    /// Check-mode form of `match`. Each arm body is checked against
    /// `expected`. Mirrors `infer_match` for scrutinee/guard/pattern
    /// machinery and exhaustiveness; only the arm-body inference is
    /// replaced with check-mode dispatch. Per-arm assignability
    /// diagnostics from `check_expr` replace the synth path's aggregate
    /// `BranchTypeMismatch` (more specific — points at the offending
    /// arm rather than the whole match).
    pub(super) fn check_match_against(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        expected: &Type,
        span: &Span,
    ) -> Type {
        let scrut_ty = self.infer_expr(scrutinee);
        let (mode, dispatch_ty) = ScrutineeMode::classify(&scrut_ty);
        let dispatch_ty = dispatch_ty.clone();
        let mut arm_types: Vec<Type> = Vec::new();
        for arm in arms {
            self.local_scope.push();
            self.check_pattern_against(&arm.pattern, &dispatch_ty, mode);
            if let Some(guard) = &arm.guard {
                let guard_ty = self.infer_expr(guard);
                if guard_ty != Type::Bool && guard_ty != Type::Error {
                    self.type_error(
                        format!(
                            "match guard must be 'bool', found '{}'",
                            type_display(&guard_ty)
                        ),
                        guard.span.clone(),
                        TypeErrorKind::ConditionNotBool,
                    );
                }
            }
            let arm_ty = self.check_expr(&arm.body, expected);
            arm_types.push(arm_ty);
            self.local_scope.pop();
        }
        self.check_exhaustiveness(&scrut_ty, arms, span.clone());
        let result_ty = arm_types
            .iter()
            .find(|t| **t != Type::Never)
            .cloned()
            .unwrap_or(Type::Never);
        self.record_expr_type(span, &result_ty);
        result_ty
    }

    pub(super) fn infer_match(&mut self, scrutinee: &Expr, arms: &[MatchArm], span: &Span) -> Type {
        let scrut_ty = self.infer_expr(scrutinee);
        let (mode, dispatch_ty) = ScrutineeMode::classify(&scrut_ty);
        let dispatch_ty = dispatch_ty.clone();
        let mut arm_types: Vec<Type> = Vec::new();

        for arm in arms {
            self.local_scope.push();
            self.check_pattern_against(&arm.pattern, &dispatch_ty, mode);
            if let Some(guard) = &arm.guard {
                let guard_ty = self.infer_expr(guard);
                if guard_ty != Type::Bool && guard_ty != Type::Error {
                    self.type_error(
                        format!(
                            "match guard must be 'bool', found '{}'",
                            type_display(&guard_ty)
                        ),
                        guard.span.clone(),
                        TypeErrorKind::ConditionNotBool,
                    );
                }
            }
            let arm_ty = self.infer_expr(&arm.body);
            arm_types.push(arm_ty);
            self.local_scope.pop();
        }

        // Check exhaustiveness for enum types
        self.check_exhaustiveness(&scrut_ty, arms, span.clone());

        // Check all arm types are compatible
        let result_ty = arm_types
            .iter()
            .find(|t| **t != Type::Never)
            .cloned()
            .unwrap_or(Type::Never);

        for arm_ty in &arm_types {
            if *arm_ty != Type::Never
                && *arm_ty != Type::Error
                && result_ty != Type::Error
                && !self.types_compatible_with_projections(&result_ty, arm_ty)
            {
                self.type_error(
                    format!(
                        "match arms have incompatible types: '{}' and '{}'",
                        type_display(&result_ty),
                        type_display(arm_ty)
                    ),
                    span.clone(),
                    TypeErrorKind::BranchTypeMismatch,
                );
                break;
            }
        }

        result_ty
    }

    /// Validate a slice/array pattern against the scrutinee type and
    /// return `(element_type, rest_binding_type)` — the per-element type
    /// used to recurse into prefix/suffix sub-patterns, and the type the
    /// `..name` rest binding should receive. Emits per-case diagnostics
    /// (arity overflow, under-coverage without `..`, non-literal `Array`
    /// size, `String` redirect, unsupported scrutinee shape). Returns
    /// `(Type::Error, Type::Error)` on rejection so callers can still
    /// recurse cleanly without crashing. Sub-item 2 of the slice/array-
    /// patterns entry (phase 5.2).
    fn slice_pattern_types(
        &mut self,
        prefix: &[Pattern],
        rest: &Option<RestPattern>,
        suffix: &[Pattern],
        scrutinee_type: &Type,
        span: &Span,
    ) -> (Type, Type) {
        let head = prefix.len();
        let tail = suffix.len();
        let used = head + tail;
        match scrutinee_type {
            Type::Error => (Type::Error, Type::Error),
            Type::Array { element, size } => match size.as_usize() {
                Some(n) => {
                    if used > n {
                        self.type_error(
                            format!(
                                "slice pattern has {used} element{} but \
                                 `Array[_, {n}]` has length {n}; remove \
                                 element patterns or add a `..` marker",
                                if used == 1 { "" } else { "s" },
                            ),
                            span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        return (Type::Error, Type::Error);
                    }
                    if rest.is_none() && used != n {
                        self.type_error(
                            format!(
                                "slice pattern covers {used} of {n} \
                                 positions on `Array[_, {n}]`; add a `..` \
                                 marker if the remaining positions should \
                                 match anything"
                            ),
                            span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        return ((**element).clone(), Type::Error);
                    }
                    let remainder = (n - used) as i64;
                    let rest_ty = Type::Array {
                        element: element.clone(),
                        size: ConstArg::Literal(remainder),
                    };
                    ((**element).clone(), rest_ty)
                }
                None => {
                    self.type_error(
                        "slice patterns on `Array[T, N]` require `N` to be \
                         a compile-time literal; const-parameter array \
                         sizes are not yet supported in pattern position"
                            .to_string(),
                        span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    ((**element).clone(), Type::Error)
                }
            },
            Type::Slice { element, mutable } => {
                let rest_ty = Type::Slice {
                    element: element.clone(),
                    mutable: *mutable,
                };
                ((**element).clone(), rest_ty)
            }
            Type::Named { name, args } if name == "Vec" => {
                let element = args.first().cloned().unwrap_or(Type::Error);
                let rest_ty = Type::Slice {
                    element: Box::new(element.clone()),
                    mutable: false,
                };
                (element, rest_ty)
            }
            Type::Str => {
                self.type_error(
                    "slice patterns do not apply to `String` — UTF-8 is \
                     variable-width per code point, so byte-level positional \
                     patterns would produce invalid boundaries. To match on \
                     string content, convert to `Slice[u8]` with `.bytes()` \
                     or `Iterator[char]` with `.chars()` first"
                        .to_string(),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                (Type::Error, Type::Error)
            }
            _ => {
                self.type_error(
                    format!(
                        "slice patterns apply to `Array[T, N]`, `Vec[T]`, \
                         and `Slice[T]`; cannot match a value of type `{}`",
                        type_display(scrutinee_type)
                    ),
                    span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                (Type::Error, Type::Error)
            }
        }
    }

    /// Walk `pattern` against the scrutinee's structural type `expected`
    /// under `mode` (Owned / Ref / MutRef), binding any introduced names
    /// into `self.local_scope`. The mode is the **match scrutinee's**
    /// borrow form, captured at the match entry by
    /// `ScrutineeMode::classify` after stripping one outer `ref` /
    /// `mut ref` (so the variant / struct / tuple dispatch below keeps
    /// matching the unwrapped shape). `mode` propagates transitively
    /// into every sub-pattern; each leaf binding's type is wrapped via
    /// `ScrutineeMode::wrap_binding_ty` so a `ref T` scrutinee yields
    /// `ref FieldType` bindings — design.md § Match Arm Binding Modes.
    /// Owned scrutinees keep the prior behaviour exactly (no wrap).
    pub(super) fn check_pattern_against(
        &mut self,
        pattern: &Pattern,
        expected: &Type,
        mode: ScrutineeMode,
    ) {
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding(name) => {
                // Check if this binding name is actually an enum variant
                // (unit variants are parsed as Binding since the parser can't distinguish)
                if let Type::Named {
                    name: enum_name, ..
                } = expected
                {
                    if let Some(enum_info) = self.env.enums.get(enum_name).cloned() {
                        if enum_info.variants.iter().any(|(vn, _)| vn == name) {
                            // It's a unit variant match, not a variable binding
                            return;
                        }
                    }
                }
                let binding_ty = mode.wrap_binding_ty(expected.clone());
                self.local_scope.insert(name.clone(), binding_ty);
                self.record_pattern_binding_borrow_mode(&pattern.span, mode, expected);
                // Mirror bind_pattern_types's side-table write so codegen
                // can reconstitute struct payloads for match-arm bindings.
                // `Type::Str` registers `"String"` parallel to how
                // `Type::Named { name: "Vec" }` registers `"Vec"` —
                // required by the tuple-payload destructure path
                // (`pattern_payload_word_count`) for variant-payload
                // tuples containing String elements (Theme 5, 2026-05-10).
                // `Type::Shared(name)` registers under its bare struct
                // name so codegen's `shared_type_for_expr` lookup finds
                // the heap layout for `node.field` access after a
                // `Some(node)` pattern binding.
                if let Type::Named {
                    name: type_name, ..
                } = expected
                {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), type_name.clone());
                } else if matches!(expected, Type::Str) {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), "String".to_string());
                } else if let Type::Shared(type_name) = expected {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), type_name.clone());
                }
                // PB sibling slice (2026-05-09): mirror
                // `bind_pattern_types`'s sibling-table write so direct
                // method dispatch on a pattern-bound `Vec[T]` / `Slice[T]`
                // payload (the canonical match-arm shape) routes through
                // the right element-typed path.
                self.record_pattern_inner_type(pattern, expected);
            }
            PatternKind::Literal(_) => {
                // Type checking of literal patterns deferred
            }
            PatternKind::TupleVariant { path, patterns } => {
                let variant_name = path.last().cloned().unwrap_or_default();
                if let Type::Named { name, args } = expected {
                    if let Some(enum_info) = self.env.enums.get(name).cloned() {
                        if let Some((_, VariantTypeInfo::Tuple(field_types))) =
                            enum_info.variants.iter().find(|(n, _)| n == &variant_name)
                        {
                            // Substitute the enum's generic params with the
                            // concrete args from the scrutinee's type so
                            // sub-patterns see the resolved payload type
                            // (e.g. `Err(e)` against `Result[i64, MyError]`
                            // sees `e: MyError`, not `e: TypeParam("E")`).
                            let subs: HashMap<String, SubstValue> = enum_info
                                .generic_params
                                .iter()
                                .cloned()
                                .zip(args.iter().cloned().map(SubstValue::Type))
                                .collect();
                            for (pat, ty) in patterns.iter().zip(field_types.iter()) {
                                let resolved = if subs.is_empty() {
                                    ty.clone()
                                } else {
                                    substitute_type_params(ty, &subs)
                                };
                                self.check_pattern_against(pat, &resolved, mode);
                            }
                            return;
                        }
                    }
                }
                // Fallback: bind sub-patterns to Error
                for pat in patterns {
                    self.check_pattern_against(pat, &Type::Error, mode);
                }
            }
            PatternKind::Struct { path, fields } => {
                let struct_name = path.last().cloned().unwrap_or_default();
                // Look up struct or enum variant
                let field_types: Option<Vec<(String, Type)>> =
                    if let Some(info) = self.env.structs.get(&struct_name) {
                        Some(
                            info.fields
                                .iter()
                                .map(|(n, t, _)| (n.clone(), t.clone()))
                                .collect(),
                        )
                    } else if let Type::Named { name, .. } = expected {
                        self.env.enums.get(name).and_then(|e| {
                            e.variants
                                .iter()
                                .find(|(n, _)| n == &struct_name)
                                .and_then(|(_, v)| {
                                    if let VariantTypeInfo::Struct(fields) = v {
                                        Some(fields.clone())
                                    } else {
                                        None
                                    }
                                })
                        })
                    } else {
                        None
                    };

                if let Some(ft) = field_types {
                    for field in fields {
                        let field_ty = ft
                            .iter()
                            .find(|(n, _)| n == &field.name)
                            .map(|(_, t)| t.clone())
                            .unwrap_or(Type::Error);
                        if let Some(ref sub_pattern) = field.pattern {
                            self.check_pattern_against(sub_pattern, &field_ty, mode);
                        } else {
                            // Shorthand: field name becomes binding. Under a
                            // `ref`/`mut ref` scrutinee, the binding type is
                            // wrapped per the match-arm binding-mode rule
                            // (design.md § Match Arm Binding Modes).
                            let binding_ty = mode.wrap_binding_ty(field_ty.clone());
                            self.local_scope.insert(field.name.clone(), binding_ty);
                            // Record borrow mode keyed by the field's span
                            // so codegen can apply the ref-binding shim at
                            // the synthesized leaf binding site (codegen
                            // synthesizes `Pattern { Binding(field.name),
                            // span: field.span }` for shorthand).
                            self.record_pattern_binding_borrow_mode(&field.span, mode, &field_ty);
                        }
                    }
                }
            }
            PatternKind::Tuple(patterns) => {
                if let Type::Tuple(types) = expected {
                    for (pat, ty) in patterns.iter().zip(types.iter()) {
                        self.check_pattern_against(pat, ty, mode);
                    }
                }
            }
            PatternKind::RangePattern { .. } => {
                // Nothing to bind for range patterns
            }
            PatternKind::AtBinding {
                name,
                pattern: inner,
            } => {
                let binding_ty = mode.wrap_binding_ty(expected.clone());
                self.local_scope.insert(name.clone(), binding_ty);
                // The `name @` outer alias is recorded against the outer
                // pattern's span; the inner sub-pattern records its own
                // bindings via the recursive call.
                self.record_pattern_binding_borrow_mode(&pattern.span, mode, expected);
                self.check_pattern_against(inner, expected, mode);
            }
            PatternKind::Or(alternatives) => {
                for alt in alternatives {
                    self.check_pattern_against(alt, expected, mode);
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let (element_ty, rest_ty) =
                    self.slice_pattern_types(prefix, rest, suffix, expected, &pattern.span);
                for pat in prefix.iter().chain(suffix.iter()) {
                    self.check_pattern_against(pat, &element_ty, mode);
                }
                if let Some(RestPattern::Bound(name)) = rest {
                    // Slice-rest mutability propagation
                    // (design.md § Slice and array patterns > Mutability
                    // propagation, lines 7657–7661): a `..rest` over a
                    // `Vec[T]` / `Slice[T]` produces `Slice[T]`
                    // (immutable) under `Owned` / `Ref` and
                    // `mut Slice[T]` under `MutRef`; over an
                    // `Array[T, N]` the rest binds `Array[T, K]` under
                    // `Owned`, `ref Array[T, K]` under `Ref`, and
                    // `mut ref Array[T, K]` under `MutRef`. Generic
                    // `wrap_binding_ty` skips `Type::Slice` (so struct
                    // fields typed `Slice[T]` are never elevated) —
                    // the rest binding's source is the freshly
                    // synthesised window, so the elevation rule is
                    // applied locally here.
                    let binding_ty = match (mode, rest_ty) {
                        (ScrutineeMode::MutRef, Type::Slice { element, .. }) => Type::Slice {
                            element,
                            mutable: true,
                        },
                        (m, ty) => m.wrap_binding_ty(ty),
                    };
                    self.local_scope.insert(name.clone(), binding_ty);
                }
            }
        }
    }

    fn check_exhaustiveness(&mut self, scrutinee_type: &Type, arms: &[MatchArm], span: Span) {
        use crate::exhaustive::{check_match_exhaustive, unreachable_arms, ExhaustiveResult};
        for idx in unreachable_arms(scrutinee_type, arms, &self.env) {
            self.type_warning(
                "unreachable match arm: pattern is fully covered by an earlier arm".to_string(),
                arms[idx].pattern.span.clone(),
                TypeErrorKind::UnreachableArm,
            );
        }

        // `#[non_exhaustive]` slice 5 — cross-package enum match must
        // include a wildcard arm regardless of variant coverage. The
        // defining package may add variants without breaking source
        // compatibility, so outside-package consumers cannot enumerate
        // the current variant set exhaustively. Same-package matches
        // fall through to the strict variant-by-variant rule below.
        // The check fires BEFORE `check_match_exhaustive` so a missing
        // wildcard surfaces as the slice-5 diagnostic, not as a
        // generic "missing variant Vn" tail (which would mislead the
        // author into adding the listed variant rather than `_`).
        if let Type::Named { name, .. } = scrutinee_type {
            if let Some(enum_info) = self.env.enums.get(name) {
                // Variant names short-circuit `PatternKind::Binding`'s
                // catch-all classification: `Read` in pattern position
                // parses as `Binding("Read")` but is actually a
                // unit-variant constructor, not a fresh binding.
                let variant_names: std::collections::HashSet<&str> =
                    enum_info.variants.iter().map(|(n, _)| n.as_str()).collect();
                if enum_info.is_non_exhaustive
                    && enum_info.defining_stdlib_origin
                    && !self.current_fn_stdlib_origin
                    && !arms_contain_catchall(arms, &variant_names)
                {
                    self.type_error(
                        format!(
                            "error[E_NON_EXHAUSTIVE_CROSS_PACKAGE_MATCH]: \
                             match on `{name}` must include a wildcard arm \
                             (`_ => ...`) — `{name}` is `#[non_exhaustive]` \
                             and defined in another package, so new variants \
                             may land without breaking source compatibility. \
                             Add `_ => todo!(\"handle new variant\")` (or a \
                             real handler) before the closing brace. \
                             See design.md § `#[non_exhaustive]` for \
                             Evolvable Public Types."
                        ),
                        span.clone(),
                        TypeErrorKind::NonExhaustiveCrossPackageMatch,
                    );
                    return;
                }
            }
        }

        match check_match_exhaustive(scrutinee_type, arms, &self.env) {
            ExhaustiveResult::Exhaustive | ExhaustiveResult::Skipped => {}
            ExhaustiveResult::NonExhaustive { witness } => {
                // Preserve the prior diagnostic wording for bool and enum
                // scrutinees when the witness names a single top-level
                // constructor (no nested compound payload). Compound
                // witnesses and non-enum scrutinees use the pattern form.
                let is_simple_witness = !witness.contains('(') && !witness.contains('{');
                let message = match scrutinee_type {
                    Type::Bool if is_simple_witness => {
                        format!("non-exhaustive match on bool: missing {witness}")
                    }
                    Type::Named { name, .. }
                        if is_simple_witness && self.env.enums.contains_key(name) =>
                    {
                        format!("non-exhaustive match: missing variants: {witness}")
                    }
                    _ => format!("non-exhaustive match: pattern `{witness}` not covered"),
                };
                self.type_error(message, span, TypeErrorKind::NonExhaustiveMatch);
            }
        }
    }

    // ── Pattern Binding for Let ─────────────────────────────────

    /// Reverse direction of `lower_type_expr` for the subset needed by the
    /// pattern-binding sibling table (PB sibling slice 2026-05-09): convert
    /// a `Type` back to a synthetic `TypeExpr` so it can be forwarded
    /// through the lowering pass for codegen consumption (it lowers each
    /// surface element type back to an LLVM type via
    /// `llvm_type_for_type_expr`). Coverage: primitive integer / float /
    /// bool / char / str / unit, `Type::Named` (Vec, Slice, Map, struct,
    /// enum names), `Type::Tuple`, `Type::Array`, `Type::Slice`, `Type::Ref`,
    /// `Type::MutRef`, `Type::Shared`, `Type::Rc`, `Type::Arc`,
    /// `Type::TypeParam`. Pieces outside this set (function types, type
    /// vars, assoc projections, errors) fall back to `TypeKind::Error`,
    /// which `llvm_type_for_type_expr` lowers to i64 — adequate for the
    /// element-type registration use case (those payloads are not
    /// supported in pattern-bound Vec/Slice element positions today).
    pub(super) fn type_to_type_expr(ty: &Type) -> TypeExpr {
        let span = Span::default();
        let path = |name: &str, args: Vec<TypeExpr>| TypeExpr {
            kind: TypeKind::Path(PathExpr {
                segments: vec![name.to_string()],
                generic_args: if args.is_empty() {
                    None
                } else {
                    Some(args.into_iter().map(GenericArg::Type).collect())
                },
                span: span.clone(),
            }),
            span: span.clone(),
        };
        match ty {
            Type::Int(IntSize::I8) => path("i8", vec![]),
            Type::Int(IntSize::I16) => path("i16", vec![]),
            Type::Int(IntSize::I32) => path("i32", vec![]),
            Type::Int(IntSize::I64) => path("i64", vec![]),
            Type::UInt(UIntSize::U8) => path("u8", vec![]),
            Type::UInt(UIntSize::U16) => path("u16", vec![]),
            Type::UInt(UIntSize::U32) => path("u32", vec![]),
            Type::UInt(UIntSize::U64) => path("u64", vec![]),
            Type::UInt(UIntSize::Usize) => path("usize", vec![]),
            Type::Float(FloatSize::F32) => path("f32", vec![]),
            Type::Float(FloatSize::F64) => path("f64", vec![]),
            Type::Bool => path("bool", vec![]),
            Type::Char => path("char", vec![]),
            Type::Str => path("str", vec![]),
            Type::Unit => TypeExpr {
                kind: TypeKind::Unit,
                span,
            },
            Type::Named { name, args } => {
                let arg_exprs = args.iter().map(Self::type_to_type_expr).collect();
                path(name, arg_exprs)
            }
            Type::Shared(name) => path(name, vec![]),
            Type::Rc(inner) => path("Rc", vec![Self::type_to_type_expr(inner)]),
            Type::Arc(inner) => path("Arc", vec![Self::type_to_type_expr(inner)]),
            Type::Tuple(elems) => TypeExpr {
                kind: TypeKind::Tuple(elems.iter().map(Self::type_to_type_expr).collect()),
                span,
            },
            Type::Array { element, size } => TypeExpr {
                kind: TypeKind::Array {
                    element: Box::new(Self::type_to_type_expr(element)),
                    size: Box::new(Expr {
                        kind: ExprKind::Integer(size.as_literal().unwrap_or(0), None),
                        span: span.clone(),
                    }),
                },
                span,
            },
            Type::Slice { element, mutable } => {
                let inner = Box::new(Self::type_to_type_expr(element));
                if *mutable {
                    TypeExpr {
                        kind: TypeKind::MutSlice(inner),
                        span,
                    }
                } else {
                    path("Slice", vec![*inner])
                }
            }
            Type::Ref(inner) => TypeExpr {
                kind: TypeKind::Ref(Box::new(Self::type_to_type_expr(inner))),
                span,
            },
            Type::MutRef(inner) => TypeExpr {
                kind: TypeKind::MutRef(Box::new(Self::type_to_type_expr(inner))),
                span,
            },
            Type::Weak(inner) => TypeExpr {
                kind: TypeKind::Weak(Box::new(Self::type_to_type_expr(inner))),
                span,
            },
            Type::TypeParam(name) => path(name, vec![]),
            // Fallback for shapes that don't have a clean TypeExpr round-trip
            // (TypeVar, Function, OnceFunction, Pointer, AssocProjection,
            // Error). The element-type registration use case never sees
            // these as Vec[T] / Slice[T] inner types in a well-typed
            // program, so falling back to Error → i64 lowering is safe.
            _ => TypeExpr {
                kind: TypeKind::Error,
                span,
            },
        }
    }

    /// If `ty` is `Vec[T]` / `Slice[T]` / `mut Slice[T]`, record the inner
    /// element type at `pattern.span` in the sibling table so codegen can
    /// register it under the binding's variable name (`vec_elem_types` /
    /// `slice_elem_types`). For `Type::Slice` (which isn't a `Type::Named`
    /// shape), also write the canonical `"Slice"` surface name into the
    /// String-name table so codegen's `bind_pattern_values` knows which
    /// element-type registry to populate. `Vec` already gets a String-name
    /// entry from the existing `Type::Named` write. PB sibling slice
    /// (2026-05-09).
    /// Stamp the pattern-binding span with the borrow form derived from
    /// the enclosing `ScrutineeMode`. Owned mode produces no entry. The
    /// `dispatch_ty` is the unwrapped binding type (i.e., the type
    /// _before_ `wrap_binding_ty` re-wraps it) — used to skip recording
    /// when the field's own type is already a borrow shape, mirroring
    /// `wrap_binding_ty`'s pass-through rule (a `ref FieldT` field
    /// through a `ref Container` scrutinee stays `ref FieldT`, not
    /// `ref ref FieldT`, so the codegen shim must not wrap it again).
    fn record_pattern_binding_borrow_mode(
        &mut self,
        span: &Span,
        mode: ScrutineeMode,
        dispatch_ty: &Type,
    ) {
        let borrow = match mode {
            ScrutineeMode::Owned => return,
            ScrutineeMode::Ref => crate::ast::PatternBindingBorrow::Ref,
            ScrutineeMode::MutRef => crate::ast::PatternBindingBorrow::MutRef,
        };
        // Skip already-borrow leaf shapes (parity with
        // `ScrutineeMode::wrap_binding_ty`'s identity arm). A struct
        // field declared `ref T` / `mut ref T` / `Slice[T]` keeps its
        // own borrow form — codegen would already lower it as a
        // pointer / slice header, so the ref-shim would double-wrap.
        if matches!(
            dispatch_ty,
            Type::Ref(_) | Type::MutRef(_) | Type::Slice { .. }
        ) {
            return;
        }
        self.pattern_binding_borrow_modes
            .insert(SpanKey::from_span(span), borrow);
    }

    fn record_pattern_inner_type(&mut self, pattern: &Pattern, ty: &Type) {
        // Tuple bindings (e.g. `Some(node)` where `node: (i64, i64)`):
        // record the whole tuple `TypeExpr` so codegen can reconstruct
        // a tuple struct from the multi-word payload. Without this,
        // `pattern_binding_types` skips anonymous tuple shapes and
        // the codegen's `reconstruct_payload_value` Binding arm falls
        // through to single-word — the downstream `let (a, b) = node`
        // then fails because `node` isn't a struct value.
        if let Type::Tuple(_) = ty {
            let tup_te = Self::type_to_type_expr(ty);
            self.pattern_binding_inner_types
                .insert(SpanKey::from_span(&pattern.span), tup_te);
            self.pattern_binding_types
                .insert(SpanKey::from_span(&pattern.span), "Tuple".to_string());
            return;
        }
        let (elem, name): (Option<&Type>, Option<&'static str>) = match ty {
            // `VecDeque[T]` shares `Vec[T]`'s `{ptr, len, cap}` codegen
            // layout (see `extract_vec_elem_type`), so record it under
            // the same sibling-table path. Without this arm, an untyped
            // `let mut q = VecDeque.new();` leaves `vec_elem_types`
            // empty and `q.push_back(x)` falls through method dispatch.
            Type::Named { name, args }
                if (name == "Vec" || name == "VecDeque") && args.len() == 1 =>
            {
                (Some(&args[0]), None)
            }
            Type::Slice { element, .. } => (Some(element.as_ref()), Some("Slice")),
            _ => (None, None),
        };
        if let Some(elem_ty) = elem {
            let elem_te = Self::type_to_type_expr(elem_ty);
            let key = SpanKey::from_span(&pattern.span);
            self.pattern_binding_inner_types.insert(key, elem_te);
            // Stash the raw `Type` so `finalize_pattern_binding_inner_types`
            // can re-resolve typevars after the function body completes.
            // Bindings whose element type contains a typevar (e.g. `let mut
            // q = VecDeque.new();` — `?T` solved later by `q.push_back(x)`)
            // depend on this re-resolution to surface the right TypeExpr.
            self.pattern_binding_inner_unresolved
                .insert(key, elem_ty.clone());
            if let Some(canon_name) = name {
                self.pattern_binding_types
                    .insert(SpanKey::from_span(&pattern.span), canon_name.to_string());
            }
        }
    }

    /// Resolve any `Type::TypeVar` entries captured by
    /// `record_pattern_inner_type` against `env.substitutions` and
    /// overwrite the public `pattern_binding_inner_types` table. Runs
    /// once at the end of `check`, after every function body has been
    /// inferred and all typevars have either been solved or surfaced as
    /// "cannot infer" diagnostics.
    pub(super) fn finalize_pattern_binding_inner_types(&mut self) {
        let id_to_name: HashMap<TypeVarId, String> = HashMap::new();
        let const_id_to_name: HashMap<ConstVarId, String> = HashMap::new();
        let updates: Vec<(SpanKey, TypeExpr)> = self
            .pattern_binding_inner_unresolved
            .iter()
            .map(|(key, ty)| {
                let resolved = resolve_type_vars(
                    ty,
                    &self.env.substitutions,
                    &id_to_name,
                    &self.env.const_substitutions,
                    &const_id_to_name,
                );
                (*key, Self::type_to_type_expr(&resolved))
            })
            .collect();
        for (key, te) in updates {
            self.pattern_binding_inner_types.insert(key, te);
        }
    }

    pub(super) fn bind_pattern_types(&mut self, pattern: &Pattern, ty: &Type) {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                self.local_scope.insert(name.clone(), ty.clone());
                // Record the surface type for codegen so it can reconstitute
                // struct payloads from the i64 word at match-arm bind sites
                // (see TypeCheckResult.pattern_binding_types). Named types
                // record under their canonical name; `Type::Str` registers
                // its 3-word `String` surface name parallel to how
                // `Type::Named { name: "Vec" }` registers `"Vec"` —
                // required by the tuple-payload destructure path
                // (`pattern_payload_word_count`) which needs to slice a
                // flat tuple payload into per-element word ranges.
                // Other primitives and references stay unrecorded — their
                // 1-word default matches their actual layout.
                if let Type::Named {
                    name: type_name, ..
                } = ty
                {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), type_name.clone());
                } else if matches!(ty, Type::Str) {
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), "String".to_string());
                } else if let Type::Shared(type_name) = ty {
                    // Parallel to `check_pattern_against`: register the
                    // bare struct name so codegen's `shared_type_for_expr`
                    // lookup resolves `node.field` access on a pattern-
                    // bound shared-struct handle.
                    self.pattern_binding_types
                        .insert(SpanKey::from_span(&pattern.span), type_name.clone());
                }
                // PB sibling slice (2026-05-09): record the inner element
                // type for `Vec[T]` / `Slice[T]` bindings so codegen can
                // register the LLVM elem type under the binding's variable
                // name (vec_elem_types / slice_elem_types). Lights up
                // direct method dispatch (`xs.len()`, `xs[0]`, `xs.push(...)`)
                // on a pattern-bound collection payload without needing
                // function-arg routing as a work-around.
                self.record_pattern_inner_type(pattern, ty);
            }
            PatternKind::Tuple(patterns) => {
                if let Type::Tuple(types) = ty {
                    if patterns.len() != types.len() {
                        self.type_error(
                            format!(
                                "tuple pattern has {} element(s) but type has {}",
                                patterns.len(),
                                types.len()
                            ),
                            pattern.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        for pat in patterns {
                            self.bind_pattern_types(pat, &Type::Error);
                        }
                    } else {
                        for (pat, t) in patterns.iter().zip(types.iter()) {
                            self.bind_pattern_types(pat, t);
                        }
                    }
                } else if *ty != Type::Error {
                    self.type_error(
                        format!("tuple pattern used but type is `{}`", type_display(ty)),
                        pattern.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    for pat in patterns {
                        self.bind_pattern_types(pat, &Type::Error);
                    }
                } else {
                    for pat in patterns {
                        self.bind_pattern_types(pat, &Type::Error);
                    }
                }
            }
            PatternKind::Struct { path, fields } => {
                let struct_name = path.last().map(String::as_str).unwrap_or("");
                let field_source_ty = if let Type::Named { name, .. } = ty {
                    if name == struct_name || ty == &Type::Error {
                        Some(name.clone())
                    } else if *ty != Type::Error {
                        self.type_error(
                            format!(
                                "struct pattern `{}` used but type is `{}`",
                                struct_name,
                                type_display(ty)
                            ),
                            pattern.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        None
                    } else {
                        None
                    }
                } else if *ty != Type::Error {
                    self.type_error(
                        format!(
                            "struct pattern `{}` used but type is `{}`",
                            struct_name,
                            type_display(ty)
                        ),
                        pattern.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                    None
                } else {
                    None
                };
                for field in fields {
                    let field_ty = if let Some(ref sname) = field_source_ty {
                        if let Some(info) = self.env.structs.get(sname) {
                            if let Some((_, t, _)) =
                                info.fields.iter().find(|(n, _, _)| n == &field.name)
                            {
                                t.clone()
                            } else {
                                self.type_error(
                                    format!(
                                        "no field `{}` found on struct `{}`",
                                        field.name, sname
                                    ),
                                    field.span.clone(),
                                    TypeErrorKind::UndefinedField,
                                );
                                Type::Error
                            }
                        } else {
                            Type::Error
                        }
                    } else {
                        Type::Error
                    };
                    if let Some(ref sub) = field.pattern {
                        self.bind_pattern_types(sub, &field_ty);
                    } else {
                        self.local_scope.insert(field.name.clone(), field_ty);
                    }
                }
            }
            PatternKind::TupleVariant { patterns, .. } => {
                for pat in patterns {
                    self.bind_pattern_types(pat, &Type::Error);
                }
            }
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {}
            PatternKind::AtBinding { name, pattern } => {
                self.local_scope.insert(name.clone(), ty.clone());
                self.bind_pattern_types(pattern, ty);
            }
            PatternKind::Or(alternatives) => {
                if let Some(first) = alternatives.first() {
                    self.bind_pattern_types(first, ty);
                }
            }
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                let (element_ty, rest_ty) =
                    self.slice_pattern_types(prefix, rest, suffix, ty, &pattern.span);
                for pat in prefix.iter().chain(suffix.iter()) {
                    self.bind_pattern_types(pat, &element_ty);
                }
                if let Some(RestPattern::Bound(name)) = rest {
                    self.local_scope.insert(name.clone(), rest_ty);
                }
            }
        }
    }

    // ── Irrefutability Check for Parameter Patterns ──────────────

    /// Returns true if `pat` is irrefutable for a value of type `ty`. Prefers
    /// the Maranget machinery (`U([PAT], _) == false`) when the type is in
    /// the handled set; falls back to the legacy syntactic check on
    /// `ref`/`function`/`typeparam`/etc. types that Maranget skips. Slice 6
    /// of the exhaustiveness upgrade.
    pub(super) fn is_irrefutable_pattern(&self, pat: &Pattern, ty: &Type) -> bool {
        match crate::exhaustive::is_pattern_irrefutable(pat, ty, &self.env) {
            Some(b) => b,
            None => self.is_irrefutable_param_pattern(pat),
        }
    }

    /// Legacy syntactic refutability check. Retained as the fallback for
    /// types Maranget doesn't reason about (refs, function values, generic
    /// parameters, etc.). Prefer `is_irrefutable_pattern` when a type is
    /// available.
    fn is_irrefutable_param_pattern(&self, pat: &Pattern) -> bool {
        match &pat.kind {
            PatternKind::Binding(_) | PatternKind::Wildcard => true,
            PatternKind::Tuple(patterns) => patterns
                .iter()
                .all(|p| self.is_irrefutable_param_pattern(p)),
            PatternKind::Struct { path, fields } => {
                // A struct pattern is irrefutable only if the name refers to a
                // struct type (not an enum variant). Enum variant names are
                // refutable — they only match one branch.
                if path.len() == 1 {
                    let name = &path[0];
                    if self.env.structs.contains_key(name) {
                        // Known struct — irrefutable iff all field sub-patterns are
                        fields.iter().all(|f| {
                            f.pattern
                                .as_ref()
                                .is_none_or(|p| self.is_irrefutable_param_pattern(p))
                        })
                    } else if self
                        .env
                        .enums
                        .values()
                        .any(|e| e.variants.iter().any(|(v, _)| v == name))
                    {
                        // Known enum variant name — refutable
                        false
                    } else {
                        // Unknown name — let type errors surface; treat as irrefutable
                        // to avoid double-diagnosing the same source mistake.
                        true
                    }
                } else {
                    false
                }
            }
            PatternKind::AtBinding { pattern, .. } => self.is_irrefutable_param_pattern(pattern),
            PatternKind::Literal(_)
            | PatternKind::RangePattern { .. }
            | PatternKind::TupleVariant { .. }
            | PatternKind::Or(_) => false,
            // Slice patterns are refutable in general — `[]` does not cover
            // `Vec[T]`, and any non-`..` prefix/suffix narrows the matched
            // length class. Sub-item 2 will refine this to `irrefutable iff
            // every nested pattern is irrefutable` for `Array[T, N]` only.
            PatternKind::Slice { .. } => false,
        }
    }

    /// Emit a `RefutablePattern` error if `param`'s pattern is refutable
    /// for its declared type.
    pub(super) fn check_param_irrefutable(&mut self, param: &Param, ty: &Type) {
        if !self.is_irrefutable_pattern(&param.pattern, ty) {
            self.type_error(
                "refutable pattern in function parameter; use `if let` or `match` for patterns that may not match".to_string(),
                param.pattern.span.clone(),
                TypeErrorKind::RefutablePattern,
            );
        }
    }
}

/// Slice 5 helper — does the arm list include a catch-all pattern?
/// `_` (Wildcard) is the canonical form; a bare binding (`x`) whose
/// name is NOT one of the scrutinee enum's variants without a guard
/// also catches every value and counts as a catch-all. Or-patterns
/// whose alternatives include a wildcard, and `name @ _` at-bindings,
/// are also catch-alls. A guarded arm (`x if cond =>`) is NOT a
/// catch-all because the guard can fail.
///
/// `variant_names` is the scrutinee enum's variant-name set:
/// `PatternKind::Binding("Read")` matches a `Read` variant constructor
/// when `Read` is in the set (not a catch-all) but a free binding
/// otherwise (catch-all). The parser cannot distinguish these at
/// parse time — `Binding(name)` is the surface for both — so the
/// typechecker discriminates here using its env.
fn arms_contain_catchall(
    arms: &[MatchArm],
    variant_names: &std::collections::HashSet<&str>,
) -> bool {
    arms.iter()
        .any(|arm| arm.guard.is_none() && pattern_is_catchall(&arm.pattern, variant_names))
}

fn pattern_is_catchall(p: &Pattern, variant_names: &std::collections::HashSet<&str>) -> bool {
    match &p.kind {
        PatternKind::Wildcard => true,
        PatternKind::Binding(name) => !variant_names.contains(name.as_str()),
        PatternKind::AtBinding { pattern, .. } => pattern_is_catchall(pattern, variant_names),
        PatternKind::Or(alts) => alts.iter().any(|p| pattern_is_catchall(p, variant_names)),
        _ => false,
    }
}
