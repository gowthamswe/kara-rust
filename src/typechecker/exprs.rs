//! Expression inference — the largest single submodule.
//!
//! Houses the central `check_expr` / `infer_expr` / `infer_expr_inner`
//! dispatch alongside every per-shape inference rule: binary / unary
//! operators, identifier / path resolution, the `offset_of` intrinsic,
//! the layout-query intrinsic, call inference (`infer_call`,
//! `check_call_site_marker`, explicit-generic-args, `infer_pipe`,
//! `infer_question`), method-call inference (`infer_method_call`),
//! and the `Into` / `TryInto` coercion arms. Bound-discharge and
//! call-site type substitution recording live here too because they
//! fire as part of call-site inference.

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::token::Span;
use std::collections::HashMap;

use super::env::{FunctionSig, ImplInfo};
use super::inference::{
    const_value_from_literal, instantiate_signature_with_fresh_vars, resolve_const_arg,
    resolve_type_vars, substitute_const_idents_in_expr, unify_types, InstantiatedSignature,
};
use super::types::{
    contains_type_param, impl_args_match, impl_table_key, is_integer, lub_block_type, type_display,
    type_to_concrete_or_param_name, types_compatible, ConstArg, IntSize, ScrutineeMode, Type,
};
use super::TypeErrorKind;

