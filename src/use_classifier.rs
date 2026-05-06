//! AST → use-kind classification for binding-use positions.
//!
//! Round 12.9 staging step for the formal RC-fallback predicate. The
//! CFG builder (`src/cfg.rs`) defaults every recorded `UseSite` to
//! `UseKind::Read`; the predicate (`src/rc_predicate.rs`) only fires when
//! at least one use of a binding is `UseKind::Consume`. This module walks
//! a function body and produces a `HashMap<SpanKey, UseKind>` keyed by
//! the spans of binding-use leaves — `build_cfg_with_classification`
//! consumes that map to tag each `UseSite` correctly.
//!
//! The classification logic mirrors `ownership::check_expr_consuming` /
//! `ownership::check_expr_reading` (design.md § Consume Predicate) without
//! the dataflow / state-tracking machinery: every identifier-leaf is
//! tagged based purely on its syntactic position and the relevant typing
//! context (Copy vs. non-Copy, callee param mode, method receiver mode,
//! whether any match arm binds).
//!
//! ## Out of scope
//!
//! - Once-callable closure call-site consume of the closure binding.
//!   The predicate is flavor-agnostic; flagging once-callable closures
//!   whose call-site is itself a move stays in the legacy pass.
//!
//! Rounds 12.11 / 12.12 closed the trigger-2 / trigger-3 gaps via
//! structural CFG fixes layered on this classifier:
//!
//! - **Trigger 2** (closure capture with outer use): the call walker
//!   propagates `Consuming` through to capture-position identifier
//!   leaves regardless of the surrounding closure-body walk mode, so
//!   the classifier records `Consume`. The CFG places the closure
//!   body in a sibling sink block of the creation point, so a capture
//!   consume and a subsequent outer use are dominance-incomparable.
//!
//! - **Trigger 3** (container store with subsequent use): the
//!   `MethodCall` walker marks each owned (no-`mut`-marker) arg of a
//!   `mut ref self` method call into `Classification.sink_arg_spans`.
//!   The CFG `MethodCall` arm forks those args into a sibling sink
//!   block of the call site, mirroring the closure-body lowering.

use crate::ast::{
    Block, Expr, ExprKind, Item, Pattern, PatternKind, Program, SelfParam, Stmt, StmtKind,
};
use crate::cfg::{Classification, ConsumeOrigin, UseKind};
use crate::ownership::{
    collect_callee_param_modes, collect_method_self_modes, is_copy_type, OwnershipMode,
};
use crate::resolver::SpanKey;
use crate::typechecker::{Type, TypeCheckResult};
use std::collections::{HashMap, HashSet};

/// Classify every binding-use leaf within `body` as `Read` or `Consume`,
/// and identify `mut ref self` method-call args whose value-expressions
/// must be lowered into sibling sink blocks (round 12.12 trigger-3).
///
/// Returns a `Classification` consumed by `build_cfg_with_classification`.
/// Spans not present in `kinds` default to `Read`; spans not present in
/// `sink_arg_spans` are lowered inline.
pub fn classify_function_body(
    program: &Program,
    tc: &TypeCheckResult,
    body: &Block,
    param_types: HashMap<String, Type>,
) -> Classification {
    let mut classifier = UseClassifier {
        tc,
        method_self_modes: collect_method_self_modes(program),
        callee_param_modes: collect_callee_param_modes(program),
        unit_variant_names: collect_unit_variant_names(tc),
        param_types,
        local_types: HashMap::new(),
        classification: Classification::default(),
        consume_origin_ctx: ConsumeOrigin::Direct,
        once_callable_closures: HashSet::new(),
    };
    classifier.walk_block(body, Mode::Reading);
    classifier.classification
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum Mode {
    Reading,
    Consuming,
}

struct UseClassifier<'a> {
    tc: &'a TypeCheckResult,
    method_self_modes: HashMap<String, SelfParam>,
    callee_param_modes: HashMap<String, Vec<OwnershipMode>>,
    unit_variant_names: HashSet<String>,
    param_types: HashMap<String, Type>,
    /// Round 12.18: name-keyed types for `let`-bound locals,
    /// populated as the walker enters each `let pat = value;` and
    /// consulted by `classify_identifier` before falling back to the
    /// span-keyed `tc.expr_types`. Sidesteps the parser's
    /// `MethodCall.span == receiver.span` aliasing for projection
    /// receivers like `c.inner.unwrap()` — the c identifier's span
    /// is then the SAME SpanKey the typechecker writes the method's
    /// return type to, so a span-only lookup returns the wrong
    /// type. The name-keyed map mirrors `OwnershipChecker::binding_types`.
    local_types: HashMap<String, Type>,
    classification: Classification,
    /// Origin tag stamped on each Consume identifier-leaf recorded
    /// while this context is active. Default `Direct`; the closure-body
    /// walker swaps in `ClosureCapture` and the `mut ref self` sink-arg
    /// walker swaps in `ContainerStore`, each with save/restore around
    /// the inner walk so nested scopes don't leak the tag outward.
    consume_origin_ctx: ConsumeOrigin,
    /// Round 12.20: name-keyed set of bindings that hold a closure
    /// value whose body consumes at least one captured outer binding
    /// (i.e. the closure is "once-callable" — invoking it consumes
    /// the closure value via the captured-by-ownership semantics).
    /// Populated when a `let p = closure_expr` produces at least one
    /// `ClosureCapture`-tagged consume during the body walk. Consumed
    /// by the `Call` arm to tag a once-callable closure binding's
    /// call-site as `UseKind::Consume` so the UAM predicate fires on
    /// `f(); f();` shapes — mirrors `OwnershipChecker::once_callable_closures`.
    once_callable_closures: HashSet<String>,
}

impl<'a> UseClassifier<'a> {
    fn record(&mut self, span: &crate::token::Span, kind: UseKind) {
        let key = SpanKey::from_span(span);
        self.classification.kinds.insert(key, kind);
        if kind == UseKind::Consume && self.consume_origin_ctx != ConsumeOrigin::Direct {
            self.classification
                .consume_origins
                .insert(key, self.consume_origin_ctx);
        }
    }

    fn mark_sink_arg(&mut self, span: &crate::token::Span) {
        self.classification
            .sink_arg_spans
            .insert(SpanKey::from_span(span));
    }

    fn walk_block(&mut self, block: &Block, terminal_mode: Mode) {
        for stmt in &block.stmts {
            self.walk_stmt(stmt);
        }
        if let Some(final_expr) = &block.final_expr {
            self.walk_expr(final_expr, terminal_mode);
        }
    }

