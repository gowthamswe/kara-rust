//! Expression-level ownership checking: consume / read passes.
//!
//! Houses the two recursive expression walkers and their helpers:
//!
//! - `check_expr_consuming` — visits an expression in
//!   *consuming* context (the value flows into an owned slot —
//!   let binding, function call by value, return). Marks the
//!   underlying binding's `ValueState` as `Moved` on the
//!   consume-leaf identifier path.
//! - `check_expr_reading` — visits an expression in *reading*
//!   context (the value is only inspected; ownership stays where
//!   it is). The big match dispatch covers every `ExprKind` —
//!   field access, method call, control-flow expressions, closures,
//!   borrows, etc.
//! - `consume_named_binding` — moves a single named binding into
//!   `Moved` state, applying Copy-typed semantics + RC trigger
//!   recording.
//! - `handle_uninit_read`, `handle_moved_use` — diagnostic-emission
//!   helpers for the two consume-after-X error shapes.
//! - `check_call_callee` — call-position dispatch shared between
//!   the consuming + reading walks.
//! - Pattern probe helpers: `is_unit_variant_name`,
//!   `pattern_binds_anything`, `is_borrow_typed_scrutinee`,
//!   `root_identifier`.
//!
//! Lives in a sibling `impl<'a> super::OwnershipChecker<'a>` block.

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::resolver::SpanKey;
use crate::token::Span;
use crate::typechecker::Type;

use super::{
    merge_branch_into, merge_states, restore_uninit_after_loop, snapshot_uninit, OwnershipError,
    OwnershipErrorKind, OwnershipMode, ParamUsage, ValueState,
};

impl<'a> super::OwnershipChecker<'a> {
    /// Mark a named binding as consumed at `use_span`. Used by the
    /// MethodCall receiver-consume path (step 1) so the consume does
    /// not depend on `expr_types[span]`, which is unreliable at the
    /// root of a chained access (`c.inner.unwrap()` aliases all spans
    /// to `c`'s span and the typechecker's last-write-wins puts the
    /// method's return type there). Reads the binding's actual type
    /// from `param_types` (params) or `binding_types` (locals); both
    /// are keyed by name and so are immune to the span aliasing.
    pub(crate) fn consume_named_binding(
        &mut self,
        name: &str,
        use_span: &Span,
        states: &mut HashMap<String, ValueState>,
        param_types: &HashMap<String, Type>,
        param_usage: &mut HashMap<String, ParamUsage>,
    ) {
        if self.handle_uninit_read(name, use_span, states) {
            return;
        }
        if self.handle_moved_use(name, use_span, states) {
            return;
        }
        let is_copy = if let Some(t) = param_types.get(name) {
            self.is_copy_type(t)
        } else if let Some(t) = self.binding_types.get(name) {
            self.is_copy_type(t)
        } else {
            // Unknown — conservative default: assume non-Copy so the
            // consume actually fires. False-positive Copy classification
            // here (a "consume" of a Copy local that the table missed)
            // would silently miss real moves; default to non-Copy is the
            // safer error mode.
            false
        };
        if !is_copy {
            states.insert(
                name.to_string(),
                ValueState::Moved {
                    at: use_span.clone(),
                },
            );
            if let Some(usage) = param_usage.get_mut(name) {
                *usage = ParamUsage::Consumed;
            }
        } else if let Some(usage) = param_usage.get_mut(name) {
            if *usage == ParamUsage::Unused {
                *usage = ParamUsage::Read;
            }
        }
    }

    /// Whether `name` is a unit-variant of any known enum. The parser cannot
    /// distinguish `None` (variant ref) from `let None = ...` (fresh binding),
    /// so both reach ownership as `PatternKind::Binding(name)`. The typechecker
    /// disambiguates per-arm against the scrutinee's type; for pattern-binding
    /// classification in match scrutinee analysis we use a coarser global
    /// check — matching any unit variant by name. Over-permissive only in the
    /// pathological case of a real binding shadowing a known variant name,
    /// which is non-idiomatic.
    fn is_unit_variant_name(&self, name: &str) -> bool {
        self.typecheck_result
            .enum_info
            .values()
            .any(|info| info.variants.iter().any(|(vn, _)| vn == name))
    }