impl<'a> super::TypeChecker<'a> {
    pub(super) fn check_expr(&mut self, expr: &Expr, expected: &Type) -> Type {
        // Built-in collection constructors at check-mode: `Vec.new()` /
        // `VecDeque.new()` / `Set.new()` / `SortedSet.new()` / `Map.new()`
        // resolve to the expected type directly when the surface names
        // line up. Without this short-circuit the constructor's synth-
        // mode return (`Vec[?T]` minted by `infer_call`) flows through
        // `types_compatible`, which can't unify the fresh typevar
        // against `Vec<Fn()>` etc. (the existing legacy callers' shape).
        if let ExprKind::Call { callee, args } = &expr.kind {
            if args.is_empty() {
                if let ExprKind::Path { segments, .. } = &callee.kind {
                    if segments.len() == 2 && segments[1] == "new" {
                        let collection = segments[0].as_str();
                        let matches_expected = match (collection, expected) {
                            ("Vec", Type::Named { name, .. }) => name == "Vec",
                            ("VecDeque", Type::Named { name, .. }) => name == "VecDeque",
                            ("Set", Type::Named { name, .. }) => name == "Set",
                            ("SortedSet", Type::Named { name, .. }) => name == "SortedSet",
                            ("Map", Type::Named { name, .. }) => name == "Map",
                            _ => false,
                        };
                        if matches_expected {
                            self.record_expr_type(&expr.span, expected);
                            return expected.clone();
                        }
                    }
                }
            }
        }

        // Empty prefix-literal (`Vec[]` / `Array[]` / `Set[]` / `Map[]`) at
        // a check-mode position: recover via the expected type. Synthesis-
        // mode use (no annotation, no expected-type carrier) hits the
        // matching arm in `infer_expr_inner` and emits
        // `E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION`. Per design.md
        // § Collection Literals: an empty prefix-literal has no element
        // type to infer.
        if let ExprKind::PrefixCollectionLiteral { type_name, items } = &expr.kind {
            if items.is_empty() {
                let matches_expected = match (type_name.as_str(), expected) {
                    ("Vec", Type::Named { name, .. }) => name == "Vec",
                    ("Set", Type::Named { name, .. }) => name == "Set",
                    ("Map", Type::Named { name, .. }) => name == "Map" || name == "HashMap",
                    ("Array", Type::Array { .. }) => true,
                    _ => false,
                };
                if matches_expected {
                    self.record_expr_type(&expr.span, expected);
                    return expected.clone();
                }
            }
        }
        // Bare-identifier call at an expected-type position: `default()` where
        // expected is `T: Default` or a concrete type with an `impl Default`.
        // Intercepts before normal inference so the typechecker can substitute
        // the missing receiver (`T.default()` / `Wrapper.default()`).
        if let ExprKind::Call { callee, args } = &expr.kind {
            if let ExprKind::Identifier(name) = &callee.kind {
                if let Some(ty) =
                    self.try_apply_expected_assoc_fn_inference(name, args, expected, &expr.span)
                {
                    return ty;
                }
            }
        }

        // Check-mode coercion: bare `[...]` literal → `Array[T, N]` when the
        // expected type is a fixed-size array. This overrides the synthesis-mode
        // default of Vec[T] so annotated lets and typed call arguments work.
        if let (ExprKind::ArrayLiteral(elements), Type::Array { element, size }) =
            (&expr.kind, expected)
        {
            // Length-mismatch check skipped for non-literal sizes (slice 3
            // `ConstParam` / `ConstVar` resolve at mono-emission time).
            if let Some(n) = size.as_usize() {
                if elements.len() != n {
                    self.type_error(
                        format!(
                            "array literal has {} element(s), expected {}",
                            elements.len(),
                            n
                        ),
                        expr.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
            }
            for elem in elements {
                self.check_expr(elem, element);
            }
            self.record_expr_type(&expr.span, expected);
            return expected.clone();
        }
        // Same coercion for bare `[v; n]` against an `Array[T, N]` expected:
        // the literal's count must equal N, and the value's type must match T.
        if let (
            ExprKind::RepeatLiteral {
                type_name: None,
                value,
                count,
            },
            Type::Array { element, size },
        ) = (&expr.kind, expected)
        {
            if let ExprKind::Integer(n, _) = &count.kind {
                // Length-mismatch check skipped for non-literal sizes
                // (slice 3 `ConstParam` / `ConstVar` resolve at mono-
                // emission time).
                if let Some(expected_size) = size.as_usize() {
                    if *n < 0 || *n as usize != expected_size {
                        self.type_error(
                            format!(
                                "repeat-literal count {} does not match expected array length {}",
                                n, expected_size
                            ),
                            count.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
            } else {
                self.type_error(
                    "Array[T, N] repeat-literal requires a non-negative integer literal count"
                        .to_string(),
                    count.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                self.infer_expr(count);
            }
            self.check_expr(value, element);
            self.record_expr_type(&expr.span, expected);
            return expected.clone();
        }
        if let Some(coerced) = self.try_apply_into_coercion(expr, expected) {
            return coerced;
        }
        if let Some(coerced) = self.try_apply_tryinto_coercion(expr, expected) {
            return coerced;
        }
        // Closure pushdown: when expected is `Type::Function { params, return }`
        // (or `Type::OnceFunction { ... }`, item 131 sub-step 3) and `expr` is
        // a closure literal, seed each closure param's type from the expected
        // param type instead of letting the synth path fall back to
        // `fresh_type_var()`. Required for compound type+effect polymorphism
        // (round 10.1 step 2): once the call site has solved `T = Iter[i32]`
        // and substituted `T.Item -> &i32` into the param's `Fn(T.Item) -> ...`,
        // the closure body must be type-checked against that concrete shape.
        // Explicit param annotations on the closure still take priority.
        // OnceFunction slots use the same pushdown — the slot's signature
        // describes call arity/types regardless of repeat-callability, and
        // sub-step 3's `is_subtype` then admits a Function-typed closure
        // into an OnceFunction slot via the cross-arm subsumption rule.
        let expected_fn_shape = match expected {
            Type::Function {
                params,
                return_type,
            }
            | Type::OnceFunction {
                params,
                return_type,
            } => Some((params.as_slice(), return_type.as_ref())),
            _ => None,
        };
        if let (
            ExprKind::Closure {
                params,
                capture_mode,
                prefix_span: _,
                body,
            },
            Some((expected_params, expected_ret)),
        ) = (&expr.kind, expected_fn_shape)
        {
            if params.len() == expected_params.len() {
                // Round 12.44 (Step 2) — once-callability inference must run
                // here too so the closure's actual type reflects whether it
                // consumes a captured outer non-Copy binding. When `expected`
                // is `Type::Function` and the body promotes the closure to
                // `OnceFunction`, the trailing `check_assignable` correctly
                // rejects the cross-pair (Step 1's identity-only subtyping).
                let outer_bindings = self.flatten_local_scope_snapshot();
                let closure_param_names: Vec<String> = params
                    .iter()
                    .flat_map(|p| p.pattern.binding_names())
                    .collect();
                self.local_scope.push();
                let param_types: Vec<Type> = params
                    .iter()
                    .zip(expected_params.iter())
                    .map(|(p, expected_pty)| {
                        let ty = p
                            .ty
                            .as_ref()
                            .map(|t| self.lower_type_expr(t, &[]))
                            .unwrap_or_else(|| expected_pty.clone());
                        if !self.is_irrefutable_pattern(&p.pattern, &ty) {
                            self.type_error(
                                "refutable pattern in closure parameter; use `if let` or `match` for patterns that may not match".to_string(),
                                p.pattern.span.clone(),
                                TypeErrorKind::RefutablePattern,
                            );
                        }
                        self.bind_pattern_types(&p.pattern, &ty);
                        ty
                    })
                    .collect();
                let body_ty = self.check_expr(body, expected_ret);
                self.local_scope.pop();
                let actual = self.closure_type_with_capture_inference(
                    &expr.span,
                    *capture_mode,
                    &closure_param_names,
                    body,
                    &outer_bindings,
                    param_types,
                    body_ty,
                );
                self.check_assignable(expected, &actual, expr.span.clone());
                return actual;
            }
            // Arity mismatch: fall through to the synth path so the existing
            // `check_assignable` produces a normal `Fn` arity diagnostic.
        }

        // Block at check position: thread `expected` through to the
        // trailing expression so closures inside `let x: T = { ...; |a| body }`
        // see `T`'s shape. `check_block_against` already routes the final
        // expression through `check_expr`.
        if let ExprKind::Block(block) = &expr.kind {
            let ty = self.check_block_against(block, expected);
            self.record_expr_type(&expr.span, &ty);
            return ty;
        }

        // If/IfLet at check position: push `expected` into both branches.
        // Each branch's `check_expr` enforces assignability against the
        // expected type independently, so divergent branches surface a
        // per-branch TypeMismatch rather than the synth-mode aggregate
        // BranchTypeMismatch (more specific, points at the offending
        // branch). Condition typing is unchanged.
        if let ExprKind::If {
            condition,
            then_block,
            else_branch,
        } = &expr.kind
        {
            let ty = self.check_if_against(
                condition,
                then_block,
                else_branch.as_deref(),
                expected,
                &expr.span,
            );
            return ty;
        }
        if let ExprKind::IfLet {
            pattern,
            value,
            then_block,
            else_branch,
        } = &expr.kind
        {
            let ty = self.check_if_let_against(
                pattern,
                value,
                then_block,
                else_branch.as_deref(),
                expected,
                &expr.span,
            );
            return ty;
        }

        // Match at check position: each arm body is checked against
        // `expected` so closures in arm bodies (and other check-mode-
        // sensitive shapes) see the target type.
        if let ExprKind::Match { scrutinee, arms } = &expr.kind {
            let ty = self.check_match_against(scrutinee, arms, expected, &expr.span);
            return ty;
        }

        let actual = self.infer_expr(expr);
        // Expected-type-driven generic resolution: when a generic call's
        // return type came back as `TypeParam(T)` (the solver had no arg
        // information to fix `T`), `expected` lets us bind `T` to a concrete
        // name for the interpreter's runtime dispatch stack. Only fires for
        // `Call` expressions — other shapes don't introduce per-call generic
        // bindings.
        if matches!(expr.kind, ExprKind::Call { .. }) {
            if let Type::TypeParam(t_name) = &actual {
                if let Some(target) = type_to_concrete_or_param_name(expected) {
                    if target != *t_name {
                        self.call_type_subs
                            .entry(SpanKey::from_span(&expr.span))
                            .or_default()
                            .insert(t_name.clone(), target);
                    }
                }
            }
        }
        self.check_assignable(expected, &actual, expr.span.clone());
        actual
    }

    /// Recognize `x.into()` at an expected-type position. When `expr` is a
    /// zero-argument method call named `into` and `expected` is a Named type
    /// `T` with a registered `impl From[S] for T` (where `S` is the receiver's
    /// inferred type), record the conversion and return `expected`. Returns
    /// `Some(Error)` when `.into()` matches shape but no suitable From impl
    /// exists (emits a diagnostic). Returns `None` when the expression is not
    /// a `.into()` call — caller falls back to regular inference.
    /// Bare-call expected-type inference: `name(args)` at an expected-type
    /// position resolves to `Target.name(args)` when the expected type narrows
    /// to a single trait (or impl) declaring an associated function called
    /// `name`. Returns `Some(return_type)` on dispatch, `None` to fall through
    /// to the existing inference path. Multiple matching traits → ambiguity
    /// error + `Type::Error`.
    ///
    /// `Type::TypeParam(t)` looks up `t`'s trait bounds via `enclosing_bounds`.
    /// `Type::Named { name }` looks up the type's `impl Trait for Name` blocks
    /// in `env.impls` and uses the registered impl method signature directly.
    fn try_apply_expected_assoc_fn_inference(
        &mut self,
        name: &str,
        args: &[CallArg],
        expected: &Type,
        span: &Span,
    ) -> Option<Type> {
        // If `name` is already a known function, builtin, or local, fall
        // through. Bare-call inference only applies to identifiers that
        // would otherwise be unresolvable at the value layer.
        if self.local_scope.lookup(name).is_some()
            || self.env.functions.contains_key(name)
            || self.env.constants.contains_key(name)
            || matches!(
                name,
                "todo" | "unreachable" | "println" | "print" | "eprintln" | "panic"
            )
        {
            return None;
        }

        match expected {
            Type::TypeParam(target) => {
                let bounds = self.enclosing_bounds.get(target).cloned()?;
                let candidates: Vec<String> = bounds
                    .iter()
                    .filter_map(|b| b.path.last().cloned())
                    .filter(|trait_name| self.find_trait_method(trait_name, name).is_some())
                    .collect();
                match candidates.len() {
                    0 => None,
                    1 => {
                        let trait_method = self.find_trait_method(&candidates[0], name)?.clone();
                        // Record the typeparam target so lowering rewrites
                        // the bare call to `T.name(args)`. At runtime the
                        // interpreter resolves `T` through its substitution
                        // stack to find the concrete impl.
                        self.bare_assoc_fn_targets
                            .insert(SpanKey::from_span(span), target.clone());
                        Some(self.dispatch_trait_assoc_fn(target, &trait_method, args, span))
                    }
                    _ => {
                        let trait_list = candidates
                            .iter()
                            .map(|c| format!("`{}`", c))
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.type_error(
                            format!(
                                "ambiguous associated function '{}' on type parameter '{}': declared by {}. \
                                 Use UFCS `Trait.{}(...)` to disambiguate.",
                                name, target, trait_list, name,
                            ),
                            span.clone(),
                            TypeErrorKind::AmbiguousAssocFn,
                        );
                        Some(Type::Error)
                    }
                }
            }
            Type::Named {
                name: target_name,
                args: target_args,
            } => {
                // Match against impl methods registered on this concrete type.
                // Trait impls and inherent impls share the same `env.impls`
                // table; we collect every impl whose target is `target_name`,
                // whose method set contains `name`, and whose impl-level
                // bounds discharge against the receiver's concrete generic
                // args (slice 1 of the method-resolution CR — see
                // `phase-4-interpreter.md`).
                let matching: Vec<&ImplInfo> = self
                    .env
                    .impls
                    .iter()
                    .filter(|imp| {
                        imp.target_type == *target_name
                            && impl_args_match(&imp.target_args, target_args)
                            && imp.methods.contains_key(name)
                            && self.env.impl_bounds_discharge(imp, target_args)
                    })
                    .collect();
                match matching.len() {
                    0 => None,
                    1 => {
                        let sig = matching[0].methods.get(name)?.clone();
                        // Record the resolved target so lowering can rewrite
                        // the bare call to `Target.name(args)` for the
                        // interpreter / codegen.
                        self.bare_assoc_fn_targets
                            .insert(SpanKey::from_span(span), target_name.clone());
                        Some(self.validate_args_against_sig(name, &sig, args, span))
                    }
                    _ => {
                        let trait_list = matching
                            .iter()
                            .filter_map(|imp| imp.trait_name.clone())
                            .map(|t| format!("`{}`", t))
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.type_error(
                            format!(
                                "ambiguous associated function '{}' on type '{}': declared by {}. \
                                 Use `Trait.{}(...)` to disambiguate.",
                                name, target_name, trait_list, name,
                            ),
                            span.clone(),
                            TypeErrorKind::AmbiguousAssocFn,
                        );
                        Some(Type::Error)
                    }
                }
            }
            _ => None,
        }
    }

    /// Record per-call generic-param substitutions for use by the interpreter
    /// at runtime. Each entry maps a generic param name to a concrete type
    /// name — or to another generic param name when the caller is itself
    /// generic and propagates the binding (the interpreter resolves these
    /// transitively against its runtime substitution stack).
    fn record_call_type_subs(&mut self, span: &Span, solutions: &HashMap<String, Type>) {
        if solutions.is_empty() {
            return;
        }
        let mut frame: HashMap<String, String> = HashMap::new();
        for (name, ty) in solutions {
            if let Some(resolved) = type_to_concrete_or_param_name(ty) {
                frame.insert(name.clone(), resolved);
            }
        }
        if !frame.is_empty() {
            self.call_type_subs.insert(SpanKey::from_span(span), frame);
        }
    }

    /// Type-check call arguments against `(params, return_type)` with the
    /// round-10.1 closure-pushdown logic, returning the (possibly-substituted)
    /// return type. Shared by `infer_call` and the user-defined-method branch
    /// of `infer_method_call` so generic methods get the same inference fix as
    /// generic free functions.
    ///
    /// Behavior:
    /// - Non-generic signature: each arg checked against its slot via
    ///   `check_expr` (already does closure pushdown for monomorphic `Fn(...)`).
    /// - Generic signature: two-pass — non-closure args inferred eagerly to
    ///   solve `T`s, then closure args checked against the substituted slot
    ///   via `check_expr` (so a closure's params see the solved `T`, not a
    ///   fresh var). The substitution is recorded under
    ///   `record_subs_for_span` for downstream consumers (interpreter,
    ///   codegen).
    ///
    /// `apply_call_site_marker` controls the `mut` marker check; pass `false`
    /// for method calls (per design.md, the call-site marker rule applies only
    /// to free-function calls).
    pub(super) fn check_call_args_with_substitution(
        &mut self,
        args: &[CallArg],
        params: &[Type],
        return_type: &Type,
        record_subs_for_span: &Span,
        apply_call_site_marker: bool,
    ) -> Type {
        self.check_call_args_with_substitution_full(
            args,
            params,
            return_type,
            record_subs_for_span,
            apply_call_site_marker,
            None,
            None,
            None,
            record_subs_for_span,
        )
    }

    /// Extended variant of `check_call_args_with_substitution` that
    /// accepts explicit call-site generic args + the function's
    /// declaration-order generic-param names (const generics slice 1c)
    /// and the callee's where-clause for bound discharge (slice 3c).
    /// When `explicit_generic_args` and `formal_generic_params` are
    /// both supplied, each (formal_name, explicit_arg) pair pre-binds
    /// the corresponding metavar so subsequent arg-position
    /// unification flows from the explicit binding. After the
    /// inference solver runs, each `WhereConstraint::ConstPredicate`
    /// in `where_clause` is evaluated against the resolved const-args;
    /// `Bool(false)` triggers a `"const constraint violated"`
    /// diagnostic at `discharge_span`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn check_call_args_with_substitution_full(
        &mut self,
        args: &[CallArg],
        params: &[Type],
        return_type: &Type,
        record_subs_for_span: &Span,
        apply_call_site_marker: bool,
        explicit_generic_args: Option<&[GenericArg]>,
        formal_generic_params: Option<&[String]>,
        where_clause: Option<&WhereClause>,
        discharge_span: &Span,
    ) -> Type {
        // Const generics slice 3c: when the callee declares a
        // where-clause with `ConstPredicate`s, force the full
        // instantiate+unify+resolve+discharge path even if neither
        // params nor return reference a generic — the predicate may
        // reference const-params that don't appear in the signature's
        // types (`fn f[const N: i64]() where N >= 0`). Without this
        // override the early-return below skips discharge entirely.
        let has_where_const_predicate = where_clause
            .map(|wc| {
                wc.constraints
                    .iter()
                    .any(|c| matches!(c, WhereConstraint::ConstPredicate { .. }))
            })
            .unwrap_or(false);
        let has_generic = params.iter().any(contains_type_param)
            || contains_type_param(return_type)
            || has_where_const_predicate;
        if !has_generic {
            for (arg, param_ty) in args.iter().zip(params.iter()) {
                let arg_ty = self.check_expr(&arg.value, param_ty);
                if apply_call_site_marker {
                    self.check_call_site_marker(arg, param_ty, &arg_ty);
                }
            }
            return return_type.clone();
        }
        // Generic case: types-first / effects-second per design.md
        // § Monomorphization order for compound polymorphism. Item 131
        // sub-step 2b — replaces the per-call ad-hoc `solve_type_params`
        // with fresh-metavariable instantiation: each `TypeParam(T)` in
        // the callee's signature becomes a fresh `TypeVar(?M_n)` for
        // this call only, so cross-call collisions are impossible.
        // Pass 1 infers non-closure args and unifies them against the
        // instantiated slot types; pass 2 checks each arg (including
        // closures) against the resolved slot, with check_expr's
        // pushdown seeing concrete (i.e. solved) slot types when
        // available.
        let InstantiatedSignature {
            params: sub_params,
            return_type: sub_ret,
            name_to_id,
            id_to_name,
            name_to_const_id,
            const_id_to_name,
        } = instantiate_signature_with_fresh_vars(
            params,
            return_type,
            &mut self.env.next_type_var,
            &mut self.env.next_const_var,
        );

        // Const generics slice 1c: pre-bind metavars from explicit
        // call-site generic args. Walk the formal-param names and the
        // user-supplied args in lockstep; each `GenericArg::Const`
        // literal binds the corresponding `ConstVar`, each
        // `GenericArg::Type` binds the corresponding `TypeVar`. The
        // subsequent arg-position unification flow runs against these
        // pre-bindings (so a mismatch between explicit and inferred
        // const-args surfaces at the per-position unify call).
        if let (Some(explicit), Some(formal_names)) = (explicit_generic_args, formal_generic_params)
        {
            for (formal_name, explicit_arg) in formal_names.iter().zip(explicit.iter()) {
                if let Some(&const_id) = name_to_const_id.get(formal_name) {
                    if let GenericArg::Const(expr) = explicit_arg {
                        if let Some(cv) = const_value_from_literal(expr) {
                            self.env
                                .const_substitutions
                                .insert(const_id, ConstArg::Literal(cv));
                        }
                    }
                } else if let Some(&type_id) = name_to_id.get(formal_name) {
                    if let GenericArg::Type(te) = explicit_arg {
                        let ty = self.lower_type_expr(te, &[]);
                        self.env.substitutions.insert(type_id, ty);
                    }
                }
            }
        }

        let mut arg_tys: Vec<Option<Type>> = Vec::with_capacity(args.len());
        for arg in args {
            if matches!(arg.value.kind, ExprKind::Closure { .. }) {
                arg_tys.push(None);
            } else {
                arg_tys.push(Some(self.infer_expr(&arg.value)));
            }
        }
        // Pass 1: unify non-closure arg types into the instantiated
        // slot types so the metavars get bound from arguments. Failure
        // is silent here — pass 2's `check_assignable` produces the
        // user-facing diagnostic, and unify already records partial
        // structural matches.
        for (sub_param_ty, arg_ty_opt) in sub_params.iter().zip(arg_tys.iter()) {
            if let Some(arg_ty) = arg_ty_opt {
                unify_types(
                    sub_param_ty,
                    arg_ty,
                    &mut self.env.substitutions,
                    &mut self.env.const_substitutions,
                );
            }
        }
        // Pass 2: check each arg against the resolved slot. For
        // closure args, the resolved slot may be a concrete
        // `Fn(i64) -> i64` (when T solved) and check_expr's pushdown
        // gives the closure params their types.
        for ((arg, sub_param_ty), arg_ty_opt) in
            args.iter().zip(sub_params.iter()).zip(arg_tys.iter())
        {
            let resolved = resolve_type_vars(
                sub_param_ty,
                &self.env.substitutions,
                &id_to_name,
                &self.env.const_substitutions,
                &const_id_to_name,
            );
            let resolved = self.resolve_assoc_projections(&resolved);
            match arg_ty_opt {
                Some(arg_ty) => {
                    self.check_assignable(&resolved, arg_ty, arg.value.span.clone());
                    if apply_call_site_marker {
                        self.check_call_site_marker(arg, &resolved, arg_ty);
                    }
                }
                None => {
                    let arg_ty = self.check_expr(&arg.value, &resolved);
                    if apply_call_site_marker {
                        self.check_call_site_marker(arg, &resolved, &arg_ty);
                    }
                }
            }
        }
        // Translate solved metavars back to the original `T → ConcreteType`
        // shape `record_call_type_subs` expects — this is what the
        // interpreter's runtime dispatch consumes for generic-method
        // resolution. Only entries that resolved to something other
        // than the originating TypeParam are recorded; unsolved ones
        // are skipped so the interpreter's resolution stack doesn't
        // see a self-referential `T → T` binding.
        let mut solutions: HashMap<String, Type> = HashMap::new();
        for (name, &id) in &name_to_id {
            let resolved = resolve_type_vars(
                &Type::TypeVar(id),
                &self.env.substitutions,
                &id_to_name,
                &self.env.const_substitutions,
                &const_id_to_name,
            );
            if !matches!(&resolved, Type::TypeParam(n) if n == name) {
                solutions.insert(name.clone(), resolved);
            }
        }
        self.record_call_type_subs(record_subs_for_span, &solutions);

        // Resolve the return type. Unsolved metavars come back as
        // `TypeParam(originating_name)` so the caller's
        // `find_unbound_type_param` (slice 2a) still surfaces the
        // unsolved-T diagnostic.
        let ret = resolve_type_vars(
            &sub_ret,
            &self.env.substitutions,
            &id_to_name,
            &self.env.const_substitutions,
            &const_id_to_name,
        );
        let ret = self.resolve_assoc_projections(&ret);

        // Const generics slice 3c: discharge `WhereConstraint::ConstPredicate`
        // entries against the resolved const-args. The substitution
        // map is built from two sources: inferred const-args (via
        // `name_to_const_id` + `env.const_substitutions` resolved
        // through `resolve_const_arg`), and explicit call-site args
        // (when supplied — formal-param names paired with
        // `explicit_generic_args` positions). Explicit args win on
        // collision (the user-supplied value pins the predicate
        // discharge directly without needing the inference solver to
        // have minted a ConstVar for the param). Slice 2's
        // `eval_const_expr` consumes the substituted predicate.
        if let Some(wc) = where_clause {
            let mut const_arg_subst: HashMap<String, i64> = HashMap::new();
            for (name, &id) in &name_to_const_id {
                let resolved = resolve_const_arg(
                    &ConstArg::ConstVar(id),
                    &self.env.const_substitutions,
                    &const_id_to_name,
                );
                if let ConstArg::Literal(n) = resolved {
                    const_arg_subst.insert(name.clone(), n);
                }
            }
            if let (Some(explicit), Some(formal_names)) =
                (explicit_generic_args, formal_generic_params)
            {
                for (formal_name, explicit_arg) in formal_names.iter().zip(explicit.iter()) {
                    if let GenericArg::Const(e) = explicit_arg {
                        if let Some(v) = const_value_from_literal(e) {
                            const_arg_subst.insert(formal_name.clone(), v);
                        }
                    }
                }
            }
            self.discharge_const_predicates(wc, &const_arg_subst, discharge_span);
            // Trait-bounds-at-codegen enforcement (slice 0.a, sub-step 1
            // of monomorphized collections prereq). Walks
            // `WhereConstraint::TypeBound` predicates in the same where-
            // clause and verifies each formal type-param's concrete
            // binding satisfies its declared bounds. Inline param bounds
            // (`fn f[T: Hash + Eq](...)`) were normalized into the
            // where-clause at FunctionSig construction
            // (`normalize_bounds_into_where_clause`) so this single
            // discharge call covers both inline and where-clause surfaces.
            self.discharge_type_bounds(wc, &solutions, discharge_span);
        }

        ret
    }

    /// Walk a where-clause and discharge each `TypeBound { T: Trait, ... }`
    /// predicate against the resolved type substitution. For each formal
    /// type-param T bound to a concrete type via `solutions`, check that
    /// the concrete type satisfies the trait. Emits a `TypeMismatch`
    /// diagnostic on miss.
    ///
    /// Built-in trait coverage (Hash / Eq / PartialEq / Ord / PartialOrd /
    /// Display on primitives, plus `#[derive(...)]` on named struct/enum
    /// types) flows through `type_satisfies_bound`, which consults the
    /// existing `type_supports_*` helpers before falling back to the
    /// `env.impls` table lookup.
    ///
    /// Slice 0.a, sub-step 1 of monomorphized collections prereq
    /// ([`phase-7-codegen.md`](../docs/implementation_checklist/phase-7-codegen.md)).
    /// Counterpart to `discharge_const_predicates` for ConstPredicate
    /// where-clauses (const generics slice 3c).
    fn discharge_type_bounds(
        &mut self,
        where_clause: &WhereClause,
        solutions: &HashMap<String, Type>,
        discharge_span: &Span,
    ) {
        for constraint in &where_clause.constraints {
            let WhereConstraint::TypeBound {
                type_name, bounds, ..
            } = constraint
            else {
                continue;
            };
            let Some(concrete_ty) = solutions.get(type_name) else {
                // Param unbound at this call site — the unsolved-T
                // diagnostic (slice 2a) handles this; don't double-report.
                continue;
            };
            if matches!(
                concrete_ty,
                Type::TypeParam(_) | Type::TypeVar(_) | Type::Error
            ) {
                // Unresolved metavar / propagating-param / already-error —
                // upstream diagnostics handle. Avoid noise.
                continue;
            }
            for bound in bounds {
                let Some(trait_name) = bound.path.last() else {
                    continue;
                };
                if self.type_satisfies_bound(concrete_ty, trait_name) {
                    continue;
                }
                self.type_error(
                    format!(
                        "trait bound `{}: {}` is not satisfied; `{}` does not implement `{}`",
                        type_name,
                        trait_name,
                        type_display(concrete_ty),
                        trait_name
                    ),
                    discharge_span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
            }
        }
    }

    /// Check whether `ty` satisfies the named trait. Consults three
    /// sources in order:
    ///
    /// 1. **Built-in primitive coverage** for standard traits (Hash, Eq,
    ///    PartialEq, Ord, PartialOrd, Display) — primitives like `i64` /
    ///    `char` / `bool` satisfy these implicitly. The existing
    ///    `type_supports_*` helpers carry this knowledge, including
    ///    `#[derive(...)]`-driven satisfaction on named struct / enum types.
    /// 2. **Other named traits** via the impl table — direct impl lookup
    ///    plus supertrait closure walk via `env.type_satisfies_trait`.
    ///
    /// Returns `false` for types that can't satisfy nominal trait bounds
    /// (function types, raw pointers, type variables) — the discharge
    /// engine guards `TypeVar` / `TypeParam` / `Error` upstream so those
    /// don't reach here in practice.
    pub(super) fn type_satisfies_bound(&self, ty: &Type, trait_name: &str) -> bool {
        // Built-in coverage via the type_supports_* helpers — these
        // recognize primitives implicitly + named types via
        // `#[derive(Trait)]` registration.
        match trait_name {
            "Hash" => return self.type_supports_hash(ty),
            "Eq" => return self.type_supports_eq(ty),
            "PartialEq" => return self.type_supports_partial_eq(ty),
            "Ord" => return self.type_supports_ord(ty),
            "PartialOrd" => return self.type_supports_partial_ord(ty),
            "Display" => return self.type_supports_display(ty),
            _ => {}
        }
        // Other traits: explicit impl in the table, with supertrait closure.
        let Some((ty_name, ty_args)) = impl_table_key(ty) else {
            return false;
        };
        self.env
            .type_satisfies_trait(&ty_name, &ty_args, trait_name)
    }

    /// Walk a where-clause and discharge each `ConstPredicate(expr)`
    /// against the resolved const-args (const generics slice 3c).
    /// Substitutes `Identifier(name)` references in the predicate with
    /// `Integer(value)` literals from `const_arg_subst`, then evaluates
    /// via `eval_const_expr` against `Type::Bool`. Emits a focused
    /// `"const constraint violated"` diagnostic on `Bool(false)`; other
    /// eval errors propagate via the existing `emit_const_eval_error`.
    fn discharge_const_predicates(
        &mut self,
        where_clause: &WhereClause,
        const_arg_subst: &HashMap<String, i64>,
        discharge_span: &Span,
    ) {
        for constraint in &where_clause.constraints {
            let WhereConstraint::ConstPredicate { expr, .. } = constraint else {
                continue;
            };
            let substituted = substitute_const_idents_in_expr(expr, const_arg_subst);
            match self.eval_const_expr(&substituted, &Type::Bool) {
                Ok(crate::prelude::ConstValue::Bool(true)) => {}
                Ok(crate::prelude::ConstValue::Bool(false)) => {
                    let bindings_summary: Vec<String> = const_arg_subst
                        .iter()
                        .map(|(n, v)| format!("{}={}", n, v))
                        .collect();
                    let bindings_str = if bindings_summary.is_empty() {
                        String::new()
                    } else {
                        format!(" with {}", bindings_summary.join(", "))
                    };
                    self.type_error(
                        format!(
                            "const constraint violated: predicate is false{}",
                            bindings_str
                        ),
                        discharge_span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                Ok(_) => {
                    // Non-Bool result — the predicate expression isn't a
                    // boolean test. Slice 2's evaluator routes type
                    // mismatches through ConstEvalError, but the
                    // surface here is "predicate must return bool" —
                    // skip silently (slice 2's per-operator checks
                    // already surfaced any type errors).
                }
                Err(e) => self.emit_const_eval_error(e),
            }
        }
    }

    /// Validate `args` against a concrete `FunctionSig`. Used by the
    /// expected-type bare-call dispatch when the target is a concrete type and
    /// the impl's stored signature is the source of truth (no Self
    /// substitution needed).
    fn validate_args_against_sig(
        &mut self,
        name: &str,
        sig: &FunctionSig,
        args: &[CallArg],
        span: &Span,
    ) -> Type {
        if args.len() != sig.params.len() {
            self.type_error(
                format!(
                    "associated function '{}' expects {} argument(s), found {}",
                    name,
                    sig.params.len(),
                    args.len()
                ),
                span.clone(),
                TypeErrorKind::WrongNumberOfArgs,
            );
            for arg in args {
                self.infer_expr(&arg.value);
            }
            return sig.return_type.clone();
        }
        for (arg, param_ty) in args.iter().zip(sig.params.iter()) {
            let arg_ty = self.infer_expr(&arg.value);
            self.check_assignable(param_ty, &arg_ty, arg.value.span.clone());
        }
        sig.return_type.clone()
    }

    fn try_apply_into_coercion(&mut self, expr: &Expr, expected: &Type) -> Option<Type> {
        let ExprKind::MethodCall {
            object,
            method,
            args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        if method != "into" || !args.is_empty() {
            return None;
        }
        let target_name = match expected {
            Type::Named { name, .. } => name.clone(),
            Type::Int(_) | Type::UInt(_) | Type::Float(_) | Type::Bool | Type::Char | Type::Str => {
                type_display(expected)
            }
            _ => return None,
        };
        let src_ty = self.infer_expr(object);
        if src_ty == Type::Error {
            self.record_expr_type(&expr.span, &Type::Error);
            return Some(Type::Error);
        }
        if self
            .env
            .find_from_impl(&src_ty, &target_name, &[])
            .is_some()
        {
            self.into_conversions
                .insert(SpanKey::from_span(&expr.span), target_name);
            self.record_expr_type(&expr.span, expected);
            return Some(expected.clone());
        }
        self.type_error(
            format!(
                "no `impl From[{}] for {}` is in scope; cannot `.into()`",
                type_display(&src_ty),
                target_name
            ),
            expr.span.clone(),
            TypeErrorKind::TypeMismatch,
        );
        self.record_expr_type(&expr.span, &Type::Error);
        Some(Type::Error)
    }

    /// Recognize `x.try_into()` at an expected `Result[Target, _]` position.
    /// Mirrors `try_apply_into_coercion` with one twist: the target type is
    /// `Result.args[0]`, not the bare expected type. On a hit (matching
    /// `impl TryFrom[S] for Target`), records the rewrite span in
    /// `try_into_conversions` and returns the expected `Result[Target, E]`.
    /// On a miss, emits a "no `impl TryFrom[S] for T`" diagnostic and returns
    /// `Type::Error`. Returns `None` (caller falls through) when the
    /// expression isn't a zero-arg `.try_into()` call or when the expected
    /// type isn't `Result[_, _]`.
    fn try_apply_tryinto_coercion(&mut self, expr: &Expr, expected: &Type) -> Option<Type> {
        let ExprKind::MethodCall {
            object,
            method,
            args,
            ..
        } = &expr.kind
        else {
            return None;
        };
        if method != "try_into" || !args.is_empty() {
            return None;
        }
        // Expected must be `Result[Target, _]`. Extract Target.
        let target_ty = match expected {
            Type::Named { name, args } if name == "Result" && args.len() == 2 => &args[0],
            _ => return None,
        };
        let target_name = match target_ty {
            Type::Named { name, .. } => name.clone(),
            Type::Int(_) | Type::UInt(_) | Type::Float(_) | Type::Bool | Type::Char | Type::Str => {
                type_display(target_ty)
            }
            _ => return None,
        };
        let src_ty = self.infer_expr(object);
        if src_ty == Type::Error {
            self.record_expr_type(&expr.span, &Type::Error);
            return Some(Type::Error);
        }
        if self
            .env
            .find_tryfrom_impl(&src_ty, &target_name, &[])
            .is_some()
        {
            self.try_into_conversions
                .insert(SpanKey::from_span(&expr.span), target_name);
            self.record_expr_type(&expr.span, expected);
            return Some(expected.clone());
        }
        self.type_error(
            format!(
                "no `impl TryFrom[{}] for {}` is in scope; cannot `.try_into()`",
                type_display(&src_ty),
                target_name
            ),
            expr.span.clone(),
            TypeErrorKind::TypeMismatch,
        );
        self.record_expr_type(&expr.span, &Type::Error);
        Some(Type::Error)
    }

    pub(super) fn infer_expr(&mut self, expr: &Expr) -> Type {
        let ty = self.infer_expr_inner(expr);
        self.record_expr_type(&expr.span, &ty);
        ty
    }

    fn infer_expr_inner(&mut self, expr: &Expr) -> Type {
        match &expr.kind {
            // Literals
            ExprKind::Integer(_, sfx) => self.type_from_int_suffix(*sfx, expr.span.clone()),
            ExprKind::Float(_, sfx) => Self::type_from_float_suffix(*sfx),
            ExprKind::CharLit(_) => Type::Char,
            ExprKind::StringLit(_) | ExprKind::MultiStringLit(_) => Type::Str,
            ExprKind::InterpolatedStringLit(parts) => {
                for part in parts {
                    if let ParsedInterpolationPart::Expr(inner_expr) = part {
                        let ty = self.infer_expr(inner_expr);
                        if ty != Type::Error && !self.type_supports_display(&ty) {
                            self.type_error(
                                format!(
                                    "type '{}' does not implement Display; \
                                     cannot interpolate in f-string",
                                    type_display(&ty)
                                ),
                                inner_expr.span.clone(),
                                TypeErrorKind::TraitBoundNotSatisfied,
                            );
                        }
                    }
                }
                Type::Str
            }
            ExprKind::Bool(_) => Type::Bool,

            // Identifiers
            ExprKind::Identifier(name) => self.resolve_identifier_type(name, &expr.span),
            ExprKind::Path { segments, .. } => self.resolve_path_type(segments, &expr.span),

            ExprKind::SelfValue => self.current_self_type.clone().unwrap_or(Type::Error),
            ExprKind::SelfType => self.current_self_type.clone().unwrap_or(Type::Error),

            // Operators
            ExprKind::Binary { op, left, right } => self.infer_binary(op, left, right, &expr.span),
            ExprKind::Pipe { left, right } => self.infer_pipe(left, right, &expr.span),
            ExprKind::Unary { op, operand } => self.infer_unary(op, operand, &expr.span),

            // Postfix
            ExprKind::Question(inner) => {
                if self.in_defer {
                    self.type_error(
                        "'?' operator is not allowed inside defer/errdefer blocks".to_string(),
                        expr.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                self.infer_question(inner, &expr.span)
            }

            ExprKind::OptionalChain { object, .. } => {
                let _obj_ty = self.infer_expr(object);
                Type::Error // Needs advanced option handling, stubbed for now
            }

            // Infix
            ExprKind::NilCoalesce { left, right } => {
                let l_ty = self.infer_expr(left);
                let r_ty = self.infer_expr(right);
                if l_ty != Type::Error && r_ty != Type::Error {
                    if let Type::Named { name, args } = &l_ty {
                        if name == "Option" && args.len() == 1 {
                            self.check_assignable(&args[0], &r_ty, right.span.clone());
                            return args[0].clone();
                        }
                    }
                }
                Type::Error
            }

            ExprKind::Call { callee, args } => self.infer_call(callee, args, &expr.span),

            ExprKind::MethodCall {
                object,
                method,
                args,
                turbofish: _,
            } => self.infer_method_call(object, method, args, &expr.span),

            ExprKind::FieldAccess { object, field } => {
                self.infer_field_access(object, field, &expr.span)
            }

            ExprKind::TupleIndex { object, index } => {
                let obj_ty = self.infer_expr(object);
                match &obj_ty {
                    Type::Tuple(types) => {
                        let idx = *index as usize;
                        if idx < types.len() {
                            types[idx].clone()
                        } else {
                            self.type_error(
                                format!(
                                    "tuple index {} out of bounds for tuple of length {}",
                                    idx,
                                    types.len()
                                ),
                                expr.span.clone(),
                                TypeErrorKind::InvalidTupleIndex,
                            );
                            Type::Error
                        }
                    }
                    Type::Error => Type::Error,
                    _ => {
                        self.type_error(
                            format!("tuple index on non-tuple type '{}'", type_display(&obj_ty)),
                            expr.span.clone(),
                            TypeErrorKind::InvalidTupleIndex,
                        );
                        Type::Error
                    }
                }
            }

            ExprKind::Index { object, index } => {
                let obj_ty = self.infer_expr(object);
                let idx_ty = self.infer_expr(index);
                let is_range_idx = matches!(&idx_ty, Type::Named { name, .. }
                    if matches!(name.as_str(), "Range" | "RangeInclusive" | "RangeFrom"
                        | "RangeTo" | "RangeToInclusive" | "RangeFull"));
                if !is_integer(&idx_ty) && !is_range_idx && idx_ty != Type::Error {
                    self.type_error(
                        format!(
                            "index must be an integer or range, found '{}'",
                            type_display(&idx_ty)
                        ),
                        index.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                if is_range_idx {
                    // Range indexing: `collection[a..b]` → `Slice[T]` where T
                    // is the element type of the indexed collection. See
                    // design.md § Slices and § Subscript Trait.
                    let element_ty = match &obj_ty {
                        Type::Array { element, .. } => Some(*element.clone()),
                        Type::Slice { element, .. } => Some(*element.clone()),
                        Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                            Some(args[0].clone())
                        }
                        Type::Ref(inner) | Type::MutRef(inner) => match inner.as_ref() {
                            Type::Array { element, .. } => Some(*element.clone()),
                            Type::Slice { element, .. } => Some(*element.clone()),
                            Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                                Some(args[0].clone())
                            }
                            _ => None,
                        },
                        Type::Error => return Type::Error,
                        _ => None,
                    };
                    return match element_ty {
                        Some(el) => Type::Slice {
                            element: Box::new(el),
                            mutable: false,
                        },
                        None => {
                            self.type_error(
                                format!(
                                    "range indexing requires a Vec, Array, or Slice; found '{}'",
                                    type_display(&obj_ty)
                                ),
                                expr.span.clone(),
                                TypeErrorKind::TypeMismatch,
                            );
                            Type::Error
                        }
                    };
                }
                match &obj_ty {
                    Type::Array { element, .. } => *element.clone(),
                    Type::Slice { element, .. } => *element.clone(),
                    Type::Named { name, args } if name == "Vec" && args.len() == 1 => {
                        args[0].clone()
                    }
                    Type::Error => Type::Error,
                    _ => Type::Error,
                }
            }

            // Compound
            ExprKind::Block(block) => self.infer_block(block),

            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
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
                let then_ty = self.infer_block(then_block);
                if let Some(ref else_expr) = else_branch {
                    let else_ty = self.infer_expr(else_expr);
                    if then_ty == Type::Never {
                        return else_ty;
                    }
                    if else_ty == Type::Never {
                        return then_ty;
                    }
                    if !types_compatible(&then_ty, &else_ty)
                        && then_ty != Type::Error
                        && else_ty != Type::Error
                    {
                        self.type_error(
                            format!(
                                "if/else branches have incompatible types: '{}' and '{}'",
                                type_display(&then_ty),
                                type_display(&else_ty)
                            ),
                            expr.span.clone(),
                            TypeErrorKind::BranchTypeMismatch,
                        );
                    }
                    then_ty
                } else {
                    Type::Unit
                }
            }

            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                let scrut_ty = self.infer_expr(value);
                // Bind the pattern's variables for the duration of the
                // then-block so identifier-leaf bindings inside if-let
                // (e.g. `if let Some(l) = cur.left { queue.push_back(l); }`)
                // get the right scrutinee-derived type. Without this the
                // pattern's bindings stay un-typed (silent fall-through
                // to `Type::Error`), which breaks downstream
                // `pattern_binding_types` recording, codegen's
                // `var_type_names` propagation, and method dispatch.
                let (mode, dispatch_ty) = ScrutineeMode::classify(&scrut_ty);
                let dispatch_ty = dispatch_ty.clone();
                self.local_scope.push();
                self.check_pattern_against(pattern, &dispatch_ty, mode);
                let then_ty = self.infer_block(then_block);
                self.local_scope.pop();
                if let Some(ref else_expr) = else_branch {
                    let else_ty = self.infer_expr(else_expr);
                    if then_ty == Type::Never {
                        return else_ty;
                    }
                    if else_ty == Type::Never {
                        return then_ty;
                    }
                    if !types_compatible(&then_ty, &else_ty)
                        && then_ty != Type::Error
                        && else_ty != Type::Error
                    {
                        self.type_error(
                            format!(
                                "if let/else branches have incompatible types: '{}' and '{}'",
                                type_display(&then_ty),
                                type_display(&else_ty)
                            ),
                            expr.span.clone(),
                            TypeErrorKind::BranchTypeMismatch,
                        );
                    }
                    then_ty
                } else {
                    Type::Unit
                }
            }

            ExprKind::Match { scrutinee, arms } => self.infer_match(scrutinee, arms, &expr.span),

            ExprKind::While {
                condition, body, ..
            } => {
                let cond_ty = self.infer_expr(condition);
                if cond_ty != Type::Bool && cond_ty != Type::Error {
                    self.type_error(
                        format!(
                            "while condition must be 'bool', found '{}'",
                            type_display(&cond_ty)
                        ),
                        condition.span.clone(),
                        TypeErrorKind::ConditionNotBool,
                    );
                }
                self.infer_block(body);
                Type::Unit
            }

            ExprKind::For {
                pattern,
                iterable,
                body,
                ..
            } => {
                let iter_ty = self.infer_expr(iterable);
                self.local_scope.push();
                // Resolve element type via IntoIterator.Item (impl_assoc_types),
                // covering Vec, Map, SortedSet, Set, Slice, Array, Range* and
                // any user type that has registered an "Item" assoc binding.
                let elem_ty = self.element_type_of(&iter_ty);
                self.bind_pattern_types(pattern, &elem_ty);
                for stmt in &body.stmts {
                    self.check_stmt(stmt);
                }
                if let Some(ref final_expr) = body.final_expr {
                    self.infer_expr(final_expr);
                }
                self.local_scope.pop();
                Type::Unit
            }

            ExprKind::Loop { body, .. } => {
                self.infer_block(body);
                Type::Never
            }

            ExprKind::LabeledBlock { label, body, .. } => {
                // LB3 — push a fresh per-label collector frame, infer the
                // body's tail type, pop the frame, and compute the block's
                // type as the LUB of `tail_type` and the collected
                // `break label expr` value types.
                self.break_value_types.push((label.clone(), Vec::new()));
                let tail_ty = self.infer_block(body);
                let frame = self
                    .break_value_types
                    .pop()
                    .map(|(_, v)| v)
                    .unwrap_or_default();
                lub_block_type(tail_ty, &frame)
            }

            ExprKind::Closure {
                params,
                capture_mode,
                prefix_span: _,
                body,
            } => {
                // Round 12.44 (Step 2) — once-callability inference at construction.
                // Snapshot the OUTER local scope before pushing the closure's
                // own param scope so the body walker can identify which
                // identifiers refer to outer bindings (captures).
                let outer_bindings = self.flatten_local_scope_snapshot();
                let closure_param_names: Vec<String> = params
                    .iter()
                    .flat_map(|p| p.pattern.binding_names())
                    .collect();
                // LB4 — closure-boundary rule for the LUB collector. A
                // `break label` inside a closure body cannot target an
                // enclosing labeled block (the resolver rejects it as
                // `undefined loop label`), but we still save/restore the
                // collector stack defensively so an inner labeled-block
                // frame doesn't leak across closure bodies if the
                // resolver's check is bypassed (e.g., during
                // single-phase typechecker tests). Closure bodies start
                // with a fresh empty stack; restored on exit.
                let saved_break_values = std::mem::take(&mut self.break_value_types);
                self.local_scope.push();
                let param_types: Vec<Type> = params
                    .iter()
                    .map(|p| {
                        let ty =
                            p.ty.as_ref()
                                .map(|t| self.lower_type_expr(t, &[]))
                                .unwrap_or_else(|| self.env.fresh_type_var());
                        if !self.is_irrefutable_pattern(&p.pattern, &ty) {
                            self.type_error(
                                "refutable pattern in closure parameter; use `if let` or `match` for patterns that may not match".to_string(),
                                p.pattern.span.clone(),
                                TypeErrorKind::RefutablePattern,
                            );
                        }
                        self.bind_pattern_types(&p.pattern, &ty);
                        ty
                    })
                    .collect();
                let body_ty = self.infer_expr(body);
                self.local_scope.pop();
                self.break_value_types = saved_break_values;
                self.closure_type_with_capture_inference(
                    &expr.span,
                    *capture_mode,
                    &closure_param_names,
                    body,
                    &outer_bindings,
                    param_types,
                    body_ty,
                )
            }

            ExprKind::Return(inner) => {
                if let Some(ref expr) = inner {
                    if let Some(ref ret_ty) = self.current_return_type.clone() {
                        self.check_expr(expr, ret_ty);
                    } else {
                        self.infer_expr(expr);
                    }
                } else if let Some(ref ret_ty) = self.current_return_type.clone() {
                    if *ret_ty != Type::Unit && *ret_ty != Type::Error {
                        self.type_error(
                            format!("expected return value of type '{}'", type_display(ret_ty)),
                            expr.span.clone(),
                            TypeErrorKind::ReturnTypeMismatch,
                        );
                    }
                }
                Type::Never
            }

            ExprKind::Break { label, value } => {
                let val_ty = if let Some(ref e) = value {
                    self.infer_expr(e)
                } else {
                    Type::Unit
                };
                // LB3 — feed the per-label LUB collector for labeled
                // blocks. Find the matching frame by label name (innermost
                // wins) and append the value type. Unlabeled `break`s
                // and breaks targeting a labeled loop have no matching
                // collector frame and are ignored here — loops keep
                // their `Type::Never`-by-default behavior.
                if let Some(name) = label {
                    if let Some(frame) = self
                        .break_value_types
                        .iter_mut()
                        .rev()
                        .find(|(n, _)| n == name)
                    {
                        frame.1.push(val_ty);
                    }
                }
                Type::Never
            }
            ExprKind::Continue { .. } => Type::Never,

            ExprKind::Tuple(exprs) => {
                let types: Vec<Type> = exprs.iter().map(|e| self.infer_expr(e)).collect();
                Type::Tuple(types)
            }

            ExprKind::StructLiteral {
                path,
                fields,
                spread,
            } => {
                if let Some(ref spread_expr) = spread {
                    self.infer_expr(spread_expr);
                }
                self.infer_struct_literal(path, fields, &expr.span)
            }

            ExprKind::Cast { expr: inner, ty } => {
                let from_ty = self.infer_expr(inner);
                let to_ty = self.lower_type_expr(ty, &[]);
                self.check_cast_pair(&from_ty, &to_ty, &inner.span);
                to_ty
            }

            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                let start_ty = start.as_deref().map(|e| self.infer_expr(e));
                let end_ty = end.as_deref().map(|e| self.infer_expr(e));
                // When both bounds are present, verify they share a type.
                if let (Some(ref s), Some(ref e)) = (&start_ty, &end_ty) {
                    if !types_compatible(s, e) && *s != Type::Error && *e != Type::Error {
                        self.type_error(
                            format!(
                                "range bounds must have same type: '{}' and '{}'",
                                type_display(s),
                                type_display(e)
                            ),
                            expr.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                    }
                }
                // Synthesise the appropriate Range variant.
                let elem_ty = start_ty.or(end_ty).unwrap_or(Type::Int(IntSize::I64));
                let name = match (start.is_some(), end.is_some(), inclusive) {
                    (true, true, false) => "Range",
                    (true, true, true) => "RangeInclusive",
                    (true, false, _) => "RangeFrom",
                    (false, true, false) => "RangeTo",
                    (false, true, true) => "RangeToInclusive",
                    (false, false, _) => "RangeFull",
                };
                if name == "RangeFull" {
                    Type::Named {
                        name: "RangeFull".to_string(),
                        args: vec![],
                    }
                } else {
                    Type::Named {
                        name: name.to_string(),
                        args: vec![elem_ty],
                    }
                }
            }

            ExprKind::Unsafe(block) => self.infer_block(block),

            ExprKind::Try(block) => {
                // v1 stub — typechecker pipeline (?-retargeting against
                // the block, error-type unification, From-chain coercion)
                // lands in P1 per design.md § Error Handling > Try Blocks.
                // We still type-check inner expressions so unrelated
                // errors inside the body still surface; the block's
                // overall type is the error sentinel.
                self.infer_block(block);
                self.type_error(
                    "error[E_TRY_BLOCK_NOT_IMPLEMENTED_YET]: try block syntax \
                     is recognized but the typechecker pipeline lands in P1 \
                     — extract the body into a helper function returning \
                     Result for now"
                        .to_string(),
                    expr.span.clone(),
                    TypeErrorKind::TypeMismatch,
                );
                Type::Error
            }

            ExprKind::WhileLet { value, body, .. } => {
                self.infer_expr(value);
                self.infer_block(body);
                Type::Unit
            }

            ExprKind::Seq(block) => self.infer_block(block),
            ExprKind::Par(block) => self.infer_block(block),

            ExprKind::Lock { body, .. } => self.infer_block(body),

            ExprKind::Providers { bindings, body } => {
                // Provider values are plain expressions; infer their types
                // for side effects (diagnostics, subexpression typing). The
                // block's type is the body's type. Full provider-trait
                // conformance — verifying each provider implements the
                // resource's declared `ProviderTrait` — is deferred along
                // with the `Send + Sync` auto-trait enforcement tracked at
                // `docs/deferred.md § Send + Sync Enforcement on
                // with_provider Concrete Provider Type`.
                for b in bindings {
                    self.infer_expr(&b.value);
                }
                self.infer_block(body)
            }

            ExprKind::ArrayLiteral(elements) => {
                // Bare `[...]` defaults to `Vec[T]` in synthesis mode.
                // Use check_expr when an Array annotation is present (handled in check_expr).
                if elements.is_empty() {
                    Type::Named {
                        name: "Vec".to_string(),
                        args: vec![Type::Error],
                    }
                } else {
                    let first_ty = self.infer_expr(&elements[0]);
                    for elem in &elements[1..] {
                        let elem_ty = self.infer_expr(elem);
                        self.check_assignable(&first_ty, &elem_ty, elem.span.clone());
                    }
                    Type::Named {
                        name: "Vec".to_string(),
                        args: vec![first_ty],
                    }
                }
            }

            ExprKind::PrefixCollectionLiteral { type_name, items } => {
                // Empty prefix-literal in synthesis mode — no element type
                // to infer. Check-mode (`let v: Vec[T] = Vec[]`, typed call
                // arguments, typed struct-field initializers) intercepts
                // earlier in `check_expr` and recovers via the expected
                // type. Anything that reaches this branch had no annotation
                // and gets the focused
                // `E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION` diagnostic per
                // design.md § Collection Literals.
                if items.is_empty() {
                    self.report_empty_prefix_literal(type_name, &expr.span);
                    return match type_name.as_str() {
                        "Array" => Type::Array {
                            element: Box::new(Type::Error),
                            size: ConstArg::Literal(0),
                        },
                        _ => Type::Named {
                            name: type_name.clone(),
                            args: vec![Type::Error],
                        },
                    };
                }
                match type_name.as_str() {
                    "Array" => {
                        let first_ty = self.infer_expr(&items[0]);
                        for item in &items[1..] {
                            let ty = self.infer_expr(item);
                            self.check_assignable(&first_ty, &ty, item.span.clone());
                        }
                        Type::Array {
                            element: Box::new(first_ty),
                            size: ConstArg::Literal(items.len() as i64),
                        }
                    }
                    "Vec" => {
                        let first_ty = self.infer_expr(&items[0]);
                        for item in &items[1..] {
                            let ty = self.infer_expr(item);
                            self.check_assignable(&first_ty, &ty, item.span.clone());
                        }
                        Type::Named {
                            name: "Vec".to_string(),
                            args: vec![first_ty],
                        }
                    }
                    "Set" => {
                        let first_ty = self.infer_expr(&items[0]);
                        for item in &items[1..] {
                            let ty = self.infer_expr(item);
                            self.check_assignable(&first_ty, &ty, item.span.clone());
                        }
                        Type::Named {
                            name: "Set".to_string(),
                            args: vec![first_ty],
                        }
                    }
                    other => {
                        // Map's `Map[k: v, ...]` form goes through
                        // `ExprKind::MapLiteral` separately; this arm
                        // catches future prefix-literal types and the
                        // `Map[v1, v2, ...]` (positional-only, no `:`) shape
                        // — which the parser does not emit today but is
                        // future-compatible.
                        let first_ty = self.infer_expr(&items[0]);
                        for item in &items[1..] {
                            self.infer_expr(item);
                        }
                        Type::Named {
                            name: other.to_string(),
                            args: vec![first_ty],
                        }
                    }
                }
            }

            ExprKind::RepeatLiteral {
                type_name,
                value,
                count,
            } => {
                let elem_ty = self.infer_expr(value);
                let count_ty = self.infer_expr(count);
                // Count must be an integer type; report otherwise but keep going.
                let count_is_int = matches!(count_ty, Type::Int(_) | Type::UInt(_) | Type::Error);
                if !count_is_int {
                    self.type_error(
                        format!(
                            "repeat-literal count must be an integer, found '{}'",
                            type_display(&count_ty)
                        ),
                        count.span.clone(),
                        TypeErrorKind::TypeMismatch,
                    );
                }
                match type_name.as_deref() {
                    Some("Array") => {
                        // `Array[v; n]` requires a compile-time integer literal.
                        let size = match &count.kind {
                            ExprKind::Integer(n, _) if *n >= 0 => *n as usize,
                            _ => {
                                self.type_error(
                                    "Array[v; n] requires n to be a non-negative integer literal"
                                        .to_string(),
                                    count.span.clone(),
                                    TypeErrorKind::TypeMismatch,
                                );
                                0
                            }
                        };
                        Type::Array {
                            element: Box::new(elem_ty),
                            size: ConstArg::Literal(size as i64),
                        }
                    }
                    None | Some("Vec") => {
                        // Bare `[v; n]` defaults to `Vec[T]` in synthesis mode
                        // (check_expr coerces against `Array[T, N]` when an
                        // array annotation is present).
                        Type::Named {
                            name: "Vec".to_string(),
                            args: vec![elem_ty],
                        }
                    }
                    Some(other) => {
                        self.type_error(
                            format!(
                                "{}[v; n] is not supported; repeat literals only apply to `Vec` and `Array`",
                                other
                            ),
                            expr.span.clone(),
                            TypeErrorKind::TypeMismatch,
                        );
                        Type::Error
                    }
                }
            }

            ExprKind::MapLiteral(entries) => {
                let (first_key, first_val) = &entries[0];
                let key_ty = self.infer_expr(first_key);
                let val_ty = self.infer_expr(first_val);
                for (k, v) in &entries[1..] {
                    let kt = self.infer_expr(k);
                    let vt = self.infer_expr(v);
                    self.check_assignable(&key_ty, &kt, k.span.clone());
                    self.check_assignable(&val_ty, &vt, v.span.clone());
                }
                Type::Named {
                    name: "HashMap".to_string(),
                    args: vec![key_ty, val_ty],
                }
            }

            ExprKind::PipePlaceholder => {
                self.type_error(
                    "'_' placeholder is only valid inside a pipe expression argument list"
                        .to_string(),
                    expr.span.clone(),
                    TypeErrorKind::InvalidPipePlaceholder,
                );
                Type::Error
            }

            ExprKind::OffsetOf { ty, field_path } => {
                self.infer_offset_of(ty, field_path, &expr.span)
            }

            ExprKind::Error => Type::Error,
        }
    }
}