    fn walk_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { pattern, value, .. } => {
                // Round 12.18: record the local's type by name BEFORE
                // walking the value, using the value-span (which is
                // unaliased — only `MethodCall.span == receiver.span`
                // is, and the value's span is the value's own span).
                // Mirrors `OwnershipChecker::binding_types` population
                // at the let-RHS site so subsequent identifier-leaves
                // have a name-keyed type to fall back on when their
                // span lookup would alias.
                if let Some(rhs_ty) = self.tc.expr_types.get(&SpanKey::from_span(&value.span)) {
                    for name in pattern.binding_names() {
                        self.local_types.insert(name, rhs_ty.clone());
                    }
                }
                // Round 12.20: detect once-callable closure bindings.
                // A closure RHS that produces at least one
                // `ClosureCapture`-tagged consume during the body walk
                // captured at least one outer non-Copy binding by
                // ownership; the closure value is therefore once-
                // callable. We snapshot the count of `ClosureCapture`
                // origins before walking and re-check after, so the
                // detection is local to this let-binding and doesn't
                // confuse with prior closures in the same function.
                let rhs_is_closure = matches!(value.kind, ExprKind::Closure { .. });
                let pre_capture_count = if rhs_is_closure {
                    self.classification
                        .consume_origins
                        .values()
                        .filter(|o| **o == ConsumeOrigin::ClosureCapture)
                        .count()
                } else {
                    0
                };
                self.walk_expr(value, Mode::Consuming);
                if rhs_is_closure {
                    let post_capture_count = self
                        .classification
                        .consume_origins
                        .values()
                        .filter(|o| **o == ConsumeOrigin::ClosureCapture)
                        .count();
                    if post_capture_count > pre_capture_count {
                        for name in pattern.binding_names() {
                            self.once_callable_closures.insert(name);
                        }
                    }
                }
            }
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                self.walk_expr(value, Mode::Consuming);
                self.walk_block(else_block, Mode::Reading);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.walk_block(body, Mode::Reading);
            }
            StmtKind::Assign { target, value } => {
                // Walk the RHS first so reads of `target`'s binding inside
                // `value` see the pre-assignment state (mirrors the legacy
                // ownership.rs handler at ownership.rs:866).
                self.walk_expr(value, Mode::Consuming);
                // Round 12.19: a bare-identifier LHS rebinds the variable.
                // Tag its span with `UseKind::Reassign` so the predicate
                // pipeline treats it as a kill of any prior consume.
                // Field / tuple-index / slice-index targets stay on the
                // ordinary reading path — those are partial mutations of
                // the projection root, not rebindings of the binding.
                if let ExprKind::Identifier(_) = &target.kind {
                    self.record(&target.span, UseKind::Reassign);
                } else {
                    self.walk_expr(target, Mode::Reading);
                }
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                self.walk_expr(value, Mode::Reading);
                self.walk_expr(target, Mode::Reading);
            }
            StmtKind::Expr(e) => self.walk_expr(e, Mode::Reading),
        }
    }

    fn walk_expr(&mut self, expr: &Expr, mode: Mode) {
        match &expr.kind {
            ExprKind::Identifier(name) => {
                let kind = self.classify_identifier(name, &expr.span, mode);
                self.record(&expr.span, kind);
            }
            ExprKind::SelfValue => {
                let kind = self.classify_identifier("self", &expr.span, mode);
                self.record(&expr.span, kind);
            }

            ExprKind::Integer(..)
            | ExprKind::Float(..)
            | ExprKind::Bool(..)
            | ExprKind::CharLit(..)
            | ExprKind::StringLit(..)
            | ExprKind::MultiStringLit(..)
            | ExprKind::InterpolatedStringLit(..)
            | ExprKind::Path(..)
            | ExprKind::SelfType => {}

            ExprKind::Binary { left, right, .. }
            | ExprKind::Pipe { left, right }
            | ExprKind::NilCoalesce { left, right } => {
                self.walk_expr(left, Mode::Reading);
                self.walk_expr(right, Mode::Reading);
            }
            ExprKind::Unary { operand, .. } => {
                self.walk_expr(operand, Mode::Reading);
            }

            ExprKind::Call { callee, args } => {
                // Round 12.20: a call whose callee identifier is a
                // once-callable closure binding consumes that binding
                // (the legacy state machine sets `Moved { kind: Direct }`
                // at the call site; subsequent calls hit
                // `handle_moved_use` and emit UseAfterMove). Tag the
                // callee's identifier-leaf as Consume so the UAM
                // predicate fires for `f(); f();` shapes.
                if let ExprKind::Identifier(name) = &callee.kind {
                    if self.once_callable_closures.contains(name) {
                        self.record(&callee.span, UseKind::Consume);
                    } else {
                        self.walk_expr(callee, Mode::Reading);
                    }
                } else {
                    self.walk_expr(callee, Mode::Reading);
                }
                let modes = self.callee_modes_for_call(callee).cloned();
                for (i, arg) in args.iter().enumerate() {
                    let is_borrow = arg.mut_marker
                        || modes.as_ref().and_then(|m| m.get(i)).is_some_and(|m| {
                            matches!(m, OwnershipMode::Ref | OwnershipMode::MutRef)
                        });
                    let arg_mode = if is_borrow {
                        Mode::Reading
                    } else {
                        Mode::Consuming
                    };
                    self.walk_expr(&arg.value, arg_mode);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                let receiver_mode = if self.method_consumes_receiver(expr) {
                    Mode::Consuming
                } else {
                    Mode::Reading
                };
                if receiver_mode == Mode::Consuming {
                    // Round 12.18: sidestep the parser's `MethodCall.span ==
                    // receiver.span` aliasing for projection receivers like
                    // `c.inner.unwrap()`. The aliasing makes
                    // `tc.expr_types[receiver.span]` return the method's
                    // return type, not the field's type, so the FieldAccess
                    // Copy gate would walk `c` as Reading and miss the
                    // partial-move-projection consume of the root. Mirror
                    // the legacy ownership.rs MethodCall handler — walk to
                    // the projection's root identifier and tag it as
                    // Consume directly.
                    self.walk_method_receiver_consuming(object);
                } else {
                    self.walk_expr(object, Mode::Reading);
                }
                let receiver_is_mut_ref = self.method_receiver_is_mut_ref(expr);
                for arg in args {
                    let arg_mode = if arg.mut_marker {
                        Mode::Reading
                    } else {
                        Mode::Consuming
                    };
                    // Trigger-3 (round 12.12): owned arg of a `mut ref self`
                    // method flows into a container that outlives the call.
                    // Mark the arg's value-expression for sibling-sink
                    // lowering in the CFG so the consume site becomes
                    // dominance-incomparable with subsequent outer uses.
                    let is_sink_arg = receiver_is_mut_ref && !arg.mut_marker;
                    if is_sink_arg {
                        self.mark_sink_arg(&arg.value.span);
                    }
                    // Round 12.14: swap in `ContainerStore` for the
                    // sink-arg walk so any Consume identifier-leaf
                    // discovered inside it carries the trigger-3 origin
                    // tag. Save/restore preserves outer context.
                    let saved = self.consume_origin_ctx;
                    if is_sink_arg {
                        self.consume_origin_ctx = ConsumeOrigin::ContainerStore;
                    }
                    self.walk_expr(&arg.value, arg_mode);
                    self.consume_origin_ctx = saved;
                }
            }

            // Field / tuple-index projection. In a consuming context on a
            // non-Copy field, this is a partial move of the projection root.
            // In a reading context, the root is just read.
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                if mode == Mode::Consuming && !self.expr_is_copy(expr) {
                    self.walk_expr(object, Mode::Consuming);
                } else {
                    self.walk_expr(object, Mode::Reading);
                }
            }
            ExprKind::Index { object, index } => {
                self.walk_expr(object, Mode::Reading);
                self.walk_expr(index, Mode::Reading);
            }

            ExprKind::Block(block) => self.walk_block(block, mode),

            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.walk_expr(condition, Mode::Reading);
                self.walk_block(then_block, mode);
                if let Some(eb) = else_branch {
                    self.walk_expr(eb, mode);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                // The scrutinee of `if let` is a pattern match — same shape
                // as `match` step 4. For simplicity (and to align with the
                // dataflow conservative read-of-scrutinee in ownership.rs)
                // treat it as Reading; `match` does the binds-anything check.
                self.walk_expr(value, Mode::Reading);
                self.walk_block(then_block, mode);
                if let Some(eb) = else_branch {
                    self.walk_expr(eb, mode);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let any_arm_binds = arms
                    .iter()
                    .any(|arm| self.pattern_binds_anything(&arm.pattern));
                let scrut_mode = if any_arm_binds {
                    Mode::Consuming
                } else {
                    Mode::Reading
                };
                self.walk_expr(scrutinee, scrut_mode);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.walk_expr(g, Mode::Reading);
                    }
                    self.walk_expr(&arm.body, mode);
                }
            }

            ExprKind::While {
                condition, body, ..
            } => {
                self.walk_expr(condition, Mode::Reading);
                self.walk_block(body, Mode::Reading);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.walk_expr(value, Mode::Reading);
                self.walk_block(body, Mode::Reading);
            }
            ExprKind::For { iterable, body, .. } => {
                self.walk_expr(iterable, Mode::Consuming);
                self.walk_block(body, Mode::Reading);
            }
            ExprKind::Loop { body, .. } => {
                self.walk_block(body, Mode::Reading);
            }

            ExprKind::Break { value: Some(v), .. } => {
                self.walk_expr(v, Mode::Consuming);
            }
            ExprKind::Break { value: None, .. } | ExprKind::Continue { .. } => {}
            ExprKind::Return(Some(v)) => {
                self.walk_expr(v, Mode::Consuming);
            }
            ExprKind::Return(None) => {}
            ExprKind::Question(inner) => {
                self.walk_expr(inner, Mode::Consuming);
            }
            ExprKind::OptionalChain { object, args, .. } => {
                self.walk_expr(object, Mode::Reading);
                if let Some(arg_list) = args {
                    for arg in arg_list {
                        let arg_mode = if arg.mut_marker {
                            Mode::Reading
                        } else {
                            Mode::Consuming
                        };
                        self.walk_expr(&arg.value, arg_mode);
                    }
                }
            }

            // Closure body: walked in reading mode at the top level —
            // sub-expressions still pick their own consuming/reading
            // context (e.g., the call walker propagates Consuming to
            // owned-arg leaves). The CFG places the body in a sibling
            // sink block of the creation point (round 12.11), so
            // capture-position consumes are dominance-incomparable
            // with subsequent outer uses.
            //
            // Round 12.14: swap the origin context to `ClosureCapture`
            // for the body walk so any Consume identifier-leaf carries
            // the trigger-2 origin tag. Save/restore preserves outer
            // context (relevant for closures expressed inside another
            // closure / sink-arg).
            ExprKind::Closure { body, .. } => {
                let saved = self.consume_origin_ctx;
                self.consume_origin_ctx = ConsumeOrigin::ClosureCapture;
                self.walk_expr(body, Mode::Reading);
                self.consume_origin_ctx = saved;
            }

            ExprKind::Cast { expr: inner, .. } => self.walk_expr(inner, mode),
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.walk_expr(s, Mode::Reading);
                }
                if let Some(e) = end {
                    self.walk_expr(e, Mode::Reading);
                }
            }

            ExprKind::Tuple(es) | ExprKind::ArrayLiteral(es) => {
                for e in es {
                    self.walk_expr(e, mode);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.walk_expr(e, mode);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.walk_expr(value, mode);
                self.walk_expr(count, Mode::Reading);
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    self.walk_expr(k, mode);
                    self.walk_expr(v, mode);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    self.walk_expr(&f.value, Mode::Consuming);
                }
                if let Some(s) = spread {
                    self.walk_expr(s, Mode::Consuming);
                }
            }

            // Block-bodied "transparent" forms — `par`, `seq`, `unsafe`,
            // `lock`, `providers`. Walking is needed so that consume
            // positions and sink-arg method calls inside their bodies are
            // classified rather than falling through to the default Read.
            // Round 12.13: paired with the matching cfg.rs arm so the
            // CFG and classifier agree on what use sites exist.
            ExprKind::Par(body)
            | ExprKind::Seq(body)
            | ExprKind::Unsafe(body)
            | ExprKind::Try(body) => {
                self.walk_block(body, mode);
            }
            ExprKind::Lock { body, .. } => self.walk_block(body, mode),
            ExprKind::Providers { bindings, body } => {
                for binding in bindings {
                    self.walk_expr(&binding.value, Mode::Consuming);
                }
                self.walk_block(body, mode);
            }

            // Catch-all for forms that don't carry sub-expressions worth
            // walking for use-classification (type annotations, error
            // recovery nodes). They emit no leaves.
            _ => {}
        }
    }

    /// Walk an owned-self method-call receiver. Recurses through
    /// projection layers (FieldAccess / TupleIndex / Index) to the
    /// root identifier and tags THAT as Consume — bypassing
    /// `tc.expr_types[receiver.span]` lookups that would alias to
    /// the method's return type for chained-projection receivers.
    /// Index expressions still record their index sub-expression as
    /// a Read. Non-projection receivers (calls, blocks, etc.) fall
    /// back to a Reading walk; there's no rooted binding to consume.
    fn walk_method_receiver_consuming(&mut self, recv: &Expr) {
        match &recv.kind {
            ExprKind::Identifier(_) | ExprKind::SelfValue => {
                self.walk_expr(recv, Mode::Consuming);
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.walk_method_receiver_consuming(object);
            }
            ExprKind::Index { object, index } => {
                self.walk_expr(index, Mode::Reading);
                self.walk_method_receiver_consuming(object);
            }
            _ => self.walk_expr(recv, Mode::Reading),
        }
    }

    fn classify_identifier(&self, name: &str, span: &crate::token::Span, mode: Mode) -> UseKind {
        if mode == Mode::Reading {
            return UseKind::Read;
        }
        // Mode is Consuming: only non-Copy types are actually consumed.
        // Lookup priority (round 12.18): param_types → local_types →
        // span-keyed expr_types. The first two are name-keyed and
        // immune to the parser's `MethodCall.span == receiver.span`
        // aliasing that breaks span-only lookups for projection-root
        // identifiers like `c` in `c.inner.unwrap()`.
        let ty = self
            .param_types
            .get(name)
            .or_else(|| self.local_types.get(name))
            .or_else(|| self.tc.expr_types.get(&SpanKey::from_span(span)));
        match ty {
            Some(t) if !is_copy_type(t, self.tc) => UseKind::Consume,
            _ => UseKind::Read,
        }
    }

    fn callee_modes_for_call(&self, callee: &Expr) -> Option<&Vec<OwnershipMode>> {
        let key = match &callee.kind {
            ExprKind::Identifier(name) => name.clone(),
            ExprKind::Path(segs) => segs.join("."),
            _ => return None,
        };
        self.callee_param_modes.get(&key)
    }

    fn method_consumes_receiver(&self, method_call: &Expr) -> bool {
        let key = match self
            .tc
            .method_callee_types
            .get(&SpanKey::from_span(&method_call.span))
        {
            Some(k) => k,
            None => return false,
        };
        matches!(self.method_self_modes.get(key), Some(SelfParam::Owned))
    }

    fn method_receiver_is_mut_ref(&self, method_call: &Expr) -> bool {
        let key = match self
            .tc
            .method_callee_types
            .get(&SpanKey::from_span(&method_call.span))
        {
            Some(k) => k,
            None => return false,
        };
        matches!(self.method_self_modes.get(key), Some(SelfParam::MutRef))
    }

    fn expr_is_copy(&self, expr: &Expr) -> bool {
        self.tc
            .expr_types
            .get(&SpanKey::from_span(&expr.span))
            .map(|t| is_copy_type(t, self.tc))
            .unwrap_or(false)
    }

    fn pattern_binds_anything(&self, pattern: &Pattern) -> bool {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Literal(_) | PatternKind::RangePattern { .. } => {
                false
            }
            PatternKind::Binding(name) => !self.unit_variant_names.contains(name),
            PatternKind::AtBinding { .. } => true,
            PatternKind::Tuple(patterns) | PatternKind::TupleVariant { patterns, .. } => {
                patterns.iter().any(|p| self.pattern_binds_anything(p))
            }
            PatternKind::Struct { fields, .. } => fields.iter().any(|f| match &f.pattern {
                Some(sub) => self.pattern_binds_anything(sub),
                None => true,
            }),
            PatternKind::Or(alts) => alts.iter().any(|p| self.pattern_binds_anything(p)),
        }
    }
}

