//! Unary / binary / short-circuit / pipe operator evaluation.
//!
//! Houses `eval_unary` (`-x`, `!b`, `~i`), `eval_short_circuit` (`and`
//! / `or` with documented RHS-short-circuit semantics — design.md /
//! roadmap.md), `eval_binary` (the big op-dispatch table for arithmetic
//! / comparison / bitwise / string / shift ops with checked-arithmetic
//! overflow trapping), `eval_pipe` (`a |> f` / `a |> f(args)` /
//! `a |> f(_, args)` desugaring into a synthesized Call), and the
//! shared `record_integer_overflow` helper.
//!
//! Lives in a sibling `impl<'a> super::Interpreter<'a>` block.

use crate::ast::*;
use crate::token::Span;

use super::value::Value;

impl<'a> super::Interpreter<'a> {
    // ── Operators ───────────────────────────────────────────────

    pub(crate) fn eval_unary(&mut self, op: &UnaryOp, operand: Value, span: &Span) -> Value {
        match (op, operand) {
            (UnaryOp::Neg, Value::Int(i)) => Value::Int(match i.checked_neg() {
                Some(v) => v,
                None => return self.record_integer_overflow(span),
            }),
            (UnaryOp::Neg, Value::Float(f)) => Value::Float(-f),
            (UnaryOp::Not, Value::Bool(b)) => Value::Bool(!b),
            (UnaryOp::BitNot, Value::Int(i)) => Value::Int(!i),
            // In the tree-walk interpreter references are passed by value; `*r` is
            // a semantic no-op that returns the underlying value unchanged.
            (UnaryOp::Deref, v) => v,
            _ => unreachable!(
                "type mismatch in unary operation at {}:{}; should be caught by typechecker",
                span.line, span.column
            ),
        }
    }

    /// Evaluate `lhs and rhs` / `lhs or rhs` with short-circuit
    /// semantics — RHS is only evaluated when the LHS doesn't already
    /// determine the result, so RHS side-effects (panicking index,
    /// dropped fn call) don't fire when short-circuited.
    pub(crate) fn eval_short_circuit(
        &mut self,
        op: &BinOp,
        left: &Expr,
        right: &Expr,
        span: &Span,
    ) -> Value {
        let lhs = match self.eval_expr_inner(left) {
            Value::Bool(b) => b,
            _ => unreachable!(
                "type mismatch in `{:?}` LHS at {}:{}; should be caught by typechecker",
                op, span.line, span.column
            ),
        };
        match (op, lhs) {
            (BinOp::And, false) => Value::Bool(false),
            (BinOp::Or, true) => Value::Bool(true),
            (BinOp::And, true) | (BinOp::Or, false) => self.eval_expr_inner(right),
            _ => unreachable!("eval_short_circuit only handles And/Or"),
        }
    }

