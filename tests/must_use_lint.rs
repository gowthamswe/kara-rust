// tests/must_use_lint.rs
//
// Slice 1 of the `#[must_use]` mandate
// (`docs/implementation_checklist/phase-5-diagnostics.md` § `#[must_use]`
// mandate, slice 1): the two language-level types `Result[T, E]` and
// `Option[T]` are implicitly `#[must_use]`. Discarding a value of either
// type at statement position emits `warning[must_use]` with help / note
// continuation lines.
//
// The tests below pin the walker's discard-site coverage (positive
// cases) and its scoping (negative cases — bindings, tail expressions,
// non-must-use return types).

use karac::ast::{Item, Program};
use karac::must_use_lint::{check_implicit_must_use, LintDiagnostic, LintLevel};
use karac::prelude::STDLIB_SOURCES;
use karac::typechecker::TypeCheckResult;

fn parse_and_typecheck(source: &str) -> (Program, TypeCheckResult) {
    let parsed = karac::parse(source);
    assert!(
        parsed.errors.is_empty(),
        "Parse errors: {}",
        parsed
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let resolved = karac::resolve(&parsed.program);
    assert!(
        resolved.errors.is_empty(),
        "Resolve errors: {}",
        resolved
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let typed = karac::typecheck(&parsed.program, &resolved);
    (parsed.program, typed)
}

fn lint(source: &str) -> Vec<LintDiagnostic> {
    let (prog, typed) = parse_and_typecheck(source);
    check_implicit_must_use(&prog, Some(&typed))
}

fn assert_must_use_warning(diags: &[LintDiagnostic], needle: &str) {
    assert!(
        diags.iter().any(|d| d.lint_name == "must_use"
            && d.level == LintLevel::Warning
            && d.message.contains(needle)),
        "expected `must_use` warning containing '{needle}', got: {diags:?}"
    );
}

// ── Positive cases (must_use warning fires) ──────────────────────────

#[test]
fn test_discarded_option_call_warns() {
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller() { produce(); }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_must_use_warning(&diags, "discarded `Option` value");
}

#[test]
fn test_discarded_result_call_warns() {
    let diags = lint(
        "fn try_it() -> Result[i64, i64] { Result.Ok(7) }\n\
         fn caller() { try_it(); }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_must_use_warning(&diags, "discarded `Result` value");
}

#[test]
fn test_discarded_option_inside_unsafe_block_warns() {
    // The walker recurses into nested blocks. A discarded Option inside
    // an `unsafe { }` block is still a discarded must-use value — the
    // `unsafe` context controls trust for the *contained operation*,
    // not whether values can be silently dropped.
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller() { unsafe { produce(); } }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_must_use_warning(&diags, "discarded `Option` value");
}

#[test]
fn test_discarded_in_if_then_branch_warns() {
    // The then-block of an `if` is a nested block; the `;` after the
    // call inside it makes the call a statement-position expression.
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller(c: bool) { if c { produce(); } }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_must_use_warning(&diags, "discarded `Option` value");
}

#[test]
fn test_discarded_in_loop_body_warns() {
    let diags = lint(
        "fn produce() -> Option[i64] { Option.None }\n\
         fn caller() { loop { produce(); break; } }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_must_use_warning(&diags, "discarded `Option` value");
}

#[test]
fn test_multiple_discards_each_warn() {
    let diags = lint(
        "fn produce() -> Option[i64] { Option.None }\n\
         fn caller() { produce(); produce(); }",
    );
    assert_eq!(
        diags.len(),
        2,
        "expected two warnings (one per discard), got: {diags:?}"
    );
}

#[test]
fn test_discarded_method_call_returning_option_warns() {
    // The lint checks the *return type* of the statement-position
    // expression — the receiver doesn't matter, only the result type
    // recorded by the typechecker.
    let diags = lint(
        "struct S { x: i64 }\n\
         impl S { fn take(self) -> Option[i64] { Option.Some(self.x) } }\n\
         fn caller(s: S) { s.take(); }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_must_use_warning(&diags, "discarded `Option` value");
}

// ── Negative cases (must_use warning does NOT fire) ──────────────────

#[test]
fn test_let_binding_does_not_trigger() {
    // `let x = produce();` binds the value; no discard at this site.
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller() { let x = produce(); }",
    );
    assert!(diags.is_empty(), "expected no warnings, got: {diags:?}");
}

#[test]
fn test_let_underscore_discard_does_not_trigger() {
    // The canonical explicit-discard form. Slice 1 distinguishes
    // discard-at-statement-position from discard-by-explicit-binding:
    // the former is a hazard, the latter is the author saying "I
    // intentionally drop this".
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller() { let _ = produce(); }",
    );
    assert!(diags.is_empty(), "expected no warnings, got: {diags:?}");
}

#[test]
fn test_tail_expression_does_not_trigger() {
    // The block's `final_expr` flows as the block's value to its
    // consumer (here the function's return). The walker recurses
    // through `final_expr` but does not check it for discard.
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller() -> Option[i64] { produce() }",
    );
    assert!(diags.is_empty(), "expected no warnings, got: {diags:?}");
}

#[test]
fn test_return_does_not_trigger() {
    // `return produce();` — the expression flows out via the return.
    // The Return expression itself is the stmt-position expression and
    // has type `Never`, not `Option`, so it never matches the implicit-
    // must-use type set.
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller() -> Option[i64] { return produce(); }",
    );
    assert!(diags.is_empty(), "expected no warnings, got: {diags:?}");
}

#[test]
fn test_non_must_use_return_type_does_not_trigger() {
    // A discarded i64 call is not a must-use type — slice 1 is scoped
    // to `Result[T, E]` and `Option[T]`. Slice 4 will extend this to
    // user-annotated `#[must_use]` types.
    let diags = lint(
        "fn produce() -> i64 { 7 }\n\
         fn caller() { produce(); }",
    );
    assert!(diags.is_empty(), "expected no warnings, got: {diags:?}");
}

#[test]
fn test_method_call_returning_non_must_use_does_not_trigger() {
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller() { produce().is_some(); }",
    );
    // `produce()` is consumed by `.is_some()` — not discarded at stmt
    // position. The discarded value is `bool` (from `is_some`), which
    // is not implicit must-use.
    assert!(diags.is_empty(), "expected no warnings, got: {diags:?}");
}

