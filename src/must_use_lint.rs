// src/must_use_lint.rs
//! `must_use` lint — slice 1 of the `#[must_use]` mandate
//! (`docs/implementation_checklist/phase-5-diagnostics.md` §
//! `#[must_use]` mandate).
//!
//! Slice 1 ships the *implicit* `#[must_use]` recognition for the two
//! language-level types `Result[T, E]` and `Option[T]`. Discarding the
//! return value of an expression of either type at statement position
//! produces a `warning[must_use]` pointing at the discarded value, with
//! a `help` line offering the canonical fix (`let _ = ...` to
//! acknowledge the discard explicitly, or `match` / `if let` to consume
//! the value) and a `note` line explaining *why* these two types are
//! treated as implicitly must-use (silently dropping them abandons the
//! error / absence branch the author meant to handle).
//!
//! Why a lint module rather than the typechecker's error stream: the
//! typechecker treats `TypeCheckResult.errors` as fatal (the codegen
//! path bails on the first non-empty errors list — see
//! `src/cli.rs::pipeline.typed.errors`-gated bail), so a must-use
//! diagnostic emitted through that channel would block compilation
//! rather than warn. The lint module pattern (`undocumented_unsafe`,
//! `unsafe_op_in_unsafe_fn`, `ffi_float_eq`, `ambiguous_not_comparison`)
//! is the right shape for a non-fatal warning that consumers can
//! acknowledge by binding to `_`.
//!
//! Slices 2–4 of the broader `#[must_use]` mandate extend this module:
//! slice 2 applies the `#[must_use]` attribute to stdlib types (iterator
//! adapters, guards, builders, `JoinHandle[T]`, pure-transformation
//! methods); slice 3 adds the `missing_must_use` stdlib-hygiene lint;
//! slice 4 generalises the discarded-value detection to honour the
//! attribute on user-defined types and functions. The walker shape and
//! diagnostic shape established here carry forward unchanged.

use crate::ast::{
    Block, Expr, ExprKind, FieldInit, ImplItem, Item, MatchArm, Program, Stmt, StmtKind, TraitItem,
};
use crate::resolver::SpanKey;
use crate::token::Span;
use crate::typechecker::{Type, TypeCheckResult};

/// Recognise the two slice-1 implicit-must-use types — `Result[T, E]` and
/// `Option[T]` — by name. Returns `Some((kind, why))` where `kind` is the
/// rendered type name and `why` is the consequence phrase used in the
/// diagnostic's `note:` line. Returns `None` for every other named type
/// (those flow through the slice-4 `type_level_must_use` path).
fn implicit_must_use_kind(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "Result" => Some(("Result", "an `Err` branch the caller meant to handle")),
        "Option" => Some(("Option", "a `None` branch the caller meant to handle")),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Actionable suggestion. Rendered as a `= help:` continuation line
    /// under the primary diagnostic, mirroring `unsafe_lint`'s slice 4
    /// shape so the CLI rendering helper can carry across lint modules.
    pub help: Option<String>,
    /// Conceptual explanation: why the rule fires, surfaced in the same
    /// diagnostic so a first-time reader does not have to chase the spec
    /// to understand why discarding `Result` / `Option` is a hazard.
    pub note: Option<String>,
}

