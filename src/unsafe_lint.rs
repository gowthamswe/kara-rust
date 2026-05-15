// src/unsafe_lint.rs
//! `undocumented_unsafe` lint with two carriers, one diagnostic name:
//!
//! 1. **Expression form (`unsafe { ... }`).** Every block must be preceded
//!    by a line comment whose text (after stripping the leading `//`) begins
//!    with `Safety:` (case-insensitive). The check is source-text-based
//!    because regular line comments are stripped from the token stream
//!    during lexing.
//! 2. **Declaration form (`unsafe extern "ABI" { ... }`).** Every block must
//!    carry a `///` doc-comment containing a `# Safety` markdown section
//!    (case-insensitive, any header level). The doc-comment is parsed onto
//!    `ExternBlock.doc_comment` and is also rendered by `karac doc`, so the
//!    lint and the renderer share one carrier — authors don't write a
//!    safety justification twice.
//!
//! Suppression (both forms):
//!   - `#[allow(undocumented_unsafe)]` on the enclosing function (form 1)
//!     or on the block itself (form 2) silences the warning.
//!   - `#[deny(undocumented_unsafe)]` promotes the warning to an error.

use crate::ast::{
    Attribute, Block, Expr, ExprKind, ExternBlock, FieldInit, ImplItem, Item, MatchArm, Program,
    Stmt, StmtKind, TraitItem, TypeKind, UnaryOp,
};
use crate::resolver::SpanKey;
use crate::token::Span;
use crate::typechecker::{Type, TypeCheckResult};
use std::collections::HashSet;

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
    /// Actionable suggestion: how to fix the offending operation. Rendered
    /// as a `= help:` continuation line under the primary diagnostic.
    pub help: Option<String>,
    /// Conceptual explanation: why the rule exists or what subtle invariant
    /// the reader may be missing. Rendered as a `= note:` continuation line.
    pub note: Option<String>,
}

/// Shared explanation of the two roles `unsafe` plays in the language. Emitted
/// as the `note:` line on every `unsafe_op_in_unsafe_fn` diagnostic so that a
/// first-time reader who hits the error in any of its forms (raw-pointer
/// deref, top-level `unsafe fn` call, impl-method `unsafe fn` call) sees the
/// declaration-side / implementer-side distinction in the same message — the
/// two roles are easy to conflate, and the diagnostic is where the
/// disambiguation matters most.
const UNSAFE_TWO_ROLES_NOTE: &str =
    "`unsafe` has two distinct roles — on a function (`unsafe fn`) it declares \
     a precondition the *caller* must uphold before calling; on a block \
     (`unsafe { ... }`) the writer asserts the operation's preconditions hold \
     here. `unsafe fn` does not implicitly wrap its body — every unsafe \
     operation in the body still needs its own `unsafe { ... }` block.";

