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
//! two analyses with different inputs and stopping rules. Slice 2
//! will run mode inference per path on the output of this walker;
//! slice 3 will pass the path set to the borrow checker.
//!
//! Lives in a sibling `impl<'a> super::OwnershipChecker<'a>` block.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::ast::*;

use super::{CaptureBodyUsage, CapturePath};

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
    /// name. Output is sorted lexicographically by `(root, projection)`
    /// for deterministic test pins.
    pub(crate) fn classify_capture_body_paths(
        &self,
        body: &Expr,
        pre_live: &[String],
    ) -> Vec<CapturePath> {
        let live: HashSet<&str> = pre_live.iter().map(String::as_str).collect();
        let mut paths: BTreeSet<CapturePath> = BTreeSet::new();
        Self::walk_capture_paths_expr(body, &live, &mut paths);
        paths.into_iter().collect()
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
    ) {
        // A pure place expression rooted at a pre-live name — register
        // the projection chain and stop. The chain has no sub-
        // expressions to recurse into beyond the (already-walked) root
        // identifier.
        if let Some(p) = Self::extract_pure_path(expr, pre_live) {
            paths.insert(p);
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
                Self::walk_capture_paths_expr(object, pre_live, paths);
            }
            // Stopping construct: index. If the indexed expression is
            // rooted at a pre-live name, the root is captured whole;
            // the index expression itself is walked normally for
            // nested captures.
            ExprKind::Index { object, index } => {
                if let Some(root) = Self::place_root_for_capture(object, pre_live) {
                    paths.insert(CapturePath {
                        root,
                        projection: Vec::new(),
                    });
                } else {
                    Self::walk_capture_paths_expr(object, pre_live, paths);
                }
                Self::walk_capture_paths_expr(index, pre_live, paths);
            }
            // Stopping construct: method call. The receiver, if rooted
            // at a pre-live name, captures the root whole. Args are
            // walked normally — each may itself capture a different
            // path under the same or a different root.
            ExprKind::MethodCall { object, args, .. } => {
                if let Some(root) = Self::place_root_for_capture(object, pre_live) {
                    paths.insert(CapturePath {
                        root,
                        projection: Vec::new(),
                    });
                } else {
                    Self::walk_capture_paths_expr(object, pre_live, paths);
                }
                for arg in args {
                    Self::walk_capture_paths_expr(&arg.value, pre_live, paths);
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
                        root,
                        projection: Vec::new(),
                    });
                } else {
                    Self::walk_capture_paths_expr(operand, pre_live, paths);
                }
            }
            ExprKind::Unary { operand, .. } => {
                Self::walk_capture_paths_expr(operand, pre_live, paths);
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
            // mode pass.
            ExprKind::Call { callee, args } => {
                Self::walk_capture_paths_expr(callee, pre_live, paths);
                for arg in args {
                    Self::walk_capture_paths_expr(&arg.value, pre_live, paths);
                }
            }
            ExprKind::Binary { left, right, .. } | ExprKind::NilCoalesce { left, right } => {
                Self::walk_capture_paths_expr(left, pre_live, paths);
                Self::walk_capture_paths_expr(right, pre_live, paths);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                Self::walk_capture_paths_expr(condition, pre_live, paths);
                Self::walk_capture_paths_block(then_block, pre_live, paths);
                if let Some(eb) = else_branch {
                    Self::walk_capture_paths_expr(eb, pre_live, paths);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                Self::walk_capture_paths_expr(value, pre_live, paths);
                Self::walk_capture_paths_block(then_block, pre_live, paths);
                if let Some(eb) = else_branch {
                    Self::walk_capture_paths_expr(eb, pre_live, paths);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                Self::walk_capture_paths_expr(scrutinee, pre_live, paths);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        Self::walk_capture_paths_expr(g, pre_live, paths);
                    }
                    Self::walk_capture_paths_expr(&arm.body, pre_live, paths);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                Self::walk_capture_paths_expr(condition, pre_live, paths);
                Self::walk_capture_paths_block(body, pre_live, paths);
            }
            ExprKind::WhileLet { value, body, .. } => {
                Self::walk_capture_paths_expr(value, pre_live, paths);
                Self::walk_capture_paths_block(body, pre_live, paths);
            }
            ExprKind::For { iterable, body, .. } => {
                Self::walk_capture_paths_expr(iterable, pre_live, paths);
                Self::walk_capture_paths_block(body, pre_live, paths);
            }
            ExprKind::Loop { body, .. } => {
                Self::walk_capture_paths_block(body, pre_live, paths);
            }
            ExprKind::Closure { body, .. } => {
                // Recurse into nested closure bodies — captures of an
                // outer-outer binding by an inner closure still appear
                // as captures of this closure (it must capture the
                // outer binding to make it available to the inner one).
                Self::walk_capture_paths_expr(body, pre_live, paths);
            }
            ExprKind::Block(block)
            | ExprKind::Unsafe(block)
            | ExprKind::Try(block)
            | ExprKind::Seq(block)
            | ExprKind::Par(block)
            | ExprKind::Lock { body: block, .. } => {
                Self::walk_capture_paths_block(block, pre_live, paths);
            }
            ExprKind::Question(inner)
            | ExprKind::OptionalChain { object: inner, .. }
            | ExprKind::Cast { expr: inner, .. } => {
                Self::walk_capture_paths_expr(inner, pre_live, paths);
            }
            ExprKind::Tuple(exprs) | ExprKind::ArrayLiteral(exprs) => {
                for e in exprs {
                    Self::walk_capture_paths_expr(e, pre_live, paths);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    Self::walk_capture_paths_expr(e, pre_live, paths);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                Self::walk_capture_paths_expr(value, pre_live, paths);
                Self::walk_capture_paths_expr(count, pre_live, paths);
            }
            ExprKind::MapLiteral(entries) => {
                for (k, v) in entries {
                    Self::walk_capture_paths_expr(k, pre_live, paths);
                    Self::walk_capture_paths_expr(v, pre_live, paths);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for field in fields {
                    Self::walk_capture_paths_expr(&field.value, pre_live, paths);
                }
                if let Some(s) = spread {
                    Self::walk_capture_paths_expr(s, pre_live, paths);
                }
            }
            ExprKind::Pipe { left, right } => {
                Self::walk_capture_paths_expr(left, pre_live, paths);
                Self::walk_capture_paths_expr(right, pre_live, paths);
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    Self::walk_capture_paths_expr(s, pre_live, paths);
                }
                if let Some(e) = end {
                    Self::walk_capture_paths_expr(e, pre_live, paths);
                }
            }
            ExprKind::Return(Some(inner))
            | ExprKind::Break {
                value: Some(inner), ..
            } => {
                Self::walk_capture_paths_expr(inner, pre_live, paths);
            }
            ExprKind::Providers { bindings, body } => {
                for b in bindings {
                    Self::walk_capture_paths_expr(&b.value, pre_live, paths);
                }
                Self::walk_capture_paths_block(body, pre_live, paths);
            }
            // Identifier handled by `extract_pure_path` above; any leaf
            // identifier whose name isn't in `pre_live` is not a
            // capture and produces no path. Other leaves and
            // unhandled forms have no sub-expressions that could
            // reference captures.
            _ => {}
        }
    }

    fn walk_capture_paths_block(
        block: &Block,
        pre_live: &HashSet<&str>,
        paths: &mut BTreeSet<CapturePath>,
    ) {
        for stmt in &block.stmts {
            Self::walk_capture_paths_stmt(stmt, pre_live, paths);
        }
        if let Some(expr) = &block.final_expr {
            Self::walk_capture_paths_expr(expr, pre_live, paths);
        }
    }

    fn walk_capture_paths_stmt(
        stmt: &Stmt,
        pre_live: &HashSet<&str>,
        paths: &mut BTreeSet<CapturePath>,
    ) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => {
                Self::walk_capture_paths_expr(value, pre_live, paths);
            }
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                Self::walk_capture_paths_expr(value, pre_live, paths);
                Self::walk_capture_paths_block(else_block, pre_live, paths);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                Self::walk_capture_paths_block(body, pre_live, paths);
            }
            // Assignment target: walked normally. A bare-identifier
            // target (`cfg = ...`) registers `(cfg, [])`; a field-chain
            // target (`cfg.field = ...`) registers the projection. The
            // distinction "is this a mutate or a read" is the per-name
            // walker's job (slice 2 will fold that into per-path mode
            // inference).
            StmtKind::Assign { target, value } => {
                Self::walk_capture_paths_expr(target, pre_live, paths);
                Self::walk_capture_paths_expr(value, pre_live, paths);
            }
            StmtKind::CompoundAssign { target, value, .. } => {
                Self::walk_capture_paths_expr(target, pre_live, paths);
                Self::walk_capture_paths_expr(value, pre_live, paths);
            }
            StmtKind::Expr(e) => Self::walk_capture_paths_expr(e, pre_live, paths),
        }
    }
}