/// Run the implicit-`#[must_use]` lint over the parsed program.
///
/// `typed` is optional: the lint is a no-op when type information is
/// unavailable, since the check fundamentally consults `expr_types` to
/// recognise `Result[T, E]` / `Option[T]` at statement position. The
/// only-language-level-types shape of slice 1 means there is nothing
/// useful to report without typecheck data.
pub fn check_implicit_must_use(
    program: &Program,
    typed: Option<&TypeCheckResult>,
    cli_lint_overrides: &crate::lints::CliLintOverrides,
) -> Vec<LintDiagnostic> {
    let Some(typed) = typed else {
        return Vec::new();
    };
    // Slice 4b cross-cutting — resolve the CLI fall-through severity
    // once (the lint name is constant per pass). Suppress is a fast
    // exit; Warn / Deny set the level the walker stamps on each
    // emitted diagnostic.
    let severity = crate::lints::effective_level_for_module_lint(
        false,
        false,
        false,
        cli_lint_overrides,
        "must_use",
    );
    if matches!(severity, crate::lints::ModuleLintSeverity::Suppress) {
        return Vec::new();
    }
    let level = match severity {
        crate::lints::ModuleLintSeverity::Deny => LintLevel::Error,
        _ => LintLevel::Warning,
    };
    let mut diags: Vec<LintDiagnostic> = Vec::new();
    {
        let mut walker = Walker {
            typed,
            level,
            diags: &mut diags,
        };
        for item in &program.items {
            match item {
                Item::Function(f) => walker.walk_block(&f.body),
                Item::ImplBlock(imp) => {
                    for it in &imp.items {
                        if let ImplItem::Method(m) = it {
                            walker.walk_block(&m.body);
                        }
                    }
                }
                Item::TraitDef(t) => {
                    for ti in &t.items {
                        if let TraitItem::Method(m) = ti {
                            if let Some(body) = &m.body {
                                walker.walk_block(body);
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

struct Walker<'a> {
    typed: &'a TypeCheckResult,
    /// Slice 4b cross-cutting — the post-cascade severity for every
    /// emission this walker produces. Computed once at the entry
    /// point so the per-emission path is just a `level` field read.
    level: LintLevel,
    diags: &'a mut Vec<LintDiagnostic>,
}

impl Walker<'_> {
    fn walk_block(&mut self, block: &Block) {
        // Statement-position expressions are the discard sites — the
        // value flows nowhere. The block's `final_expr` is the block's
        // *value* and is consumed by whatever consumes the block, so it
        // is recursed into but NOT checked for must-use at this level
        // (the enclosing context decides whether the value flows out or
        // is itself discarded).
        for stmt in &block.stmts {
            self.walk_stmt(stmt);
        }
        if let Some(tail) = &block.final_expr {
            self.walk_expr(tail);
        }
    }

    fn walk_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Expr(e) => {
                self.check_discard(e);
                self.walk_expr(e);
            }
            StmtKind::Let { value, .. } => self.walk_expr(value),
            StmtKind::LetUninit { .. } => {}
            StmtKind::LetElse {
                value, else_block, ..
            } => {
                self.walk_expr(value);
                self.walk_block(else_block);
            }
            StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
                self.walk_expr(target);
                self.walk_expr(value);
            }
            StmtKind::Defer { body } | StmtKind::ErrDefer { body, .. } => {
                self.walk_block(body);
            }
        }
    }

    /// At a statement-position expression, look up the expression's
    /// inferred type and emit a `must_use` warning. The check has three
    /// layered sources, in priority order — only the highest-priority
    /// match fires, so a discarded value never produces more than one
    /// `must_use` diagnostic at the same site:
    ///
    /// 1. **Implicit (slice 1).** `Result[T, E]` / `Option[T]` are
    ///    treated as implicitly must-use; the language-level types
    ///    carry the recognition in the typechecker rather than as a
    ///    user-visible `#[must_use]` attribute. Slice 1's wording is
    ///    preserved verbatim because the diagnostic explains the
    ///    `Err` / `None` branch hazard specifically.
    ///
    /// 2. **Type-level `#[must_use]` (slice 4).** Any other named type
    ///    whose `StructInfo` / `EnumInfo` carries
    ///    `must_use_message: Some(_)` — slice 2 annotations on
    ///    `Peekable[T]` / `PooledConnection[T]`, the Iterator pseudo-
    ///    struct annotation in `register_compiler_intrinsic_env`, and
    ///    every future user-authored `#[must_use]` on a struct/enum
    ///    declaration. The message rendered in `note:` is the
    ///    author's string from the attribute.
    ///
    /// 3. **Function-level `#[must_use]` (slice 4).** If the discarded
    ///    expression is a call (free function, static method, or
    ///    instance method) and the callee carries `#[must_use]`, fire
    ///    against the call site with the author's reason. Looked up
    ///    via `typed.must_use_functions` keyed by `"name"` (free fn)
    ///    or `"Type.method"` (impl method, resolved through
    ///    `method_callee_types` for instance calls or by joining a
    ///    `Path` callee's segments for static calls).
    ///
    /// The slice 4 ordering — type-level before function-level — is
    /// deliberate: a function returning a `#[must_use]` type with its
    /// own attribute message gets the type-level message, which is
    /// more specific to the value being discarded. The function-level
    /// path is the fallback for cases where the return type itself is
    /// freely droppable but the function's purpose is to mint a fresh
    /// value the caller should consume (`String.to_lowercase()` once
    /// String lands; `Vec.iter()` is already covered by the Iterator
    /// type-level annotation).
    fn check_discard(&mut self, expr: &Expr) {
        let key = SpanKey::from_span(&expr.span);
        let Some(ty) = self.typed.expr_types.get(&key) else {
            return;
        };

        // Source 1: implicit (slice 1) — Result / Option.
        if let Type::Named { name, .. } = ty {
            if let Some((kind, why)) = implicit_must_use_kind(name) {
                self.diags.push(self.make_implicit_diag(expr, kind, why));
                return;
            }
        }

        // Source 2: type-level `#[must_use]` (slice 4).
        if let Some((name, msg)) = self.type_level_must_use(ty) {
            self.diags.push(self.make_type_level_diag(expr, name, msg));
            return;
        }

        // Source 3: function-level `#[must_use]` (slice 4).
        if let Some((callee_name, msg)) = self.function_level_must_use(expr) {
            self.diags
                .push(self.make_function_level_diag(expr, callee_name, msg));
        }
    }

    /// Resolve a statement-position expression's type to a
    /// `(type_name, must_use_message)` pair when the type carries a
    /// slice-4 `#[must_use]` annotation. The lookup goes through
    /// `typed.struct_info` (for struct types) and `typed.enum_info`
    /// (for enum types) — both `HashMap<String, StructInfo|EnumInfo>`
    /// snapshots of the typechecker env at end-of-check. Result /
    /// Option are intentionally excluded here because source 1
    /// catches them with a tighter language-level message.
    fn type_level_must_use(&self, ty: &Type) -> Option<(String, String)> {
        let Type::Named { name, .. } = ty else {
            return None;
        };
        if implicit_must_use_kind(name).is_some() {
            return None;
        }
        if let Some(info) = self.typed.struct_info.get(name) {
            if let Some(msg) = &info.must_use_message {
                return Some((name.clone(), msg.clone()));
            }
        }
        if let Some(info) = self.typed.enum_info.get(name) {
            if let Some(msg) = &info.must_use_message {
                return Some((name.clone(), msg.clone()));
            }
        }
        None
    }

    /// Resolve a statement-position expression to a function-level
    /// `#[must_use]` annotation, when the discarded expression is a
    /// call whose callee is in `typed.must_use_functions`. Three call
    /// shapes resolve to a registry key:
    ///
    /// - `foo()` — `ExprKind::Call` with `callee = Identifier(name)`.
    ///   Registry key is `"name"`.
    /// - `Type.method()` — `ExprKind::Call` with
    ///   `callee = Path { segments: ["Type", "method"] }`. Registry
    ///   key is `"Type.method"`.
    /// - `obj.method()` — `ExprKind::MethodCall`. The canonical
    ///   `"Type.method"` key lives in
    ///   `typed.method_callee_types[span]` (populated by the
    ///   typechecker during `infer_method_call`).
    fn function_level_must_use(&self, expr: &Expr) -> Option<(String, String)> {
        let key = SpanKey::from_span(&expr.span);
        let lookup_key = match &expr.kind {
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Identifier(name) => name.clone(),
                ExprKind::Path { segments, .. } if segments.len() >= 2 => segments.join("."),
                _ => return None,
            },
            ExprKind::MethodCall { .. } => self.typed.method_callee_types.get(&key)?.clone(),
            _ => return None,
        };
        let entry = self.typed.must_use_functions.get(&lookup_key)?;
        Some((lookup_key, entry.clone().unwrap_or_default()))
    }

    fn make_implicit_diag(&self, expr: &Expr, kind: &str, why: &str) -> LintDiagnostic {
        LintDiagnostic {
            level: self.level,
            span: expr.span.clone(),
            message: format!("discarded `{kind}` value — `{kind}` is implicitly `#[must_use]`",),
            lint_name: "must_use".to_string(),
            help: Some(
                "bind the value with `let _ = ...` to acknowledge the discard \
                 explicitly, or consume it via `match` / `if let` so each variant \
                 is handled."
                    .to_string(),
            ),
            note: Some(format!(
                "`{kind}` is treated as implicitly `#[must_use]` because dropping it \
                 silently abandons {why} — the two language-level types `Result[T, E]` \
                 and `Option[T]` carry this recognition in the typechecker rather than \
                 as a user-visible `#[must_use]` attribute."
            )),
        }
    }

    fn make_type_level_diag(&self, expr: &Expr, type_name: String, msg: String) -> LintDiagnostic {
        let note = if msg.is_empty() {
            format!(
                "`{type_name}` is annotated `#[must_use]` (no author-supplied reason — the bare \
                 attribute form). Bind the value or consume it explicitly to acknowledge the \
                 discard."
            )
        } else {
            format!("`{type_name}` is annotated `#[must_use = \"{msg}\"]`. {msg}.")
        };
        LintDiagnostic {
            level: self.level,
            span: expr.span.clone(),
            message: format!(
                "discarded `{type_name}` value — `{type_name}` is annotated `#[must_use]`"
            ),
            lint_name: "must_use".to_string(),
            help: Some(
                "bind the value with `let _ = ...` to acknowledge the discard \
                 explicitly, or pass it to the consuming function."
                    .to_string(),
            ),
            note: Some(note),
        }
    }

    fn make_function_level_diag(
        &self,
        expr: &Expr,
        callee_name: String,
        msg: String,
    ) -> LintDiagnostic {
        let note = if msg.is_empty() {
            format!(
                "`{callee_name}` is annotated `#[must_use]` (no author-supplied reason). The \
                 return value is meaningful — bind it or pass it to the consuming function."
            )
        } else {
            format!("`{callee_name}` is annotated `#[must_use = \"{msg}\"]`. {msg}.")
        };
        LintDiagnostic {
            level: self.level,
            span: expr.span.clone(),
            message: format!(
                "discarded return value of `{callee_name}` — the function is annotated \
                 `#[must_use]`"
            ),
            lint_name: "must_use".to_string(),
            help: Some(
                "bind the value with `let _ = ...` to acknowledge the discard \
                 explicitly, or pass it to the consuming function."
                    .to_string(),
            ),
            note: Some(note),
        }
    }

    fn walk_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Block(block)
            | ExprKind::Unsafe(block)
            | ExprKind::Loop { body: block, .. }
            | ExprKind::LabeledBlock { body: block, .. }
            | ExprKind::Seq(block)
            | ExprKind::Par(block)
            | ExprKind::Try(block) => self.walk_block(block),
            ExprKind::Lock { body, .. } | ExprKind::Providers { body, .. } => {
                self.walk_block(body);
            }
            ExprKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                self.walk_expr(condition);
                self.walk_block(then_block);
                if let Some(e) = else_branch {
                    self.walk_expr(e);
                }
            }
            ExprKind::IfLet {
                value,
                then_block,
                else_branch,
                ..
            } => {
                self.walk_expr(value);
                self.walk_block(then_block);
                if let Some(e) = else_branch {
                    self.walk_expr(e);
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
                self.walk_expr(condition);
                self.walk_block(body);
            }
            ExprKind::For { iterable, body, .. } => {
                self.walk_expr(iterable);
                self.walk_block(body);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee);
                for arm in arms {
                    self.walk_match_arm(arm);
                }
            }
            ExprKind::Binary { left, right, .. } => {
                self.walk_expr(left);
                self.walk_expr(right);
            }
            ExprKind::Unary { operand, .. } => self.walk_expr(operand),
            ExprKind::NilCoalesce { left, right } | ExprKind::Pipe { left, right } => {
                self.walk_expr(left);
                self.walk_expr(right);
            }
            ExprKind::Call { callee, args } => {
                self.walk_expr(callee);
                for a in args {
                    self.walk_expr(&a.value);
                }
            }
            ExprKind::MethodCall { object, args, .. }
            | ExprKind::OptionalChain {
                object,
                args: Some(args),
                ..
            } => {
                self.walk_expr(object);
                for a in args {
                    self.walk_expr(&a.value);
                }
            }
            ExprKind::OptionalChain {
                object, args: None, ..
            } => self.walk_expr(object),
            ExprKind::FieldAccess { object, .. } | ExprKind::TupleIndex { object, .. } => {
                self.walk_expr(object);
            }
            ExprKind::Index { object, index } => {
                self.walk_expr(object);
                self.walk_expr(index);
            }
            ExprKind::Closure { body, .. } => self.walk_expr(body),
            ExprKind::Return(Some(e)) | ExprKind::Question(e) | ExprKind::Cast { expr: e, .. } => {
                self.walk_expr(e);
            }
            ExprKind::Break { value: Some(e), .. } => self.walk_expr(e),
            ExprKind::Tuple(elems) | ExprKind::ArrayLiteral(elems) => {
                for e in elems {
                    self.walk_expr(e);
                }
            }
            ExprKind::RepeatLiteral { value, count, .. } => {
                self.walk_expr(value);
                self.walk_expr(count);
            }
            ExprKind::PrefixCollectionLiteral { items, .. } => {
                for e in items {
                    self.walk_expr(e);
                }
            }
            ExprKind::MapLiteral(pairs) => {
                for (k, v) in pairs {
                    self.walk_expr(k);
                    self.walk_expr(v);
                }
            }
            ExprKind::StructLiteral { fields, spread, .. } => {
                for f in fields {
                    self.walk_field_init(f);
                }
                if let Some(s) = spread {
                    self.walk_expr(s);
                }
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(s) = start {
                    self.walk_expr(s);
                }
                if let Some(e) = end {
                    self.walk_expr(e);
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

    fn walk_match_arm(&mut self, arm: &MatchArm) {
        if let Some(guard) = &arm.guard {
            self.walk_expr(guard);
        }
        self.walk_expr(&arm.body);
    }

    fn walk_field_init(&mut self, f: &FieldInit) {
        self.walk_expr(&f.value);
    }
}
