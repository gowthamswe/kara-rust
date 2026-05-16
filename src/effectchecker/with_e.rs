//! `with E` (named effect-variable) unification check.
//!
//! Round 10.3 (named-effect-variable unification): when a function
//! signature reuses the same `E` across multiple `Fn(...) with E`
//! parameter slots, every closure argument's actual effect set must
//! agree. A variable that appears in only one slot adds no
//! constraint (it behaves like `with _`); a variable at 2+ slots
//! requires every closure argument's effect set to be equal, with
//! a structured E0411 diagnostic on mismatch.
//!
//! Houses `check_with_e_unification` (the driver) and the three-way
//! body walker (`check_with_e_in_block`, `check_with_e_in_stmt`,
//! `check_with_e_in_expr`) plus the per-call unification check
//! (`check_call_with_e_unification`).
//!
//! Lives in a sibling `impl<'a> super::EffectChecker<'a>` block.

use std::collections::HashSet;

use crate::ast::*;

use super::{verb_name, Effect, EffectError, EffectErrorKind, EffectOrigin, EffectSet};

impl<'a> super::EffectChecker<'a> {
    ///
    /// `with _` slots are not in `fn_effect_var_positions` (they're not named),
    /// so they remain independent — a function with two `with _` slots gets
    /// no cross-slot constraint, exactly as today.
    pub(crate) fn check_with_e_unification(&mut self) {
        let bodies: Vec<Block> = self
            .function_bodies
            .values()
            .map(|f| f.body.clone())
            .chain(self.method_bodies.values().map(|f| f.body.clone()))
            .collect();
        for body in bodies {
            self.check_with_e_in_block(&body);
        }
    }

