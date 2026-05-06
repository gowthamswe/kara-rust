// src/unsafe_lint.rs
//! `undocumented_unsafe` lint: every `unsafe { ... }` block must be preceded
//! by a line comment whose text (after stripping the leading `//`) begins with
//! `Safety:` (case-insensitive). The check is source-text-based because regular
//! line comments are stripped from the token stream during lexing.
//!
//! Suppression:
//!   - `#[allow(undocumented_unsafe)]` on the enclosing function silences the
//!     warning for all `unsafe` blocks inside that function.
//!   - `#[deny(undocumented_unsafe)]` promotes the warning to an error.

use crate::ast::{
    Attribute, Block, Expr, ExprKind, FieldInit, Item, MatchArm, Program, Stmt, StmtKind,
};
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

/// Run the `undocumented_unsafe` lint over the parsed program.
///
/// `source` is the raw source text used to look up comment lines preceding
/// each `unsafe` block. Returns a (possibly empty) list of diagnostics.
pub fn check_undocumented_unsafe(program: &Program, source: &str) -> Vec<LintDiagnostic> {
    let lines: Vec<&str> = source.lines().collect();
    let mut diags = Vec::new();
    for item in &program.items {
        let (fn_allow, fn_deny) = match item {
            Item::Function(f) => (
                has_lint_attr(&f.attributes, "allow"),
                has_lint_attr(&f.attributes, "deny"),
            ),
            _ => (false, false),
        };
        if fn_allow {
            continue;
        }
        collect_item_unsafe(item, &lines, fn_deny, &mut diags);
    }
    diags
}

fn has_lint_attr(attrs: &[Attribute], kind: &str) -> bool {
    attrs.iter().any(|a| {
        if a.name != kind {
            return false;
        }
        a.args.iter().any(|arg| {
            arg.name
                .as_deref()
                .map(|n| n == "undocumented_unsafe")
                .unwrap_or(false)
                || arg
                    .value
                    .as_ref()
                    .map(|v| {
                        matches!(&v.kind, ExprKind::Identifier(n) if n == "undocumented_unsafe")
                    })
                    .unwrap_or(false)
        })
    })
}

fn collect_item_unsafe(item: &Item, lines: &[&str], deny: bool, diags: &mut Vec<LintDiagnostic>) {
    match item {
        Item::Function(f) => walk_block(&f.body, lines, deny, diags),
        Item::ImplBlock(imp) => {
            for item in &imp.items {
                if let crate::ast::ImplItem::Method(method) = item {
                    let allow = has_lint_attr(&method.attributes, "allow");
                    let deny_m = has_lint_attr(&method.attributes, "deny");
                    if !allow {
                        walk_block(&method.body, lines, deny || deny_m, diags);
                    }
                }
            }
        }
        _ => {}
    }
}

fn check_unsafe_span(span: &Span, lines: &[&str], deny: bool, diags: &mut Vec<LintDiagnostic>) {
    // span.line is 1-indexed. The preceding line is at index span.line - 2.
    let preceding_ok = if span.line >= 2 {
        let preceding = lines[span.line - 2];
        is_safety_comment(preceding.trim())
    } else {
        false
    };
    if !preceding_ok {
        diags.push(LintDiagnostic {
            level: if deny {
                LintLevel::Error
            } else {
                LintLevel::Warning
            },
            span: span.clone(),
            message: "unsafe block is not preceded by a `// Safety:` comment".to_string(),
            lint_name: "undocumented_unsafe".to_string(),
        });
    }
}

fn is_safety_comment(line: &str) -> bool {
    let body = if let Some(rest) = line.strip_prefix("///") {
        rest
    } else if let Some(rest) = line.strip_prefix("//") {
        rest
    } else {
        return false;
    };
    body.trim_start()
        .to_ascii_lowercase()
        .starts_with("safety:")
}

// ── AST walker ────────────────────────────────────────────────────

fn walk_block(block: &Block, lines: &[&str], deny: bool, diags: &mut Vec<LintDiagnostic>) {
    for stmt in &block.stmts {
        walk_stmt(stmt, lines, deny, diags);
    }
    if let Some(tail) = &block.final_expr {
        walk_expr(tail, lines, deny, diags);
    }
}

fn walk_stmt(stmt: &Stmt, lines: &[&str], deny: bool, diags: &mut Vec<LintDiagnostic>) {
    match &stmt.kind {
        StmtKind::Let { value, .. } => walk_expr(value, lines, deny, diags),
        StmtKind::LetUninit { .. } => {}
        StmtKind::LetElse {
            value, else_block, ..
        } => {
            walk_expr(value, lines, deny, diags);
            walk_block(else_block, lines, deny, diags);
        }
        StmtKind::Expr(e) => walk_expr(e, lines, deny, diags),
        StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
            walk_expr(target, lines, deny, diags);
            walk_expr(value, lines, deny, diags);
        }
        StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
            walk_block(body, lines, deny, diags);
        }
    }
}

