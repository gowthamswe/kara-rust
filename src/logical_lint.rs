// src/logical_lint.rs
//! `ambiguous_not_comparison` lint: warn when `not` is adjacent to a comparison
//! operator. `not x == y` parses as `(not x) == y` because `not` binds tighter
//! than `==`/`!=`/`<`/`<=`/`>`/`>=` — same precedence relationship that `!`
//! has with comparison in C-family languages. The natural-English reading
//! suggests `not (x == y)`, so the lint flags every comparison whose left or
//! right operand is a `not` expression and asks the writer to disambiguate
//! with explicit parentheses.
//!
//! The AST does not preserve parentheses, so this lint cannot distinguish
//! `not x == y` (ambiguous source) from `(not x) == y` (explicitly grouped
//! source). The false-positive rate is acceptable because `(not x) cmp y` is
//! virtually never written intentionally — it only makes sense when `x` is a
//! boolean being compared to another boolean, which is awkward and clearer
//! as `x != y` or similar. To silence the lint, write `not (x == y)`: the
//! parens force the comparison to bind first, producing
//! `Unary(Not, Binary(Eq, x, y))` — no comparison adjacent to a `not` at the
//! AST level.

use crate::ast::{Block, Expr, ExprKind, Item, MatchArm, Program, Stmt, StmtKind, UnaryOp};
use crate::token::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintLevel {
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct LintDiagnostic {
    pub level: LintLevel,
    pub span: Span,
    pub message: String,
    pub lint_name: String,
}

pub fn check_ambiguous_not_comparison(program: &Program) -> Vec<LintDiagnostic> {
    let mut diags = Vec::new();
    for item in &program.items {
        walk_item(item, &mut diags);
    }
    diags
}

fn walk_item(item: &Item, diags: &mut Vec<LintDiagnostic>) {
    match item {
        Item::Function(f) => walk_block(&f.body, diags),
        Item::ImplBlock(imp) => {
            for it in &imp.items {
                if let crate::ast::ImplItem::Method(m) = it {
                    walk_block(&m.body, diags);
                }
            }
        }
        _ => {}
    }
}

fn walk_block(block: &Block, diags: &mut Vec<LintDiagnostic>) {
    for stmt in &block.stmts {
        walk_stmt(stmt, diags);
    }
    if let Some(tail) = &block.final_expr {
        walk_expr(tail, diags);
    }
}

fn walk_stmt(stmt: &Stmt, diags: &mut Vec<LintDiagnostic>) {
    match &stmt.kind {
        StmtKind::Let { value, .. } => walk_expr(value, diags),
        StmtKind::LetUninit { .. } => {}
        StmtKind::LetElse {
            value, else_block, ..
        } => {
            walk_expr(value, diags);
            walk_block(else_block, diags);
        }
        StmtKind::Expr(e) => walk_expr(e, diags),
        StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
            walk_expr(target, diags);
            walk_expr(value, diags);
        }
        StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => walk_block(body, diags),
    }
}

fn is_comparison(op: &crate::ast::BinOp) -> bool {
    use crate::ast::BinOp;
    matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
    )
}

fn is_not_unary(expr: &Expr) -> bool {
    matches!(
        &expr.kind,
        ExprKind::Unary {
            op: UnaryOp::Not,
            ..
        }
    )
}

fn walk_expr(expr: &Expr, diags: &mut Vec<LintDiagnostic>) {
    if let ExprKind::Binary { op, left, right } = &expr.kind {
        if is_comparison(op) && (is_not_unary(left) || is_not_unary(right)) {
            diags.push(LintDiagnostic {
                level: LintLevel::Warning,
                span: expr.span.clone(),
                message: "`not` binds tighter than comparison operators; \
                    `not x == y` parses as `(not x) == y`. \
                    Add parentheses to disambiguate: \
                    write `not (x == y)` for the negation of the comparison, \
                    or `(not x) == y` if `not x` was intended as the operand."
                    .to_string(),
                lint_name: "ambiguous_not_comparison".to_string(),
            });
        }
    }
    walk_expr_children(expr, diags);
}