#[test]
fn test_nested_in_let_value_does_not_trigger() {
    // The discarded-at-stmt-position rule is precise: a call appearing
    // as the right-hand side of a `let` is consumed by the binding,
    // even when the binding's pattern would itself discard. Slice 1
    // matches the language semantics, not a textual approximation.
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller() { let _opt = produce(); }",
    );
    assert!(diags.is_empty(), "expected no warnings, got: {diags:?}");
}

#[test]
fn test_let_binding_inside_block_with_discard_warns_once() {
    // Mixed body: one binding (consumed) plus one stmt-position
    // discard. Only the latter warns.
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller() {\n\
             let _x = produce();\n\
             produce();\n\
         }",
    );
    assert_eq!(
        diags.len(),
        1,
        "expected exactly one warning, got: {diags:?}"
    );
    assert_must_use_warning(&diags, "discarded `Option` value");
}

// ── Diagnostic shape ────────────────────────────────────────────────

#[test]
fn test_discarded_option_diagnostic_has_help_and_note() {
    let diags = lint(
        "fn produce() -> Option[i64] { Option.Some(7) }\n\
         fn caller() { produce(); }",
    );
    assert_eq!(diags.len(), 1);
    let d = &diags[0];
    let help = d
        .help
        .as_ref()
        .expect("must_use diagnostic should carry help");
    assert!(
        help.contains("let _ = "),
        "help should suggest `let _ = ...`, got: {help}"
    );
    assert!(
        help.contains("match") || help.contains("if let"),
        "help should mention pattern-matching alternatives, got: {help}"
    );
    let note = d
        .note
        .as_ref()
        .expect("must_use diagnostic should carry note");
    assert!(
        note.contains("`None` branch"),
        "note should explain why dropping Option is a hazard, got: {note}"
    );
    assert!(
        note.contains("language-level"),
        "note should pin that this is a language-level recognition, got: {note}"
    );
}