    /// Whether the pattern binds at least one fresh value-name. Wildcards,
    /// literal patterns, range patterns, and pure unit-variant references
    /// don't bind. Used by step 4 of the consume predicate (match scrutinee
    /// classification): if any arm pattern binds anything, the scrutinee
    /// is consumed (subject to Copy).
    fn pattern_binds_anything(&self, pattern: &Pattern) -> bool {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {
                false
            }
            PatternKind::Binding(name) => !self.is_unit_variant_name(name),
            PatternKind::AtBinding { .. } => true,
            PatternKind::Tuple(patterns) | PatternKind::TupleVariant { patterns, .. } => {
                patterns.iter().any(|p| self.pattern_binds_anything(p))
            }
            PatternKind::Struct { fields, .. } => fields.iter().any(|f| match &f.pattern {
                Some(sub) => self.pattern_binds_anything(sub),
                // Shorthand `Container { name }` binds `name`.
                None => true,
            }),
            PatternKind::Or(alts) => alts.iter().any(|p| self.pattern_binds_anything(p)),
            PatternKind::Slice {
                prefix,
                rest,
                suffix,
            } => {
                matches!(rest, Some(RestPattern::Bound(_)))
                    || prefix
                        .iter()
                        .chain(suffix.iter())
                        .any(|p| self.pattern_binds_anything(p))
            }
        }
    }

    /// Whether `scrutinee`'s type is `ref T` / `mut ref T` — match
    /// ergonomics treat such scrutinees as always-read regardless of
    /// what the arms bind, since the typechecker has wrapped each arm
    /// binding's type in the corresponding borrow form (design.md
    /// § Match Arm Binding Modes). Looks up the scrutinee's type via
    /// the per-span `expr_types` table the typechecker populates;
    /// falls back to `param_types` when the scrutinee is a bare
    /// parameter identifier (the span table can lag in synthesised
    /// expressions, but param_types is authoritative for parameters).
    fn is_borrow_typed_scrutinee(
        &self,
        scrutinee: &Expr,
        param_types: &HashMap<String, Type>,
    ) -> bool {
        let ty = self
            .typecheck_result
            .expr_types
            .get(&SpanKey::from_span(&scrutinee.span))
            .cloned()
            .or_else(|| match &scrutinee.kind {
                ExprKind::Identifier(name) => param_types.get(name).cloned(),
                _ => None,
            });
        matches!(ty, Some(Type::Ref(_)) | Some(Type::MutRef(_)))
    }

    pub(crate) fn root_identifier(expr: &Expr) -> Option<String> {
        match &expr.kind {
            ExprKind::Identifier(name) => Some(name.clone()),
            ExprKind::FieldAccess { object, .. }
            | ExprKind::TupleIndex { object, .. }
            | ExprKind::Index { object, .. } => Self::root_identifier(object),
            // `*r` — the root being mutated is the reference variable `r` itself.
            ExprKind::Unary {
                op: crate::ast::UnaryOp::Deref,
                operand,
            } => Self::root_identifier(operand),
            _ => None,
        }
    }

    /// Check an expression in a "consuming" context (e.g., passed to a function,
    /// returned, assigned to a variable). Non-Copy values are moved.
    pub(crate) fn check_expr_consuming(
        &mut self,
        expr: &Expr,
        states: &mut HashMap<String, ValueState>,
        param_types: &HashMap<String, Type>,
        param_usage: &mut HashMap<String, ParamUsage>,
    ) {
        match &expr.kind {
            ExprKind::Identifier(name) => {
                if self.handle_uninit_read(name, &expr.span, states) {
                    return;
                }
                let is_copy = if let Some(t) = param_types.get(name) {
                    self.is_copy_type(t)
                } else {
                    // Local binding not in param_types — consult typecheck result
                    self.typecheck_result
                        .expr_types
                        .get(&SpanKey::from_span(&expr.span))
                        .map(|t| self.is_copy_type(t))
                        .unwrap_or(false)
                };

                if self.handle_moved_use(name, &expr.span, states) {
                    return;
                }

                if !is_copy {
                    // Slice 2 — before the move proceeds, check whether
                    // any slice borrow into this binding is live. If so,
                    // emit shape C (move-of-borrowed). The move itself
                    // still proceeds so downstream state stays consistent.
                    self.check_move_of_borrowed(name, &expr.span);
                    // Non-copy value is consumed → mark as moved.
                    states.insert(
                        name.clone(),
                        ValueState::Moved {
                            at: expr.span.clone(),
                        },
                    );
                    if let Some(usage) = param_usage.get_mut(name) {
                        *usage = ParamUsage::Consumed;
                    }
                } else if let Some(usage) = param_usage.get_mut(name) {
                    if *usage == ParamUsage::Unused {
                        *usage = ParamUsage::Read;
                    }
                }
            }
            ExprKind::Call { callee, args } => {
                // Slice 2 — call-statement-scoped slice borrows; see
                // `check_expr_reading`'s Call arm for rationale.
                let snapshot = self.snapshot_active_borrow_lens();
                self.check_call_callee(callee, states, param_types, param_usage);
                for (i, arg) in args.iter().enumerate() {
                    // Step 2 (consume-predicate): the arg's classification
                    // is driven by the callee's declared parameter mode.
                    // `ref T` / `mut ref T` / `mut Slice[T]` slots are
                    // borrow positions — read, not consume — regardless of
                    // whether the call-site `mut <expr>` marker is present
                    // (the marker is required by Part 1½ for `MutRef` slots
                    // but is itself a borrow signal, not a move signal).
                    // Bare-T slots consume per the existing rule. Unknown
                    // callees (function-typed values, etc.) fall back to
                    // the prior consume-on-no-marker default.
                    let is_borrow = arg.mut_marker || self.arg_is_borrow_position(callee, i);
                    if is_borrow {
                        self.check_expr_reading(&arg.value, states, param_types, param_usage);
                    } else {
                        self.check_expr_consuming(&arg.value, states, param_types, param_usage);
                    }
                    // Slice 1: site (iii) call-arg coercion — see
                    // `check_expr_reading`'s Call arm for rationale.
                    if let Some(formal_mutable) = self.arg_formal_slice_kind(callee, i) {
                        self.record_slice_creation(&arg.value.span, &arg.value, formal_mutable);
                    } else if let Some(borrow_kind) = self.arg_formal_ref_borrow_kind(callee, i) {
                        // Slice 2 follow-up — non-slice ref formal cross-
                        // borrow push; see `check_expr_reading`'s Call arm
                        // for rationale.
                        if let Some(place) = self.place_expr_root(&arg.value) {
                            self.push_active_borrow(borrow_kind, place, arg.value.span.clone());
                        }
                    }
                }
                self.restore_active_borrows_to_snapshot(&snapshot);
            }
            ExprKind::Return(Some(inner)) => {
                self.check_expr_consuming(inner, states, param_types, param_usage);
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for field in fields {
                    self.check_expr_consuming(&field.value, states, param_types, param_usage);
                }
                if let Some(ref s) = spread {
                    self.check_expr_consuming(s, states, param_types, param_usage);
                }
            }
            // Partial move through field projection (design.md § Consume
            // Predicate step 3). Consume of `v.field` / `v.0` / `v.a.b` is a
            // consume of the root binding `v`. Walk the projection chain by
            // recursing on `object` until the base `Identifier` fires the
            // standard consume logic. Copy fields short-circuit through the
            // reading path so the root is not falsely moved.
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                let field_is_copy = self
                    .typecheck_result
                    .expr_types
                    .get(&SpanKey::from_span(&expr.span))
                    .map(|t| self.is_copy_type(t))
                    .unwrap_or(false);
                if field_is_copy {
                    self.check_expr_reading(expr, states, param_types, param_usage);
                } else {
                    self.check_expr_consuming(object, states, param_types, param_usage);
                }
            }
            // For compound expressions, delegate to reading (they don't consume at top level)
            _ => self.check_expr_reading(expr, states, param_types, param_usage),
        }
    }

    /// If `name`'s state is Uninit at this read, push a UseOfUninitialized
    /// error and return `true` (caller should bail out — no point trying to
    /// classify the read further). Definite-assignment failure.
    ///
    /// When the binding's declared type is `Array[T, N]` the message and
    /// suggestion are array-specific: per design.md §1097 the v1 DA analyser
    /// tracks whole-value assignment only — per-slot fills like `arr[0] = ...`
    /// do not satisfy DA — so the suggestion points users at the canonical
    /// fully-initialized constructors (`Array[v; N]` literal, `Array.from_fn`).
    pub(crate) fn handle_uninit_read(
        &mut self,
        name: &str,
        use_span: &Span,
        states: &HashMap<String, ValueState>,
    ) -> bool {
        let Some(ValueState::Uninit { let_span, .. }) = states.get(name) else {
            return false;
        };
        let is_array = matches!(self.binding_types.get(name), Some(Type::Array { .. }));
        let (message, suggestion) = if is_array {
            (
                format!(
                    "read of uninitialized array `{}` (declared at line {}:{})",
                    name, let_span.line, let_span.column
                ),
                format!(
                    "assign the whole value first — try `{} = Array[v; N]` or `{} = Array.from_fn(N, |i| ...)`",
                    name, name
                ),
            )
        } else {
            (
                format!(
                    "use of uninitialized binding `{}` (declared at line {}:{})",
                    name, let_span.line, let_span.column
                ),
                format!("assign to `{}` before reading it", name),
            )
        };
        self.errors.push(OwnershipError {
            message,
            span: use_span.clone(),
            kind: OwnershipErrorKind::UseOfUninitialized,
            suggestion: Some(suggestion),
            replacement: None,
            consume_span: None,
        });
        true
    }

    /// Examine `states[name]`. Returns `true` when the binding is in
    /// `Moved` state (so the caller should bail out of further
    /// processing of this expression). All UAM and RC fallback
    /// diagnostic emission is driven by the predicate pre-pass in
    /// `populate_predicate_outputs` — round 12.17 collapsed the RC
    /// kinds; round 12.21 collapsed the `Direct` UAM kind; round
    /// 12.42 collapsed `MoveKind` into the binary
    /// `ValueState::Moved`. The legacy state machine's only remaining
    /// jobs are this short-circuit (so descendant expressions inside
    /// an already-moved identifier don't emit cascading reads) and
    /// closure-capture mode classification in `check_expr_consuming`'s
    /// `Closure` arm.
    #[allow(clippy::unused_self)]
    pub(crate) fn handle_moved_use(
        &mut self,
        name: &str,
        _use_span: &Span,
        states: &HashMap<String, ValueState>,
    ) -> bool {
        matches!(states.get(name), Some(ValueState::Moved { .. }))
    }

    /// Handle the callee position of a `Call` expression.
    ///
    /// For once-callable closure bindings (those whose body consumed
    /// at least one captured owned non-Copy value), calling the
    /// closure is itself a consuming operation. Per round 12.38, the
    /// once-callable state-machine bookkeeping moved to the predicate
    /// pipeline: `UseClassifier` (round 12.20) tags every call site
    /// of a once-callable binding as `UseKind::Consume`, the predicate
    /// pairs the first/second call as a UAM witness (or as an RC
    /// witness when the calls are dominance-incomparable), and
    /// `populate_predicate_outputs` emits the diagnostic. The legacy
    /// state-machine still walks the body for parent-state propagation
    /// and the K2 closure-capture retag, but it no longer mutates
    /// `states` for the closure binding itself on call — the predicate
    /// owns that. The callee is walked through the regular reading
    /// path so any nested non-callee subexpressions (turbofish,
    /// receiver projections) still record their use sites for inference.
    pub(crate) fn check_call_callee(
        &mut self,
        callee: &Expr,
        states: &mut HashMap<String, ValueState>,
        param_types: &HashMap<String, Type>,
        param_usage: &mut HashMap<String, ParamUsage>,
    ) {
        // Normal callee: just read it (functions are not consumed by being called).
        self.check_expr_reading(callee, states, param_types, param_usage);
    }

    /// Check an expression in a "reading" context. Values are not moved.
    pub(crate) fn check_expr_reading(
        &mut self,
        expr: &Expr,
        states: &mut HashMap<String, ValueState>,
        param_types: &HashMap<String, Type>,
        param_usage: &mut HashMap<String, ParamUsage>,
    ) {
        match &expr.kind {
            ExprKind::Identifier(name) => {
                if self.handle_uninit_read(name, &expr.span, states) {
                    return;
                }
                if self.handle_moved_use(name, &expr.span, states) {
                    return;
                }
                // Track as read for param mode inference
                if let Some(usage) = param_usage.get_mut(name) {
                    if *usage == ParamUsage::Unused {
                        *usage = ParamUsage::Read;
                    }
                }
            }
            ExprKind::SelfValue => {
                self.handle_moved_use("self", &expr.span, states);
            }
            ExprKind::Binary { left, right, .. } | ExprKind::Pipe { left, right } => {
                self.check_expr_reading(left, states, param_types, param_usage);
                self.check_expr_reading(right, states, param_types, param_usage);
            }
            ExprKind::Unary { operand, .. } => {
                self.check_expr_reading(operand, states, param_types, param_usage);
            }
            ExprKind::Call { callee, args } => {
                // Slice 2 — snapshot before arg walking so call-arg-
                // coerced slice borrows AND ref-formal transient borrows
                // are call-statement-scoped (drop at call return).
                // Conflicts mid-call still fire because the push side-
                // effects the diagnostic.
                let snapshot = self.snapshot_active_borrow_lens();
                self.check_call_callee(callee, states, param_types, param_usage);
                for (i, arg) in args.iter().enumerate() {
                    // Step 2 (consume-predicate): see the analogous arm in
                    // `check_expr_consuming` for the rationale.
                    let is_borrow = arg.mut_marker || self.arg_is_borrow_position(callee, i);
                    if is_borrow {
                        self.check_expr_reading(&arg.value, states, param_types, param_usage);
                    } else {
                        self.check_expr_consuming(&arg.value, states, param_types, param_usage);
                    }
                    // Slice 1: site (iii) call-arg coercion. When the
                    // formal slot is `Slice[T]` / `mut Slice[T]` and the
                    // arg flows in as a `Vec` / `Array` / `Slice`, the
                    // typechecker inserts an implicit slice view. Record
                    // the source attribution against the arg's span.
                    if let Some(formal_mutable) = self.arg_formal_slice_kind(callee, i) {
                        self.record_slice_creation(&arg.value.span, &arg.value, formal_mutable);
                    } else if let Some(borrow_kind) = self.arg_formal_ref_borrow_kind(callee, i) {
                        // Slice 2 follow-up — non-slice `ref T` / `mut ref T`
                        // formal at this position. Push a transient borrow
                        // on the arg's root so cross-borrow detection fires
                        // against any live slice on the same source. The
                        // borrow drops at the call's snapshot-restore.
                        if let Some(place) = self.place_expr_root(&arg.value) {
                            self.push_active_borrow(borrow_kind, place, arg.value.span.clone());
                        }
                    }
                }
                self.restore_active_borrows_to_snapshot(&snapshot);
            }
            ExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } => {
                // Step 1 (consume-predicate): receiver mode comes from the
                // resolved method's `self_param`, not from a name heuristic.
                // `bare self` → consume the receiver; `ref self` /
                // `mut ref self` → read. Falls back to read when the method
                // can't be resolved (e.g. typecheck error upstream).
                //
                // For a projection receiver like `c.inner.unwrap()`, walking
                // to the root identifier and consuming *that* is necessary
                // because the parser aliases `MethodCall.span == receiver
                // .span`, so the round-11.2 `expr_types`-driven Copy check
                // on the FieldAccess receiver would see the method's return
                // type instead of the field's type. Going via the root
                // identifier sidesteps the alias entirely.
                if self.method_call_consumes_receiver(expr) {
                    if let Some(root_name) = Self::root_identifier(object) {
                        self.consume_named_binding(
                            &root_name,
                            &object.span,
                            states,
                            param_types,
                            param_usage,
                        );
                    } else {
                        self.check_expr_consuming(object, states, param_types, param_usage);
                    }
                } else {
                    self.check_expr_reading(object, states, param_types, param_usage);
                }
                // Slice 1: `.as_slice()` / `.as_slice_mut()` are slice
                // creation site (i). Record the source attribution so
                // Slice 2's conflict detector can match later uses against
                // the original storage binding. No-op when the receiver
                // is a temporary (function call result, etc.). Recorded
                // BEFORE the receiver-side push snapshot so the slice
                // borrow persists past the method call.
                if (method == "as_slice" || method == "as_slice_mut") && args.is_empty() {
                    self.record_slice_creation(&expr.span, object, method == "as_slice_mut");
                }
                // Slice 2 — receiver-side borrow push for instance methods.
                // The push fires the conflict matrix against any existing
                // borrows on the receiver's root, surfacing CrossBorrowConflict
                // when a slice into the receiver is already live (e.g.,
                // `let _s = h.v.as_slice_mut(); h.modify();`). Skipped for
                // `.as_slice` / `.as_slice_mut` (the slice creation push
                // above is the correct representation; a redundant
                // receiver-side ref push would false-positive on the
                // method's own slice result). Snapshot taken AFTER the
                // slice creation push so persistent slice borrows survive
                // the restore at end-of-call. Static methods, bare-self
                // consumes, and unresolved methods (stdlib impls etc.) all
                // return None from `method_self_borrow_kind` and skip.
                let receiver_snapshot = self.snapshot_active_borrow_lens();
                if !matches!(method.as_str(), "as_slice" | "as_slice_mut") {
                    if let Some(borrow_kind) = self.method_self_borrow_kind(expr) {
                        if let Some(place) = self.place_expr_root(object) {
                            self.push_active_borrow(borrow_kind, place, expr.span.clone());
                        }
                    }
                }
                // Trigger 3 (container store + subsequent use) was
                // formerly routed by snapshotting Live arg-rooted
                // bindings, walking the args, and retagging any that
                // flipped to `MoveKind::Direct` as `ContainerStore` so
                // a later sequential use landed in RC fallback. Round
                // 12.42 removed the retag — the predicate pipeline's
                // `use_classifier` already tags each owned (no
                // `mut`-marker) arg of a `mut ref self` method call as
                // `ConsumeOrigin::ContainerStore` (round 12.12), and
                // `populate_predicate_outputs` emits the flavor-correct
                // `RcEntry` directly. The call-arg consume walk below
                // is now the only ownership-side action.
                for arg in args {
                    if arg.mut_marker {
                        self.check_expr_reading(&arg.value, states, param_types, param_usage);
                    } else {
                        self.check_expr_consuming(&arg.value, states, param_types, param_usage);
                    }
                }
                // Slice 2 — drop the receiver-side ref borrow + any call-
                // arg-coerced slice borrows added during the args walk.
                // The `.as_slice` / `.as_slice_mut` slice creation push
                // happens BEFORE the snapshot so it's preserved.
                self.restore_active_borrows_to_snapshot(&receiver_snapshot);
            }
            ExprKind::FieldAccess { object, .. } => {
                self.check_expr_reading(object, states, param_types, param_usage);
            }
            ExprKind::TupleIndex { object, .. } => {
                self.check_expr_reading(object, states, param_types, param_usage);
            }
            ExprKind::Index { object, index } => {
                self.check_expr_reading(object, states, param_types, param_usage);
                self.check_expr_reading(index, states, param_types, param_usage);
                // Slice 1: range-indexing produces a slice view (site (ii)).
                // The typechecker types `v[a..b]` as `Slice[T]` (immutable
                // — see typechecker.rs:7244-7282); record the source
                // attribution against the index expression's span.
                if matches!(&index.kind, ExprKind::Range { .. }) {
                    self.record_slice_creation(&expr.span, object, false);
                }
            }
            ExprKind::Block(block) => {
                self.check_block(block, states, param_types, param_usage);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.check_expr_reading(condition, states, param_types, param_usage);
                // Clone states for branches — conservative: if moved in either branch,
                // consider moved after the if
                let mut then_states = states.clone();
                self.check_block(then_block, &mut then_states, param_types, param_usage);
                if let Some(ref else_expr) = else_branch {
                    let mut else_states = states.clone();
                    self.check_expr_reading(else_expr, &mut else_states, param_types, param_usage);
                    // Merge: if moved in EITHER branch, it's moved
                    merge_states(states, &then_states, &else_states);
                } else {
                    // Only then branch ran — promote any conditional move
                    // to BranchMerged so the next use lands in RC fallback
                    // rather than firing a use-after-move error.
                    merge_branch_into(states, &then_states);
                }
            }
            ExprKind::IfLet {
                pattern,
                value,
                then_block,
                else_branch,
            } => {
                self.check_expr_reading(value, states, param_types, param_usage);
                let mut then_states = states.clone();
                self.define_pattern_states(pattern, &mut then_states);
                self.check_block(then_block, &mut then_states, param_types, param_usage);
                if let Some(ref else_expr) = else_branch {
                    let mut else_states = states.clone();
                    self.check_expr_reading(else_expr, &mut else_states, param_types, param_usage);
                    merge_states(states, &then_states, &else_states);
                } else {
                    merge_branch_into(states, &then_states);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                // Step 4 (consume-predicate): classify the scrutinee as
                // consume iff *any* arm pattern binds at least one name
                // by-move. All Kāra pattern bindings are by-move, so a
                // pattern that binds anything pulls part of the scrutinee
                // out. Wildcard / literal / range / pure unit-variant
                // arms read only. `pattern_binds_anything` filters unit
                // variants like `None` (parsed as `Binding("None")`) so
                // an all-`Some(_) | None`-style match doesn't false-
                // positive consume.
                //
                // Match-ergonomics exception (design.md § Match Arm
                // Binding Modes): when the scrutinee's type is
                // `ref T` / `mut ref T`, bindings borrow rather than
                // move, so the scrutinee is always read — never
                // consumed — regardless of what the arms bind. The
                // typechecker has already wrapped each arm binding's
                // type in the corresponding borrow form (see
                // `ScrutineeMode::wrap_binding_ty` in
                // `typechecker.rs`); we mirror that decision here so
                // the scrutinee's source binding (typically a `ref T`
                // parameter) doesn't get falsely marked `Moved`.
                let scrut_is_borrow = self.is_borrow_typed_scrutinee(scrutinee, param_types);
                let any_arm_binds = arms
                    .iter()
                    .any(|arm| self.pattern_binds_anything(&arm.pattern));
                if any_arm_binds && !scrut_is_borrow {
                    self.check_expr_consuming(scrutinee, states, param_types, param_usage);
                } else {
                    self.check_expr_reading(scrutinee, states, param_types, param_usage);
                }
                let mut all_arm_states: Vec<HashMap<String, ValueState>> = Vec::new();
                for arm in arms {
                    let mut arm_states = states.clone();
                    self.define_pattern_states(&arm.pattern, &mut arm_states);
                    if let Some(guard) = &arm.guard {
                        self.check_expr_reading(guard, &mut arm_states, param_types, param_usage);
                    }
                    self.check_expr_reading(&arm.body, &mut arm_states, param_types, param_usage);
                    all_arm_states.push(arm_states);
                }
                // Merge all arm states — moved in any arm → BranchMerged.
                for arm_states in &all_arm_states {
                    merge_branch_into(states, arm_states);
                }
                // DA promotion across an exhaustive match: if every arm
                // initialized a previously-Uninit binding, the join is
                // initialized. Match exhaustiveness is enforced by the
                // typechecker, so all reachable arms run at least one path.
                let to_check: Vec<String> = states
                    .iter()
                    .filter(|(_, s)| matches!(s, ValueState::Uninit { .. }))
                    .map(|(n, _)| n.clone())
                    .collect();
                for name in to_check {
                    if all_arm_states.is_empty() {
                        break;
                    }
                    let mut merged: Option<ValueState> = None;
                    let mut all_init = true;
                    for arm_states in &all_arm_states {
                        match arm_states.get(&name) {
                            Some(v @ ValueState::Live) | Some(v @ ValueState::InitOnce { .. }) => {
                                merged = Some(match (&merged, v) {
                                    (Some(ValueState::Live), _) | (_, ValueState::Live) => {
                                        ValueState::Live
                                    }
                                    _ => v.clone(),
                                });
                            }
                            _ => {
                                all_init = false;
                                break;
                            }
                        }
                    }
                    if all_init {
                        if let Some(state) = merged {
                            states.insert(name, state);
                        }
                    }
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.check_expr_reading(condition, states, param_types, param_usage);
                let pre_uninit = snapshot_uninit(states);
                self.check_block(body, states, param_types, param_usage);
                restore_uninit_after_loop(pre_uninit, states);
            }
            ExprKind::WhileLet {
                value,
                pattern,
                body,
                ..
            } => {
                self.check_expr_reading(value, states, param_types, param_usage);
                let pre_uninit = snapshot_uninit(states);
                self.define_pattern_states(pattern, states);
                self.check_block(body, states, param_types, param_usage);
                restore_uninit_after_loop(pre_uninit, states);
            }
            ExprKind::For {
                pattern,
                iterable,
                body,
                ..
            } => {
                self.check_expr_reading(iterable, states, param_types, param_usage);
                let pre_uninit = snapshot_uninit(states);
                self.define_pattern_states(pattern, states);
                self.check_block(body, states, param_types, param_usage);
                restore_uninit_after_loop(pre_uninit, states);
            }
            ExprKind::Loop { body, .. } => {
                let pre_uninit = snapshot_uninit(states);
                self.check_block(body, states, param_types, param_usage);
                restore_uninit_after_loop(pre_uninit, states);
            }
            ExprKind::LabeledBlock { body, .. } => {
                self.check_block(body, states, param_types, param_usage);
            }
            ExprKind::Unsafe(body)
            | ExprKind::Try(body)
            | ExprKind::Seq(body)
            | ExprKind::Par(body) => {
                self.check_block(body, states, param_types, param_usage);
            }
            ExprKind::Lock { body, .. } => {
                self.check_block(body, states, param_types, param_usage);
            }
            ExprKind::Closure {
                params: closure_params,
                body,
                capture_mode,
                prefix_span,
            } => {
                // Snapshot live bindings so we can identify which captures
                // the body consumed and retag them as ClosureCapture moves.
                // This is what routes "consume inside closure body + outer
                // use" to RC trigger 2 instead of a use-after-move error.
                let pre_live: Vec<String> = states
                    .iter()
                    .filter(|(_, s)| matches!(s, ValueState::Live))
                    .map(|(n, _)| n.clone())
                    .collect();

                // Round 12.23 — Closure ownership Step 1: bind closure
                // parameters into `states` / `param_usage` for the
                // duration of the body walk so the same use-predicate
                // scan that infers fn-param modes classifies closure
                // params too. Snapshot any pre-existing entries with
                // the same name so shadowing of an outer binding is
                // reversible at the end of the walk. Build a fresh
                // `param_types` map for the body walk so the
                // copy-vs-non-copy gate at `check_expr_consuming` reads
                // the closure-local parameter type, not a shadowed
                // outer-scope type.
                let closure_param_names: Vec<String> = closure_params
                    .iter()
                    .flat_map(|cp| cp.pattern.binding_names())
                    .collect();
                let mut prev_states: Vec<(String, Option<ValueState>)> = Vec::new();
                let mut prev_usage: Vec<(String, Option<ParamUsage>)> = Vec::new();
                for name in &closure_param_names {
                    prev_states.push((name.clone(), states.remove(name)));
                    prev_usage.push((name.clone(), param_usage.remove(name)));
                    states.insert(name.clone(), ValueState::Live);
                    param_usage.insert(name.clone(), ParamUsage::Unused);
                }
                let mut closure_param_types: HashMap<String, Type> = param_types.clone();
                let closure_fn_type = self
                    .typecheck_result
                    .expr_types
                    .get(&SpanKey::from_span(&expr.span))
                    .cloned();
                let inferred_param_types: Vec<Option<Type>> = match &closure_fn_type {
                    Some(Type::Function { params, .. })
                    | Some(Type::OnceFunction { params, .. }) => {
                        params.iter().cloned().map(Some).collect()
                    }
                    _ => vec![None; closure_params.len()],
                };
                for (i, cp) in closure_params.iter().enumerate() {
                    let ty = if let Some(annot) = &cp.ty {
                        self.lower_type_for_ownership(annot)
                    } else if let Some(Some(t)) = inferred_param_types.get(i) {
                        t.clone()
                    } else {
                        Type::Error
                    };
                    for name in cp.pattern.binding_names() {
                        closure_param_types.insert(name, ty.clone());
                    }
                }

                self.check_expr_reading(body, states, &closure_param_types, param_usage);

                // Harvest closure-param mode classifications. Each
                // `param_usage` entry was zeroed before the walk, so
                // its post-walk state reflects only the closure body's
                // contribution. Map to `OwnershipMode` with the same
                // rule used for fn-param inference at `check_function`.
                let mut closure_modes: Vec<(String, OwnershipMode)> = Vec::new();
                for cp in closure_params {
                    for name in cp.pattern.binding_names() {
                        let usage = param_usage
                            .get(&name)
                            .cloned()
                            .unwrap_or(ParamUsage::Unused);
                        let mode = match usage {
                            ParamUsage::Unused | ParamUsage::Read => OwnershipMode::Ref,
                            ParamUsage::Mutated => OwnershipMode::MutRef,
                            ParamUsage::Consumed => OwnershipMode::Own,
                        };
                        closure_modes.push((name, mode));
                    }
                }
                let closure_key = SpanKey::from_span(&expr.span);
                self.closure_param_modes.insert(closure_key, closure_modes);
                // Round 12.25: record the enclosing function so
                // `karac query ownership <fn>` can filter to
                // closures created inside that function. Also stash
                // the full span so consumers can render line/column.
                self.closure_function
                    .insert(closure_key, self.current_function.clone());
                self.closure_spans.insert(closure_key, expr.span.clone());

                // Restore the outer scope: drop closure-param entries
                // that didn't pre-exist and reinstate any shadowed
                // outer bindings.
                for (name, prev) in prev_states {
                    match prev {
                        Some(s) => {
                            states.insert(name, s);
                        }
                        None => {
                            states.remove(&name);
                        }
                    }
                }
                for (name, prev) in prev_usage {
                    match prev {
                        Some(u) => {
                            param_usage.insert(name, u);
                        }
                        None => {
                            param_usage.remove(&name);
                        }
                    }
                }

                // Round 12.24 — Closure ownership Step 2: identify
                // captures. A capture is an outer-scope binding that
                // the closure body references. Names lexically
                // shadowed by the closure's own parameter list are
                // excluded — body references to those names are to
                // the closure-local, not the outer binding. Detection
                // runs after the outer-scope restore so `states[N]`
                // for non-shadowed names reflects the body walk's
                // effect (consumed → `Moved`); shadowed names'
                // outer-scope state was restored to its pre-walk
                // value, which is what we want (body did not consume
                // the outer binding, the closure-local has gone out
                // of scope). Read/mutate signals come from
                // `classify_capture_body_uses`'s AST walk; consume
                // signals come from `states[N] == Moved`. Detection
                // happens before the K2 retag loop so the legacy
                // retag behavior (Direct → ClosureCapture state
                // transition for non-K2-error captures) does not
                // confuse the consume check — the kind variant
                // doesn't matter, only that the state is `Moved`.
                let captures_usage = self.classify_capture_body_uses(body, &pre_live);
                let closure_param_set: HashSet<String> =
                    closure_param_names.iter().cloned().collect();
                let mut captures: Vec<(String, OwnershipMode)> = Vec::new();
                for name in &pre_live {
                    if closure_param_set.contains(name) {
                        continue;
                    }
                    let consumed = matches!(states.get(name), Some(ValueState::Moved { .. }));
                    let body_usage = captures_usage.get(name).copied().unwrap_or_default();
                    if !body_usage.referenced && !consumed {
                        continue;
                    }
                    let mode = if consumed {
                        OwnershipMode::Own
                    } else if body_usage.mutated {
                        OwnershipMode::MutRef
                    } else {
                        OwnershipMode::Ref
                    };
                    captures.push((name.clone(), mode));
                }
                captures.sort_by(|a, b| a.0.cmp(&b.0));
                self.closure_captures
                    .insert(SpanKey::from_span(&expr.span), captures);

                // Disjoint capture slice 1 (line 353 phase-5
                // checklist) — record the set of distinct capture
                // paths the body touches against each pre-live root.
                // Shares `pre_live` with the per-name walker above and
                // applies the closure-param shadow filter identically.
                // Slice 2 will run mode inference per path; slice 3
                // will pass this set to the borrow checker so outer-
                // scope sibling-path access is permitted.
                let path_pre_live: Vec<String> = pre_live
                    .iter()
                    .filter(|name| !closure_param_set.contains(*name))
                    .cloned()
                    .collect();
                let capture_paths = self.classify_capture_body_paths(body, &path_pre_live);
                self.closure_capture_paths
                    .insert(SpanKey::from_span(&expr.span), capture_paths);

                // K2 conflict-table row "mut ref + reads only" (Rule 2½):
                // if the closure declared `mut ref` but the body never
                // mutates a referenced capture, emit a Tier 2 perf note.
                // Done before the consume-pass below so a body that *also*
                // consumes a different capture (which fires the K2 error
                // path) still emits the unused-mut note for any read-only
                // siblings.
                if matches!(capture_mode, Some(CaptureMode::MutRef)) {
                    let usage = self.classify_capture_body_uses(body, &pre_live);
                    for name in &pre_live {
                        let u = match usage.get(name) {
                            Some(u) => u,
                            None => continue,
                        };
                        if u.referenced && !u.mutated {
                            // The parser stored the prefix span on the
                            // Closure expression — when present, attach a
                            // machine-applicable rewrite that swaps `mut ref`
                            // for `ref` over exactly those tokens. Multiple
                            // unused-mut captures on the same closure
                            // produce one note per capture, each carrying
                            // the same edit (the dispatcher in `cmd_fix`
                            // dedupes overlapping edits before applying).
                            let replacement = prefix_span.as_ref().map(|sp| {
                                Box::new(crate::resolver::TextEdit {
                                    offset: sp.offset,
                                    length: sp.length,
                                    replacement: "ref".to_string(),
                                })
                            });
                            self.notes.push(OwnershipError {
                                message: format!(
                                    "capture `{name}` declared `mut ref` but never mutated — consider `ref`",
                                ),
                                span: expr.span.clone(),
                                kind: OwnershipErrorKind::UnusedMutCaptureNote,
                                suggestion: Some(
                                    "change the closure prefix from `mut ref` to `ref`"
                                        .to_string(),
                                ),
                                replacement,
                                consume_span: None,
                            });
                        }
                    }
                }
                for name in pre_live {
                    if let Some(ValueState::Moved { at }) = states.get(&name) {
                        // A consume that happened inside the closure body
                        // is a closure-capture-by-move from the outer
                        // function's perspective. Round 12.42 removed
                        // the post-K2 retag (formerly Direct / ContainerStore
                        // → ClosureCapture) — RC trigger 2 routing now
                        // lives entirely in the predicate pipeline:
                        // `use_classifier` tags capture-position
                        // identifier-leaves with
                        // `ConsumeOrigin::ClosureCapture` (round 12.14)
                        // and `populate_predicate_outputs` emits the
                        // flavor-correct `RcEntry`. The K2 enforcement
                        // below fires the explicit-ref / mut-ref-mode
                        // diagnostic, which is the only ownership-side
                        // action remaining for this pre-live walk.
                        let at = at.clone();
                        // K2 enforcement (design.md § Closure Behavior,
                        // Rule 2½): an explicit `ref` / `mut ref` prefix
                        // forbids consume of any captured name. Fire the
                        // error at the closure expression, naming the
                        // capture and the consume site. `own` declares
                        // consume, so a consuming body is consistent.
                        if let Some(declared @ (CaptureMode::Ref | CaptureMode::MutRef)) =
                            capture_mode
                        {
                            let declared_str = match declared {
                                CaptureMode::Ref => "ref",
                                CaptureMode::MutRef => "mut ref",
                                CaptureMode::Own => unreachable!(),
                            };
                            let fix = match declared {
                                CaptureMode::Ref => {
                                    "drop the `ref` prefix (use `own` or bare) or remove the consume"
                                }
                                CaptureMode::MutRef => {
                                    "drop the `mut ref` prefix and use `own`"
                                }
                                CaptureMode::Own => unreachable!(),
                            };
                            self.errors.push(OwnershipError {
                                message: format!(
                                    "capture `{name}` declared `{declared_str}` but consumed in closure body at {}:{} — {fix}",
                                    at.line, at.column,
                                ),
                                span: expr.span.clone(),
                                kind: OwnershipErrorKind::CaptureModeViolation,
                                suggestion: Some(fix.to_string()),
                                replacement: None,
                                consume_span: None,
                            });
                        }
                    }
                }
            }
            ExprKind::Return(Some(inner)) => {
                self.check_expr_consuming(inner, states, param_types, param_usage);
            }
            ExprKind::Break {
                value: Some(inner), ..
            }
            | ExprKind::Question(inner)
            | ExprKind::OptionalChain { object: inner, .. } => {
                self.check_expr_reading(inner, states, param_types, param_usage);
            }
            ExprKind::NilCoalesce { left, right } => {
                self.check_expr_reading(left, states, param_types, param_usage);
                self.check_expr_reading(right, states, param_types, param_usage);
            }
            ExprKind::Tuple(exprs) => {
                for e in exprs {
                    self.check_expr_consuming(e, states, param_types, param_usage);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for field in fields {
                    self.check_expr_consuming(&field.value, states, param_types, param_usage);
                }
                if let Some(ref s) = spread {
                    self.check_expr_consuming(s, states, param_types, param_usage);
                }
            }
            ExprKind::Cast { expr: inner, .. } => {
                self.check_expr_reading(inner, states, param_types, param_usage);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.check_expr_reading(s, states, param_types, param_usage);
                }
                if let Some(e) = end {
                    self.check_expr_reading(e, states, param_types, param_usage);
                }
            }
            ExprKind::ArrayLiteral(elements) => {
                for elem in elements {
                    self.check_expr_reading(elem, states, param_types, param_usage);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.check_expr_reading(value, states, param_types, param_usage);
                self.check_expr_reading(count, states, param_types, param_usage);
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for elem in items {
                    self.check_expr_reading(elem, states, param_types, param_usage);
                }
            }
            ExprKind::MapLiteral(entries) => {
                for (key, val) in entries {
                    self.check_expr_reading(key, states, param_types, param_usage);
                    self.check_expr_reading(val, states, param_types, param_usage);
                }
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    self.check_expr_reading(&b.value, states, param_types, param_usage);
                }
                self.check_block(body, states, param_types, param_usage);
            }
            ExprKind::Path { .. }
            | ExprKind::SelfType
            | ExprKind::Integer(_, _)
            | ExprKind::Float(_, _)
            | ExprKind::CharLit(_)
            | ExprKind::StringLit(_)
            | ExprKind::MultiStringLit(_)
            | ExprKind::InterpolatedStringLit(_)
            | ExprKind::Bool(_)
            | ExprKind::Continue { .. }
            | ExprKind::Return(None)
            | ExprKind::Break { value: None, .. }
            | ExprKind::PipePlaceholder
            | ExprKind::OffsetOf { .. }
            | ExprKind::Error => {}
        }
    }
}
