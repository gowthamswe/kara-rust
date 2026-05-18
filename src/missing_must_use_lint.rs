// src/missing_must_use_lint.rs
//! `missing_must_use` lint — slice 3 of the `#[must_use]` mandate
//! (`docs/implementation_checklist/phase-5-diagnostics.md` § `#[must_use]`
//! mandate, slice 3).
//!
//! **Role.** Stdlib-hygiene lint. Warns when a baked stdlib function
//! returns a value that the caller should not silently discard but is
//! NOT annotated with `#[must_use]`. The lint exists so the stdlib
//! maintainer (the karac developer, not the end-user) catches missing
//! `#[must_use]` attributes mechanically — the slice 2 annotations
//! land per-type, but the per-function annotations (builders,
//! constructors, pure-transformation methods) are case-by-case and
//! easy to forget when adding a new stdlib surface.
//!
//! **Heuristics.** Two fire today (the third — guard-shaped via a
//! `Guard` marker trait — is deferred until that trait lands):
//!
//! 1. *Iterator-adapter return.* The function returns an iterator-
//!    shaped type (`Iterator[Item = …]` or `Peekable[T]`). Discarding
//!    an iterator drops the adapter chain without ever running it.
//!
//! 2. *New-value-from-self.* The function takes `ref self` or no `self`
//!    (so the receiver is *not* consumed) and returns a non-trivial
//!    value (≠ `Unit`, ≠ `Self`, ≠ already-implicit-must-use such as
//!    `Result` / `Option`). This catches builders that mint a fresh
//!    value (`Command.new(prog) -> Command`), pure-transformation
//!    accessors (`Stats.mean(xs: Slice[f64]) -> f64`), and
//!    information-only queries (`Ordering.is_lt(ref self) -> bool`).
//!    Builders that *consume* the receiver (`Command.arg(self, …) ->
//!    Command`) are intentionally excluded — the spec scopes the
//!    receiver check to `ref self` or no `self` precisely because
//!    `own self` builders return the consumed-and-respun receiver, and
//!    the lint would have a higher false-positive rate if it widened
//!    to those.
//!
//! **Scope (stdlib only).** The lint walks every `Item::Function` and
//! every inherent-impl method (`ImplItem::Method` inside an
//! `Item::ImplBlock` whose `trait_name` is `None`) and fires only when
//! `Function.stdlib_origin == true`. Trait method declarations
//! (`TraitItem::Method`) and trait-impl methods (impl blocks where
//! `trait_name = Some(...)`) are intentionally skipped — the
//! `#[must_use]` semantic on a trait method belongs on the trait
//! declaration itself, not on every implementation, so linting impls
//! would be noisy. User code (`stdlib_origin == false`) is silent by
//! design (the spec's "allow-for-user-code" wording); when the lint-
//! level-attributes infrastructure lands, user code can opt in via
//! `#[deny(missing_must_use)]`.
//!
//! **What's already excluded.** Functions already carrying
//! `#[must_use]` or `#[allow(missing_must_use)]` are skipped. Returns
//! of `Result[T, E]` / `Option[T]` are skipped (those types are
//! implicitly must-use per slice 1; firing the missing-`#[must_use]`
//! warning on top of the discard-site warning would be redundant
//! noise). Returns of `Unit`, `Self`, or the impl-target type are
//! skipped (the spec's `≠ ()` / `≠ Self` clauses).
//!
//! **Diagnostic shape** matches `must_use_lint`'s rustc-style three-
//! piece (primary / `= note:` / `= help:`) so the CLI rendering helper
//! introduced in slice 1 (`render_must_use_lint_diag`) carries across.
//! The lint's `LintDiagnostic` is intentionally structurally
//! identical to `must_use_lint::LintDiagnostic` so a future lint-
//! registry refactor (per the deferred "Lint level attributes" entry
//! in phase-5-diagnostics.md) can unify them without re-tooling
//! callers.

use crate::ast::{
    Attribute, ExprKind, Function, ImplBlock, ImplItem, Item, Program, TypeExpr, TypeKind,
};
use crate::token::Span;

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
    pub help: Option<String>,
    pub note: Option<String>,
}

/// Reason the lint fired on a given function — surfaced inside the
/// primary diagnostic so the user sees *why* the heuristic matched.
/// Both variants produce the same lint name (`missing_must_use`); the
/// message and help text differ to point the reader at the right fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FireReason {
    IteratorReturn,
    NewValueFromSelf,
}

