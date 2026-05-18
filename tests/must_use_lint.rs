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
    check_implicit_must_use(
        &prog,
        Some(&typed),
        &karac::lints::CliLintOverrides::default(),
    )
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
        .find(|a| a.is_bare("must_use"))
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
        .find(|a| a.is_bare("must_use"))
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
        s.attributes.iter().all(|a| !a.is_bare("must_use")),
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
        s.attributes.iter().all(|a| !a.is_bare("must_use")),
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
            s.attributes.iter().all(|a| !a.is_bare("must_use")),
            "{struct_name}[T] should NOT carry #[must_use] (channel half, not a guard)"
        );
    }
}

// ── Slice 4 — General `#[must_use]` honoring (registry-backed) ─────
//
// Slice 4 of the `#[must_use]` mandate
// (`docs/implementation_checklist/phase-5-diagnostics.md` § `#[must_use]`
// mandate, slice 4) generalises slice 1's discard-site check to honour
// `#[must_use]` on arbitrary user-defined types (via `StructInfo` /
// `EnumInfo`'s new `must_use_message` field), on the Iterator pseudo-
// struct (annotated programmatically in
// `register_compiler_intrinsic_env`), and on functions (via
// `TypeCheckResult.must_use_functions`, populated from `env_add_function`
// and `env_add_impl`). The three sources layer in priority order:
// implicit (Result/Option) > type-level > function-level — see the
// `check_discard` doc-comment in `src/must_use_lint.rs`.
//
// The tests below pin every layer plus their interaction (precedence,
// suppression by let-binding, no-double-fire for chained calls).

fn assert_warns_with(diags: &[LintDiagnostic], expected_message_substring: &str) {
    assert!(
        diags.iter().any(|d| d.lint_name == "must_use"
            && d.level == LintLevel::Warning
            && d.message.contains(expected_message_substring)),
        "expected `must_use` warning containing '{expected_message_substring}', got: {diags:?}"
    );
}

// ── Type-level: Iterator pseudo-struct annotation ────────────────────

