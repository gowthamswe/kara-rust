// src/ffi_lint.rs
//! `ffi_float_eq` lint: warn when the result of an `extern "C"` function call
//! that returns a float type is directly compared with `==` or `!=`.
//!
//! Direct pattern covered: `extern_fn() == 0.0` / `extern_fn() != 1.0`.
//! Indirect patterns (storing in a `let` first) are not tracked — they
//! require full data-flow analysis beyond this static-only pass.
//!
//! Rationale: FFI floats may not round-trip exactly due to extended-precision
//! FPU registers or ABI differences; equality comparison is almost always
//! unintentional. Use an epsilon comparison instead.

use crate::ast::{BinOp, Block, Expr, ExprKind, Item, MatchArm, Program, Stmt, StmtKind, TypeKind};
use crate::token::Span;

#[derive(Debug, Clone)]
pub struct FfiFloatEqDiagnostic {
    pub span: Span,
    pub extern_fn: String,
    pub message: String,
}

/// Run the `ffi_float_eq` lint.
///
/// Returns diagnostics for every direct `extern_fn() == expr` or
/// `extern_fn() != expr` call where `extern_fn` is declared `extern "C"` and
/// returns a float type.
pub fn check_ffi_float_eq(program: &Program) -> Vec<FfiFloatEqDiagnostic> {
    let ffi_float_fns = collect_ffi_float_fns(program);
    let mut diags = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => walk_block(&f.body, &ffi_float_fns, &mut diags),
            Item::ImplBlock(imp) => {
                for iitem in &imp.items {
                    if let crate::ast::ImplItem::Method(m) = iitem {
                        walk_block(&m.body, &ffi_float_fns, &mut diags);
                    }
                }
            }
            _ => {}
        }
    }
    diags
}

/// Collect names of `extern "C"` functions that return `f32` or `f64`.
fn collect_ffi_float_fns(program: &Program) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for item in &program.items {
        if let Item::ExternFunction(ef) = item {
            if let Some(ret) = &ef.return_type {
                if is_float_typexpr(ret) {
                    set.insert(ef.name.clone());
                }
            }
        }
    }
    set
}

fn is_float_typexpr(ty: &crate::ast::TypeExpr) -> bool {
    if let TypeKind::Path(path) = &ty.kind {
        matches!(
            path.segments.first().map(String::as_str),
            Some("f32") | Some("f64")
        )
    } else {
        false
    }
}

fn is_ffi_float_call(expr: &Expr, ffi_fns: &std::collections::HashSet<String>) -> Option<String> {
    match &expr.kind {
        ExprKind::Call { callee, .. } => match &callee.kind {
            ExprKind::Identifier(name) if ffi_fns.contains(name) => Some(name.clone()),
            ExprKind::Path(segs) if segs.last().map(|s| ffi_fns.contains(s)).unwrap_or(false) => {
                segs.last().cloned()
            }
            _ => None,
        },
        _ => None,
    }
}

fn walk_block(
    block: &Block,
    ffi_fns: &std::collections::HashSet<String>,
    diags: &mut Vec<FfiFloatEqDiagnostic>,
) {
    for stmt in &block.stmts {
        walk_stmt(stmt, ffi_fns, diags);
    }
    if let Some(tail) = &block.final_expr {
        walk_expr(tail, ffi_fns, diags);
    }
}

fn walk_stmt(
    stmt: &Stmt,
    ffi_fns: &std::collections::HashSet<String>,
    diags: &mut Vec<FfiFloatEqDiagnostic>,
) {
    match &stmt.kind {
        StmtKind::Let { value, .. } => walk_expr(value, ffi_fns, diags),
        StmtKind::LetUninit { .. } => {}
        StmtKind::LetElse {
            value, else_block, ..
        } => {
            walk_expr(value, ffi_fns, diags);
            walk_block(else_block, ffi_fns, diags);
        }
        StmtKind::Expr(e) => walk_expr(e, ffi_fns, diags),
        StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
            walk_expr(target, ffi_fns, diags);
            walk_expr(value, ffi_fns, diags);
        }
        StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
            walk_block(body, ffi_fns, diags);
        }
    }
}

