//! Closure-capture inference and the once-callability walker.
//!
//! Houses `closure_type_with_capture_inference` (the driver that
//! computes a closure's `Fn` vs. `OnceFn` type from its body),
//! `closure_consumes_captured_non_copy` (the use-predicate scan),
//! the recursive `walk_capture_consume` / `walk_capture_consume_block`
//! walkers that track whether outer-captured non-Copy bindings are
//! consumed, and supporting helpers (`name_is_shadowed`,
//! `flatten_local_scope_snapshot`, `is_copy_type_during_check`).
//! Lives in a sibling `impl<'a> super::TypeChecker<'a>` block.

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::token::Span;
use std::collections::{HashMap, HashSet};

use super::types::Type;
use super::{CaptureWalkMode, OnceReason};

impl<'a> super::TypeChecker<'a> {
    pub(super) fn flatten_local_scope_snapshot(&self) -> HashMap<String, Type> {
        let mut out: HashMap<String, Type> = HashMap::new();
        for scope in &self.local_scope.scopes {
            for (k, v) in scope {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// Lightweight Copy classification using the in-progress type env.
    /// Mirrors `ownership::is_copy_type` but reads from `self.env.structs`
    /// / `self.env.enums` / `self.env.distinct_types` directly — the
    /// typechecker is mid-build, so the canonical `TypeCheckResult` does
    /// not yet exist. Used by the once-callability walker to decide
    /// whether a captured outer binding's type is Copy (no consume
    /// possible) or non-Copy (consume promotes the closure to OnceFn).
    fn is_copy_type_during_check(&self, ty: &Type) -> bool {
        if matches!(
            ty,
            Type::Int(_)
                | Type::UInt(_)
                | Type::Float(_)
                | Type::Bool
                | Type::Char
                | Type::Unit
                | Type::Never
                | Type::Error
        ) {
            return true;
        }
        match ty {
            Type::Tuple(types) => types.iter().all(|t| self.is_copy_type_during_check(t)),
            Type::Array { element, .. } => self.is_copy_type_during_check(element),
            Type::Slice { mutable, .. } => !mutable,
            Type::Named { name, args } => {
                if matches!(name.as_str(), "Option" | "Result") {
                    return args.iter().all(|a| self.is_copy_type_during_check(a));
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

    /// Decide the closure expression's type based on capture-mode prefix
    /// and a body walk for capture-consumes. Round 12.44 (Step 2) wires
    /// this in BOTH the synth path (`infer_expr`'s `Closure` arm) and the
    /// expected-type pushdown (`check_expr`'s `Closure` arm) so the type
    /// the typechecker assigns to a closure expression reflects whether
    /// it consumes a captured outer non-Copy binding.
    ///
    /// `Some(CaptureMode::Ref)` / `Some(CaptureMode::MutRef)` force
    /// `Type::Function` regardless of body — the explicit prefix is
    /// the user's promise that captures are borrowed, never moved
    /// (matches round 12.6's repeatable-closure rule). `None` /
    /// `Some(CaptureMode::Own)` (capture-by-ownership) walk the body.
    ///
    /// Round 12.45 (Step 3): when the walk produces a reason, the reason
    /// is recorded in `closure_once_reasons` keyed by the closure-expr
    /// span so the slot-rejection diagnostic can name the consumed binding.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn closure_type_with_capture_inference(
        &mut self,
        closure_span: &Span,
        capture_mode: Option<CaptureMode>,
        closure_param_names: &[String],
        body: &Expr,
        outer_bindings: &HashMap<String, Type>,
        param_types: Vec<Type>,
        body_ty: Type,
    ) -> Type {
        let return_type = Box::new(body_ty);
        let force_repeatable = matches!(
            capture_mode,
            Some(CaptureMode::Ref) | Some(CaptureMode::MutRef)
        );
        let reason = if force_repeatable {
            None
        } else {
            self.closure_consumes_captured_non_copy(body, closure_param_names, outer_bindings)
        };
        match reason {
            Some(r) => {
                self.closure_once_reasons
                    .insert(SpanKey::from_span(closure_span), r);
                Type::OnceFunction {
                    params: param_types,
                    return_type,
                }
            }
            None => Type::Function {
                params: param_types,
                return_type,
            },
        }
    }

    /// Returns `true` iff `body` consumes at least one captured outer
    /// non-Copy binding — the criterion that flips a closure's type from
    /// `Function` to `OnceFunction`. Mirrors the legacy ownership-side
    /// detection (`use_classifier::once_callable_closures`, populated
    /// when a `let p = closure_expr;` body walk produces a
    /// `ConsumeOrigin::ClosureCapture`-tagged consume) directly inside
    /// the typechecker, so the closure expression's inferred type stays
    /// self-consistent without a cross-phase rewrite of `expr_types`.
    ///
    /// `outer_bindings` is the `flatten_local_scope_snapshot` taken just
    /// BEFORE the closure pushes its own param scope. Closure params
    /// themselves are pushed onto the shadow stack (`closure_param_names`)
    /// so a body identifier matching a param name isn't mistaken for a
    /// capture. Body-local `let`/`for`/`match`/`if let` bindings push
    /// their own shadow scopes during the walk.
    ///
    /// The walker tracks Reading vs Consuming mode mirroring
    /// `use_classifier::walk_expr`. Owned-arg slots in `Call` (decided
    /// by the callee's signature) and the owned positions in
    /// `MethodCall` / `StructLiteral` / `Return(Some)` / `Question` /
    /// `Break(Some)` flip into Consuming. An identifier-leaf in
    /// Consuming mode whose name resolves to an outer non-Copy binding
    /// flags the closure as once-callable. The `MethodCall` receiver is
    /// walked in Reading mode (a conservative simplification — the
    /// typechecker doesn't currently track per-method `SelfParam` modes;
    /// the classifier on the ownership side does, and Step 3 closes any
    /// remaining slot-rejection gap).
    pub(super) fn closure_consumes_captured_non_copy(
        &self,
        body: &Expr,
        closure_param_names: &[String],
        outer_bindings: &HashMap<String, Type>,
    ) -> Option<OnceReason> {
        let mut shadow_stack: Vec<HashSet<String>> = Vec::new();
        let mut params_set: HashSet<String> = HashSet::new();
        for n in closure_param_names {
            params_set.insert(n.clone());
        }
        shadow_stack.push(params_set);
        let mut reason: Option<OnceReason> = None;
        self.walk_capture_consume(
            body,
            CaptureWalkMode::Reading,
            outer_bindings,
            &mut shadow_stack,
            &mut reason,
        );
        reason
    }

    fn name_is_shadowed(name: &str, shadow_stack: &[HashSet<String>]) -> bool {
        shadow_stack.iter().any(|s| s.contains(name))
    }

    fn walk_capture_consume_block(
        &self,
        block: &Block,
        terminal_mode: CaptureWalkMode,
        outer: &HashMap<String, Type>,
        shadows: &mut Vec<HashSet<String>>,
        reason: &mut Option<OnceReason>,
    ) {
        if reason.is_some() {
            return;
        }
        shadows.push(HashSet::new());
        for stmt in &block.stmts {
            if reason.is_some() {
                break;
            }
            match &stmt.kind {
                StmtKind::Let { pattern, value, .. } => {
                    self.walk_capture_consume(
                        value,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                    let names = pattern.binding_names();
                    if let Some(top) = shadows.last_mut() {
                        for n in names {
                            top.insert(n);
                        }
                    }
                }
                StmtKind::LetUninit { name, .. } => {
                    if let Some(top) = shadows.last_mut() {
                        top.insert(name.clone());
                    }
                }
                StmtKind::LetElse {
                    pattern,
                    value,
                    else_block,
                    ..
                } => {
                    self.walk_capture_consume(
                        value,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                    self.walk_capture_consume_block(
                        else_block,
                        CaptureWalkMode::Reading,
                        outer,
                        shadows,
                        reason,
                    );
                    let names = pattern.binding_names();
                    if let Some(top) = shadows.last_mut() {
                        for n in names {
                            top.insert(n);
                        }
                    }
                }
                StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                    self.walk_capture_consume_block(
                        body,
                        CaptureWalkMode::Reading,
                        outer,
                        shadows,
                        reason,
                    );
                }
                StmtKind::Assign { target, value } => {
                    self.walk_capture_consume(
                        value,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                    self.walk_capture_consume(
                        target,
                        CaptureWalkMode::Reading,
                        outer,
                        shadows,
                        reason,
                    );
                }
                StmtKind::CompoundAssign { target, value, .. } => {
                    self.walk_capture_consume(
                        value,
                        CaptureWalkMode::Reading,
                        outer,
                        shadows,
                        reason,
                    );
                    self.walk_capture_consume(
                        target,
                        CaptureWalkMode::Reading,
                        outer,
                        shadows,
                        reason,
                    );
                }
                StmtKind::Expr(e) => {
                    self.walk_capture_consume(e, CaptureWalkMode::Reading, outer, shadows, reason);
                }
            }
        }
        if reason.is_none() {
            if let Some(tail) = &block.final_expr {
                self.walk_capture_consume(tail, terminal_mode, outer, shadows, reason);
            }
        }
        shadows.pop();
    }

    fn walk_capture_consume(
        &self,
        expr: &Expr,
        mode: CaptureWalkMode,
        outer: &HashMap<String, Type>,
        shadows: &mut Vec<HashSet<String>>,
        reason: &mut Option<OnceReason>,
    ) {
        if reason.is_some() {
            return;
        }
        match &expr.kind {
            ExprKind::Identifier(name) => {
                if mode == CaptureWalkMode::Consuming && !Self::name_is_shadowed(name, shadows) {
                    if let Some(ty) = outer.get(name) {
                        if !self.is_copy_type_during_check(ty) {
                            *reason = Some(OnceReason {
                                consumed_binding: name.clone(),
                                consumed_span: expr.span.clone(),
                            });
                        }
                    }
                }
            }
            ExprKind::SelfValue => {
                if mode == CaptureWalkMode::Consuming && !Self::name_is_shadowed("self", shadows) {
                    if let Some(ty) = outer.get("self") {
                        if !self.is_copy_type_during_check(ty) {
                            *reason = Some(OnceReason {
                                consumed_binding: "self".to_string(),
                                consumed_span: expr.span.clone(),
                            });
                        }
                    }
                }
            }

            ExprKind::Integer(..)
            | ExprKind::Float(..)
            | ExprKind::Bool(..)
            | ExprKind::CharLit(..)
            | ExprKind::StringLit(..)
            | ExprKind::MultiStringLit(..)
            | ExprKind::InterpolatedStringLit(..)
            | ExprKind::Path { .. }
            | ExprKind::SelfType
            | ExprKind::PipePlaceholder
            | ExprKind::OffsetOf { .. }
            | ExprKind::Error => {}

            ExprKind::Binary { left, right, .. }
            | ExprKind::Pipe { left, right }
            | ExprKind::NilCoalesce { left, right } => {
                self.walk_capture_consume(left, CaptureWalkMode::Reading, outer, shadows, reason);
                self.walk_capture_consume(right, CaptureWalkMode::Reading, outer, shadows, reason);
            }
            ExprKind::Unary { operand, .. } => {
                self.walk_capture_consume(
                    operand,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
            }

            ExprKind::Call { callee, args } => {
                self.walk_capture_consume(callee, CaptureWalkMode::Reading, outer, shadows, reason);
                let borrow_modes = self.callee_borrow_positions(callee);
                for (i, arg) in args.iter().enumerate() {
                    let is_borrow = arg.mut_marker
                        || borrow_modes
                            .as_ref()
                            .and_then(|m| m.get(i))
                            .copied()
                            .unwrap_or(false);
                    let arg_mode = if is_borrow {
                        CaptureWalkMode::Reading
                    } else {
                        CaptureWalkMode::Consuming
                    };
                    self.walk_capture_consume(&arg.value, arg_mode, outer, shadows, reason);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                self.walk_capture_consume(object, CaptureWalkMode::Reading, outer, shadows, reason);
                for arg in args {
                    let arg_mode = if arg.mut_marker {
                        CaptureWalkMode::Reading
                    } else {
                        CaptureWalkMode::Consuming
                    };
                    self.walk_capture_consume(&arg.value, arg_mode, outer, shadows, reason);
                }
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.walk_capture_consume(object, CaptureWalkMode::Reading, outer, shadows, reason);
            }
            ExprKind::Index { object, index } => {
                self.walk_capture_consume(object, CaptureWalkMode::Reading, outer, shadows, reason);
                self.walk_capture_consume(index, CaptureWalkMode::Reading, outer, shadows, reason);
            }

            ExprKind::Block(block) => {
                self.walk_capture_consume_block(block, mode, outer, shadows, reason);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.walk_capture_consume(
                    condition,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
                self.walk_capture_consume_block(then_block, mode, outer, shadows, reason);
                if let Some(eb) = else_branch {
                    self.walk_capture_consume(eb, mode, outer, shadows, reason);
                }
            }
            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                self.walk_capture_consume(value, CaptureWalkMode::Reading, outer, shadows, reason);
                let mut arm_scope: HashSet<String> = HashSet::new();
                for n in pattern.binding_names() {
                    arm_scope.insert(n);
                }
                shadows.push(arm_scope);
                self.walk_capture_consume_block(then_block, mode, outer, shadows, reason);
                shadows.pop();
                if let Some(eb) = else_branch {
                    self.walk_capture_consume(eb, mode, outer, shadows, reason);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_capture_consume(
                    scrutinee,
                    CaptureWalkMode::Consuming,
                    outer,
                    shadows,
                    reason,
                );
                for arm in arms {
                    let mut arm_scope: HashSet<String> = HashSet::new();
                    for n in arm.pattern.binding_names() {
                        arm_scope.insert(n);
                    }
                    shadows.push(arm_scope);
                    if let Some(g) = &arm.guard {
                        self.walk_capture_consume(
                            g,
                            CaptureWalkMode::Reading,
                            outer,
                            shadows,
                            reason,
                        );
                    }
                    self.walk_capture_consume(&arm.body, mode, outer, shadows, reason);
                    shadows.pop();
                }
            }

            ExprKind::While {
                condition, body, ..
            } => {
                self.walk_capture_consume(
                    condition,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
                self.walk_capture_consume_block(
                    body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
            }
            ExprKind::WhileLet {
                pattern,
                value,
                body,
                ..
            } => {
                self.walk_capture_consume(value, CaptureWalkMode::Reading, outer, shadows, reason);
                let mut arm_scope: HashSet<String> = HashSet::new();
                for n in pattern.binding_names() {
                    arm_scope.insert(n);
                }
                shadows.push(arm_scope);
                self.walk_capture_consume_block(
                    body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
                shadows.pop();
            }
            ExprKind::For {
                pattern,
                iterable,
                body,
                ..
            } => {
                self.walk_capture_consume(
                    iterable,
                    CaptureWalkMode::Consuming,
                    outer,
                    shadows,
                    reason,
                );
                let mut arm_scope: HashSet<String> = HashSet::new();
                for n in pattern.binding_names() {
                    arm_scope.insert(n);
                }
                shadows.push(arm_scope);
                self.walk_capture_consume_block(
                    body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
                shadows.pop();
            }
            ExprKind::Loop { body, .. } => {
                self.walk_capture_consume_block(
                    body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
            }

            ExprKind::LabeledBlock { body, .. } => {
                self.walk_capture_consume_block(
                    body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
            }

            ExprKind::Break { value: Some(v), .. } | ExprKind::Return(Some(v)) => {
                self.walk_capture_consume(v, CaptureWalkMode::Consuming, outer, shadows, reason);
            }
            ExprKind::Break { value: None, .. }
            | ExprKind::Continue { .. }
            | ExprKind::Return(None) => {}

            ExprKind::Question(inner) => {
                self.walk_capture_consume(
                    inner,
                    CaptureWalkMode::Consuming,
                    outer,
                    shadows,
                    reason,
                );
            }
            ExprKind::OptionalChain { object, args, .. } => {
                self.walk_capture_consume(object, CaptureWalkMode::Reading, outer, shadows, reason);
                if let Some(arg_list) = args {
                    for arg in arg_list {
                        let arg_mode = if arg.mut_marker {
                            CaptureWalkMode::Reading
                        } else {
                            CaptureWalkMode::Consuming
                        };
                        self.walk_capture_consume(&arg.value, arg_mode, outer, shadows, reason);
                    }
                }
            }

            ExprKind::Closure {
                params: nested_params,
                body: nested_body,
                ..
            } => {
                let mut nested_scope: HashSet<String> = HashSet::new();
                for p in nested_params {
                    for n in p.pattern.binding_names() {
                        nested_scope.insert(n);
                    }
                }
                shadows.push(nested_scope);
                self.walk_capture_consume(
                    nested_body,
                    CaptureWalkMode::Reading,
                    outer,
                    shadows,
                    reason,
                );
                shadows.pop();
            }

            ExprKind::Cast { expr: inner, .. } => {
                self.walk_capture_consume(inner, mode, outer, shadows, reason);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.walk_capture_consume(s, CaptureWalkMode::Reading, outer, shadows, reason);
                }
                if let Some(e) = end {
                    self.walk_capture_consume(e, CaptureWalkMode::Reading, outer, shadows, reason);
                }
            }

            ExprKind::Tuple(es) | ExprKind::ArrayLiteral(es) => {
                for e in es {
                    self.walk_capture_consume(e, mode, outer, shadows, reason);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.walk_capture_consume(e, mode, outer, shadows, reason);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.walk_capture_consume(value, mode, outer, shadows, reason);
                self.walk_capture_consume(count, CaptureWalkMode::Reading, outer, shadows, reason);
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    self.walk_capture_consume(k, mode, outer, shadows, reason);
                    self.walk_capture_consume(v, mode, outer, shadows, reason);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    self.walk_capture_consume(
                        &f.value,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                }
                if let Some(s) = spread {
                    self.walk_capture_consume(
                        s,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                }
            }

            ExprKind::Par(body)
            | ExprKind::Seq(body)
            | ExprKind::Unsafe(body)
            | ExprKind::Try(body) => {
                self.walk_capture_consume_block(body, mode, outer, shadows, reason);
            }
            ExprKind::Lock { body, .. } => {
                self.walk_capture_consume_block(body, mode, outer, shadows, reason);
            }
            ExprKind::Providers { bindings, body } => {
                for binding in bindings {
                    self.walk_capture_consume(
                        &binding.value,
                        CaptureWalkMode::Consuming,
                        outer,
                        shadows,
                        reason,
                    );
                }
                self.walk_capture_consume_block(body, mode, outer, shadows, reason);
            }
        }
    }
}