#[test]
fn test_discarded_iterator_chain_warns_via_type_level_must_use() {
    // `vec.iter().map(|x| x + 1);` — the chain's tail expression type
    // is `Iterator[i64]`, which the slice 4 Iterator pseudo-struct
    // annotation (set in `register_compiler_intrinsic_env`) marks as
    // must-use. The lint fires at the discard site with the slice 2
    // spec-mandated message about dropping the adapter chain.
    let diags = lint(
        "fn caller() {\n\
             let v = [1_i64, 2, 3, 4];\n\
             v.iter().map(|x| x + 1);\n\
         }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_warns_with(&diags, "Iterator");
    let note = diags[0].note.as_ref().unwrap();
    assert!(
        note.contains("terminal method") || note.contains("adapter"),
        "note should mention the slice 2 spec wording about adapter/terminal, got: {note}"
    );
}

#[test]
fn test_iterator_chain_with_terminal_collect_does_not_warn() {
    // `vec.iter().map(|x| x + 1).collect();` — the chain's tail
    // expression type is `Vec[i64]`. Vec isn't must-use, so the
    // discard is silent. Pins that the type-level check looks at the
    // OUTERMOST expression's type, not at intermediate adapter
    // returns: the `.collect()` consumes the iterator, so the bug
    // the lint guards against ("you forgot a terminal") doesn't
    // apply.
    let diags = lint(
        "fn caller() {\n\
             let v = [1_i64, 2, 3, 4];\n\
             v.iter().map(|x| x + 1).collect();\n\
         }",
    );
    assert!(
        diags.is_empty(),
        "terminal `.collect()` should silence the iterator must-use warning, got: {diags:?}"
    );
}

#[test]
fn test_let_binding_iterator_chain_does_not_warn() {
    // `let it = vec.iter().map(...);` — bound, not discarded. The
    // walker doesn't check `StmtKind::Let` values for must-use; the
    // value flows into the binding's scope.
    let diags = lint(
        "fn caller() {\n\
             let v = [1_i64, 2, 3, 4];\n\
             let it = v.iter().map(|x| x + 1);\n\
             for x in it { let _y = x; }\n\
         }",
    );
    assert!(
        diags.is_empty(),
        "let-bound iterator should not fire must-use, got: {diags:?}"
    );
}

// ── Type-level: user-authored `#[must_use]` on struct/enum ───────────

#[test]
fn test_discarded_user_struct_with_must_use_attribute_warns() {
    // User-authored `#[must_use = "..."]` on a struct: the slice 4
    // registry path picks up `StructInfo.must_use_message` and fires
    // the type-level diagnostic with the author's reason in the
    // `note:` line.
    let diags = lint(
        "#[must_use = \"loses the slot back to the pool\"]\n\
         struct Token { x: i64 }\n\
         fn make() -> Token { Token { x: 7 } }\n\
         fn caller() { make(); }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_warns_with(&diags, "Token");
    let note = diags[0].note.as_ref().unwrap();
    assert!(
        note.contains("loses the slot back to the pool"),
        "note should surface the author's reason string verbatim, got: {note}"
    );
    let help = diags[0].help.as_ref().unwrap();
    assert!(
        help.contains("let _ = "),
        "help should offer the canonical `let _ = ...` fix, got: {help}"
    );
}

#[test]
fn test_discarded_user_enum_with_must_use_attribute_warns() {
    // Same shape on an enum declaration. Slice 4 populates
    // `EnumInfo.must_use_message` symmetrically with `StructInfo`.
    let diags = lint(
        "#[must_use = \"every variant carries a hazard\"]\n\
         enum Status { Ok, Pending, Failed }\n\
         fn make() -> Status { Status.Ok }\n\
         fn caller() { make(); }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_warns_with(&diags, "Status");
    let note = diags[0].note.as_ref().unwrap();
    assert!(note.contains("every variant carries a hazard"));
}

#[test]
fn test_bare_must_use_attribute_on_struct_warns_with_default_note() {
    // Bare `#[must_use]` (no string value) — `extract_must_use_message`
    // returns `Some("")`. The walker renders a generic "no author-
    // supplied reason" note rather than echoing an empty string.
    let diags = lint(
        "#[must_use]\n\
         struct Handle { id: i64 }\n\
         fn make() -> Handle { Handle { id: 0 } }\n\
         fn caller() { make(); }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    let note = diags[0].note.as_ref().unwrap();
    assert!(
        note.contains("no author-supplied reason"),
        "bare `#[must_use]` should render the no-reason fallback note, got: {note}"
    );
}

#[test]
fn test_user_struct_without_must_use_does_not_warn() {
    // Negative-space pin: a plain struct without the attribute is
    // freely droppable. Catches an over-broad lookup that fires on
    // every `Type::Named` regardless of the must_use_message field.
    let diags = lint(
        "struct Plain { x: i64 }\n\
         fn make() -> Plain { Plain { x: 7 } }\n\
         fn caller() { make(); }",
    );
    assert!(
        diags.is_empty(),
        "plain struct should be silent, got: {diags:?}"
    );
}

// ── Function-level: free function with `#[must_use]` ─────────────────

#[test]
fn test_discarded_free_function_with_must_use_warns() {
    // Free `pub fn` annotated `#[must_use]` returning a non-must-use
    // type. The type-level check is silent (i64 has no
    // must_use_message); the function-level check fires.
    let diags = lint(
        "#[must_use = \"the computed value is the only point of calling\"]\n\
         pub fn compute() -> i64 { 42 }\n\
         fn caller() { compute(); }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_warns_with(&diags, "compute");
    let note = diags[0].note.as_ref().unwrap();
    assert!(
        note.contains("the computed value is the only point of calling"),
        "note should surface the author's reason, got: {note}"
    );
    let message = &diags[0].message;
    assert!(
        message.contains("discarded return value"),
        "message should name the function-level shape, got: {message}"
    );
}

#[test]
fn test_function_without_must_use_does_not_warn() {
    let diags = lint(
        "pub fn compute() -> i64 { 42 }\n\
         fn caller() { compute(); }",
    );
    assert!(
        diags.is_empty(),
        "plain fn return should be silent, got: {diags:?}"
    );
}

#[test]
fn test_let_bound_must_use_function_call_does_not_warn() {
    // `let x = compute();` — bound, even though `compute` is
    // `#[must_use]`. Slice 1's `StmtKind::Let` exclusion carries over
    // to slice 4 unchanged: the value flows into a binding, the
    // discard hazard doesn't apply.
    let diags = lint(
        "#[must_use]\n\
         pub fn compute() -> i64 { 42 }\n\
         fn caller() {\n\
             let x = compute();\n\
             let _y = x;\n\
         }",
    );
    assert!(
        diags.is_empty(),
        "let-bound must-use call should be silent, got: {diags:?}"
    );
}

// ── Function-level: impl methods with `#[must_use]` ──────────────────

#[test]
fn test_discarded_static_method_with_must_use_warns() {
    // `Type.factory()` resolves through `ExprKind::Call` with
    // `callee = Path { segments: ["Type", "factory"] }`. The walker
    // joins the segments and looks up `"Type.factory"` in
    // `must_use_functions`. `env_add_impl` registers the entry
    // when the method carries `#[must_use]`.
    let diags = lint(
        "struct Builder { x: i64 }\n\
         impl Builder {\n\
             #[must_use = \"the builder needs a finalising call\"]\n\
             fn new() -> Builder { Builder { x: 0 } }\n\
         }\n\
         fn caller() { Builder.new(); }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_warns_with(&diags, "Builder.new");
    let note = diags[0].note.as_ref().unwrap();
    assert!(note.contains("finalising call"));
}

#[test]
fn test_discarded_instance_method_with_must_use_warns() {
    // `obj.method()` — `ExprKind::MethodCall`. The canonical
    // `"Type.method"` key lives in `method_callee_types` (populated
    // by the typechecker during `infer_method_call`). The walker
    // looks it up and threads it through the function-level lookup.
    let diags = lint(
        "struct Acc { total: i64 }\n\
         impl Acc {\n\
             fn new() -> Acc { Acc { total: 0 } }\n\
             #[must_use = \"the accumulated total is what callers want\"]\n\
             fn finalize(ref self) -> i64 { self.total }\n\
         }\n\
         fn caller() {\n\
             let a = Acc.new();\n\
             a.finalize();\n\
         }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    assert_warns_with(&diags, "Acc.finalize");
    let note = diags[0].note.as_ref().unwrap();
    assert!(note.contains("accumulated total"));
}

// ── Precedence: type-level beats function-level ──────────────────────

#[test]
fn test_function_returning_must_use_type_prefers_type_level_diag() {
    // A `#[must_use]` function returning a `#[must_use]` type fires
    // exactly one warning — the type-level diagnostic, which carries
    // the more specific message about the value being discarded. The
    // function-level fallback would be noise when the type-level
    // message is already present.
    let diags = lint(
        "#[must_use = \"the wrapper carries the hazard\"]\n\
         struct Wrap { x: i64 }\n\
         #[must_use = \"the function also says don't drop\"]\n\
         pub fn produce() -> Wrap { Wrap { x: 7 } }\n\
         fn caller() { produce(); }",
    );
    assert_eq!(
        diags.len(),
        1,
        "expected exactly one warning (no double-fire), got: {diags:?}"
    );
    let note = diags[0].note.as_ref().unwrap();
    assert!(
        note.contains("the wrapper carries the hazard"),
        "should surface the type-level message (more specific), got: {note}"
    );
    assert!(
        !note.contains("the function also says don't drop"),
        "function-level message should NOT appear when type-level fires, got: {note}"
    );
}

#[test]
fn test_result_discard_prefers_implicit_slice_1_diag() {
    // Even if a Result-returning function carries `#[must_use]`, the
    // implicit (slice 1) check fires first with the language-level
    // "Err branch" wording. This is the test case the slice 4 spec
    // calls out: "Result / Option discard warns regardless of
    // attribute (continues to fire via slice 1's check)".
    let diags = lint(
        "#[must_use = \"function-level reason\"]\n\
         pub fn try_it() -> Result[i64, i64] { Result.Ok(7) }\n\
         fn caller() { try_it(); }",
    );
    assert_eq!(diags.len(), 1, "expected one warning, got: {diags:?}");
    let note = diags[0].note.as_ref().unwrap();
    assert!(
        note.contains("`Err` branch") && note.contains("language-level"),
        "should fire the slice 1 implicit diagnostic, got: {note}"
    );
    assert!(
        !note.contains("function-level reason"),
        "should NOT fall through to the function-level diagnostic, got: {note}"
    );
}

// ── Iterator pseudo-struct registry pin ──────────────────────────────

#[test]
fn test_iterator_pseudo_struct_carries_must_use_in_typechecker_env() {
    // Direct env-side pin: after typecheck, the Iterator pseudo-struct
    // in `TypeCheckResult.struct_info` carries `must_use_message`.
    // Guards the `register_compiler_intrinsic_env` setup so a future
    // refactor that drops the annotation surfaces here instead of
    // silently disabling the warning chain.
    let (_prog, typed) = parse_and_typecheck("fn main() { }");
    let info = typed
        .struct_info
        .get("Iterator")
        .expect("Iterator pseudo-struct should be registered after typecheck");
    let msg = info
        .must_use_message
        .as_ref()
        .expect("Iterator should carry must_use_message after slice 4");
    assert!(
        msg.contains("terminal method") && msg.contains("bind the result"),
        "Iterator must_use_message should match the slice 2 spec wording, got: {msg}"
    );
}

#[test]
fn test_peekable_baked_struct_carries_must_use_in_typechecker_env() {
    // Parallel pin for `Peekable[T]` — baked source carries the
    // `#[must_use = "..."]` attribute (shipped slice 2);
    // `env_add_struct` reads it via `extract_must_use_message` and
    // populates `StructInfo.must_use_message`.
    let (_prog, typed) = parse_and_typecheck("fn main() { }");
    let info = typed
        .struct_info
        .get("Peekable")
        .expect("Peekable should be registered after typecheck");
    let msg = info
        .must_use_message
        .as_ref()
        .expect("Peekable should carry must_use_message after slice 4");
    assert!(
        msg.contains("terminal method"),
        "Peekable must_use_message should match the slice 2 wording, got: {msg}"
    );
}

#[test]
fn test_must_use_functions_registry_populates_from_free_function() {
    // Env-side pin: `env_add_function` writes the entry into
    // `TypeEnv.must_use_functions`, which `TypeCheckResult` snapshots.
    let (_prog, typed) = parse_and_typecheck(
        "#[must_use = \"why\"]\n\
         pub fn produce() -> i64 { 7 }\n\
         fn main() { }",
    );
    let entry = typed
        .must_use_functions
        .get("produce")
        .expect("free fn with #[must_use] should be in registry");
    assert_eq!(entry.as_deref(), Some("why"));
}

#[test]
fn test_must_use_functions_registry_populates_from_impl_method() {
    // Env-side pin: `env_add_impl` writes the entry under the
    // canonical `"TargetType.method"` key shape (matching what
    // `method_callee_types` produces at call sites).
    let (_prog, typed) = parse_and_typecheck(
        "struct Foo { x: i64 }\n\
         impl Foo {\n\
             #[must_use = \"why\"]\n\
             fn make() -> Foo { Foo { x: 0 } }\n\
         }\n\
         fn main() { }",
    );
    let entry = typed
        .must_use_functions
        .get("Foo.make")
        .expect("impl method with #[must_use] should be in registry under `Type.method` key");
    assert_eq!(entry.as_deref(), Some("why"));
}

// ── Slice 4b cross-cutting — CLI fall-through ──────────────────

#[test]
fn test_cli_allow_suppresses_must_use() {
    let (prog, typed) = parse_and_typecheck(
        "fn returns_opt() -> Option[i64] { Some(1) }\n\
         fn main() { returns_opt(); }",
    );
    let cli =
        karac::lints::CliLintOverrides::with_level("must_use", karac::lints::LintLevel::Allow);
    let diags = check_implicit_must_use(&prog, Some(&typed), &cli);
    assert!(
        diags.is_empty(),
        "`-A must_use` should suppress; got: {diags:?}",
    );
}

#[test]
fn test_cli_deny_promotes_must_use() {
    let (prog, typed) = parse_and_typecheck(
        "fn returns_opt() -> Option[i64] { Some(1) }\n\
         fn main() { returns_opt(); }",
    );
    let cli = karac::lints::CliLintOverrides::with_level("must_use", karac::lints::LintLevel::Deny);
    let diags = check_implicit_must_use(&prog, Some(&typed), &cli);
    assert!(!diags.is_empty(), "expected at least one diagnostic");
    assert!(
        diags.iter().all(|d| d.level == LintLevel::Error),
        "`-D must_use` should promote every emission; got: {diags:?}",
    );
}