fn walk_expr(
    expr: &Expr,
    ffi_fns: &std::collections::HashSet<String>,
    diags: &mut Vec<FfiFloatEqDiagnostic>,
) {
    match &expr.kind {
        ExprKind::Binary { op, left, right } => {
            // Check for direct ffi_fn() == expr or ffi_fn() != expr patterns.
            if matches!(op, BinOp::Eq | BinOp::NotEq) {
                if let Some(fn_name) = is_ffi_float_call(left, ffi_fns) {
                    diags.push(FfiFloatEqDiagnostic {
                        span: expr.span.clone(),
                        extern_fn: fn_name.clone(),
                        message: format!(
                            "comparing result of FFI float function `{}` with `{}` is unreliable; use an epsilon comparison",
                            fn_name,
                            if matches!(op, BinOp::Eq) { "==" } else { "!=" }
                        ),
                    });
                } else if let Some(fn_name) = is_ffi_float_call(right, ffi_fns) {
                    diags.push(FfiFloatEqDiagnostic {
                        span: expr.span.clone(),
                        extern_fn: fn_name.clone(),
                        message: format!(
                            "comparing result of FFI float function `{}` with `{}` is unreliable; use an epsilon comparison",
                            fn_name,
                            if matches!(op, BinOp::Eq) { "==" } else { "!=" }
                        ),
                    });
                }
            }
            walk_expr(left, ffi_fns, diags);
            walk_expr(right, ffi_fns, diags);
        }
        ExprKind::Block(block)
        | ExprKind::Loop { body: block, .. }
        | ExprKind::Seq(block)
        | ExprKind::Par(block)
        | ExprKind::Unsafe(block)
        | ExprKind::Try(block) => walk_block(block, ffi_fns, diags),
        ExprKind::Lock { body, .. } | ExprKind::Providers { body, .. } => {
            walk_block(body, ffi_fns, diags)
        }
        ExprKind::If {
            condition,
            then_block,
            else_branch,
        } => {
            walk_expr(condition, ffi_fns, diags);
            walk_block(then_block, ffi_fns, diags);
            if let Some(e) = else_branch {
                walk_expr(e, ffi_fns, diags);
            }
        }
        ExprKind::IfLet {
            value,
            then_block,
            else_branch,
            ..
        } => {
            walk_expr(value, ffi_fns, diags);
            walk_block(then_block, ffi_fns, diags);
            if let Some(e) = else_branch {
                walk_expr(e, ffi_fns, diags);
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
            walk_expr(condition, ffi_fns, diags);
            walk_block(body, ffi_fns, diags);
        }
        ExprKind::For { iterable, body, .. } => {
            walk_expr(iterable, ffi_fns, diags);
            walk_block(body, ffi_fns, diags);
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, ffi_fns, diags);
            for arm in arms {
                walk_match_arm(arm, ffi_fns, diags);
            }
        }
        ExprKind::Unary { operand, .. } => walk_expr(operand, ffi_fns, diags),
        ExprKind::NilCoalesce { left, right } | ExprKind::Pipe { left, right } => {
            walk_expr(left, ffi_fns, diags);
            walk_expr(right, ffi_fns, diags);
        }
        ExprKind::Call { callee, args } => {
            walk_expr(callee, ffi_fns, diags);
            for a in args {
                walk_expr(&a.value, ffi_fns, diags);
            }
        }
        ExprKind::MethodCall { object, args, .. }
        | ExprKind::OptionalChain {
            object,
            args: Some(args),
            ..
        } => {
            walk_expr(object, ffi_fns, diags);
            for a in args {
                walk_expr(&a.value, ffi_fns, diags);
            }
        }
        ExprKind::OptionalChain {
            object, args: None, ..
        }
        | ExprKind::FieldAccess { object, .. }
        | ExprKind::TupleIndex { object, .. } => walk_expr(object, ffi_fns, diags),
        ExprKind::Index { object, index } => {
            walk_expr(object, ffi_fns, diags);
            walk_expr(index, ffi_fns, diags);
        }
        ExprKind::Closure { body, .. } => walk_expr(body, ffi_fns, diags),
        ExprKind::Return(Some(e)) | ExprKind::Question(e) | ExprKind::Cast { expr: e, .. } => {
            walk_expr(e, ffi_fns, diags);
        }
        ExprKind::Break { value: Some(e), .. } => walk_expr(e, ffi_fns, diags),
        ExprKind::Tuple(elems) | ExprKind::ArrayLiteral(elems) => {
            for e in elems {
                walk_expr(e, ffi_fns, diags);
            }
        }
        ExprKind::RepeatLiteral { value, count, .. } => {
            walk_expr(value, ffi_fns, diags);
            walk_expr(count, ffi_fns, diags);
        }
        ExprKind::PrefixCollectionLiteral { items, .. } => {
            for e in items {
                walk_expr(e, ffi_fns, diags);
            }
        }
        ExprKind::MapLiteral(pairs) => {
            for (k, v) in pairs {
                walk_expr(k, ffi_fns, diags);
                walk_expr(v, ffi_fns, diags);
            }
        }
        ExprKind::StructLiteral { fields, spread, .. } => {
            for f in fields {
                walk_expr(&f.value, ffi_fns, diags);
            }
            if let Some(s) = spread {
                walk_expr(s, ffi_fns, diags);
            }
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(s) = start {
                walk_expr(s, ffi_fns, diags);
            }
            if let Some(e) = end {
                walk_expr(e, ffi_fns, diags);
            }
        }
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

fn walk_match_arm(
    arm: &MatchArm,
    ffi_fns: &std::collections::HashSet<String>,
    diags: &mut Vec<FfiFloatEqDiagnostic>,
) {
    if let Some(guard) = &arm.guard {
        walk_expr(guard, ffi_fns, diags);
    }
    walk_expr(&arm.body, ffi_fns, diags);
}