#[test]
fn test_discarded_result_diagnostic_has_help_and_note() {
    let diags = lint(
        "fn try_it() -> Result[i64, i64] { Result.Ok(7) }\n\
         fn caller() { try_it(); }",
    );
    assert_eq!(diags.len(), 1);
    let d = &diags[0];
    let note = d
        .note
        .as_ref()
        .expect("must_use diagnostic should carry note");
    assert!(
        note.contains("`Err` branch"),
        "note should explain why dropping Result is a hazard, got: {note}"
    );
}

// ── Slice 2 — baked-stdlib `#[must_use]` annotation pins ─────────────
//
// Slice 2 of the `#[must_use]` mandate
// (`docs/implementation_checklist/phase-5-diagnostics.md` § `#[must_use]`
// mandate, slice 2): apply `#[must_use]` to every iterator-adapter
// return type, every guard / lock type, every builder that isn't the
// terminal `.build()/.finish()`, `JoinHandle[T]`, and pure-transformation
// methods (case-by-case as stdlib lands). The attribute is inert in
// today's compiler — slice 4 wires the discard-site enforcement that
// reads it. These tests pin the annotations themselves (the bytes on
// disk + the parser's attribute-capture path) so that a regression
// dropping the attribute from a stdlib `.kara` file fails here rather
// than silently disabling the slice-4 warning once that lands.
//
// What's annotated at slice 2 in current v1 stdlib:
//   - `Peekable[T]` (peekable.kara) — iterator-adapter category
//   - `PooledConnection[T]` (pool.kara) — guard category (drop-releases-
//      automatically RAII handle, matches the MutexGuard / RwLockGuard /
//      RefCellGuard slot in the slice 2 spec)
//
// What's deferred to a later slice (per slice 2 spec's "(when builders
// ship)" / "(case-by-case as stdlib lands)" scoping):
//   - `MutexGuard` / `RwLockReadGuard` / `RwLockWriteGuard` /
//     `RefCellRefGuard` / `RefCellMutGuard` — Mutex / RwLock / RefCell
//      not in stdlib yet (P1 / Phase 6)
//   - `JoinHandle[T]` — not in stdlib (Phase 6)
//   - Iterator pseudo-struct (`Type::Named { name: "Iterator", … }` —
//     the return type of map / filter / take / skip / chain / zip /
//     enumerate / rev / flatten / flat_map / inspect / cycle / step_by /
//     vec.iter()) — registered programmatically in
//     `env_build.rs::register_compiler_intrinsic_env` with no baked-
//     source surface. Wiring the must-use intent here requires the
//     slice 4 `StructInfo.must_use_message` field; slice 4 picks it up.
//   - `String.to_lowercase` / `String.trim` / `String.replace` /
//     `Path.with_extension` — `String` and `Path` are not in stdlib yet.

fn parse_stdlib_file(file_basename: &str) -> Program {
    let src = STDLIB_SOURCES
        .iter()
        .find(|(name, _)| *name == file_basename)
        .unwrap_or_else(|| panic!("stdlib file '{file_basename}' missing from STDLIB_SOURCES"))
        .1;
    let parsed = karac::parse(src);
    assert!(
        parsed.errors.is_empty(),
        "Parse errors for stdlib file '{file_basename}': {}",
        parsed
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    parsed.program
}

fn find_struct<'a>(prog: &'a Program, name: &str) -> &'a karac::ast::StructDef {
    prog.items
        .iter()
        .find_map(|i| match i {
            Item::StructDef(s) if s.name == name => Some(s),
            _ => None,
        })
        .unwrap_or_else(|| panic!("struct `{name}` not found in stdlib file"))
}