/// Run the `missing_must_use` lint over the parsed program.
///
/// Walks every `Item::Function` and every inherent-impl method. Fires
/// only on items where `Function.stdlib_origin == true` — see the
/// module-level comment for the stdlib-vs-user scoping rationale.
pub fn check_missing_must_use(
    program: &Program,
    cli_lint_overrides: &crate::lints::CliLintOverrides,
) -> Vec<LintDiagnostic> {
    // Slice 4b cross-cutting — `-A missing_must_use` short-circuits
    // the whole pass; `-D missing_must_use` (or `-D warnings`)
    // promotes every emission to Error. The per-function `#[allow]`
    // and `#[deny]` checks layered on top of this still let
    // individual stdlib `pub fn`s opt out (or opt into stricter
    // treatment) — source attributes win over CLI per the cascade
    // precedence rule.
    let cli_severity = crate::lints::effective_level_for_module_lint(
        false,
        false,
        false,
        cli_lint_overrides,
        "missing_must_use",
    );
    if matches!(cli_severity, crate::lints::ModuleLintSeverity::Suppress) {
        return Vec::new();
    }
    let default_level = match cli_severity {
        crate::lints::ModuleLintSeverity::Deny => LintLevel::Error,
        _ => LintLevel::Warning,
    };
    let mut diags: Vec<LintDiagnostic> = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => {
                check_free_function(f, default_level, cli_lint_overrides, &mut diags)
            }
            Item::ImplBlock(imp) => {
                check_impl_block(imp, default_level, cli_lint_overrides, &mut diags)
            }
            _ => {}
        }
    }
    diags
}

fn check_free_function(
    f: &Function,
    default_level: LintLevel,
    cli: &crate::lints::CliLintOverrides,
    diags: &mut Vec<LintDiagnostic>,
) {
    if !f.stdlib_origin {
        return;
    }
    // Free functions: the visibility check is meaningful — `pub fn`
    // is the public surface. Project-internal stdlib helpers (rare in
    // practice but possible) are out of scope.
    if !f.is_pub {
        return;
    }
    check_function_with_impl_target(f, None, default_level, cli, diags);
}

fn check_impl_block(
    imp: &ImplBlock,
    default_level: LintLevel,
    cli: &crate::lints::CliLintOverrides,
    diags: &mut Vec<LintDiagnostic>,
) {
    // Trait-impl methods inherit `#[must_use]` from the trait
    // declaration (when slice 4 wires the must-use registry, the
    // attribute on the trait method flows to every impl). Linting
    // them here would force every implementor to carry the attribute
    // separately — wrong layering. Skip.
    if imp.trait_name.is_some() {
        return;
    }
    // Inherent impl: the methods are the type's public surface. No
    // per-method `is_pub` check — the convention across baked stdlib
    // is to omit `pub` on inherent methods when the impl target is
    // pub (see `runtime/stdlib/pool.kara`, `runtime/stdlib/process.kara`
    // — every method on `impl Command` etc. lacks `pub`). The impl-
    // target type's visibility is the gate; today the lint applies
    // uniformly to every baked-stdlib inherent method.
    let target = &imp.target_type;
    for it in &imp.items {
        if let ImplItem::Method(m) = it {
            if !m.stdlib_origin {
                continue;
            }
            check_function_with_impl_target(m, Some(target), default_level, cli, diags);
        }
    }
}

