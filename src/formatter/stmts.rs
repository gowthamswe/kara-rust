//! Statement and block printing for the canonical formatter.

use crate::ast::*;

impl super::Formatter {
    pub(super) fn format_block(&mut self, block: &Block) {
        if block.stmts.is_empty() && block.final_expr.is_none() {
            self.write_str("{}");
            return;
        }
        self.write_str("{\n");
        self.push_indent();
        for stmt in &block.stmts {
            self.format_stmt(stmt);
        }
        if let Some(ref expr) = block.final_expr {
            self.write_indent();
            self.format_expr(expr);
            self.output.push('\n');
        }
        self.pop_indent();
        self.write_indent();
        self.write_str("}");
    }

    // ── Statements ──────────────────────────────────────────────

    pub(super) fn format_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let {
                is_mut,
                pattern,
                ty,
                value,
            } => {
                self.write_indent();
                self.write_str("let ");
                if *is_mut {
                    self.write_str("mut ");
                }
                self.format_pattern(pattern);
                if let Some(ref t) = ty {
                    self.write_str(": ");
                    self.format_type_expr(t);
                }
                self.write_str(" = ");
                self.format_expr(value);
                self.write_str(";\n");
            }
            StmtKind::LetUninit {
                is_mut, name, ty, ..
            } => {
                self.write_indent();
                self.write_str("let ");
                if *is_mut {
                    self.write_str("mut ");
                }
                self.write_ident(name);
                self.write_str(": ");
                self.format_type_expr(ty);
                self.write_str(";\n");
            }
            StmtKind::LetElse {
                pattern,
                ty,
                value,
                else_block,
            } => {
                self.write_indent();
                self.write_str("let ");
                self.format_pattern(pattern);
                if let Some(ref t) = ty {
                    self.write_str(": ");
                    self.format_type_expr(t);
                }
                self.write_str(" = ");
                self.format_expr(value);
                self.write_str(" else ");
                self.format_block(else_block);
                self.write_str(";\n");
            }
            StmtKind::Defer { body } => {
                self.write_indent();
                self.write_str("defer ");
                self.format_block(body);
                self.output.push('\n');
            }
            StmtKind::ErrDefer { binding, body } => {
                self.write_indent();
                self.write_str("errdefer");
                if let Some(ref b) = binding {
                    self.write_str("(");
                    self.write_ident(b);
                    self.write_str(")");
                }
                self.write_str(" ");
                self.format_block(body);
                self.output.push('\n');
            }
            StmtKind::Assign { target, value } => {
                self.write_indent();
                self.format_expr(target);
                self.write_str(" = ");
                self.format_expr(value);
                self.write_str(";\n");
            }
            StmtKind::CompoundAssign { target, op, value } => {
                self.write_indent();
                self.format_expr(target);
                self.write_str(match op {
                    CompoundOp::Add => " += ",
                    CompoundOp::Sub => " -= ",
                    CompoundOp::Mul => " *= ",
                    CompoundOp::Div => " /= ",
                    CompoundOp::Mod => " %= ",
                    CompoundOp::BitAnd => " &= ",
                    CompoundOp::BitOr => " |= ",
                    CompoundOp::BitXor => " ^= ",
                    CompoundOp::Shl => " <<= ",
                    CompoundOp::Shr => " >>= ",
                });
                self.format_expr(value);
                self.write_str(";\n");
            }
            StmtKind::Expr(expr) => {
                self.write_indent();
                self.format_expr(expr);
                self.write_str(";\n");
            }
        }
    }
}