/// Run the `undocumented_unsafe` lint over the parsed program.
///
/// `source` is the raw source text used to look up comment lines preceding
/// each `unsafe` block. Returns a (possibly empty) list of diagnostics.
pub fn check_undocumented_unsafe(program: &Program, source: &str) -> Vec<LintDiagnostic> {
    let lines: Vec<&str> = source.lines().collect();
    let mut diags = Vec::new();
    for item in &program.items {
        if let Item::ExternBlock(b) = item {
            let allow = has_lint_attr(&b.attributes, "allow");
            let deny = has_lint_attr(&b.attributes, "deny");
            if !allow {
                check_extern_block_safety_doc(b, deny, &mut diags);
            }
            continue;
        }
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

fn check_extern_block_safety_doc(block: &ExternBlock, deny: bool, diags: &mut Vec<LintDiagnostic>) {
    let has_safety = block
        .doc_comment
        .as_deref()
        .map(doc_has_safety_section)
        .unwrap_or(false);
    if !has_safety {
        diags.push(LintDiagnostic {
            level: if deny {
                LintLevel::Error
            } else {
                LintLevel::Warning
            },
            span: block.span.clone(),
            message: "unsafe extern block is missing a `# Safety` doc-comment \
                      section explaining the trust contract for its imports"
                .to_string(),
            lint_name: "undocumented_unsafe".to_string(),
            help: None,
            note: None,
        });
    }
}

/// True if any line of the doc-comment is a markdown header whose visible
/// text begins with "Safety" (case-insensitive). Accepts any header level
/// (`# Safety`, `## Safety`, ...) and any trailing text (`# Safety` and
/// `# Safety considerations` both qualify). The doc-comment body has
/// already been stripped of the `///` prefix by the lexer, so a header
/// line looks like `# Safety` here, not `/// # Safety`.
fn doc_has_safety_section(doc: &str) -> bool {
    doc.lines().any(|line| {
        let trimmed = line.trim_start();
        let after_hashes = trimmed.trim_start_matches('#');
        if after_hashes.len() == trimmed.len() {
            return false;
        }
        after_hashes
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("safety")
    })
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
            help: None,
            note: None,
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
        | ExprKind::LabeledBlock { body: block, .. }
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
        | ExprKind::Path { .. }
        | ExprKind::SelfValue
        | ExprKind::SelfType
        | ExprKind::PipePlaceholder
        | ExprKind::Return(None)
        | ExprKind::Break { value: None, .. }
        | ExprKind::Continue { .. }
        | ExprKind::OffsetOf { .. }
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

// ── Slice 3: `unsafe_op_in_unsafe_fn` operation lint ───────────────
//
// Walks every fn body (free functions, impl methods, trait-method
// default bodies) tracking `in_unsafe_block` context. Emits an error at
// every operation that requires an `unsafe { ... }` wrap when it appears
// outside one:
//   - Raw-pointer dereference (`*ptr` where ptr: `*const T` / `*mut T`).
//   - Call to another `unsafe fn` (free function or impl method).
//
// The rule applies uniformly inside `unsafe fn` bodies — declaring a
// function `unsafe` is a precondition for the *caller*, not an implicit
// `unsafe { }` wrap around the body. Calls into `unsafe extern { }`
// blocks are NOT in the unsafe-required set: the trust boundary is the
// block, not the call site.
//
// Slice 3 v1 covers raw-ptr deref + unsafe-fn calls. Asm-intrinsic
// calls, `volatile_read` / `volatile_write` intrinsics, and union field
// access are deferred to their respective producer features (no surface
// exists yet). Trait-method dispatch through a generic bound is also
// deferred — slice 3 v1 handles concrete impl-method dispatch via
// `TypeCheckResult.method_callee_types`.

/// Names of fn / impl-method declarations that carry `unsafe fn`. Calls
/// resolved against these targets require an `unsafe { ... }` wrap at the
/// call site. Functions declared inside `unsafe extern { ... }` blocks are
/// intentionally excluded — the trust boundary is the block.
struct UnsafeFnRegistry {
    top_level_unsafe: HashSet<String>,
    impl_method_unsafe: HashSet<(String, String)>,
}

fn build_unsafe_fn_registry(program: &Program) -> UnsafeFnRegistry {
    let mut top_level_unsafe: HashSet<String> = HashSet::new();
    let mut impl_method_unsafe: HashSet<(String, String)> = HashSet::new();
    // Built-in `unsafe fn` methods on prelude collections. These have no
    // user-declared `impl` block to carry an `is_unsafe` flag, so the
    // unsafe-required diagnostic is wired by seeding the registry here.
    // Currently:
    //   - `Vec.get_unchecked(i)`: skips the runtime bounds check — UB on
    //     out-of-range index. Counterpart to the deferred Slice variant.
    impl_method_unsafe.insert(("Vec".to_string(), "get_unchecked".to_string()));
    for item in &program.items {
        match item {
            Item::Function(f) if f.is_unsafe => {
                top_level_unsafe.insert(f.name.clone());
            }
            Item::ImplBlock(imp) => {
                let recv = match &imp.target_type.kind {
                    TypeKind::Path(p) => p.segments.last().cloned(),
                    _ => None,
                };
                if let Some(recv) = recv {
                    for it in &imp.items {
                        if let ImplItem::Method(m) = it {
                            if m.is_unsafe {
                                impl_method_unsafe.insert((recv.clone(), m.name.clone()));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    UnsafeFnRegistry {
        top_level_unsafe,
        impl_method_unsafe,
    }
}

/// Run the `unsafe_op_in_unsafe_fn` operation lint.
///
/// `typed` is optional: when absent, raw-pointer-deref and method-call
/// detection are skipped (those rely on the typechecker's expression-type
/// and method-callee tables). Top-level `unsafe fn` call detection works
/// without typecheck info.
pub fn check_unsafe_op_in_unsafe_fn(
    program: &Program,
    typed: Option<&TypeCheckResult>,
) -> Vec<LintDiagnostic> {
    let registry = build_unsafe_fn_registry(program);
    let mut diags: Vec<LintDiagnostic> = Vec::new();
    {
        let mut walker = OpWalker {
            registry: &registry,
            typed,
            diags: &mut diags,
        };
        for item in &program.items {
            match item {
                Item::Function(f) => walker.walk_block(&f.body, false),
                Item::ImplBlock(imp) => {
                    for it in &imp.items {
                        if let ImplItem::Method(m) = it {
                            walker.walk_block(&m.body, false);
                        }
                    }
                }
                Item::TraitDef(t) => {
                    for ti in &t.items {
                        if let TraitItem::Method(m) = ti {
                            if let Some(body) = &m.body {
                                walker.walk_block(body, false);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    diags
}

struct OpWalker<'a> {
    registry: &'a UnsafeFnRegistry,
    typed: Option<&'a TypeCheckResult>,
    diags: &'a mut Vec<LintDiagnostic>,
}

impl OpWalker<'_> {
    fn walk_block(&mut self, block: &Block, in_unsafe: bool) {
        for stmt in &block.stmts {
            self.walk_stmt(stmt, in_unsafe);
        }
        if let Some(tail) = &block.final_expr {
            self.walk_expr(tail, in_unsafe);
        }
    }

    fn walk_stmt(&mut self, stmt: &Stmt, in_unsafe: bool) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => self.walk_expr(value, in_unsafe),
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                self.walk_expr(value, in_unsafe);
                self.walk_block(else_block, in_unsafe);
            }
            StmtKind::Expr(e) => self.walk_expr(e, in_unsafe),
            StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
                self.walk_expr(target, in_unsafe);
                self.walk_expr(value, in_unsafe);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.walk_block(body, in_unsafe);
            }
        }
    }

    fn walk_expr(&mut self, expr: &Expr, in_unsafe: bool) {
        match &expr.kind {
            ExprKind::Unsafe(block) => {
                // Entering an `unsafe { }` block flips the context for its
                // body — ops inside are accepted, ops outside still aren't.
                self.walk_block(block, true);
            }
            ExprKind::Unary {
                op: UnaryOp::Deref,
                operand,
            } => {
                if !in_unsafe && self.is_raw_pointer_deref(operand) {
                    self.diags.push(LintDiagnostic {
                        level: LintLevel::Error,
                        span: expr.span.clone(),
                        message: "raw-pointer dereference must be wrapped in an \
                                  `unsafe { ... }` block"
                            .to_string(),
                        lint_name: "unsafe_op_in_unsafe_fn".to_string(),
                        help: Some(
                            "wrap the dereference in `unsafe { ... }` and add a \
                             `// Safety: ...` comment above the block explaining why the \
                             pointer is valid (per the `undocumented_unsafe` lint)."
                                .to_string(),
                        ),
                        note: Some(UNSAFE_TWO_ROLES_NOTE.to_string()),
                    });
                }
                self.walk_expr(operand, in_unsafe);
            }
            ExprKind::Call { callee, args } => {
                if !in_unsafe {
                    if let ExprKind::Identifier(name) = &callee.kind {
                        if self.registry.top_level_unsafe.contains(name) {
                            self.diags.push(LintDiagnostic {
                                level: LintLevel::Error,
                                span: expr.span.clone(),
                                message: format!(
                                    "call to `unsafe fn {name}` must be wrapped in an \
                                     `unsafe {{ ... }}` block"
                                ),
                                lint_name: "unsafe_op_in_unsafe_fn".to_string(),
                                help: Some(format!(
                                    "wrap the call in `unsafe {{ ... }}` and add a \
                                     `// Safety: ...` comment above the block explaining why \
                                     `{name}`'s preconditions are satisfied (per the \
                                     `undocumented_unsafe` lint)."
                                )),
                                note: Some(UNSAFE_TWO_ROLES_NOTE.to_string()),
                            });
                        }
                    }
                }
                self.walk_expr(callee, in_unsafe);
                for a in args {
                    self.walk_expr(&a.value, in_unsafe);
                }
            }
            ExprKind::MethodCall { object, args, .. } => {
                if !in_unsafe {
                    if let Some((recv, m)) = self.method_callee(&expr.span) {
                        if self
                            .registry
                            .impl_method_unsafe
                            .contains(&(recv.clone(), m.clone()))
                        {
                            self.diags.push(LintDiagnostic {
                                level: LintLevel::Error,
                                span: expr.span.clone(),
                                message: format!(
                                    "call to `unsafe fn {recv}.{m}` must be wrapped in an \
                                     `unsafe {{ ... }}` block"
                                ),
                                lint_name: "unsafe_op_in_unsafe_fn".to_string(),
                                help: Some(format!(
                                    "wrap the call in `unsafe {{ ... }}` and add a \
                                     `// Safety: ...` comment above the block explaining why \
                                     `{recv}.{m}`'s preconditions are satisfied (per the \
                                     `undocumented_unsafe` lint)."
                                )),
                                note: Some(UNSAFE_TWO_ROLES_NOTE.to_string()),
                            });
                        }
                    }
                }
                self.walk_expr(object, in_unsafe);
                for a in args {
                    self.walk_expr(&a.value, in_unsafe);
                }
            }
            ExprKind::OptionalChain {
                object,
                args: Some(args),
                ..
            } => {
                self.walk_expr(object, in_unsafe);
                for a in args {
                    self.walk_expr(&a.value, in_unsafe);
                }
            }
            ExprKind::OptionalChain {
                object, args: None, ..
            } => {
                self.walk_expr(object, in_unsafe);
            }
            ExprKind::Block(block)
            | ExprKind::Loop { body: block, .. }
            | ExprKind::LabeledBlock { body: block, .. }
            | ExprKind::Seq(block)
            | ExprKind::Par(block)
            | ExprKind::Try(block) => self.walk_block(block, in_unsafe),
            ExprKind::Lock { body, .. } | ExprKind::Providers { body, .. } => {
                self.walk_block(body, in_unsafe)
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.walk_expr(condition, in_unsafe);
                self.walk_block(then_block, in_unsafe);
                if let Some(e) = else_branch {
                    self.walk_expr(e, in_unsafe);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.walk_expr(value, in_unsafe);
                self.walk_block(then_block, in_unsafe);
                if let Some(e) = else_branch {
                    self.walk_expr(e, in_unsafe);
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
                self.walk_expr(condition, in_unsafe);
                self.walk_block(body, in_unsafe);
            }
            ExprKind::For { iterable, body, .. } => {
                self.walk_expr(iterable, in_unsafe);
                self.walk_block(body, in_unsafe);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee, in_unsafe);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.walk_expr(guard, in_unsafe);
                    }
                    self.walk_expr(&arm.body, in_unsafe);
                }
            }
            ExprKind::Binary { left, right, .. } => {
                self.walk_expr(left, in_unsafe);
                self.walk_expr(right, in_unsafe);
            }
            ExprKind::Unary { operand, .. } => self.walk_expr(operand, in_unsafe),
            ExprKind::NilCoalesce { left, right } | ExprKind::Pipe { left, right } => {
                self.walk_expr(left, in_unsafe);
                self.walk_expr(right, in_unsafe);
            }
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.walk_expr(object, in_unsafe);
            }
            ExprKind::Index { object, index } => {
                self.walk_expr(object, in_unsafe);
                self.walk_expr(index, in_unsafe);
            }
            ExprKind::Closure { body, .. } => self.walk_expr(body, in_unsafe),
            ExprKind::Return(Some(e)) | ExprKind::Question(e) | ExprKind::Cast { expr: e, .. } => {
                self.walk_expr(e, in_unsafe);
            }
            ExprKind::Break { value: Some(e), .. } => self.walk_expr(e, in_unsafe),
            ExprKind::Tuple(elems) | ExprKind::ArrayLiteral(elems) => {
                for e in elems {
                    self.walk_expr(e, in_unsafe);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.walk_expr(value, in_unsafe);
                self.walk_expr(count, in_unsafe);
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.walk_expr(e, in_unsafe);
                }
            }
            ExprKind::MapLiteral(pairs) => {
                for (k, v) in pairs {
                    self.walk_expr(k, in_unsafe);
                    self.walk_expr(v, in_unsafe);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    self.walk_expr(&f.value, in_unsafe);
                }
                if let Some(s) = spread {
                    self.walk_expr(s, in_unsafe);
                }
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.walk_expr(s, in_unsafe);
                }
                if let Some(e) = end {
                    self.walk_expr(e, in_unsafe);
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
            | ExprKind::Path { .. }
            | ExprKind::SelfValue
            | ExprKind::SelfType
            | ExprKind::PipePlaceholder
            | ExprKind::Return(None)
            | ExprKind::Break { value: None, .. }
            | ExprKind::Continue { .. }
            | ExprKind::OffsetOf { .. }
            | ExprKind::Error => {}
        }
    }

    fn is_raw_pointer_deref(&self, operand: &Expr) -> bool {
        let Some(typed) = self.typed else {
            return false;
        };
        let key = SpanKey::from_span(&operand.span);
        matches!(typed.expr_types.get(&key), Some(Type::Pointer { .. }))
    }

    /// Returns the `(receiver_type, method)` resolved by the typechecker for
    /// a method-call expression, parsing the canonical `"Type.method"` form
    /// stored in `method_callee_types`. Returns `None` if typecheck info is
    /// unavailable or the call wasn't resolved (e.g. on an upstream error).
    fn method_callee(&self, call_span: &Span) -> Option<(String, String)> {
        let typed = self.typed?;
        let key = SpanKey::from_span(call_span);
        let s = typed.method_callee_types.get(&key)?;
        let (recv, m) = s.split_once('.')?;
        Some((recv.to_string(), m.to_string()))
    }
}