fn walk_expr(expr: &Expr, lines: &[&str], deny: bool, diags: &mut Vec<LintDiagnostic>) {
    match &expr.kind {
        ExprKind::Unsafe(block) => {
            check_unsafe_span(&expr.span, lines, deny, diags);
            walk_block(block, lines, deny, diags);
        }
        ExprKind::Block(block)
        | ExprKind::Loop { body: block, .. }
        | ExprKind::Seq(block)
        | ExprKind::Par(block)
        | ExprKind::Try(block) => walk_block(block, lines, deny, diags),
        ExprKind::Lock { body, .. } | ExprKind::Providers { body, .. } => {
            walk_block(body, lines, deny, diags)
        }
        ExprKind::If {
            condition,
            then_block,
            else_branch,
        } => {
            walk_expr(condition, lines, deny, diags);
            walk_block(then_block, lines, deny, diags);
            if let Some(e) = else_branch {
                walk_expr(e, lines, deny, diags);
            }
        }
        ExprKind::IfLet {
            value,
            then_block,
            else_branch,
            ..
        } => {
            walk_expr(value, lines, deny, diags);
            walk_block(then_block, lines, deny, diags);
            if let Some(e) = else_branch {
                walk_expr(e, lines, deny, diags);
            }
        }
        ExprKind::While {
            condition, body, ..
        }
        | ExprKind::WhileLet {
            value: condition,
            body,
            ..
        } => {
            walk_expr(condition, lines, deny, diags);
            walk_block(body, lines, deny, diags);
        }
        ExprKind::For { iterable, body, .. } => {
            walk_expr(iterable, lines, deny, diags);
            walk_block(body, lines, deny, diags);
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, lines, deny, diags);
            for arm in arms {
                walk_match_arm(arm, lines, deny, diags);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            walk_expr(left, lines, deny, diags);
            walk_expr(right, lines, deny, diags);
        }
        ExprKind::Unary { operand, .. } => walk_expr(operand, lines, deny, diags),
        ExprKind::NilCoalesce { left, right } | ExprKind::Pipe { left, right } => {
            walk_expr(left, lines, deny, diags);
            walk_expr(right, lines, deny, diags);
        }
        ExprKind::Call { callee, args } => {
            walk_expr(callee, lines, deny, diags);
            for a in args {
                walk_expr(&a.value, lines, deny, diags);
            }
        }
        ExprKind::MethodCall { object, args, .. }
        | ExprKind::OptionalChain {
            object,
            args: Some(args),
            ..
        } => {
            walk_expr(object, lines, deny, diags);
            for a in args {
                walk_expr(&a.value, lines, deny, diags);
            }
        }
        ExprKind::OptionalChain {
            object, args: None, ..
        } => {
            walk_expr(object, lines, deny, diags);
        }
        ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
            walk_expr(object, lines, deny, diags);
        }
        ExprKind::Index { object, index } => {
            walk_expr(object, lines, deny, diags);
            walk_expr(index, lines, deny, diags);
        }
        ExprKind::Closure { body, .. } => walk_expr(body, lines, deny, diags),
        ExprKind::Return(Some(e)) | ExprKind::Question(e) | ExprKind::Cast { expr: e, .. } => {
            walk_expr(e, lines, deny, diags);
        }
        ExprKind::Break { value: Some(e), .. } => walk_expr(e, lines, deny, diags),
        ExprKind::Tuple(elems) | ExprKind::ArrayLiteral(elems) => {
            for e in elems {
                walk_expr(e, lines, deny, diags);
            }
        }
        ExprKind::RepeatLiteral { value, count, .. } => {
            walk_expr(value, lines, deny, diags);
            walk_expr(count, lines, deny, diags);
        }
        ExprKind::PrefixCollectionLiteral { items, .. } => {
            for e in items {
                walk_expr(e, lines, deny, diags);
            }
        }
        ExprKind::MapLiteral(pairs) => {
            for (k, v) in pairs {
                walk_expr(k, lines, deny, diags);
                walk_expr(v, lines, deny, diags);
            }
        }
        ExprKind::StructLiteral { fields, spread, .. } => {
            for f in fields {
                walk_field_init(f, lines, deny, diags);
            }
            if let Some(s) = spread {
                walk_expr(s, lines, deny, diags);
            }
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                walk_expr(s, lines, deny, diags);
            }
            if let Some(e) = end {
                walk_expr(e, lines, deny, diags);
            }
        }
        // Terminals — no sub-expressions.
        ExprKind::Integer(..)
        | ExprKind::Float(..)
        | ExprKind::CharLit(..)
        | ExprKind::StringLit(..)
        | ExprKind::MultiStringLit(..)
        | ExprKind::InterpolatedStringLit(..)
        | ExprKind::Bool(..)
        | ExprKind::Identifier(..)
        | ExprKind::Path(..)
        | ExprKind::SelfValue
        | ExprKind::SelfType
        | ExprKind::PipePlaceholder
        | ExprKind::Return(None)
        | ExprKind::Break { value: None, .. }
        | ExprKind::Continue { .. }
        | ExprKind::Error => {}
    }
}

fn walk_match_arm(arm: &MatchArm, lines: &[&str], deny: bool, diags: &mut Vec<LintDiagnostic>) {
    if let Some(guard) = &arm.guard {
        walk_expr(guard, lines, deny, diags);
    }
    walk_expr(&arm.body, lines, deny, diags);
}

fn walk_field_init(f: &FieldInit, lines: &[&str], deny: bool, diags: &mut Vec<LintDiagnostic>) {
    walk_expr(&f.value, lines, deny, diags);
}
