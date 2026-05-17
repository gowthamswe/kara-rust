//! Closure capture-body usage classification.
//!
//! Houses `classify_capture_body_uses` (the entry called from
//! check_expr_consuming's Closure arm to decide each capture's
//! mode) plus the three-way `walk_capture_body_{expr,block,stmt}`
//! that scan a closure body once, recording per-binding
//! `referenced` and `mutated` flags. Output drives the
//! mut-ref-with-no-mutation perf note (Rule 2½ K2 row "mut ref +
//! reads only").
//!
//! Also houses `classify_capture_body_paths` — the disjoint-capture
//! slice-1 analyser (line 353 phase-5 checklist) that produces the
//! per-closure set of `CapturePath` records the body touches. The
//! path walker is structurally separate from `classify_capture_body_uses`
//! because it tracks distinct *places* (root + projection chain),
//! not per-name read/mutate signals — extending the existing
//! per-binding walker to also carry projection state would conflate
//! two analyses with different inputs and stopping rules.
//!
//! Slice 2 adds `classify_capture_path_mutations` — a second walk
//! over the body that detects mutation events (assignment targets,
//! `mut`-marker call args, `mut ref self` method-call receivers) and
//! returns the subset of slice-1's path set whose places overlap any
//! mutation target. The mode-inference layer at the Closure arm
//! combines this with root-consume detection (from the main
//! ownership-checker's `states` map) to produce the per-path mode
//! (`Own` / `MutRef` / `Ref`). Slice 3 will pass the mode-tagged set
//! to the borrow checker.
//!
//! Lives in a sibling `impl<'a> super::OwnershipChecker<'a>` block.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::ast::*;

use super::{CaptureBodyUsage, CapturePath, WholeRootCaptureReason};

impl<'a> super::OwnershipChecker<'a> {
    /// Walk `body` once and classify each pre-live capture's usage as
    /// `referenced` (any read of the bare identifier or a place expression
    /// rooted at it) and `mutated` (assignment-target root, `mut`-marker
    /// arg root, or `mut ref self` method-call receiver root). Used by the
    /// `mut ref` capture-mode unused-mut-capture perf note (Rule 2½ K2 row
    /// "mut ref + reads only").
    pub(crate) fn classify_capture_body_uses(
        &self,
        body: &Expr,
        pre_live: &[String],
    ) -> HashMap<String, CaptureBodyUsage> {
        let mut usage: HashMap<String, CaptureBodyUsage> = pre_live
            .iter()
            .map(|n| (n.clone(), CaptureBodyUsage::default()))
            .collect();
        self.walk_capture_body_expr(body, &mut usage);
        usage
    }

