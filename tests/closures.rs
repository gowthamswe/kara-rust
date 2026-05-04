// tests/closures.rs
//
// Integration sweep for the closure-calling-through-`ref` track
// (rounds 12.43–12.47). Each test crosses the full pipeline
// (`parse → resolve → typecheck`) and pins one propagation path
// for once-callability rejection at `Fn`-shaped slots.
//
// Coverage matrix (round 12.47, Step 5b):
//   - parameter passing (own `Fn` and `ref Fn`)
//   - method receiver
//   - struct field assignment (closure literal in struct literal)
//   - generic-function instantiation (Fn slot under [T] substitution)
//   - loop invocation (for-loop element typed `Fn` vs `OnceFn`)
//
// The unit tests in `src/typechecker.rs::once_fn_slot_rejection_tests`
// and `::once_fn_container_slot_tests` cover the inner check semantics;
// these tests verify the full-pipeline contract end-to-end and act as
// the regression guard alongside `tests/ownership.rs` and
// `tests/rc_predicate_parity.rs`.

use karac::typechecker::*;
use karac::{parse, resolve, typecheck};

// ── Test Helpers ────────────────────────────────────────────────

fn typecheck_src(source: &str) -> TypeCheckResult {
    let parsed = parse(source);
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
    let resolved = resolve(&parsed.program);
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
    typecheck(&parsed.program, &resolved)
}

fn errors_of_kind(result: &TypeCheckResult, kind: &TypeErrorKind) -> Vec<TypeError> {
    result
        .errors
        .iter()
        .filter(|e| std::mem::discriminant(&e.kind) == std::mem::discriminant(kind))
        .cloned()
        .collect()
}

fn assert_once_fn_into_fn_slot(result: &TypeCheckResult) -> Vec<TypeError> {
    let hits = errors_of_kind(result, &TypeErrorKind::OnceFnIntoFnSlot);
    assert!(
        !hits.is_empty(),
        "expected at least one OnceFnIntoFnSlot error; all errors: {:?}",
        result.errors
    );
    hits
}

fn assert_no_once_fn_into_fn_slot(result: &TypeCheckResult) {
    let hits = errors_of_kind(result, &TypeErrorKind::OnceFnIntoFnSlot);
    assert!(
        hits.is_empty(),
        "expected no OnceFnIntoFnSlot error; got: {:?}",
        hits
    );
}

// ── Path 1: parameter passing ───────────────────────────────────

#[test]
fn param_own_fn_slot_rejects_oncefn_closure() {
    let src = "struct Cfg { name: i64 }\n\
               fn apply(c: Cfg) { }\n\
               fn take(f: Fn()) { f() }\n\
               fn main() {\n\
                   let cfg = Cfg { name: 7 };\n\
                   take(|| apply(cfg));\n\
               }";
    assert_once_fn_into_fn_slot(&typecheck_src(src));
}

#[test]
fn param_ref_fn_slot_rejects_oncefn_closure() {
    let src = "struct Cfg { name: i64 }\n\
               fn apply(c: Cfg) { }\n\
               fn take(f: ref Fn()) { }\n\
               fn main() {\n\
                   let cfg = Cfg { name: 7 };\n\
                   take(|| apply(cfg));\n\
               }";
    assert_once_fn_into_fn_slot(&typecheck_src(src));
}

#[test]
fn param_fn_slot_accepts_repeatable_closure() {
    let src = "fn take(f: Fn()) { f() }\n\
               fn main() { take(|| { }); }";
    assert_no_once_fn_into_fn_slot(&typecheck_src(src));
}

// ── Path 2: method receiver ─────────────────────────────────────

#[test]
fn method_fn_slot_rejects_oncefn_closure() {
    let src = "struct Cfg { name: i64 }\n\
               fn apply(c: Cfg) { }\n\
               struct Runner { }\n\
               impl Runner {\n\
                   fn drive(self, f: Fn()) { f() }\n\
               }\n\
               fn main() {\n\
                   let cfg = Cfg { name: 7 };\n\
                   let r = Runner { };\n\
                   r.drive(|| apply(cfg));\n\
               }";
    assert_once_fn_into_fn_slot(&typecheck_src(src));
}

#[test]
fn method_fn_slot_accepts_repeatable_closure() {
    let src = "struct Runner { }\n\
               impl Runner {\n\
                   fn drive(self, f: Fn()) { f() }\n\
               }\n\
               fn main() {\n\
                   let r = Runner { };\n\
                   r.drive(|| { });\n\
               }";
    assert_no_once_fn_into_fn_slot(&typecheck_src(src));
}

// ── Path 3: struct field assignment ─────────────────────────────

#[test]
fn struct_field_fn_slot_rejects_oncefn_closure_literal() {
    // Closure literal flows directly into a struct-literal `f: Fn()` slot.
    // The Fn-shaped field promises repeatable invocation at field-read,
    // so the once-callable closure literal must reject.
    let src = "struct Cfg { name: i64 }\n\
               fn apply(c: Cfg) { }\n\
               struct Holder { f: Fn() }\n\
               fn main() {\n\
                   let cfg = Cfg { name: 7 };\n\
                   let _h = Holder { f: || apply(cfg) };\n\
               }";
    let result = typecheck_src(src);
    let hits = assert_once_fn_into_fn_slot(&result);
    assert!(
        hits[0].message.contains("'cfg'") || hits[0].message.contains("captured binding"),
        "expected consumed-capture name in message; got '{}'",
        hits[0].message
    );
}