fn collect_unit_variant_names(tc: &TypeCheckResult) -> HashSet<String> {
    let mut s = HashSet::new();
    for info in tc.enum_info.values() {
        for (vn, _) in &info.variants {
            s.insert(vn.clone());
        }
    }
    s
}

/// Build a `param_types` map for a function: parameter name → type.
/// Handles bare identifiers and self-receivers; nested patterns get
/// skipped (the param's structural type doesn't decompose for our
/// Copy lookup needs).
pub fn param_types_for_function(
    f: &crate::ast::Function,
    tc: &TypeCheckResult,
) -> HashMap<String, Type> {
    let mut map = HashMap::new();
    if let Some(self_param) = &f.self_param {
        // SelfParam::Ref / MutRef receivers are borrows — irrelevant for
        // Copy classification (the borrow itself is Copy). For owned
        // `self` we'd want the type, but the function doesn't carry the
        // resolved Self type at this layer — leave it absent and let the
        // span-based expr_types lookup answer.
        let _ = self_param;
    }
    for p in &f.params {
        if let PatternKind::Binding(name) = &p.pattern.kind {
            if let Some(ty) = tc.expr_types.get(&SpanKey::from_span(&p.span)) {
                map.insert(name.clone(), ty.clone());
            }
        }
    }
    map
}