    fn walk_capture_body_expr(&self, expr: &Expr, usage: &mut HashMap<String, CaptureBodyUsage>) {
        match &expr.kind {
            ExprKind::Identifier(n) => {
                if let Some(u) = usage.get_mut(n) {
                    u.referenced = true;
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                if let Some(root) = Self::root_identifier(object) {
                    if let Some(u) = usage.get_mut(&root) {
                        u.referenced = true;
                        if self.method_call_receiver_is_mut_ref(expr) {
                            u.mutated = true;
                        }
                    }
                }
                self.walk_capture_body_expr(object, usage);
                for arg in args {
                    if arg.mut_marker {
                        if let Some(root) = Self::root_identifier(&arg.value) {
                            if let Some(u) = usage.get_mut(&root) {
                                u.mutated = true;
                            }
                        }
                    }
                    self.walk_capture_body_expr(&arg.value, usage);
                }
            }
            ExprKind::Call { callee, args } => {
                self.walk_capture_body_expr(callee, usage);
                for arg in args {
                    if arg.mut_marker {
                        if let Some(root) = Self::root_identifier(&arg.value) {
                            if let Some(u) = usage.get_mut(&root) {
                                u.mutated = true;
                            }
                        }
                    }
                    self.walk_capture_body_expr(&arg.value, usage);
                }
            }
            ExprKind::Binary { left, right, .. } => {
                self.walk_capture_body_expr(left, usage);
                self.walk_capture_body_expr(right, usage);
            }
            ExprKind::Unary { operand, .. } => {
                self.walk_capture_body_expr(operand, usage);
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.walk_capture_body_expr(object, usage);
            }
            ExprKind::Index { object, index } => {
                self.walk_capture_body_expr(object, usage);
                self.walk_capture_body_expr(index, usage);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.walk_capture_body_expr(condition, usage);
                self.walk_capture_body_block(then_block, usage);
                if let Some(eb) = else_branch {
                    self.walk_capture_body_expr(eb, usage);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.walk_capture_body_expr(value, usage);
                self.walk_capture_body_block(then_block, usage);
                if let Some(eb) = else_branch {
                    self.walk_capture_body_expr(eb, usage);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_capture_body_expr(scrutinee, usage);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.walk_capture_body_expr(g, usage);
                    }
                    self.walk_capture_body_expr(&arm.body, usage);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.walk_capture_body_expr(condition, usage);
                self.walk_capture_body_block(body, usage);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.walk_capture_body_expr(value, usage);
                self.walk_capture_body_block(body, usage);
            }
            ExprKind::For { iterable, body, .. } => {
                self.walk_capture_body_expr(iterable, usage);
                self.walk_capture_body_block(body, usage);
            }
            ExprKind::Loop { body, .. } => {
                self.walk_capture_body_block(body, usage);
            }
            ExprKind::Closure { body, .. } => {
                // Recurse into nested closure bodies — a mutation of an
                // outer capture inside a nested closure still counts as a
                // mutation from this closure's perspective.
                self.walk_capture_body_expr(body, usage);
            }
            ExprKind::Block(block)
            | ExprKind::Unsafe(block)
            | ExprKind::Try(block)
            | ExprKind::Seq(block)
            | ExprKind::Par(block)
            | ExprKind::Lock { body: block, .. } => {
                self.walk_capture_body_block(block, usage);
            }
            ExprKind::Question(inner)
            | ExprKind::OptionalChain { object: inner, .. }
            | ExprKind::Cast { expr: inner, .. } => {
                self.walk_capture_body_expr(inner, usage);
            }
            ExprKind::NilCoalesce { left, right } => {
                self.walk_capture_body_expr(left, usage);
                self.walk_capture_body_expr(right, usage);
            }
            ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
                for e in exprs {
                    self.walk_capture_body_expr(e, usage);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.walk_capture_body_expr(e, usage);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.walk_capture_body_expr(value, usage);
                self.walk_capture_body_expr(count, usage);
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    self.walk_capture_body_expr(k, usage);
                    self.walk_capture_body_expr(v, usage);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for field in fields {
                    self.walk_capture_body_expr(&field.value, usage);
                }
                if let Some(s) = spread {
                    self.walk_capture_body_expr(s, usage);
                }
            }
            ExprKind::Pipe { left, right } => {
                self.walk_capture_body_expr(left, usage);
                self.walk_capture_body_expr(right, usage);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.walk_capture_body_expr(s, usage);
                }
                if let Some(e) = end {
                    self.walk_capture_body_expr(e, usage);
                }
            }
            ExprKind::Return(Some(inner))
            | ExprKind::Break {
                value: Some(inner), ..
            } => {
                self.walk_capture_body_expr(inner, usage);
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    self.walk_capture_body_expr(&b.value, usage);
                }
                self.walk_capture_body_block(body, usage);
            }
            // Leaves and other forms have no captures of interest.
            _ => {}
        }
    }

    fn walk_capture_body_block(
        &self,
        block: &Block,
        usage: &mut HashMap<String, CaptureBodyUsage>,
    ) {
        for stmt in &block.stmts {
            self.walk_capture_body_stmt(stmt, usage);
        }
        if let Some(expr) = &block.final_expr {
            self.walk_capture_body_expr(expr, usage);
        }
    }