#[test]
fn struct_field_fn_slot_accepts_repeatable_closure() {
    let src = "struct Holder { f: Fn() -> i64 }\n\
               fn main() {\n\
                   let _h = Holder { f: || 42 };\n\
               }";
    assert_no_once_fn_into_fn_slot(&typecheck_src(src));
}

#[test]
fn struct_field_oncefn_slot_accepts_oncefn_closure_literal() {
    // The reverse direction: `f: OnceFn()` field accepts a once-callable
    // closure literal because the slot type matches the closure type.
    let src = "struct Cfg { name: i64 }\n\
               fn apply(c: Cfg) { }\n\
               struct Holder { f: OnceFn() }\n\
               fn main() {\n\
                   let cfg = Cfg { name: 7 };\n\
                   let _h = Holder { f: || apply(cfg) };\n\
               }";
    assert_no_once_fn_into_fn_slot(&typecheck_src(src));
}

// ── Path 4: generic-function instantiation ──────────────────────

#[test]
fn generic_fn_slot_rejects_oncefn_closure_literal() {
    // `run[T](x: T, cb: Fn(T))` — the Fn slot is generic in T. The Fn-vs-
    // OnceFn dimension is independent of T, so a once-callable closure
    // must reject at the call site even after T is solved from `x`.
    let src = "struct Cfg { name: i64 }\n\
               fn apply(c: Cfg) { }\n\
               fn run[T](x: T, cb: Fn(T)) { cb(x) }\n\
               fn main() {\n\
                   let cfg = Cfg { name: 7 };\n\
                   run(1, |_| apply(cfg));\n\
               }";
    assert_once_fn_into_fn_slot(&typecheck_src(src));
}

#[test]
fn generic_fn_slot_accepts_repeatable_closure() {
    let src = "fn run[T](x: T, cb: Fn(T)) { cb(x) }\n\
               fn main() {\n\
                   run(1, |_| { });\n\
               }";
    assert_no_once_fn_into_fn_slot(&typecheck_src(src));
}

// ── Path 5: loop invocation ─────────────────────────────────────

#[test]
fn vec_fn_loop_invocation_accepts_repeatable_elements() {
    // `for f in v` over `Vec[Fn()]` types `f` as `Function` (Step 1's
    // dispatch handles `Function` and `OnceFunction` both). With a
    // repeatable closure pushed in, the body's `f()` typechecks cleanly.
    let src = "fn main() {\n\
                   let mut v: Vec[Fn()] = Vec.new();\n\
                   v.push(|| { });\n\
                   for f in v { f() }\n\
               }";
    let result = typecheck_src(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors; got: {:?}",
        result.errors
    );
}

#[test]
fn vec_fn_loop_invocation_push_oncefn_rejects_at_push_site() {
    // The push site rejects (Step 4), so the loop body never receives
    // an inconsistent element. This test pins that the rejection lives
    // at the push, not at the loop — the loop body's `f()` is fine in
    // isolation; the violation is the *insertion* of an OnceFn into a
    // `Fn()` slot.
    let src = "struct Cfg { name: i64 }\n\
               fn apply(c: Cfg) { }\n\
               fn main() {\n\
                   let cfg = Cfg { name: 7 };\n\
                   let mut v: Vec[Fn()] = Vec.new();\n\
                   v.push(|| apply(cfg));\n\
                   for f in v { f() }\n\
               }";
    assert_once_fn_into_fn_slot(&typecheck_src(src));
}

#[test]
fn vec_oncefn_loop_invocation_accepts_oncefn_elements() {
    // `Vec[OnceFn()]` — each iteration owns its element, the body's
    // single `f()` invocation succeeds. Step 4's surface annotation
    // lowers to `Type::OnceFunction`; the for-loop typer yields
    // `OnceFunction`; Step 1's `Function | OnceFunction` Call dispatch
    // accepts it.
    let src = "struct Cfg { name: i64 }\n\
               fn apply(c: Cfg) { }\n\
               fn main() {\n\
                   let cfg = Cfg { name: 7 };\n\
                   let mut v: Vec[OnceFn()] = Vec.new();\n\
                   v.push(|| apply(cfg));\n\
                   for f in v { f() }\n\
               }";
    let result = typecheck_src(src);
    assert!(
        result.errors.is_empty(),
        "expected no errors; got: {:?}",
        result.errors
    );
}

// ── Diagnostic polish (Step 5a) ─────────────────────────────────

#[test]
fn diagnostic_message_includes_all_three_fix_hints() {
    let src = "struct Cfg { name: i64 }\n\
               fn apply(c: Cfg) { }\n\
               fn take(f: Fn()) { f() }\n\
               fn main() {\n\
                   let cfg = Cfg { name: 7 };\n\
                   take(|| apply(cfg));\n\
               }";
    let result = typecheck_src(src);
    let hits = assert_once_fn_into_fn_slot(&result);
    let msg = &hits[0].message;
    assert!(
        msg.contains("clone the captured value"),
        "missing clone hint; got '{}'",
        msg
    );
    assert!(
        msg.contains("invoke the closure locally") || msg.contains("restructure"),
        "missing restructure-locally hint; got '{}'",
        msg
    );
    assert!(
        msg.contains("OnceFn(...)"),
        "missing OnceFn slot-change hint; got '{}'",
        msg
    );
}
