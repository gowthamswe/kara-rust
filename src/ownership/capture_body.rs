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
//! Lives in a sibling `impl<'a> super::OwnershipChecker<'a>` block.

use std::collections::HashMap;

use crate::ast::*;

use super::CaptureBodyUsage;

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
}