fn check_function_with_impl_target(
    f: &Function,
    impl_target: Option<&TypeExpr>,
    default_level: LintLevel,
    cli: &crate::lints::CliLintOverrides,
    diags: &mut Vec<LintDiagnostic>,
) {
    // Already-annotated functions are out of scope — slice 4's
    // discard-site enforcement reads the attribute. The lint exists
    // to catch the missing annotation, not to second-guess it.
    if has_attr_named(&f.attributes, "must_use") {
        return;
    }
    // Per-function source-attribute cascade. The function's
    // `#[allow]` / `#[deny]` / `#[expect]` on `missing_must_use`
    // override the CLI default — source is more specific than CLI.
    // `#[warn]` is the default behavior; recognising it as a
    // no-op keeps round-trip semantics.
    let source_allow = has_lint_allow_attr(&f.attributes, "missing_must_use");
    let source_deny = has_lint_level_attr(&f.attributes, "deny", "missing_must_use");
    let source_expect = has_lint_level_attr(&f.attributes, "expect", "missing_must_use");
    let per_fn_severity = crate::lints::effective_level_for_module_lint(
        source_allow,
        source_deny,
        source_expect,
        cli,
        "missing_must_use",
    );
    let level = match per_fn_severity {
        crate::lints::ModuleLintSeverity::Suppress => return,
        crate::lints::ModuleLintSeverity::Deny => LintLevel::Error,
        crate::lints::ModuleLintSeverity::Warn => default_level,
    };
    let Some(return_ty) = &f.return_type else {
        return;
    };
    // Skip returns we already cover or where firing would be wrong.
    if is_unit_or_error_return(return_ty) {
        return;
    }
    if is_implicit_must_use_return(return_ty) {
        // `Result[T, E]` / `Option[T]` — slice 1 covers the discard
        // hazard from the *consumer* side. Adding a `missing_must_use`
        // warning on top would be duplicative noise.
        return;
    }
    if is_self_or_impl_target_return(return_ty, impl_target) {
        // The spec's `≠ Self` clause: consuming-self builders
        // (`Command.arg(self, …) -> Command`) and Self-returning
        // trait methods are explicitly out of scope. The receiver
        // check below also catches consuming-self, but rejecting on
        // return type here handles `fn new(…) -> ImplTarget` (no
        // self, returns the impl target). For constructors the
        // heuristic actually wants those, but the spec excludes
        // Self-returning shapes uniformly — staying inside the spec.
        return;
    }
    let reason = classify(f, return_ty);
    let Some(reason) = reason else {
        return;
    };
    diags.push(make_diagnostic(f, reason, level));
}

/// Apply the two slice-3 heuristics in order: iterator-adapter return
/// first (the more specific signal), then new-value-from-self. Returns
/// `None` if neither matches — the lint stays silent.
fn classify(f: &Function, return_ty: &TypeExpr) -> Option<FireReason> {
    if is_iterator_shaped_return(return_ty) {
        return Some(FireReason::IteratorReturn);
    }
    // New-value-from-self: receiver is `ref self` or no receiver.
    // Consuming-self (`SelfParam::Owned`) and mutating-self
    // (`SelfParam::MutRef`) are intentionally excluded — the spec
    // scopes the check to read-only / no-receiver shapes because
    // those are where forgetting to bind the return is unambiguously
    // a bug (the original value is still in scope, so the discard is
    // wasted compute, not lost state).
    match f.self_param {
        None | Some(crate::ast::SelfParam::Ref) => Some(FireReason::NewValueFromSelf),
        Some(crate::ast::SelfParam::Owned) | Some(crate::ast::SelfParam::MutRef) => None,
    }
}

fn make_diagnostic(f: &Function, reason: FireReason, level: LintLevel) -> LintDiagnostic {
    let (message, help, note) = match reason {
        FireReason::IteratorReturn => (
            format!(
                "stdlib `fn {}` returns an iterator-shaped value but is not annotated `#[must_use]`",
                f.name
            ),
            Some(
                "annotate with `#[must_use = \"discarding the iterator drops every adapter without running it — chain a terminal method or bind the result\"]` (the slice 2 spec-mandated message for iterator-adapter return types)."
                    .to_string(),
            ),
            Some(
                "iterator-shaped returns are a stdlib hygiene concern: discarding the value silently loses every adapter the user chained onto it. The `missing_must_use` lint catches the case where the stdlib forgot to annotate; slice 4 of the `#[must_use]` mandate wires the discard-site enforcement that reads the attribute."
                    .to_string(),
            ),
        ),
        FireReason::NewValueFromSelf => (
            format!(
                "stdlib `fn {}` returns a new value but is not annotated `#[must_use]`",
                f.name
            ),
            Some(
                "annotate with `#[must_use = \"<reason discarding the value is a hazard>\"]` (the message is consumer-facing; name the wasted operation specifically — e.g. \"discarding the builder loses every chained call\" or \"discarding the computed statistic wastes the traversal\")."
                    .to_string(),
            ),
            Some(
                "the function takes `ref self` (or no `self`) and returns a non-trivial value (≠ `()`, ≠ `Self`, ≠ `Result` / `Option`). This is the spec's *new-value-from-self* heuristic: it catches constructors (`Type.new(...) -> Type`'s consumer-bound siblings), pure-transformation accessors, and information-only queries where silently dropping the return is almost always a bug. If the heuristic mis-fires on a genuinely-droppable result, suppress with `#[allow(missing_must_use)]` once the lint-level-attributes framework lands."
                    .to_string(),
            ),
        ),
    };
    LintDiagnostic {
        level,
        span: f.span.clone(),
        message,
        lint_name: "missing_must_use".to_string(),
        help,
        note,
    }
}