#[test]
fn test_slice2_peekable_carries_must_use_annotation() {
    let prog = parse_stdlib_file("peekable.kara");
    let s = find_struct(&prog, "Peekable");
    let attr = s
        .attributes
        .iter()
        .find(|a| a.name == "must_use")
        .expect("Peekable[T] should carry #[must_use] (slice 2 — iterator-adapter category)");
    let msg = attr
        .string_value
        .as_deref()
        .expect("must_use attribute on Peekable should carry the spec-mandated message string");
    // Slice 2 spec mandates the exact message for iterator-adapter
    // return types: "discarding the iterator drops every adapter
    // without running it — chain a terminal method or bind the
    // result". Pin enough of it that a drift (rewording, dropping
    // the actionable half) trips this test.
    assert!(
        msg.contains("discarding the iterator"),
        "must_use message should name the discard hazard, got: {msg:?}"
    );
    assert!(
        msg.contains("terminal method") && msg.contains("bind"),
        "must_use message should offer the canonical fixes (terminal method / bind), got: {msg:?}"
    );
}

#[test]
fn test_slice2_pooled_connection_carries_must_use_annotation() {
    let prog = parse_stdlib_file("pool.kara");
    let s = find_struct(&prog, "PooledConnection");
    let attr = s
        .attributes
        .iter()
        .find(|a| a.name == "must_use")
        .expect("PooledConnection[T] should carry #[must_use] (slice 2 — guard category)");
    let msg = attr
        .string_value
        .as_deref()
        .expect("must_use attribute on PooledConnection should carry a guard-shaped message");
    // Guard-category message should explain the wasted-acquire hazard
    // (slot released back without using the connection) and offer the
    // canonical fix (bind to a variable or pass-through).
    assert!(
        msg.contains("connection") && msg.contains("slot"),
        "must_use message should name the guard's resource (connection / slot), got: {msg:?}"
    );
    assert!(
        msg.contains("bind") || msg.contains("pass"),
        "must_use message should offer the canonical fix (bind / pass-through), got: {msg:?}"
    );
}

#[test]
fn test_slice2_pool_struct_does_not_carry_must_use() {
    // Negative-space pin: the `Pool[T]` constructor handle itself is
    // NOT must-use (it's a long-lived resource the caller stores, not
    // a guard / adapter). Catches an over-broad future edit that
    // accidentally annotates every type in `pool.kara`.
    let prog = parse_stdlib_file("pool.kara");
    let s = find_struct(&prog, "Pool");
    assert!(
        s.attributes.iter().all(|a| a.name != "must_use"),
        "Pool[T] should NOT carry #[must_use] (only PooledConnection[T] does)"
    );
}

#[test]
fn test_slice2_vec_does_not_carry_must_use() {
    // Negative-space pin: data containers (Vec, Set, Map, …) are not
    // in the slice 2 scope. Slice 2 covers iterator adapters and
    // guards; the containers themselves are freely droppable. Catches
    // a future over-application of `#[must_use]` to plain collections.
    let prog = parse_stdlib_file("vec.kara");
    let s = find_struct(&prog, "Vec");
    assert!(
        s.attributes.iter().all(|a| a.name != "must_use"),
        "Vec[T] should NOT carry #[must_use] (data container, not guard / adapter)"
    );
}

#[test]
fn test_slice2_sender_and_receiver_do_not_carry_must_use() {
    // Negative-space pin: channel halves (Sender / Receiver) are
    // long-lived resource handles the caller stores and passes
    // around, not consume-on-acquire guards. Slice 2 doesn't list
    // them.
    for (basename, struct_name) in [("sender.kara", "Sender"), ("receiver.kara", "Receiver")] {
        let prog = parse_stdlib_file(basename);
        let s = find_struct(&prog, struct_name);
        assert!(
            s.attributes.iter().all(|a| a.name != "must_use"),
            "{struct_name}[T] should NOT carry #[must_use] (channel half, not a guard)"
        );
    }
}