    fn walk_capture_body_stmt(&self, stmt: &Stmt, usage: &mut HashMap<String, CaptureBodyUsage>) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => {
                self.walk_capture_body_expr(value, usage);
            }
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                self.walk_capture_body_expr(value, usage);
                self.walk_capture_body_block(else_block, usage);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.walk_capture_body_block(body, usage);
            }
            StmtKind::Assign { target, value } => {
                if let Some(root) = Self::root_identifier(target) {
                    if let Some(u) = usage.get_mut(&root) {
                        u.mutated = true;
                    }
                }
                self.walk_capture_body_expr(target, usage);
                self.walk_capture_body_expr(value, usage);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                if let Some(root) = Self::root_identifier(target) {
                    if let Some(u) = usage.get_mut(&root) {
                        u.mutated = true;
                    }
                }
                self.walk_capture_body_expr(target, usage);
                self.walk_capture_body_expr(value, usage);
            }
            StmtKind::Expr(e) => self.walk_capture_body_expr(e, usage),
        }
    }

    // ── Disjoint capture (line 353 phase-5 checklist) ────────────────
    //
    // Slice 1 — capture-path enumeration. `classify_capture_body_paths`
    // walks the closure body once and records the set of distinct
    // `CapturePath { root, projection }` records the body touches. A
    // pure place expression rooted at a pre-live name registers its
    // full projection chain; a stopping construct (index, method call,
    // deref of a captured root, function-call receiver/argument that
    // breaks the chain) commits the root as captured whole and the
    // walk descends into sub-expressions normally.
    //
    // Mode inference (which path is `ref`/`mut ref`/`own`) is slice 2;
    // borrow-checker integration is slice 3; codegen environment
    // representation is slice 4. This slice produces only the path set
    // — purely additive; no existing path through the ownership
    // checker reads it yet.

    /// Walk `body` once and produce the set of distinct
    /// `CapturePath` records the body touches against any pre-live
    /// name *plus* the per-root reason map for any root committed to
    /// whole-root capture (line 353 phase-5 checklist disjoint-capture
    /// slice 6). Path output is sorted lexicographically by
    /// `(root, projection)` for deterministic test pins; the reason
    /// map carries the AST construct (method call / index / deref /
    /// by-value pass / bare identifier) that first committed each
    /// root as captured-whole, so the RC-fallback note can explain
    /// the user's natural sibling-path pattern doesn't compose with
    /// the closure's capture choice.
    pub(crate) fn classify_capture_body_paths(
        &self,
        body: &Expr,
        pre_live: &[String],
    ) -> (Vec<CapturePath>, HashMap<String, WholeRootCaptureReason>) {
        let live: HashSet<&str> = pre_live.iter().map(String::as_str).collect();
        let mut paths: BTreeSet<CapturePath> = BTreeSet::new();
        let mut reasons: HashMap<String, WholeRootCaptureReason> = HashMap::new();
        Self::walk_capture_paths_expr(body, &live, &mut paths, &mut reasons);
        (paths.into_iter().collect(), reasons)
    }

    /// Record `reason` against `root` with the slice-6 "first stopping
    /// construct wins; BareIdentifier loses to any stopping construct"
    /// priority. Called at every whole-root capture insertion point so
    /// the per-closure reason map carries the most-informative cause
    /// available — a method call beats a later bare-ident reference
    /// even when the walker visits the bare-ident leaf first.
    fn record_whole_root_reason(
        reasons: &mut HashMap<String, WholeRootCaptureReason>,
        root: &str,
        reason: WholeRootCaptureReason,
    ) {
        use std::collections::hash_map::Entry;
        match reasons.entry(root.to_string()) {
            Entry::Vacant(e) => {
                e.insert(reason);
            }
            Entry::Occupied(mut e) => {
                if e.get().is_bare_identifier() && !reason.is_bare_identifier() {
                    e.insert(reason);
                }
            }
        }
    }

    /// If `expr` is a chain of `FieldAccess` / `TupleIndex` rooted at
    /// a pre-live `Identifier`, return the assembled `CapturePath`.
    /// Otherwise return `None` — the caller falls through to the
    /// stopping-construct match or the generic sub-expression walk.
    /// Tuple-index segments are stringified (`t.0` → projection
    /// `["0"]`) so struct-field and tuple-position chains share one
    /// path-set machinery.
    fn extract_pure_path(expr: &Expr, pre_live: &HashSet<&str>) -> Option<CapturePath> {
        match &expr.kind {
            ExprKind::Identifier(n) if pre_live.contains(n.as_str()) => Some(CapturePath {
                root: n.clone(),
                projection: Vec::new(),
            }),
            ExprKind::FieldAccess { object, field } => {
                let mut p = Self::extract_pure_path(object, pre_live)?;
                p.projection.push(field.clone());
                Some(p)
            }
            ExprKind::TupleIndex { object, index } => {
                let mut p = Self::extract_pure_path(object, pre_live)?;
                p.projection.push(index.to_string());
                Some(p)
            }
            _ => None,
        }
    }

    /// If `expr` is a place expression rooted at a pre-live name
    /// (possibly through field / tuple-index projections), return the
    /// root identifier. Used by stopping-construct arms (Index,
    /// MethodCall, Deref) to commit the root as captured whole when
    /// the receiver is a captured-rooted place. Does *not* recurse
    /// through Index / MethodCall / Deref / Call — those are
    /// themselves stopping constructs and surface as the receiver of
    /// some enclosing form, not as path extenders.
    fn place_root_for_capture(expr: &Expr, pre_live: &HashSet<&str>) -> Option<String> {
        match &expr.kind {
            ExprKind::Identifier(n) if pre_live.contains(n.as_str()) => Some(n.clone()),
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                Self::place_root_for_capture(object, pre_live)
            }
            _ => None,
        }
    }

    fn walk_capture_paths_expr(
        expr: &Expr,
        pre_live: &HashSet<&str>,
        paths: &mut BTreeSet<CapturePath>,
        reasons: &mut HashMap<String, WholeRootCaptureReason>,
    ) {
        // A pure place expression rooted at a pre-live name — register
        // the projection chain and stop. The chain has no sub-
        // expressions to recurse into beyond the (already-walked) root
        // identifier. Whole-root (empty projection) registers a
        // low-priority `BareIdentifier` reason; any stopping construct
        // encountered elsewhere for the same root overrides via the
        // `record_whole_root_reason` priority rule.
        if let Some(p) = Self::extract_pure_path(expr, pre_live) {
            let root = p.root.clone();
            let is_whole = p.projection.is_empty();
            paths.insert(p);
            if is_whole {
                Self::record_whole_root_reason(
                    reasons,
                    &root,
                    WholeRootCaptureReason::BareIdentifier,
                );
            }
            return;
        }

        match &expr.kind {
            // FieldAccess / TupleIndex whose object is not a pure
            // place rooted at a pre-live name (extract_pure_path
            // returned None above). The projection chain cannot be
            // extended from a non-place inner expression — recurse
            // into the object to find any nested captures (e.g.,
            // `items[0].field` — the object is `items[0]`, which the
            // Index arm below will register as `(items, [])`).
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                Self::walk_capture_paths_expr(object, pre_live, paths, reasons);
            }
            // Stopping construct: index. If the indexed expression is
            // rooted at a pre-live name, the root is captured whole;
            // the index expression itself is walked normally for
            // nested captures.
            ExprKind::Index { object, index } => {
                if let Some(root) = Self::place_root_for_capture(object, pre_live) {
                    paths.insert(CapturePath {
                        root: root.clone(),
                        projection: Vec::new(),
                    });
                    Self::record_whole_root_reason(
                        reasons,
                        &root,
                        WholeRootCaptureReason::Index {
                            call_span: expr.span.clone(),
                        },
                    );
                } else {
                    Self::walk_capture_paths_expr(object, pre_live, paths, reasons);
                }
                Self::walk_capture_paths_expr(index, pre_live, paths, reasons);
            }
            // Stopping construct: method call. The receiver, if rooted
            // at a pre-live name, captures the root whole. Args are
            // walked normally — each may itself capture a different
            // path under the same or a different root. Bare-rooted
            // args additionally register `ByValuePass` (Owned param
            // semantics is the "by value" the spec's path-stopping
            // rule names — see design.md § Rule 2¼ stopping
            // constructs).
            ExprKind::MethodCall {
                object,
                method,
                args,
                ..
            } => {
                if let Some(root) = Self::place_root_for_capture(object, pre_live) {
                    paths.insert(CapturePath {
                        root: root.clone(),
                        projection: Vec::new(),
                    });
                    Self::record_whole_root_reason(
                        reasons,
                        &root,
                        WholeRootCaptureReason::MethodCall {
                            method_name: method.clone(),
                            call_span: expr.span.clone(),
                        },
                    );
                } else {
                    Self::walk_capture_paths_expr(object, pre_live, paths, reasons);
                }
                for arg in args {
                    Self::register_bare_root_arg_as_byvalue(
                        &arg.value, pre_live, paths, reasons, &expr.span,
                    );
                    Self::walk_capture_paths_expr(&arg.value, pre_live, paths, reasons);
                }
            }
            // Stopping construct: deref. Per spec, "deref of a captured
            // borrow" stops the path — but deref of a captured-rooted
            // place by definition implies the root is a borrow (deref
            // wouldn't typecheck otherwise), so we apply the rule
            // uniformly without consulting binding types.
            ExprKind::Unary {
                op: UnaryOp::Deref,
                operand,
            } => {
                if let Some(root) = Self::place_root_for_capture(operand, pre_live) {
                    paths.insert(CapturePath {
                        root: root.clone(),
                        projection: Vec::new(),
                    });
                    Self::record_whole_root_reason(
                        reasons,
                        &root,
                        WholeRootCaptureReason::Deref {
                            call_span: expr.span.clone(),
                        },
                    );
                } else {
                    Self::walk_capture_paths_expr(operand, pre_live, paths, reasons);
                }
            }
            ExprKind::Unary { operand, .. } => {
                Self::walk_capture_paths_expr(operand, pre_live, paths, reasons);
            }
            // Call: callee + args. Each arg expression is walked
            // normally; whether an arg is passed by value or by borrow
            // (and therefore whether it forces a whole-root capture)
            // is a per-arg-mode question slice 2 will answer. For
            // slice 1, the place-expression extraction does the right
            // thing: a bare `cfg` arg registers `(cfg, [])`; a
            // `cfg.value` arg registers `(cfg, [value])`. The
            // distinction between "passed-by-value collapses to whole"
            // and "passed-by-ref preserves projection" lands with the
            // mode pass. Slice 6 additionally registers a bare-rooted
            // arg's whole-root reason as `ByValuePass` so the
            // RC-fallback note can name the call as the cause.
            ExprKind::Call { callee, args } => {
                Self::walk_capture_paths_expr(callee, pre_live, paths, reasons);
                for arg in args {
                    Self::register_bare_root_arg_as_byvalue(
                        &arg.value, pre_live, paths, reasons, &expr.span,
                    );
                    Self::walk_capture_paths_expr(&arg.value, pre_live, paths, reasons);
                }
            }
            ExprKind::Binary { left, right, .. } | ExprKind::NilCoalesce { left, right } => {
                Self::walk_capture_paths_expr(left, pre_live, paths, reasons);
                Self::walk_capture_paths_expr(right, pre_live, paths, reasons);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                Self::walk_capture_paths_expr(condition, pre_live, paths, reasons);
                Self::walk_capture_paths_block(then_block, pre_live, paths, reasons);
                if let Some(eb) = else_branch {
                    Self::walk_capture_paths_expr(eb, pre_live, paths, reasons);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                Self::walk_capture_paths_expr(value, pre_live, paths, reasons);
                Self::walk_capture_paths_block(then_block, pre_live, paths, reasons);
                if let Some(eb) = else_branch {
                    Self::walk_capture_paths_expr(eb, pre_live, paths, reasons);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                Self::walk_capture_paths_expr(scrutinee, pre_live, paths, reasons);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        Self::walk_capture_paths_expr(g, pre_live, paths, reasons);
                    }
                    Self::walk_capture_paths_expr(&arm.body, pre_live, paths, reasons);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                Self::walk_capture_paths_expr(condition, pre_live, paths, reasons);
                Self::walk_capture_paths_block(body, pre_live, paths, reasons);
            }
            ExprKind::WhileLet { value, body, .. } => {
                Self::walk_capture_paths_expr(value, pre_live, paths, reasons);
                Self::walk_capture_paths_block(body, pre_live, paths, reasons);
            }
            ExprKind::For { iterable, body, .. } => {
                Self::walk_capture_paths_expr(iterable, pre_live, paths, reasons);
                Self::walk_capture_paths_block(body, pre_live, paths, reasons);
            }
            ExprKind::Loop { body, .. } => {
                Self::walk_capture_paths_block(body, pre_live, paths, reasons);
            }
            ExprKind::Closure { body, .. } => {
                // Recurse into nested closure bodies — captures of an
                // outer-outer binding by an inner closure still appear
                // as captures of this closure (it must capture the
                // outer binding to make it available to the inner one).
                Self::walk_capture_paths_expr(body, pre_live, paths, reasons);
            }
            ExprKind::Block(block)
            | ExprKind::Unsafe(block)
            | ExprKind::Try(block)
            | ExprKind::Seq(block)
            | ExprKind::Par(block)
            | ExprKind::Lock { body: block, .. } => {
                Self::walk_capture_paths_block(block, pre_live, paths, reasons);
            }
            ExprKind::Question(inner)
            | ExprKind::OptionalChain { object: inner, .. }
            | ExprKind::Cast { expr: inner, .. } => {
                Self::walk_capture_paths_expr(inner, pre_live, paths, reasons);
            }
            ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
                for e in exprs {
                    Self::walk_capture_paths_expr(e, pre_live, paths, reasons);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    Self::walk_capture_paths_expr(e, pre_live, paths, reasons);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                Self::walk_capture_paths_expr(value, pre_live, paths, reasons);
                Self::walk_capture_paths_expr(count, pre_live, paths, reasons);
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    Self::walk_capture_paths_expr(k, pre_live, paths, reasons);
                    Self::walk_capture_paths_expr(v, pre_live, paths, reasons);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for field in fields {
                    Self::walk_capture_paths_expr(&field.value, pre_live, paths, reasons);
                }
                if let Some(s) = spread {
                    Self::walk_capture_paths_expr(s, pre_live, paths, reasons);
                }
            }
            ExprKind::Pipe { left, right } => {
                Self::walk_capture_paths_expr(left, pre_live, paths, reasons);
                Self::walk_capture_paths_expr(right, pre_live, paths, reasons);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    Self::walk_capture_paths_expr(s, pre_live, paths, reasons);
                }
                if let Some(e) = end {
                    Self::walk_capture_paths_expr(e, pre_live, paths, reasons);
                }
            }
            ExprKind::Return(Some(inner))
            | ExprKind::Break {
                value: Some(inner), ..
            } => {
                Self::walk_capture_paths_expr(inner, pre_live, paths, reasons);
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    Self::walk_capture_paths_expr(&b.value, pre_live, paths, reasons);
                }
                Self::walk_capture_paths_block(body, pre_live, paths, reasons);
            }
            // Identifier handled by `extract_pure_path` above; any leaf
            // identifier whose name isn't in `pre_live` is not a
            // capture and produces no path. Other leaves and
            // unhandled forms have no sub-expressions that could
            // reference captures.
            _ => {}
        }
    }

    /// If `arg` is *syntactically* a bare identifier rooted at a
    /// pre-live name (e.g., `f(cfg)`), register `(cfg, [])` with a
    /// `ByValuePass { call_span }` reason. Slice 6 uses this to name
    /// the call site as the cause when the RC-fallback note explains
    /// why the closure committed `cfg` to whole-root capture. Only
    /// the immediate-bare-identifier shape qualifies — `f(cfg.x)`
    /// (field projection) records `(cfg, ["x"])` via the generic
    /// walker and stays *not* whole-root; `f(1 + cfg)` (binary)
    /// recurses through `Binary` and the bare `cfg` leaf registers
    /// the lower-priority `BareIdentifier` reason because the bare
    /// reference is to a computation argument, not a direct pass.
    /// The `record_whole_root_reason` priority rule respects "first
    /// stopping construct wins, BareIdentifier loses": calling this
    /// before the generic walker recursion ensures `ByValuePass`
    /// wins over the later BareIdentifier insertion from the recursion.
    fn register_bare_root_arg_as_byvalue(
        arg: &Expr,
        pre_live: &HashSet<&str>,
        paths: &mut BTreeSet<CapturePath>,
        reasons: &mut HashMap<String, WholeRootCaptureReason>,
        call_span: &crate::token::Span,
    ) {
        if let ExprKind::Identifier(n) = &arg.kind {
            if pre_live.contains(n.as_str()) {
                paths.insert(CapturePath {
                    root: n.clone(),
                    projection: Vec::new(),
                });
                Self::record_whole_root_reason(
                    reasons,
                    n,
                    WholeRootCaptureReason::ByValuePass {
                        call_span: call_span.clone(),
                    },
                );
            }
        }
    }

    fn walk_capture_paths_block(
        block: &Block,
        pre_live: &HashSet<&str>,
        paths: &mut BTreeSet<CapturePath>,
        reasons: &mut HashMap<String, WholeRootCaptureReason>,
    ) {
        for stmt in &block.stmts {
            Self::walk_capture_paths_stmt(stmt, pre_live, paths, reasons);
        }
        if let Some(expr) = &block.final_expr {
            Self::walk_capture_paths_expr(expr, pre_live, paths, reasons);
        }
    }

    fn walk_capture_paths_stmt(
        stmt: &Stmt,
        pre_live: &HashSet<&str>,
        paths: &mut BTreeSet<CapturePath>,
        reasons: &mut HashMap<String, WholeRootCaptureReason>,
    ) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => {
                Self::walk_capture_paths_expr(value, pre_live, paths, reasons);
            }
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                Self::walk_capture_paths_expr(value, pre_live, paths, reasons);
                Self::walk_capture_paths_block(else_block, pre_live, paths, reasons);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                Self::walk_capture_paths_block(body, pre_live, paths, reasons);
            }
            // Assignment target: walked normally. A bare-identifier
            // target (`cfg = ...`) registers `(cfg, [])`; a field-chain
            // target (`cfg.field = ...`) registers the projection. The
            // distinction "is this a mutate or a read" is the per-name
            // walker's job (slice 2 will fold that into per-path mode
            // inference).
            StmtKind::Assign { target, value } => {
                Self::walk_capture_paths_expr(target, pre_live, paths, reasons);
                Self::walk_capture_paths_expr(value, pre_live, paths, reasons);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                Self::walk_capture_paths_expr(target, pre_live, paths, reasons);
                Self::walk_capture_paths_expr(value, pre_live, paths, reasons);
            }
            StmtKind::Expr(e) => Self::walk_capture_paths_expr(e, pre_live, paths, reasons),
        }
    }

    // ── Disjoint capture slice 2 — per-path mutation walker ─────────
    //
    // Walks the body a second time looking for *mutation events*:
    // assignment / compound-assign targets, `mut`-marker call/method
    // args, and `mut ref self` method-call receivers. For each event
    // we extract the target's `(root, projection)` if it is a place
    // expression rooted at a pre-live name, then mark every recorded
    // slice-1 path that *overlaps* the target as mutated. Overlap is
    // bidirectional: the recorded path's projection is a prefix of
    // the target's (writing a descendant of the recorded place
    // mutates the place), OR the target's projection is a prefix of
    // the recorded path's (writing an ancestor of the recorded place
    // invalidates the place). Both cases require `MutRef` access at
    // the closure boundary.
    //
    // The mode-inference layer at the Closure arm consumes the
    // returned set; consumption of the *whole root* (mode `Own`) is
    // detected separately at that wiring point via the main
    // ownership-checker's `states` map — it does not appear as a
    // mutation event here because the consume signal is determined
    // by post-walk binding state, not by AST shape alone.

    /// Walk `body` once and return the subset of `paths` whose
    /// places overlap any mutation event in the body. Used by the
    /// Closure arm's per-path mode inference (slice 2) to classify
    /// each capture path as `MutRef` (overlapping a mutation event)
    /// or `Ref` (not). Root-consume → `Own` is decided separately at
    /// the wiring point. Result preserves no ordering — the caller
    /// iterates `paths` in source order and probes membership.
    pub(crate) fn classify_capture_path_mutations(
        &self,
        body: &Expr,
        paths: &[CapturePath],
    ) -> HashSet<CapturePath> {
        let live: HashSet<&str> = paths.iter().map(|p| p.root.as_str()).collect();
        let mut mutated: HashSet<CapturePath> = HashSet::new();
        self.walk_capture_path_mutations_expr(body, &live, paths, &mut mutated);
        mutated
    }

    /// If `expr` is a place expression rooted at a pre-live name
    /// (chain of `FieldAccess` / `TupleIndex` from a captured
    /// `Identifier`), return its `(root, projection)` shape.
    /// Otherwise `None` — the caller's mutation event does not name a
    /// captured place. Index / method-call / deref are not handled
    /// here because slice 1's walker already commits those receivers
    /// as whole-root captures; the mutation walker classifies events
    /// targeting *pure* place expressions only.
    fn extract_target_place<'b>(
        expr: &'b Expr,
        pre_live: &HashSet<&str>,
    ) -> Option<(&'b str, Vec<String>)> {
        match &expr.kind {
            ExprKind::Identifier(n) if pre_live.contains(n.as_str()) => Some((n.as_str(), vec![])),
            ExprKind::FieldAccess { object, field } => {
                let (root, mut proj) = Self::extract_target_place(object, pre_live)?;
                proj.push(field.clone());
                Some((root, proj))
            }
            ExprKind::TupleIndex { object, index } => {
                let (root, mut proj) = Self::extract_target_place(object, pre_live)?;
                proj.push(index.to_string());
                Some((root, proj))
            }
            _ => None,
        }
    }

    /// Record any path in `paths` whose place overlaps the target
    /// place `(target_root, target_proj)` into `mutated`. Overlap is
    /// bidirectional: identical projections overlap, and one
    /// projection being a prefix of the other overlaps too (a write
    /// to an ancestor invalidates descendants; a write to a
    /// descendant mutates ancestors).
    fn mark_overlapping_paths(
        target_root: &str,
        target_proj: &[String],
        paths: &[CapturePath],
        mutated: &mut HashSet<CapturePath>,
    ) {
        for path in paths {
            if path.root != target_root {
                continue;
            }
            let shorter = path.projection.len().min(target_proj.len());
            if path.projection[..shorter] == target_proj[..shorter] {
                mutated.insert(path.clone());
            }
        }
    }

    /// Record a mutation event whose target is `expr`. If `expr`
    /// extracts as a pure captured place, mark overlapping paths.
    /// Otherwise the target is rooted off-capture (or behind a
    /// stopping construct like Index — slice 1 already committed the
    /// root whole, so any mutation through it is already covered by
    /// the receiver-walk arm below for method calls). No-op when no
    /// captured place is named.
    fn note_mutation_target(
        expr: &Expr,
        pre_live: &HashSet<&str>,
        paths: &[CapturePath],
        mutated: &mut HashSet<CapturePath>,
    ) {
        if let Some((root, proj)) = Self::extract_target_place(expr, pre_live) {
            Self::mark_overlapping_paths(root, &proj, paths, mutated);
        }
    }

    fn walk_capture_path_mutations_expr(
        &self,
        expr: &Expr,
        pre_live: &HashSet<&str>,
        paths: &[CapturePath],
        mutated: &mut HashSet<CapturePath>,
    ) {
        match &expr.kind {
            ExprKind::MethodCall { object, args, .. } => {
                // `mut ref self` method-call receiver mutates its
                // root. Slice 1's walker already committed the root
                // as whole-captured `(root, [])` at this site, so
                // marking that path as mutated lifts the whole-root
                // mode to MutRef.
                if self.method_call_receiver_is_mut_ref(expr) {
                    Self::note_mutation_target(object, pre_live, paths, mutated);
                }
                self.walk_capture_path_mutations_expr(object, pre_live, paths, mutated);
                for arg in args {
                    if arg.mut_marker {
                        Self::note_mutation_target(&arg.value, pre_live, paths, mutated);
                    }
                    self.walk_capture_path_mutations_expr(&arg.value, pre_live, paths, mutated);
                }
            }
            ExprKind::Call { callee, args } => {
                self.walk_capture_path_mutations_expr(callee, pre_live, paths, mutated);
                for arg in args {
                    if arg.mut_marker {
                        Self::note_mutation_target(&arg.value, pre_live, paths, mutated);
                    }
                    self.walk_capture_path_mutations_expr(&arg.value, pre_live, paths, mutated);
                }
            }
            ExprKind::Binary { left, right, .. } | ExprKind::NilCoalesce { left, right } => {
                self.walk_capture_path_mutations_expr(left, pre_live, paths, mutated);
                self.walk_capture_path_mutations_expr(right, pre_live, paths, mutated);
            }
            ExprKind::Unary { operand, .. } => {
                self.walk_capture_path_mutations_expr(operand, pre_live, paths, mutated);
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.walk_capture_path_mutations_expr(object, pre_live, paths, mutated);
            }
            ExprKind::Index { object, index } => {
                self.walk_capture_path_mutations_expr(object, pre_live, paths, mutated);
                self.walk_capture_path_mutations_expr(index, pre_live, paths, mutated);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.walk_capture_path_mutations_expr(condition, pre_live, paths, mutated);
                self.walk_capture_path_mutations_block(then_block, pre_live, paths, mutated);
                if let Some(eb) = else_branch {
                    self.walk_capture_path_mutations_expr(eb, pre_live, paths, mutated);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.walk_capture_path_mutations_expr(value, pre_live, paths, mutated);
                self.walk_capture_path_mutations_block(then_block, pre_live, paths, mutated);
                if let Some(eb) = else_branch {
                    self.walk_capture_path_mutations_expr(eb, pre_live, paths, mutated);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_capture_path_mutations_expr(scrutinee, pre_live, paths, mutated);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.walk_capture_path_mutations_expr(g, pre_live, paths, mutated);
                    }
                    self.walk_capture_path_mutations_expr(&arm.body, pre_live, paths, mutated);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.walk_capture_path_mutations_expr(condition, pre_live, paths, mutated);
                self.walk_capture_path_mutations_block(body, pre_live, paths, mutated);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.walk_capture_path_mutations_expr(value, pre_live, paths, mutated);
                self.walk_capture_path_mutations_block(body, pre_live, paths, mutated);
            }
            ExprKind::For { iterable, body, .. } => {
                self.walk_capture_path_mutations_expr(iterable, pre_live, paths, mutated);
                self.walk_capture_path_mutations_block(body, pre_live, paths, mutated);
            }
            ExprKind::Loop { body, .. } => {
                self.walk_capture_path_mutations_block(body, pre_live, paths, mutated);
            }
            ExprKind::Closure { body, .. } => {
                // Nested closure: a mutation of an outer capture
                // inside a nested closure still mutates the outer
                // capture from this closure's perspective.
                self.walk_capture_path_mutations_expr(body, pre_live, paths, mutated);
            }
            ExprKind::Block(block)
            | ExprKind::Unsafe(block)
            | ExprKind::Try(block)
            | ExprKind::Seq(block)
            | ExprKind::Par(block)
            | ExprKind::Lock { body: block, .. } => {
                self.walk_capture_path_mutations_block(block, pre_live, paths, mutated);
            }
            ExprKind::Question(inner)
            | ExprKind::OptionalChain { object: inner, .. }
            | ExprKind::Cast { expr: inner, .. } => {
                self.walk_capture_path_mutations_expr(inner, pre_live, paths, mutated);
            }
            ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
                for e in exprs {
                    self.walk_capture_path_mutations_expr(e, pre_live, paths, mutated);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.walk_capture_path_mutations_expr(e, pre_live, paths, mutated);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.walk_capture_path_mutations_expr(value, pre_live, paths, mutated);
                self.walk_capture_path_mutations_expr(count, pre_live, paths, mutated);
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    self.walk_capture_path_mutations_expr(k, pre_live, paths, mutated);
                    self.walk_capture_path_mutations_expr(v, pre_live, paths, mutated);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for field in fields {
                    self.walk_capture_path_mutations_expr(&field.value, pre_live, paths, mutated);
                }
                if let Some(s) = spread {
                    self.walk_capture_path_mutations_expr(s, pre_live, paths, mutated);
                }
            }
            ExprKind::Pipe { left, right } => {
                self.walk_capture_path_mutations_expr(left, pre_live, paths, mutated);
                self.walk_capture_path_mutations_expr(right, pre_live, paths, mutated);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.walk_capture_path_mutations_expr(s, pre_live, paths, mutated);
                }
                if let Some(e) = end {
                    self.walk_capture_path_mutations_expr(e, pre_live, paths, mutated);
                }
            }
            ExprKind::Return(Some(inner))
            | ExprKind::Break {
                value: Some(inner), ..
            } => {
                self.walk_capture_path_mutations_expr(inner, pre_live, paths, mutated);
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    self.walk_capture_path_mutations_expr(&b.value, pre_live, paths, mutated);
                }
                self.walk_capture_path_mutations_block(body, pre_live, paths, mutated);
            }
            _ => {}
        }
    }

    fn walk_capture_path_mutations_block(
        &self,
        block: &Block,
        pre_live: &HashSet<&str>,
        paths: &[CapturePath],
        mutated: &mut HashSet<CapturePath>,
    ) {
        for stmt in &block.stmts {
            self.walk_capture_path_mutations_stmt(stmt, pre_live, paths, mutated);
        }
        if let Some(expr) = &block.final_expr {
            self.walk_capture_path_mutations_expr(expr, pre_live, paths, mutated);
        }
    }

    fn walk_capture_path_mutations_stmt(
        &self,
        stmt: &Stmt,
        pre_live: &HashSet<&str>,
        paths: &[CapturePath],
        mutated: &mut HashSet<CapturePath>,
    ) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => {
                self.walk_capture_path_mutations_expr(value, pre_live, paths, mutated);
            }
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                self.walk_capture_path_mutations_expr(value, pre_live, paths, mutated);
                self.walk_capture_path_mutations_block(else_block, pre_live, paths, mutated);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.walk_capture_path_mutations_block(body, pre_live, paths, mutated);
            }
            StmtKind::Assign { target, value } => {
                Self::note_mutation_target(target, pre_live, paths, mutated);
                self.walk_capture_path_mutations_expr(target, pre_live, paths, mutated);
                self.walk_capture_path_mutations_expr(value, pre_live, paths, mutated);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                Self::note_mutation_target(target, pre_live, paths, mutated);
                self.walk_capture_path_mutations_expr(target, pre_live, paths, mutated);
                self.walk_capture_path_mutations_expr(value, pre_live, paths, mutated);
            }
            StmtKind::Expr(e) => {
                self.walk_capture_path_mutations_expr(e, pre_live, paths, mutated);
            }
        }
    }
}