fn walk_expr_children(expr: &Expr, diags: &mut Vec<LintDiagnostic>) {
    match &expr.kind {
        ExprKind::Block(block)
        | ExprKind::Loop { body: block, .. }
        | ExprKind::Seq(block)
        | ExprKind::Par(block)
        | ExprKind::Unsafe(block)
        | ExprKind::Try(block) => walk_block(block, diags),
        ExprKind::Lock { body, .. } | ExprKind::Providers { body, .. } => walk_block(body, diags),
        ExprKind::If {
            condition,
            then_block,
            else_branch,
        } => {
            walk_expr(condition, diags);
            walk_block(then_block, diags);
            if let Some(e) = else_branch {
                walk_expr(e, diags);
            }
        }
        ExprKind::IfLet {
            value,
            then_block,
            else_branch,
            ..
        } => {
            walk_expr(value, diags);
            walk_block(then_block, diags);
            if let Some(e) = else_branch {
                walk_expr(e, diags);
            }
        }
        ExprKind::While {
            condition, body, ..
        } => {
            walk_expr(condition, diags);
            walk_block(body, diags);
        }
        ExprKind::WhileLet {
            value: condition,
            body,
            ..
        } => {
            walk_expr(condition, diags);
            walk_block(body, diags);
        }
        ExprKind::For { iterable, body, .. } => {
            walk_expr(iterable, diags);
            walk_block(body, diags);
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, diags);
            for arm in arms {
                walk_match_arm(arm, diags);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            walk_expr(left, diags);
            walk_expr(right, diags);
        }
        ExprKind::Unary { operand, .. } => walk_expr(operand, diags),
        ExprKind::NilCoalesce { left, right } | ExprKind::Pipe { left, right } => {
            walk_expr(left, diags);
            walk_expr(right, diags);
        }
        ExprKind::Call { callee, args } => {
            walk_expr(callee, diags);
            for a in args {
                walk_expr(&a.value, diags);
            }
        }
        ExprKind::MethodCall { object, args, .. } => {
            walk_expr(object, diags);
            for a in args {
                walk_expr(&a.value, diags);
            }
        }
        ExprKind::OptionalChain {
            object,
            args: Some(args),
            ..
        } => {
            walk_expr(object, diags);
            for a in args {
                walk_expr(&a.value, diags);
            }
        }
        ExprKind::OptionalChain {
            object, args: None, ..
        } => walk_expr(object, diags),
        ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
            walk_expr(object, diags);
        }
        ExprKind::Index { object, index } => {
            walk_expr(object, diags);
            walk_expr(index, diags);
        }
        ExprKind::Closure { body, .. } => walk_expr(body, diags),
        ExprKind::Return(Some(e)) | ExprKind::Question(e) | ExprKind::Cast { expr: e, .. } => {
            walk_expr(e, diags);
        }
        ExprKind::Break { value: Some(e), .. } => walk_expr(e, diags),
        ExprKind::Tuple(elems) | ExprKind::ArrayLiteral(elems) => {
            for e in elems {
                walk_expr(e, diags);
            }
        }
        ExprKind::RepeatLiteral { value, count, .. } => {
            walk_expr(value, diags);
            walk_expr(count, diags);
        }
        ExprKind::PrefixCollectionLiteral { items, .. } => {
            for e in items {
                walk_expr(e, diags);
            }
        }
        ExprKind::MapLiteral(pairs) => {
            for (k, v) in pairs {
                walk_expr(k, diags);
                walk_expr(v, diags);
            }
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                walk_expr(s, diags);
            }
            if let Some(e) = end {
                walk_expr(e, diags);
            }
        }
        ExprKind::StructLiteral { fields, spread, .. } => {
            for fi in fields {
                walk_expr(&fi.value, diags);
            }
            if let Some(s) = spread {
                walk_expr(s, diags);
            }
        }
        ExprKind::InterpolatedStringLit(parts) => {
            for p in parts {
                if let crate::ast::ParsedInterpolationPart::Expr(e) = p {
                    walk_expr(e, diags);
                }
            }
        }
        ExprKind::Identifier(_)
        | ExprKind::Path(_)
        | ExprKind::Integer(_, _)
        | ExprKind::Float(_, _)
        | ExprKind::CharLit(_)
        | ExprKind::StringLit(_)
        | ExprKind::MultiStringLit(_)
        | ExprKind::Bool(_)
        | ExprKind::SelfValue
        | ExprKind::SelfType
        | ExprKind::PipePlaceholder
        | ExprKind::Return(None)
        | ExprKind::Continue { .. }
        | ExprKind::Break { value: None, .. }
        | ExprKind::Error => {}
    }
}

fn walk_match_arm(arm: &MatchArm, diags: &mut Vec<LintDiagnostic>) {
    if let Some(g) = &arm.guard {
        walk_expr(g, diags);
    }
    walk_expr(&arm.body, diags);
}