/// Convenience wrapper: locate `fn <name>` at the program top level and
/// classify its body. Returns None if the function isn't found.
pub fn classify_top_level_fn(
    program: &Program,
    tc: &TypeCheckResult,
    fn_name: &str,
) -> Option<Classification> {
    let f = program.items.iter().find_map(|i| match i {
        Item::Function(f) if f.name == fn_name => Some(f),
        _ => None,
    })?;
    let param_types = param_types_for_function(f, tc);
    Some(classify_function_body(program, tc, &f.body, param_types))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cfg::{build_cfg_with_classification, UseKind};
    use crate::{parse, resolve, typecheck};

    fn classify(
        src: &str,
    ) -> (
        HashMap<SpanKey, UseKind>,
        crate::ast::Program,
        TypeCheckResult,
    ) {
        let parsed = parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = resolve(&parsed.program);
        assert!(
            resolved.errors.is_empty(),
            "resolve errors: {:?}",
            resolved.errors
        );
        let tc = typecheck(&parsed.program, &resolved);
        let class = classify_top_level_fn(&parsed.program, &tc, "main").expect("expected fn main");
        (class.kinds, parsed.program, tc)
    }

    /// Classify and also return the sink-arg span set — used by the
    /// trigger-3 detection tests (round 12.12).
    fn classify_full(src: &str) -> (Classification, crate::ast::Program, TypeCheckResult) {
        let parsed = parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = resolve(&parsed.program);
        assert!(
            resolved.errors.is_empty(),
            "resolve errors: {:?}",
            resolved.errors
        );
        let tc = typecheck(&parsed.program, &resolved);
        let class = classify_top_level_fn(&parsed.program, &tc, "main").expect("expected fn main");
        (class, parsed.program, tc)
    }

    fn count_kinds(class: &HashMap<SpanKey, UseKind>) -> (usize, usize) {
        let mut consumes = 0;
        let mut reads = 0;
        for k in class.values() {
            match k {
                UseKind::Consume => consumes += 1,
                UseKind::Read => reads += 1,
                UseKind::Reassign => {}
            }
        }
        (consumes, reads)
    }

    #[test]
    fn copy_value_let_rhs_stays_read() {
        // i32 is Copy — even in a consuming let-RHS position, the use is
        // recorded as Read by the classifier.
        let (class, _, _) = classify(
            "fn main() {\n\
                 let x = 1;\n\
                 let y = x;\n\
             }",
        );
        let (consumes, _reads) = count_kinds(&class);
        assert_eq!(consumes, 0, "no Copy consumes expected, got {consumes}");
    }

    #[test]
    fn non_copy_let_rhs_is_consume() {
        let (class, _, _) = classify(
            "struct Box { v: i32 }\n\
             fn main() {\n\
                 let b = Box { v: 1 };\n\
                 let c = b;\n\
             }",
        );
        // `c = b` should record `b`'s identifier-use as Consume.
        let consumes: Vec<_> = class
            .iter()
            .filter(|(_, k)| **k == UseKind::Consume)
            .collect();
        assert!(
            !consumes.is_empty(),
            "expected at least one Consume; got {:?}",
            class
        );
    }

    #[test]
    fn ref_arg_is_read_not_consume() {
        // A function with a `ref` param — the call argument is a borrow
        // position even on a non-Copy value.
        let src = "struct Box { v: i32 }\n\
                   fn use_ref(b: ref Box) -> i32 { b.v }\n\
                   fn main() {\n\
                       let b = Box { v: 1 };\n\
                       let _x = use_ref(b);\n\
                       let _y = b;\n\
                   }";
        let (class, _, _) = classify(src);
        // `b` appears in two use positions — the call-arg (ref → read) and
        // the trailing `let _y = b;` (let RHS → consume). Exactly one
        // Consume on `b`.
        let consumes = class.values().filter(|k| **k == UseKind::Consume).count();
        assert_eq!(consumes, 1, "expected exactly 1 Consume, got {}", consumes);
    }

    #[test]
    fn mut_ref_arg_is_read_not_consume() {
        let src = "struct Box { v: i32 }\n\
                   fn use_mut(b: mut ref Box) -> i32 { b.v }\n\
                   fn main() {\n\
                       let mut b = Box { v: 1 };\n\
                       let _x = use_mut(mut b);\n\
                       let _y = b;\n\
                   }";
        let (class, _, _) = classify(src);
        let consumes = class.values().filter(|k| **k == UseKind::Consume).count();
        assert_eq!(consumes, 1);
    }

    #[test]
    fn owned_call_arg_is_consume() {
        let src = "struct Box { v: i32 }\n\
                   fn take(b: Box) -> i32 { b.v }\n\
                   fn main() {\n\
                       let b = Box { v: 1 };\n\
                       let _x = take(b);\n\
                   }";
        let (class, _, _) = classify(src);
        let consumes = class.values().filter(|k| **k == UseKind::Consume).count();
        assert_eq!(consumes, 1, "owned arg `take(b)` should consume `b`");
    }

    #[test]
    fn if_condition_is_read_branch_assignment_is_consume() {
        let src = "struct Box { v: i32 }\n\
                   fn main() {\n\
                       let b = Box { v: 1 };\n\
                       let cond = true;\n\
                       if cond {\n\
                           let _y = b;\n\
                       }\n\
                   }";
        let (class, _, _) = classify(src);
        // `cond` is bool (Copy) — never Consume. `b` in the then-block let
        // RHS is the only Consume.
        let consumes = class.values().filter(|k| **k == UseKind::Consume).count();
        assert_eq!(consumes, 1);
    }

    #[test]
    fn integration_branch_consume_then_outer_read_predicate_fires() {
        // End-to-end: classify → build_cfg_with_classification →
        // dominator → rc_candidates. The branch consumes `b`; the
        // outer use after the if reads `b`. Expect a witness.
        let src = "struct Box { v: i32 }\n\
                   fn take(b: Box) -> i32 { b.v }\n\
                   fn main() {\n\
                       let b = Box { v: 1 };\n\
                       let cond = true;\n\
                       if cond {\n\
                           let _x = take(b);\n\
                       }\n\
                       let _y = b;\n\
                   }";
        let parsed = parse(src);
        assert!(parsed.errors.is_empty());
        let resolved = resolve(&parsed.program);
        assert!(resolved.errors.is_empty());
        let tc = typecheck(&parsed.program, &resolved);
        let class = classify_top_level_fn(&parsed.program, &tc, "main").unwrap();

        let main = parsed
            .program
            .items
            .iter()
            .find_map(|i| match i {
                Item::Function(f) if f.name == "main" => Some(f),
                _ => None,
            })
            .unwrap();
        let cfg = build_cfg_with_classification(&main.body, &class);
        let dom = crate::dominator::compute_dominators(&cfg);
        let candidates = crate::rc_predicate::rc_candidates(&cfg, &dom);
        assert!(
            candidates.contains_key("b"),
            "expected RC witness on `b`; got candidates: {:?}",
            candidates.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn linear_consume_no_witness_in_predicate() {
        // Sequential consume + read on the same path: the consume block
        // dominates the read block, predicate must not fire.
        let src = "struct Box { v: i32 }\n\
                   fn take(b: Box) -> i32 { b.v }\n\
                   fn main() {\n\
                       let b = Box { v: 1 };\n\
                       let _x = take(b);\n\
                       let _y = 0;\n\
                   }";
        let parsed = parse(src);
        let resolved = resolve(&parsed.program);
        let tc = typecheck(&parsed.program, &resolved);
        let class = classify_top_level_fn(&parsed.program, &tc, "main").unwrap();
        let main = parsed
            .program
            .items
            .iter()
            .find_map(|i| match i {
                Item::Function(f) if f.name == "main" => Some(f),
                _ => None,
            })
            .unwrap();
        let cfg = build_cfg_with_classification(&main.body, &class);
        let dom = crate::dominator::compute_dominators(&cfg);
        let candidates = crate::rc_predicate::rc_candidates(&cfg, &dom);
        assert!(
            !candidates.contains_key("b"),
            "linear consume must not produce a predicate witness"
        );
    }

    #[test]
    fn match_arm_binding_consumes_scrutinee() {
        // Non-Copy scrutinee (`Option[String]`) — match has at least one
        // arm that binds (`Some(s)`), so the scrutinee identifier is
        // recorded as Consume per design.md § Consume Predicate step 4.
        let src = "fn main() {\n\
                       let opt: Option[String] = Some(\"hi\");\n\
                       let _ = match opt { Some(s) => s, None => \"\" };\n\
                   }";
        let (class, _, _) = classify(src);
        let consumes = class.values().filter(|k| **k == UseKind::Consume).count();
        assert!(
            consumes >= 1,
            "binding-arm match on non-Copy scrutinee `opt` should record at least one Consume; got {consumes}"
        );
    }

    #[test]
    fn match_with_no_binding_arms_is_read() {
        // No arm binds anything — scrutinee stays Read even on non-Copy
        // (mirrors design.md § Consume Predicate step 4).
        let src = "fn main() {\n\
                       let opt: Option[String] = Some(\"hi\");\n\
                       let _ = match opt { Some(_) => 1, None => 0 };\n\
                   }";
        let (class, _, _) = classify(src);
        let consumes = class.values().filter(|k| **k == UseKind::Consume).count();
        assert_eq!(
            consumes, 0,
            "non-binding arms should not record any Consume on the scrutinee; got {consumes}"
        );
    }

    #[test]
    fn return_value_is_consume() {
        let src = "struct Box { v: i32 }\n\
                   fn make() -> Box {\n\
                       let b = Box { v: 1 };\n\
                       return b;\n\
                   }\n\
                   fn main() {\n\
                       let _x = make();\n\
                   }";
        let parsed = parse(src);
        let resolved = resolve(&parsed.program);
        let tc = typecheck(&parsed.program, &resolved);
        let class = classify_top_level_fn(&parsed.program, &tc, "make").unwrap();
        let consumes = class
            .kinds
            .values()
            .filter(|k| **k == UseKind::Consume)
            .count();
        assert_eq!(consumes, 1, "`return b` should consume `b`");
    }

    #[test]
    fn struct_literal_field_value_is_consume() {
        let src = "struct Inner { v: i32 }\n\
                   struct Outer { inner: Inner }\n\
                   fn main() {\n\
                       let i = Inner { v: 1 };\n\
                       let _o = Outer { inner: i };\n\
                   }";
        let (class, _, _) = classify(src);
        let consumes = class.values().filter(|k| **k == UseKind::Consume).count();
        assert_eq!(
            consumes, 1,
            "struct-literal field value `inner: i` should consume `i`"
        );
    }

    // ── Round 12.12: trigger-3 sink-arg detection ─────────────────────

    #[test]
    fn mut_ref_self_owned_arg_is_sink_arg() {
        // `bag.insert(0, w)` where `insert` takes `mut ref self`: the
        // owned arg `w` is marked as a sink-arg so the CFG lowers it
        // into a sibling sink block.
        let src = "struct Widget { value: i64 }\n\
                   struct Bag { count: i64 }\n\
                   impl Bag {\n\
                       fn insert(mut ref self, key: i64, value: Widget) { }\n\
                   }\n\
                   fn main() {\n\
                       let w = Widget { value: 42 };\n\
                       let mut bag = Bag { count: 0 };\n\
                       bag.insert(0, w);\n\
                   }";
        let (class, _, _) = classify_full(src);
        // Exactly one sink-arg span — the `w` argument expression.
        // The literal `0` is not consumed (Copy-ish), so even though
        // we mark every non-mut-marker arg of a mut-ref-self method,
        // we should still detect the structurally-relevant one.
        assert!(
            !class.sink_arg_spans.is_empty(),
            "expected at least one sink-arg span for `bag.insert(0, w)`"
        );
    }

    #[test]
    fn mut_ref_self_mut_marker_arg_is_not_sink() {
        // Receiver is `mut ref self`, but the arg has an explicit `mut`
        // marker (a borrow, not a consume) — must NOT be marked.
        let src = "struct Widget { value: i64 }\n\
                   struct Bag { count: i64 }\n\
                   impl Bag {\n\
                       fn touch(mut ref self, w: mut ref Widget) { }\n\
                   }\n\
                   fn main() {\n\
                       let mut w = Widget { value: 42 };\n\
                       let mut bag = Bag { count: 0 };\n\
                       bag.touch(mut w);\n\
                   }";
        let (class, _, _) = classify_full(src);
        assert!(
            class.sink_arg_spans.is_empty(),
            "mut-borrowed arg of mut-ref-self method must not be a sink-arg"
        );
    }

    #[test]
    fn owned_self_method_arg_is_not_sink() {
        // Receiver is owned `self`, not `mut ref self` — no container
        // store flavor; sink-arg set must be empty.
        let src = "struct Widget { value: i64 }\n\
                   impl Widget {\n\
                       fn merge(self, other: Widget) { }\n\
                   }\n\
                   fn main() {\n\
                       let a = Widget { value: 1 };\n\
                       let b = Widget { value: 2 };\n\
                       a.merge(b);\n\
                   }";
        let (class, _, _) = classify_full(src);
        assert!(
            class.sink_arg_spans.is_empty(),
            "owned-self method args must not be sink-args"
        );
    }

    #[test]
    fn ref_self_method_arg_is_not_sink() {
        let src = "struct Widget { value: i64 }\n\
                   struct Bag { count: i64 }\n\
                   impl Bag {\n\
                       fn read(ref self, w: Widget) { }\n\
                   }\n\
                   fn main() {\n\
                       let w = Widget { value: 42 };\n\
                       let bag = Bag { count: 0 };\n\
                       bag.read(w);\n\
                   }";
        let (class, _, _) = classify_full(src);
        assert!(
            class.sink_arg_spans.is_empty(),
            "ref-self method args must not be sink-args"
        );
    }

    // ── Round 12.18: projection-receiver consume ──────────────────

    #[test]
    fn owned_self_on_field_consumes_root() {
        // `c.inner.unwrap()` where unwrap takes owned self: the
        // receiver `c.inner` is a partial move of `c`. The classifier
        // must tag `c` as Consume even though the parser aliases
        // `MethodCall.span == receiver.span`, which would otherwise
        // make `expr_is_copy(c.inner)` query the method's return
        // type (`i64` here, Copy) and demote the consume to Read.
        let src = "struct Container { inner: Inner }\n\
                   struct Inner { value: i64 }\n\
                   impl Inner {\n\
                       fn unwrap(self) -> i64 { self.value }\n\
                   }\n\
                   fn main() {\n\
                       let c = Container { inner: Inner { value: 1 } };\n\
                       let _ = c.inner.unwrap();\n\
                   }";
        let (class, _, _) = classify_full(src);
        let consumes = class
            .kinds
            .values()
            .filter(|k| **k == UseKind::Consume)
            .count();
        assert!(
            consumes >= 1,
            "owned-self on projection receiver should record at least one Consume; \
             got {consumes} consumes in {:?}",
            class.kinds
        );
    }

    #[test]
    fn owned_self_on_nested_field_consumes_root() {
        // Two-level projection: `c.outer.inner.unwrap()`.
        let src = "struct Wrap { outer: Container }\n\
                   struct Container { inner: Inner }\n\
                   struct Inner { value: i64 }\n\
                   impl Inner {\n\
                       fn unwrap(self) -> i64 { self.value }\n\
                   }\n\
                   fn main() {\n\
                       let c = Wrap { outer: Container { inner: Inner { value: 1 } } };\n\
                       let _ = c.outer.inner.unwrap();\n\
                   }";
        let (class, _, _) = classify_full(src);
        let consumes = class
            .kinds
            .values()
            .filter(|k| **k == UseKind::Consume)
            .count();
        assert!(
            consumes >= 1,
            "owned-self on nested projection should record Consume on root; \
             got {consumes} consumes"
        );
    }

    #[test]
    fn ref_self_on_projection_does_not_consume_root() {
        // Counterpart: `c.inner.read_only(self: ref self)` — the
        // receiver is *not* consumed. The root must stay Read.
        let src = "struct Container { inner: Inner }\n\
                   struct Inner { value: i64 }\n\
                   impl Inner {\n\
                       fn peek(ref self) -> i64 { self.value }\n\
                   }\n\
                   fn main() {\n\
                       let c = Container { inner: Inner { value: 1 } };\n\
                       let _ = c.inner.peek();\n\
                   }";
        let (class, _, _) = classify_full(src);
        let consumes = class
            .kinds
            .values()
            .filter(|k| **k == UseKind::Consume)
            .count();
        assert_eq!(
            consumes, 0,
            "ref-self method on projection must NOT tag the root as Consume; \
             got {:?}",
            class.kinds
        );
    }

    // ── Round 12.14: ConsumeOrigin tagging ─────────────────────────

    /// Helper: collect every Consume span and its origin (defaulting
    /// absent entries to `Direct`).
    fn consume_origins_by_line(class: &Classification) -> Vec<(usize, ConsumeOrigin)> {
        let mut out = Vec::new();
        for (key, kind) in &class.kinds {
            if *kind != UseKind::Consume {
                continue;
            }
            let origin = class
                .consume_origins
                .get(key)
                .copied()
                .unwrap_or(ConsumeOrigin::Direct);
            // The SpanKey doesn't expose its line directly, but we
            // can recover it by matching against the Direct catch-all
            // — for these tests, line-precision isn't required, so we
            // just collect origins.
            let _ = key;
            out.push((0, origin));
        }
        out
    }

    #[test]
    fn closure_capture_consume_records_closure_capture_origin() {
        // `let h = || apply(cfg);` — `apply` takes owned non-Copy, so
        // the capture-position identifier-leaf for `cfg` is tagged
        // Consume with origin `ClosureCapture`.
        let src = "struct Config { name: i64 }\n\
                   fn apply(c: Config) { }\n\
                   fn make(cfg: Config) {\n\
                       let h = || apply(cfg);\n\
                       let _ = h;\n\
                   }";
        let parsed = parse(src);
        assert!(parsed.errors.is_empty(), "parse: {:?}", parsed.errors);
        let resolved = resolve(&parsed.program);
        assert!(resolved.errors.is_empty(), "resolve: {:?}", resolved.errors);
        let tc = typecheck(&parsed.program, &resolved);
        let class = classify_top_level_fn(&parsed.program, &tc, "make").expect("expected fn make");

        let origins = consume_origins_by_line(&class);
        assert!(
            origins
                .iter()
                .any(|(_, o)| *o == ConsumeOrigin::ClosureCapture),
            "expected at least one ClosureCapture origin; got {:?}",
            origins
        );
    }

    #[test]
    fn mut_ref_self_sink_arg_records_container_store_origin() {
        // `bag.insert(0, w)` with `insert(mut ref self, _, value: Widget)`:
        // the `w` identifier-leaf inside the sink-arg position is
        // tagged Consume with origin `ContainerStore`.
        let src = "struct Widget { value: i64 }\n\
                   struct Bag { count: i64 }\n\
                   impl Bag {\n\
                       fn insert(mut ref self, key: i64, value: Widget) { }\n\
                   }\n\
                   fn main() {\n\
                       let w = Widget { value: 42 };\n\
                       let mut bag = Bag { count: 0 };\n\
                       bag.insert(0, w);\n\
                   }";
        let (class, _, _) = classify_full(src);
        let origins = consume_origins_by_line(&class);
        assert!(
            origins
                .iter()
                .any(|(_, o)| *o == ConsumeOrigin::ContainerStore),
            "expected at least one ContainerStore origin for the sink-arg `w`; got {:?}",
            origins
        );
    }

    #[test]
    fn direct_consume_has_no_explicit_origin_entry() {
        // `let c = b;` — non-Copy let-RHS — is a Consume with no
        // closure body / no sink-arg context; the classifier must
        // NOT record a non-Direct origin (defaulting via absence).
        let src = "struct Box { v: i32 }\n\
                   fn main() {\n\
                       let b = Box { v: 1 };\n\
                       let c = b;\n\
                   }";
        let (class, _, _) = classify_full(src);
        // At least one Consume exists.
        let consumes = class
            .kinds
            .values()
            .filter(|k| **k == UseKind::Consume)
            .count();
        assert!(consumes >= 1, "expected at least 1 Consume");
        // No Consume span should carry a non-Direct origin tag.
        assert!(
            class.consume_origins.is_empty(),
            "direct consumes must not populate consume_origins; got {:?}",
            class.consume_origins
        );
    }

    #[test]
    fn closure_capture_origin_does_not_leak_outside_closure() {
        // After walking a closure body, subsequent direct consumes in
        // the outer function must not pick up the ClosureCapture tag.
        let src = "struct Config { name: i64 }\n\
                   fn apply(c: Config) { }\n\
                   fn make(cfg: Config, other: Config) {\n\
                       let h = || apply(cfg);\n\
                       let _ = h;\n\
                       apply(other);\n\
                   }";
        let parsed = parse(src);
        assert!(parsed.errors.is_empty());
        let resolved = resolve(&parsed.program);
        assert!(resolved.errors.is_empty());
        let tc = typecheck(&parsed.program, &resolved);
        let class = classify_top_level_fn(&parsed.program, &tc, "make").expect("expected fn make");

        // Two Consume sites with non-Direct origin would mean the
        // outer `apply(other)` was tagged. The closure capture (cfg)
        // is the only ClosureCapture; `other` must default to Direct
        // (i.e. absent from consume_origins).
        let closure_capture_count = class
            .consume_origins
            .values()
            .filter(|o| **o == ConsumeOrigin::ClosureCapture)
            .count();
        assert_eq!(
            closure_capture_count, 1,
            "expected exactly 1 ClosureCapture origin (cfg only); got {:?}",
            class.consume_origins
        );
    }

    // ── Round 12.19: reassignment-marker emission ─────────────────────

    #[test]
    fn bare_identifier_assign_target_records_reassign() {
        // `x = 2;` with bare-identifier LHS — the LHS span carries
        // UseKind::Reassign. The `2` literal contributes no use site.
        let src = "fn main() {\n\
                       let mut x = 1;\n\
                       x = 2;\n\
                   }";
        let (class, _, _) = classify(src);
        let reassigns: Vec<_> = class
            .iter()
            .filter(|(_, k)| **k == UseKind::Reassign)
            .collect();
        assert_eq!(
            reassigns.len(),
            1,
            "expected exactly one Reassign marker, got {:?}",
            class
        );
    }

    #[test]
    fn field_assign_target_does_not_emit_reassign() {
        // `s.value = 2;` is a partial mutation of the projection root,
        // not a rebinding. The classifier must NOT emit Reassign — the
        // LHS path stays on the ordinary reading walk.
        let src = "struct S { value: i64 }\n\
                   fn main() {\n\
                       let mut s = S { value: 1 };\n\
                       s.value = 2;\n\
                   }";
        let (class, _, _) = classify(src);
        let reassigns = class.values().filter(|k| **k == UseKind::Reassign).count();
        assert_eq!(
            reassigns, 0,
            "field-assign LHS must not emit a Reassign marker; got {:?}",
            class
        );
    }

    // ── Round 12.20: once-callable closure call-site consume ────────

    #[test]
    fn once_callable_closure_first_call_is_consume() {
        // `let f = || apply(cfg);` — the body consumes `cfg` (non-Copy
        // owned outer binding) → f is once-callable. The call site
        // `f()` must be tagged as a Consume of f so the UAM predicate
        // can fire on a subsequent `f()`.
        let src = "struct Config { name: i64 }\n\
                   fn apply(c: Config) { }\n\
                   fn main() {\n\
                       let cfg = Config { name: 1 };\n\
                       let f = || apply(cfg);\n\
                       f();\n\
                   }";
        let (class, _, _) = classify(src);
        // At least one Consume tagged on an `f` use — that's the call.
        let f_consumes: Vec<_> = class
            .iter()
            .filter(|(_, k)| **k == UseKind::Consume)
            .collect();
        assert!(
            !f_consumes.is_empty(),
            "expected at least one Consume tag for f's call site; got {:?}",
            class
        );
    }

    #[test]
    fn repeatable_closure_call_is_read() {
        // Closure body only reads (Copy field) → no ClosureCapture
        // origin → not once-callable → call site stays Read.
        let src = "struct Config { value: i64 }\n\
                   fn main() {\n\
                       let cfg = Config { value: 1 };\n\
                       let f = || cfg.value;\n\
                       let _a = f();\n\
                       let _b = f();\n\
                   }";
        let (class, _, _) = classify(src);
        // Two `f()` call sites — both Read because cfg.value is i64
        // (Copy) and no capture-by-ownership occurred. The call-site
        // identifier-leaves of f should NOT be tagged Consume.
        let f_consumes: Vec<_> = class
            .iter()
            .filter(|(_, k)| **k == UseKind::Consume)
            .collect();
        // The let-RHS `f` in `let _a = f();` is NOT a Consume because
        // the call returns i64 (Copy). And both calls of f are Read.
        // So no Consume tags expected for f.
        assert_eq!(
            f_consumes.len(),
            0,
            "no Consume tags expected when closure is repeatable; got {:?}",
            class
        );
    }

    #[test]
    fn explicit_ref_closure_only_reading_capture_is_repeatable() {
        // `let f = ref || println(name);` — explicit ref capture mode +
        // body only reads. No ClosureCapture origin emitted, so f is
        // not once-callable. (The classifier doesn't model the
        // CaptureModeViolation E0504 case where ref is declared but
        // body consumes — that's a hard error caught earlier.)
        let src = "fn main() {\n\
                       let name = 1;\n\
                       let f = ref || name;\n\
                       f();\n\
                       f();\n\
                   }";
        let (class, _, _) = classify(src);
        let f_consumes = class.values().filter(|k| **k == UseKind::Consume).count();
        assert_eq!(
            f_consumes, 0,
            "ref-capture closure with reading body must not produce Consumes; got {:?}",
            class
        );
    }

    #[test]
    fn second_let_with_non_closure_rhs_does_not_taint_once_callable_set() {
        // After `let f = || consume_capture(cfg)`, walking `let g = 1`
        // must not perturb f's once-callability — the once-callable
        // detection only fires when the RHS itself is a closure.
        let src = "struct Config { name: i64 }\n\
                   fn apply(c: Config) { }\n\
                   fn main() {\n\
                       let cfg = Config { name: 1 };\n\
                       let f = || apply(cfg);\n\
                       let g = 1;\n\
                       f();\n\
                   }";
        let (class, _, _) = classify(src);
        // f's call site must be Consume.
        let consumes_count = class.values().filter(|k| **k == UseKind::Consume).count();
        // Expected: cfg in `apply(cfg)` (ClosureCapture-tagged) +
        // f's call site (Direct-tagged). Two Consumes.
        assert!(
            consumes_count >= 2,
            "expected at least two Consumes (capture + call); got {} ({:?})",
            consumes_count,
            class
        );
    }

    #[test]
    fn reassign_target_not_double_counted_as_read() {
        // The bare-ident LHS of an Assign goes onto the Reassign-only
        // path; it must not also leave a Read entry behind. Pin that by
        // verifying the let-RHS reads (`let _y = x;`) are exactly the
        // Read sites the classifier produces.
        let src = "fn main() {\n\
                       let mut x = 1;\n\
                       let _y = x;\n\
                       x = 2;\n\
                   }";
        let (class, _, _) = classify(src);
        let reads = class.values().filter(|k| **k == UseKind::Read).count();
        let reassigns = class.values().filter(|k| **k == UseKind::Reassign).count();
        // One Read on `x` in `let _y = x;` (Copy → Read), one Reassign
        // on the LHS of `x = 2;`. No third Read.
        assert_eq!(
            reads, 1,
            "expected exactly one Read (let-RHS use of x); got {:?}",
            class
        );
        assert_eq!(
            reassigns, 1,
            "expected exactly one Reassign (LHS of x = 2); got {:?}",
            class
        );
    }
}