    fn check_with_e_in_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.check_with_e_in_stmt(stmt);
        }
        if let Some(expr) = &block.final_expr {
            self.check_with_e_in_expr(expr);
        }
    }

    fn check_with_e_in_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => self.check_with_e_in_expr(value),
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                self.check_with_e_in_expr(value);
                self.check_with_e_in_block(else_block);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.check_with_e_in_block(body);
            }
            StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
                self.check_with_e_in_expr(target);
                self.check_with_e_in_expr(value);
            }
            StmtKind::Expr(expr) => self.check_with_e_in_expr(expr),
        }
    }

    fn check_with_e_in_expr(&mut self, expr: &Expr) {
        if let ExprKind::Call { callee, args } = &expr.kind {
            if let Some(cname) = self.extract_callee_name(callee) {
                self.check_call_with_e_unification(&cname, args);
            }
            self.check_with_e_in_expr(callee);
            for a in args {
                self.check_with_e_in_expr(&a.value);
            }
            return;
        }
        // Generic structural recursion for everything else.
        match &expr.kind {
            ExprKind::MethodCall { object, args, .. } => {
                // Mirror the `Call` branch: resolve to `Type.method` via the
                // typechecker side-table and run the same `with E` unification
                // pass. The callee's `params` are the explicit (non-self)
                // parameters, so `args` indices align 1:1 with the indices
                // recorded in `fn_effect_var_positions`.
                if let Some(callee_key) = self.resolve_method_callee_key(&expr.span) {
                    self.check_call_with_e_unification(&callee_key, args);
                }
                self.check_with_e_in_expr(object);
                for a in args {
                    self.check_with_e_in_expr(&a.value);
                }
            }
            ExprKind::Binary { left, right, .. } | ExprKind::Pipe { left, right } => {
                self.check_with_e_in_expr(left);
                self.check_with_e_in_expr(right);
            }
            ExprKind::NilCoalesce { left, right } => {
                self.check_with_e_in_expr(left);
                self.check_with_e_in_expr(right);
            }
            ExprKind::Unary { operand, .. } | ExprKind::Question(operand) => {
                self.check_with_e_in_expr(operand);
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.check_with_e_in_expr(object);
            }
            ExprKind::Index { object, index } => {
                self.check_with_e_in_expr(object);
                self.check_with_e_in_expr(index);
            }
            ExprKind::Tuple(es) | ExprKind::ArrayLiteral(es) => {
                for e in es {
                    self.check_with_e_in_expr(e);
                }
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.check_with_e_in_expr(e);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.check_with_e_in_expr(value);
                self.check_with_e_in_expr(count);
            }
            ExprKind::MapLiteral(pairs) => {
                for (k, v) in pairs {
                    self.check_with_e_in_expr(k);
                    self.check_with_e_in_expr(v);
                }
            }
            ExprKind::Block(b)
            | ExprKind::Unsafe(b)
            | ExprKind::Try(b)
            | ExprKind::Seq(b)
            | ExprKind::Par(b) => {
                self.check_with_e_in_block(b);
            }
            ExprKind::Lock { body, .. } => self.check_with_e_in_block(body),
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.check_with_e_in_expr(condition);
                self.check_with_e_in_block(then_block);
                if let Some(e) = else_branch {
                    self.check_with_e_in_expr(e);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.check_with_e_in_expr(value);
                self.check_with_e_in_block(then_block);
                if let Some(e) = else_branch {
                    self.check_with_e_in_expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.check_with_e_in_expr(scrutinee);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.check_with_e_in_expr(g);
                    }
                    self.check_with_e_in_expr(&arm.body);
                }
            }
            ExprKind::While {
                condition, body, ..
            } => {
                self.check_with_e_in_expr(condition);
                self.check_with_e_in_block(body);
            }
            ExprKind::WhileLet { value, body, .. } => {
                self.check_with_e_in_expr(value);
                self.check_with_e_in_block(body);
            }
            ExprKind::For { iterable, body, .. } => {
                self.check_with_e_in_expr(iterable);
                self.check_with_e_in_block(body);
            }
            ExprKind::Loop { body, .. } => self.check_with_e_in_block(body),
            ExprKind::Closure { body, .. } => self.check_with_e_in_expr(body),
            ExprKind::Return(Some(e))
            | ExprKind::Break { value: Some(e), .. }
            | ExprKind::Cast { expr: e, .. } => self.check_with_e_in_expr(e),
            ExprKind::OptionalChain { object, args, .. } => {
                self.check_with_e_in_expr(object);
                if let Some(args) = args {
                    for a in args {
                        self.check_with_e_in_expr(&a.value);
                    }
                }
            }
            _ => {}
        }
    }

    fn check_call_with_e_unification(&mut self, callee_name: &str, args: &[CallArg]) {
        let positions = match self.fn_effect_var_positions.get(callee_name).cloned() {
            Some(p) => p,
            None => return,
        };
        for (var_name, indices) in &positions {
            // Single position → no cross-position unification needed.
            if indices.len() < 2 {
                continue;
            }
            let mut binding: Option<(usize, EffectSet)> = None;
            for &idx in indices {
                let Some(arg) = args.get(idx) else { continue };
                let arg_effects = self.get_arg_effects(&arg.value);
                let arg_set = arg_effects.effects.iter().map(|te| te.effect.clone());
                let arg_concrete: HashSet<Effect> = arg_set
                    .filter(|e| !self.is_transparent_verb(&e.verb))
                    .collect();
                match &binding {
                    None => {
                        let mut seed = EffectSet::new();
                        for e in &arg_concrete {
                            seed.add(e.clone(), EffectOrigin::Direct(arg.value.span.clone()));
                        }
                        binding = Some((idx, seed));
                    }
                    Some((first_idx, first_set)) => {
                        let first_concrete: HashSet<Effect> = first_set
                            .effects
                            .iter()
                            .map(|te| te.effect.clone())
                            .filter(|e| !self.is_transparent_verb(&e.verb))
                            .collect();
                        if first_concrete != arg_concrete {
                            let render = |s: &HashSet<Effect>| -> String {
                                let mut parts: Vec<String> = s
                                    .iter()
                                    .map(|e| format!("{}({})", verb_name(&e.verb), e.resource))
                                    .collect();
                                parts.sort();
                                if parts.is_empty() {
                                    "{}".to_string()
                                } else {
                                    format!("{{{}}}", parts.join(", "))
                                }
                            };
                            self.errors.push(EffectError {
                                message: format!(
                                    "effect variable `{}` is bound to {} at argument {} but \
                                     {} at argument {}; `with {}` requires every slot to agree",
                                    var_name,
                                    render(&first_concrete),
                                    first_idx,
                                    render(&arg_concrete),
                                    idx,
                                    var_name,
                                ),
                                span: arg.value.span.clone(),
                                kind: EffectErrorKind::EffectVariableConflict,
                                subtype_trace: None,
                            });
                        }
                    }
                }
            }
        }
    }
}