// ── Attribute helpers ────────────────────────────────────────────────

fn has_attr_named(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|a| a.is_bare(name))
}

/// Recognise `#[allow(missing_must_use)]` in either of the two surface
/// forms the parser produces (positional or named) — mirrors the shape
/// of `unsafe_lint::has_lint_attr`. When the lint-level-attributes
/// framework lands and grows `#[expect(missing_must_use)]` / per-
/// module configuration, extend this helper.
fn has_lint_allow_attr(attrs: &[Attribute], rule_name: &str) -> bool {
    has_lint_level_attr(attrs, "allow", rule_name)
}

/// Generalised variant of `has_lint_allow_attr` — recognise any of
/// the four lint-level attributes (`#[allow]` / `#[warn]` / `#[deny]`
/// / `#[expect]`) naming `rule_name`. Slice 4b cross-cutting added
/// this so the per-function effective-level computation can read
/// `#[deny(missing_must_use)]` and `#[expect(missing_must_use)]`
/// alongside the existing `#[allow]` form.
fn has_lint_level_attr(attrs: &[Attribute], level_kind: &str, rule_name: &str) -> bool {
    attrs.iter().any(|a| {
        if !a.is_bare(level_kind) {
            return false;
        }
        a.args.iter().any(|arg| {
            arg.name.as_deref() == Some(rule_name)
                || arg
                    .value
                    .as_ref()
                    .map(|v| matches!(&v.kind, ExprKind::Identifier(n) if n == rule_name))
                    .unwrap_or(false)
        })
    })
}

// ── Return-type classifiers ──────────────────────────────────────────

fn is_unit_or_error_return(ty: &TypeExpr) -> bool {
    matches!(ty.kind, TypeKind::Unit | TypeKind::Error)
}

/// Match `Result[…]` and `Option[…]` — already implicitly `#[must_use]`
/// per slice 1. The name lookup is intentionally a single-segment
/// match because both types live at scope-0; multi-segment paths (e.g.
/// `std.option.Option`) are unusual in stdlib source and not worth the
/// extra plumbing today.
fn is_implicit_must_use_return(ty: &TypeExpr) -> bool {
    let TypeKind::Path(p) = &ty.kind else {
        return false;
    };
    matches!(
        p.segments.last().map(String::as_str),
        Some("Result" | "Option")
    )
}

/// Match `Self` (trait-method shape) OR the literal name of the impl
/// target (`Command.arg(self, …) -> Command` — return type names the
/// impl target). The impl-target comparison covers consuming-self
/// builders before the receiver check has a chance; the receiver
/// check is the primary gate for these, but the return-type gate
/// catches static factories that name the type explicitly (e.g.,
/// `fn new() -> Command` on `impl Command { ... }`).
fn is_self_or_impl_target_return(ty: &TypeExpr, impl_target: Option<&TypeExpr>) -> bool {
    let TypeKind::Path(p) = &ty.kind else {
        return false;
    };
    if p.segments == ["Self"] {
        return true;
    }
    let Some(target) = impl_target else {
        return false;
    };
    let TypeKind::Path(tp) = &target.kind else {
        return false;
    };
    p.segments == tp.segments
}

/// Match the in-typechecker iterator-adapter shapes:
///   - `Iterator[Item = …]` — every adapter return type collapses to
///     this in `src/typechecker/stdlib_iter.rs`
///   - `Peekable[T]` — the one adapter that ships with its own baked
///     struct (slice 2 annotation)
fn is_iterator_shaped_return(ty: &TypeExpr) -> bool {
    let TypeKind::Path(p) = &ty.kind else {
        return false;
    };
    matches!(
        p.segments.last().map(String::as_str),
        Some("Iterator" | "Peekable")
    )
}