    pub(crate) fn eval_binary(
        &mut self,
        op: &BinOp,
        left: Value,
        right: Value,
        span: &Span,
    ) -> Value {
        match (op, left, right) {
            // Arithmetic (Int)
            (BinOp::Add, Value::Int(a), Value::Int(b)) => Value::Int(match a.checked_add(b) {
                Some(v) => v,
                None => return self.record_integer_overflow(span),
            }),
            (BinOp::Sub, Value::Int(a), Value::Int(b)) => Value::Int(match a.checked_sub(b) {
                Some(v) => v,
                None => return self.record_integer_overflow(span),
            }),
            (BinOp::Mul, Value::Int(a), Value::Int(b)) => Value::Int(match a.checked_mul(b) {
                Some(v) => v,
                None => return self.record_integer_overflow(span),
            }),
            (BinOp::Div, Value::Int(a), Value::Int(b)) => {
                if b == 0 {
                    return self.record_runtime_error("division by zero", span);
                }
                Value::Int(match a.checked_div(b) {
                    Some(v) => v,
                    None => return self.record_integer_overflow(span),
                })
            }
            (BinOp::Mod, Value::Int(a), Value::Int(b)) => {
                if b == 0 {
                    return self.record_runtime_error("division by zero", span);
                }
                Value::Int(match a.checked_rem(b) {
                    Some(v) => v,
                    None => return self.record_integer_overflow(span),
                })
            }

            // Arithmetic (Float)
            (BinOp::Add, Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (BinOp::Sub, Value::Float(a), Value::Float(b)) => Value::Float(a - b),
            (BinOp::Mul, Value::Float(a), Value::Float(b)) => Value::Float(a * b),
            (BinOp::Div, Value::Float(a), Value::Float(b)) => Value::Float(a / b),
            (BinOp::Mod, Value::Float(a), Value::Float(b)) => Value::Float(a % b),

            // String Concatenation
            (BinOp::Add, Value::String(a), Value::String(b)) => Value::String(a + &b),

            // Comparison (Int)
            (BinOp::Eq, Value::Int(a), Value::Int(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::Int(a), Value::Int(b)) => Value::Bool(a != b),
            (BinOp::Lt, Value::Int(a), Value::Int(b)) => Value::Bool(a < b),
            (BinOp::LtEq, Value::Int(a), Value::Int(b)) => Value::Bool(a <= b),
            (BinOp::Gt, Value::Int(a), Value::Int(b)) => Value::Bool(a > b),
            (BinOp::GtEq, Value::Int(a), Value::Int(b)) => Value::Bool(a >= b),

            // Comparison (Float) - IEEE 754: NaN != NaN
            (BinOp::Eq, Value::Float(a), Value::Float(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::Float(a), Value::Float(b)) => Value::Bool(a != b),
            (BinOp::Lt, Value::Float(a), Value::Float(b)) => Value::Bool(a < b),
            (BinOp::LtEq, Value::Float(a), Value::Float(b)) => Value::Bool(a <= b),
            (BinOp::Gt, Value::Float(a), Value::Float(b)) => Value::Bool(a > b),
            (BinOp::GtEq, Value::Float(a), Value::Float(b)) => Value::Bool(a >= b),

            // Comparison (TotalFloat) - total order: NaN == NaN, NaN sorts last
            (BinOp::Eq, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(a.total_cmp(&b).is_eq())
            }
            (BinOp::NotEq, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(!a.total_cmp(&b).is_eq())
            }
            (BinOp::Lt, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(a.total_cmp(&b).is_lt())
            }
            (BinOp::LtEq, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(!a.total_cmp(&b).is_gt())
            }
            (BinOp::Gt, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(a.total_cmp(&b).is_gt())
            }
            (BinOp::GtEq, Value::TotalFloat64(a), Value::TotalFloat64(b)) => {
                Value::Bool(!a.total_cmp(&b).is_lt())
            }
            (BinOp::Eq, Value::TotalFloat32(a), Value::TotalFloat32(b)) => {
                Value::Bool(a.total_cmp(&b).is_eq())
            }
            (BinOp::NotEq, Value::TotalFloat32(a), Value::TotalFloat32(b)) => {
                Value::Bool(!a.total_cmp(&b).is_eq())
            }

            // Comparison (String) — lexicographic via Rust's `Ord for String`.
            // Matches the typechecker's builtin Ord registration for `String`
            // (see `register_builtin_impl("Ord", "String", ...)`).
            (BinOp::Eq, Value::String(a), Value::String(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::String(a), Value::String(b)) => Value::Bool(a != b),
            (BinOp::Lt, Value::String(a), Value::String(b)) => Value::Bool(a < b),
            (BinOp::LtEq, Value::String(a), Value::String(b)) => Value::Bool(a <= b),
            (BinOp::Gt, Value::String(a), Value::String(b)) => Value::Bool(a > b),
            (BinOp::GtEq, Value::String(a), Value::String(b)) => Value::Bool(a >= b),

            // Comparison (Char) — codepoint order via Rust's `Ord for char`.
            // Matches the typechecker's builtin Ord registration for `char`.
            (BinOp::Eq, Value::Char(a), Value::Char(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::Char(a), Value::Char(b)) => Value::Bool(a != b),
            (BinOp::Lt, Value::Char(a), Value::Char(b)) => Value::Bool(a < b),
            (BinOp::LtEq, Value::Char(a), Value::Char(b)) => Value::Bool(a <= b),
            (BinOp::Gt, Value::Char(a), Value::Char(b)) => Value::Bool(a > b),
            (BinOp::GtEq, Value::Char(a), Value::Char(b)) => Value::Bool(a >= b),

            // Logical (Bool)
            (BinOp::And, Value::Bool(a), Value::Bool(b)) => Value::Bool(a && b),
            (BinOp::Or, Value::Bool(a), Value::Bool(b)) => Value::Bool(a || b),
            (BinOp::Eq, Value::Bool(a), Value::Bool(b)) => Value::Bool(a == b),
            (BinOp::NotEq, Value::Bool(a), Value::Bool(b)) => Value::Bool(a != b),
            (BinOp::Lt, Value::Bool(a), Value::Bool(b)) => Value::Bool(!a & b),
            (BinOp::LtEq, Value::Bool(a), Value::Bool(b)) => Value::Bool(a <= b),
            (BinOp::Gt, Value::Bool(a), Value::Bool(b)) => Value::Bool(a & !b),
            (BinOp::GtEq, Value::Bool(a), Value::Bool(b)) => Value::Bool(a >= b),

            // Bitwise (Int)
            (BinOp::BitAnd, Value::Int(a), Value::Int(b)) => Value::Int(a & b),
            (BinOp::BitOr, Value::Int(a), Value::Int(b)) => Value::Int(a | b),
            (BinOp::BitXor, Value::Int(a), Value::Int(b)) => Value::Int(a ^ b),
            (BinOp::Shl, Value::Int(a), Value::Int(b)) => Value::Int(a << b),
            (BinOp::Shr, Value::Int(a), Value::Int(b)) => Value::Int(a >> b),

            _ => unreachable!(
                "type mismatch in binary operation {:?} at {}:{}; should be caught by typechecker",
                op, span.line, span.column
            ),
        }
    }

    pub(crate) fn eval_pipe(&mut self, left: &Expr, right: &Expr) -> Value {
        match &right.kind {
            // a |> f => f(a)
            ExprKind::Identifier(_) | ExprKind::Path { .. } => {
                let desugared = Expr {
                    span: right.span.clone(),
                    kind: ExprKind::Call {
                        callee: Box::new(right.clone()),
                        args: vec![CallArg {
                            label: None,
                            mut_marker: false,
                            value: left.clone(),
                            span: left.span.clone(),
                        }],
                    },
                };
                self.eval_expr_inner(&desugared)
            }

            // a |> f(args...) => f(a, args...) or f(args with _ replaced)
            ExprKind::Call { callee, args } => {
                let has_placeholder = args
                    .iter()
                    .any(|arg| matches!(arg.value.kind, ExprKind::PipePlaceholder));

                let desugared_args: Vec<CallArg> = if has_placeholder {
                    args.iter()
                        .map(|arg| {
                            if matches!(arg.value.kind, ExprKind::PipePlaceholder) {
                                CallArg {
                                    label: arg.label.clone(),
                                    mut_marker: false,
                                    value: left.clone(),
                                    span: left.span.clone(),
                                }
                            } else {
                                arg.clone()
                            }
                        })
                        .collect()
                } else {
                    let mut new_args = vec![CallArg {
                        label: None,
                        mut_marker: false,
                        value: left.clone(),
                        span: left.span.clone(),
                    }];
                    new_args.extend(args.iter().cloned());
                    new_args
                };

                let desugared = Expr {
                    span: right.span.clone(),
                    kind: ExprKind::Call {
                        callee: callee.clone(),
                        args: desugared_args,
                    },
                };
                self.eval_expr_inner(&desugared)
            }

            _ => unreachable!(
                "invalid pipe right-hand side at {}:{}; should be caught by parser/typechecker",
                right.span.line, right.span.column
            ),
        }
    }

    fn record_integer_overflow(&mut self, span: &Span) -> Value {
        self.record_runtime_error("integer overflow", span)
    }
}
