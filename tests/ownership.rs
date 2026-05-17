// tests/ownership.rs

use karac::ownership::*;
use karac::resolver::SpanKey;
use karac::{ownershipcheck, parse, resolve, typecheck};

// ── Test Helpers ────────────────────────────────────────────────

fn ownership_ok(source: &str) -> OwnershipCheckResult {
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "Parse errors: {:?}",
        parsed.errors
    );
    let resolved = resolve(&parsed.program);
    assert!(
        resolved.errors.is_empty(),
        "Resolve errors: {:?}",
        resolved.errors
    );
    let typed = typecheck(&parsed.program, &resolved);
    assert!(
        typed.errors.is_empty(),
        "Type errors: {}",
        typed
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let result = ownershipcheck(&parsed.program, &typed);
    assert!(
        result.errors.is_empty(),
        "Ownership errors: {}",
        result
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    result
}

fn ownership_errors(source: &str) -> Vec<OwnershipError> {
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "Parse errors: {:?}",
        parsed.errors
    );
    let resolved = resolve(&parsed.program);
    assert!(
        resolved.errors.is_empty(),
        "Resolve errors: {:?}",
        resolved.errors
    );
    let typed = typecheck(&parsed.program, &resolved);
    // Type errors are OK — we might be testing ownership on type-valid code
    let result = ownershipcheck(&parsed.program, &typed);
    assert!(
        !result.errors.is_empty(),
        "Expected ownership errors but got none"
    );
    result.errors
}

// ── Copy Types ──────────────────────────────────────────────────

#[test]
fn test_empty_program() {
    ownership_ok("");
}

#[test]
fn test_primitives_dont_move() {
    // Primitives are Copy — using them multiple times is fine
    ownership_ok(
        "fn use_twice(x: i64) -> i64 {\n\
             let a = x;\n\
             let b = x;\n\
             a + b\n\
         }",
    );
}

#[test]
fn test_string_moves() {
    // String is not Copy — using after move should error
    let errors = ownership_errors(
        "fn consume(s: String) { }\n\
         fn main() {\n\
             let s = \"hello\";\n\
             consume(s);\n\
             consume(s);\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == OwnershipErrorKind::UseAfterMove));
}

#[test]
fn test_struct_moves() {
    let errors = ownership_errors(
        "struct Point { x: i64, y: i64 }\n\
         fn consume(p: Point) { }\n\
         fn main() {\n\
             let p = Point { x: 1, y: 2 };\n\
             consume(p);\n\
             consume(p);\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == OwnershipErrorKind::UseAfterMove));
}

// ── Move Tracking ───────────────────────────────────────────────

#[test]
fn test_basic_move_ok() {
    // Single use of a non-Copy value is fine
    ownership_ok(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) { }\n\
         fn main() {\n\
             let d = Data { value: 1 };\n\
             consume(d);\n\
         }",
    );
}

#[test]
fn test_use_after_move_error() {
    let errors = ownership_errors(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) { }\n\
         fn main() {\n\
             let d = Data { value: 1 };\n\
             consume(d);\n\
             consume(d);\n\
         }",
    );
    assert!(errors[0].kind == OwnershipErrorKind::UseAfterMove);
    assert!(errors[0].message.contains("moved"));
}

#[test]
fn test_reassignment_resets_state() {
    // Reassigning a mut variable resets it to Live
    ownership_ok(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) { }\n\
         fn main() {\n\
             let mut d = Data { value: 1 };\n\
             consume(d);\n\
             d = Data { value: 2 };\n\
             consume(d);\n\
         }",
    );
}

#[test]
fn test_multiple_reads_before_move() {
    // Reading a value multiple times before moving is fine
    ownership_ok(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) { }\n\
         fn main() {\n\
             let d = Data { value: 1 };\n\
             let v = d.value;\n\
             let w = d.value;\n\
             consume(d);\n\
         }",
    );
}

#[test]
fn test_return_consumes() {
    // Returning a value consumes it
    ownership_ok(
        "struct Data { value: i64 }\n\
         fn make() -> Data {\n\
             let d = Data { value: 1 };\n\
             d\n\
         }",
    );
}

#[test]
fn test_let_binding_consumes() {
    let errors = ownership_errors(
        "struct Data { value: i64 }\n\
         fn main() {\n\
             let d = Data { value: 1 };\n\
             let d2 = d;\n\
             let d3 = d;\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == OwnershipErrorKind::UseAfterMove));
}

// ── Parameter Mode Inference ────────────────────────────────────

#[test]
fn test_param_read_only_is_ref() {
    let result = ownership_ok("fn read_field(x: i64) -> i64 { x + 1 }");
    let modes = result.param_modes.get("read_field").unwrap();
    // i64 is Copy, so even though it's consumed, it stays Ref
    assert_eq!(modes[0].1, OwnershipMode::Ref);
}

#[test]
fn test_param_consumed_is_own() {
    let result = ownership_ok(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) { }\n\
         fn take_data(d: Data) {\n\
             consume(d);\n\
         }",
    );
    let modes = result.param_modes.get("take_data").unwrap();
    assert_eq!(modes[0].1, OwnershipMode::Own);
}

#[test]
fn test_pure_function_params_ref() {
    let result = ownership_ok("fn add(a: i64, b: i64) -> i64 { a + b }");
    let modes = result.param_modes.get("add").unwrap();
    assert_eq!(modes[0].1, OwnershipMode::Ref);
    assert_eq!(modes[1].1, OwnershipMode::Ref);
}

#[test]
fn test_struct_field_access_is_ref() {
    let result = ownership_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn get_x(p: Point) -> i64 { p.x }",
    );
    let modes = result.param_modes.get("get_x").unwrap();
    // Field access is a read — but for non-Copy struct, accessing through
    // it could be Own if the struct is consumed. Since p.x returns i64 (Copy),
    // p itself is only read.
    assert_eq!(modes[0].1, OwnershipMode::Ref);
}

// ── Cycle Detection ─────────────────────────────────────────────

#[test]
fn test_no_cycle_passes() {
    ownership_ok(
        "struct Parent { name: String }\n\
         struct Child { parent_name: String }",
    );
}

#[test]
fn test_direct_cycle_error() {
    let errors = ownership_errors(
        "struct A { b: B }\n\
         struct B { a: A }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == OwnershipErrorKind::OwnershipCycle));
}

#[test]
fn test_self_referential_cycle() {
    let errors = ownership_errors("struct Node { next: Node }");
    assert!(errors
        .iter()
        .any(|e| e.kind == OwnershipErrorKind::OwnershipCycle));
}

#[test]
fn test_weak_breaks_cycle() {
    ownership_ok(
        "struct Parent { child: Child }\n\
         struct Child { parent: weak Parent }",
    );
}

#[test]
fn test_ref_field_no_cycle() {
    ownership_ok(
        "struct Parent { child: Child }\n\
         struct Child { parent: ref Parent }",
    );
}

// ── Complex Programs ────────────────────────────────────────────

#[test]
fn test_multiple_params_different_modes() {
    let result = ownership_ok(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) { }\n\
         fn process(a: i64, d: Data) {\n\
             let x = a + 1;\n\
             consume(d);\n\
         }",
    );
    let modes = result.param_modes.get("process").unwrap();
    // a: i64 is Copy → Ref
    assert_eq!(modes[0].1, OwnershipMode::Ref);
    // d: Data is consumed → Own
    assert_eq!(modes[1].1, OwnershipMode::Own);
}

#[test]
fn test_for_loop_binding() {
    ownership_ok(
        "fn process(items: i64) {\n\
             for item in items {\n\
                 let x = item;\n\
             }\n\
         }",
    );
}

#[test]
fn test_field_access_then_move() {
    ownership_ok(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) { }\n\
         fn process(d: Data) {\n\
             let v = d.value;\n\
             consume(d);\n\
         }",
    );
}

#[test]
fn test_nested_function_calls() {
    ownership_ok(
        "fn double(x: i64) -> i64 { x + x }\n\
         fn main() {\n\
             let result = double(double(5));\n\
         }",
    );
}

#[test]
fn test_impl_method_ownership() {
    let result = ownership_ok(
        "struct Counter { value: i64 }\n\
         impl Counter {\n\
             fn get(self) -> i64 { self.value }\n\
         }",
    );
    let modes = result.param_modes.get("Counter.get");
    assert!(modes.is_some());
}

#[test]
fn test_complex_program() {
    ownership_ok(
        "struct User { name: String, age: i64 }\n\
         \n\
         fn get_age(user: User) -> i64 {\n\
             user.age\n\
         }\n\
         \n\
         fn main() {\n\
             let u = User { name: \"Alice\", age: 30 };\n\
             let age = get_age(u);\n\
             let x = age + 1;\n\
         }",
    );
}

// ── Copy Struct No Move ────────────────────────────────────────

#[test]
fn test_copy_struct_does_not_move() {
    // A struct with #[derive(Copy)] should not trigger use-after-move
    ownership_ok(
        "#[derive(Copy, Clone)]\n\
         struct Point { x: i64, y: i64 }\n\
         fn use_twice(p: Point) {\n\
             let a = p;\n\
             let b = p;\n\
         }",
    );
}

#[test]
fn test_non_copy_struct_moves() {
    // A struct without Copy should trigger use-after-move
    let errors = ownership_errors(
        "struct Point { x: i64, y: i64 }\n\
         fn use_twice(p: Point) {\n\
             let a = p;\n\
             let b = p;\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == OwnershipErrorKind::UseAfterMove));
}

// ── MutRef Parameter Mode Inference ────────────────────────────

#[test]
fn test_param_assigned_is_mut_ref() {
    // Parameter modes are inferred — reassigning a parameter means mut ref
    let result = ownership_ok(
        "fn increment(x: i64) -> i64 {\n\
             let mut y = x;\n\
             y = y + 1;\n\
             y\n\
         }",
    );
    // x is only read, so it should be Ref
    let modes = result.param_modes.get("increment").unwrap();
    let (_, mode) = modes.iter().find(|(n, _)| n == "x").unwrap();
    assert_eq!(*mode, OwnershipMode::Ref);
}

#[test]
fn test_local_var_mutation_tracked() {
    // Mutation of a local let mut variable should work
    ownership_ok(
        "fn counter() -> i64 {\n\
             let mut x = 0;\n\
             x = x + 1;\n\
             x += 1;\n\
             x\n\
         }",
    );
}

#[test]
fn test_param_field_mutated_is_mut_ref() {
    // Field mutation on a parameter should infer MutRef
    let result = ownership_ok(
        "struct Counter { value: i64 }\n\
         fn reset(c: Counter) {\n\
             c.value = 0;\n\
         }",
    );
    let modes = result.param_modes.get("reset").unwrap();
    let (_, mode) = modes.iter().find(|(n, _)| n == "c").unwrap();
    assert_eq!(*mode, OwnershipMode::MutRef);
}

// ── Multi-level Cycle Detection ────────────────────────────────

#[test]
fn test_three_node_cycle_detected() {
    let errors = ownership_errors(
        "struct A { b: B }\n\
         struct B { c: C }\n\
         struct C { a: A }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == OwnershipErrorKind::OwnershipCycle));
}

// ── Ownership Diagnostics ──────────────────────────────────────

#[test]
fn test_use_after_move_diagnostic_message() {
    let errors = ownership_errors(
        "struct Data { x: i64 }\n\
         fn consume(d: Data) { }\n\
         fn bad(d: Data) {\n\
             consume(d);\n\
             consume(d);\n\
         }",
    );
    let err = &errors[0];
    assert!(err.message.contains("moved here"));
    assert!(err.suggestion.is_some());
    assert!(
        err.suggestion.as_ref().unwrap().contains("cloning"),
        "Expected 'cloning' in suggestion, got: {:?}",
        err.suggestion
    );
}

#[test]
fn test_ownership_cycle_has_suggestion() {
    let errors = ownership_errors(
        "struct A { b: B }\n\
         struct B { a: A }",
    );
    let err = errors
        .iter()
        .find(|e| e.kind == OwnershipErrorKind::OwnershipCycle)
        .unwrap();
    let suggestion = err.suggestion.as_ref().expect("expected suggestion");
    // Non-shared cycles should steer toward indirection, not 'weak'.
    assert!(
        suggestion.contains("ref") || suggestion.contains("Box") || suggestion.contains("shared"),
        "non-shared cycle suggestion should mention ref/Box/shared, got: {}",
        suggestion
    );
    assert!(
        !err.message.contains("shared-type cycle"),
        "non-shared cycle should not be labeled as shared-type cycle: {}",
        err.message
    );
}

#[test]
fn test_shared_struct_cycle_distinct_diagnostic() {
    // A cycle among `shared` types should produce a distinct diagnostic that
    // steers toward `weak`, because the semantics (RC leak) are different.
    let errors = ownership_errors(
        "shared struct A { b: B }\n\
         shared struct B { a: A }",
    );
    let err = errors
        .iter()
        .find(|e| e.kind == OwnershipErrorKind::OwnershipCycle)
        .unwrap();
    assert!(
        err.message.contains("shared-type cycle"),
        "shared cycle should be labeled as shared-type cycle: {}",
        err.message
    );
    let suggestion = err.suggestion.as_ref().expect("expected suggestion");
    assert!(
        suggestion.contains("weak"),
        "shared-type cycle suggestion should mention 'weak', got: {}",
        suggestion
    );
}

#[test]
fn test_shared_typed_param_reports_as_shared_rc() {
    // A parameter whose type is declared `shared` should be reported as
    // `shared (Rc)` in the representation map, not as `owned (stack)` —
    // codegen lowers it as Rc<T> regardless of the inferred mode.
    let result = ownership_ok(
        "shared struct Data { val: i64 }\n\
         fn process(d: Data) -> i64 { d.val }",
    );
    let repr = result
        .representations
        .get("process.d")
        .expect("expected representation for process.d");
    assert_eq!(
        repr, "shared (Rc)",
        "shared-typed param should report as 'shared (Rc)', got '{}'",
        repr
    );
}

#[test]
fn test_nonshared_typed_param_unchanged() {
    // Sanity: the shared-type branch must not alter reporting for ordinary
    // (non-shared) named-type params.
    let result = ownership_ok(
        "struct Plain { val: i64 }\n\
         fn process(p: Plain) -> i64 { p.val }",
    );
    let repr = result
        .representations
        .get("process.p")
        .expect("expected representation for process.p");
    assert_ne!(
        repr, "shared (Rc)",
        "non-shared type must not be reported as shared (Rc)"
    );
}

#[test]
fn test_mixed_shared_nonshared_cycle_is_ownership_cycle() {
    // If any participant is non-shared, it's an ownership cycle (the non-shared
    // type cannot transitively contain itself regardless of the others).
    let errors = ownership_errors(
        "shared struct A { b: B }\n\
         struct B { a: A }",
    );
    let err = errors
        .iter()
        .find(|e| e.kind == OwnershipErrorKind::OwnershipCycle)
        .unwrap();
    assert!(
        !err.message.contains("shared-type cycle"),
        "mixed cycle should be treated as an ownership cycle, not shared-type: {}",
        err.message
    );
}

// ── Destructuring in function/closure parameters ─────────────────

#[test]
fn test_tuple_destructuring_param_ownership() {
    ownership_ok("fn add((a, b): (i64, i64)) -> i64 { a + b }");
}

#[test]
fn test_wildcard_destructuring_param_ownership() {
    ownership_ok("fn y_only((_, y): (i64, i64)) -> i64 { y }");
}

// ── Representation Tracking ───────────────────────────────────

#[test]
fn test_representations_populated() {
    // Verify that representations are populated for inferred param modes
    let result = ownership_ok("fn add(x: i64, y: i64) -> i64 { x + y }");
    // x and y should have representations
    assert!(!result.representations.is_empty() || result.param_modes.is_empty());
}

// ── @no_rc Struct ─────────────────────────────────────────────

#[test]
fn test_no_rc_struct_passes_ownership() {
    // @no_rc struct should pass ownership check normally
    ownership_ok(
        "@no_rc\n\
         struct Particle { x: f64, y: f64 }\n\
         fn main() { let p = Particle { x: 1.0, y: 2.0 }; }",
    );
}

// ── Once-callable closure ownership ──────────────────────────────

#[test]
fn test_once_callable_closure_first_call_ok() {
    // A closure that captures an owned non-Copy value is once-callable.
    // The first (and only) call must succeed.
    ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = || { let _ = o; };\n\
             f();\n\
         }",
    );
}

#[test]
fn test_once_callable_closure_second_call_error() {
    // Calling a once-callable closure twice is use-after-move.
    let errors = ownership_errors(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = || { let _ = o; };\n\
             f();\n\
             f();\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove on second call, got: {errors:?}"
    );
}

#[test]
fn test_closure_capturing_only_copy_is_not_once_callable() {
    // A closure that only reads Copy values can be called any number of times.
    ownership_ok(
        "fn main() {\n\
             let n: i64 = 42;\n\
             let f = || { let _ = n; };\n\
             f();\n\
             f();\n\
         }",
    );
}

// ── Consume predicate: partial move through field projection ────

#[test]
fn test_consume_of_non_copy_field_consumes_root() {
    // `take(c.inner)` where inner is non-Copy String should consume the
    // root binding `c` per design.md § Consume Predicate step 3.
    let errors = ownership_errors(
        "fn take(s: String) {}\n\
         struct Container { inner: String }\n\
         fn main() {\n\
             let c = Container { inner: \"hello\" };\n\
             take(c.inner);\n\
             let _ = c;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove from `let _ = c` after consuming non-Copy field, got: {errors:?}"
    );
}

#[test]
fn test_consume_of_copy_field_does_not_consume_root() {
    // `take(c.n)` where n is Copy i64 should NOT consume root `c`.
    ownership_ok(
        "fn take(n: i64) {}\n\
         struct Container { n: i64, inner: String }\n\
         fn main() {\n\
             let c = Container { n: 1, inner: \"hello\" };\n\
             take(c.n);\n\
             let _ = c.inner;\n\
         }",
    );
}

#[test]
fn test_deep_field_projection_consume_propagates_to_root() {
    // `take(c.a.b)` where `b` is non-Copy should consume `c`.
    let errors = ownership_errors(
        "fn take(s: String) {}\n\
         struct Inner { b: String }\n\
         struct Outer { a: Inner }\n\
         fn main() {\n\
             let c = Outer { a: Inner { b: \"hi\" } };\n\
             take(c.a.b);\n\
             let _ = c;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove on root after deep field consume, got: {errors:?}"
    );
}

#[test]
fn test_tuple_index_consume_propagates_to_root() {
    // `take(t.0)` where t is `(String, i64)` should consume `t`.
    let errors = ownership_errors(
        "fn take(s: String) {}\n\
         fn main() {\n\
             let t = (\"hi\", 1);\n\
             take(t.0);\n\
             let _ = t;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove from `let _ = t` after consuming non-Copy tuple element, got: {errors:?}"
    );
}

#[test]
fn test_field_read_only_unchanged() {
    // Regression: `let v = c.value` where value is Copy continues to read
    // the chain (no consume of root).
    ownership_ok(
        "struct Container { value: i64, name: String }\n\
         fn main() {\n\
             let c = Container { value: 42, name: \"x\" };\n\
             let v = c.value;\n\
             let w = c.value;\n\
             let _ = c.name;\n\
         }",
    );
}

// ── Consume predicate: function param mode (step 2) ────────────

#[test]
fn test_call_with_owned_param_consumes_arg() {
    // `consume(s)` where `consume(s: String)` declares the param as
    // bare-T (Owned) — consumes the arg. Existing behavior, regression.
    let errors = ownership_errors(
        "fn consume(s: String) {}\n\
         fn main() {\n\
             let s = \"hi\";\n\
             consume(s);\n\
             let _ = s;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove after Owned-param call, got: {errors:?}"
    );
}

#[test]
fn test_call_with_mut_slice_param_does_not_consume_caller_vec() {
    // `mut Slice[T]` param accepts `mut ref Vec[T]` via the typechecker's
    // implicit coercion. The arg is a borrow position — caller's `v` is
    // read, not consumed. This is the most ergonomically common case where
    // step 2's borrow classification matters in user code today.
    ownership_ok(
        "fn clear(xs: mut Slice[i64]) {}\n\
         fn caller(v: mut ref Vec[i64]) {\n\
             clear(v);\n\
             clear(v);\n\
         }",
    );
}

#[test]
fn test_static_method_call_with_owned_param_consumes_arg() {
    // `Make.from(s)` where the static method declares `from(s: String)`
    // (bare-T) — consumes `s`.
    let errors = ownership_errors(
        "struct Make {}\n\
         impl Make {\n\
             fn from(s: String) -> Make { Make {} }\n\
         }\n\
         fn main() {\n\
             let s = \"hi\";\n\
             let _ = Make.from(s);\n\
             let _ = s;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove after static-method bare-T call, got: {errors:?}"
    );
}

#[test]
fn test_call_mixed_params_classifies_per_position() {
    // Per-position classification: bare-T arg consumes, ref-T arg doesn't,
    // even when mixed in the same call.
    let errors = ownership_errors(
        "fn process(read_only: ref String, owned: String) {}\n\
         fn main() {\n\
             let r = \"reader\";\n\
             let o = \"owned\";\n\
             process(r, o);\n\
             let _ = r;\n\
             let _ = o;\n\
         }",
    );
    // r should still be usable (ref-arg). o should not (consumed).
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove && e.message.contains("'o'")),
        "expected UseAfterMove on `o` (owned) but not `r` (ref), got: {errors:?}"
    );
    assert!(
        !errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove && e.message.contains("'r'")),
        "did not expect UseAfterMove on `r` (ref-arg), got: {errors:?}"
    );
}

// ── Consume predicate: method receiver-mode (step 1) ───────────

#[test]
fn test_method_with_owned_self_consumes_receiver() {
    // `c.into_string()` where the impl declares `fn into_string(self) -> String`
    // takes the receiver by-move and so consumes `c`.
    let errors = ownership_errors(
        "struct Container { inner: String }\n\
         impl Container {\n\
             fn into_string(self) -> String { self.inner }\n\
         }\n\
         fn main() {\n\
             let c = Container { inner: \"hi\" };\n\
             let _ = c.into_string();\n\
             let _ = c;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove after `c.into_string()` (owned self), got: {errors:?}"
    );
}

#[test]
fn test_method_with_ref_self_does_not_consume() {
    // `c.peek()` where the impl declares `fn peek(ref self) -> i64` reads
    // the receiver — `c` remains usable.
    ownership_ok(
        "struct Container { value: i64 }\n\
         impl Container {\n\
             fn peek(ref self) -> i64 { self.value }\n\
         }\n\
         fn main() {\n\
             let c = Container { value: 1 };\n\
             let _ = c.peek();\n\
             let _ = c.peek();\n\
             let _ = c;\n\
         }",
    );
}

#[test]
fn test_method_with_mut_ref_self_does_not_consume() {
    // `mut ref self` is a borrow, not a consume — receiver stays usable.
    ownership_ok(
        "struct Counter { n: i64 }\n\
         impl Counter {\n\
             fn bump(mut ref self) { self.n = self.n + 1; }\n\
         }\n\
         fn main() {\n\
             let mut c = Counter { n: 0 };\n\
             c.bump();\n\
             c.bump();\n\
             let _ = c;\n\
         }",
    );
}

#[test]
fn test_method_owned_self_param_inferred_own() {
    // When a fn param is consumed via owned-self method call, the param
    // mode is inferred Own.
    let result = ownership_ok(
        "struct Container { inner: String }\n\
         impl Container {\n\
             fn into_string(self) -> String { self.inner }\n\
         }\n\
         fn unwrap(c: Container) -> String {\n\
             c.into_string()\n\
         }",
    );
    let modes = result.param_modes.get("unwrap").unwrap();
    assert_eq!(
        modes[0].1,
        OwnershipMode::Own,
        "expected param `c` of `unwrap` Own (consumed via owned-self method), got: {modes:?}"
    );
}

#[test]
fn test_method_owned_self_on_field_consumes_root() {
    // Composes with round-11.2's partial-move-through-projection: an owned-
    // self method call on `c.inner` consumes root `c`.
    let errors = ownership_errors(
        "struct Container { inner: Inner }\n\
         struct Inner { value: i64 }\n\
         impl Inner {\n\
             fn unwrap(self) -> i64 { self.value }\n\
         }\n\
         fn main() {\n\
             let c = Container { inner: Inner { value: 1 } };\n\
             let _ = c.inner.unwrap();\n\
             let _ = c;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove from c.inner.unwrap() (owned self on field), got: {errors:?}"
    );
}

#[test]
fn test_method_owned_self_on_copy_receiver_is_noop() {
    // Owned-self method on a Copy type is a noop — Copy values aren't moved.
    ownership_ok(
        "struct Counter { n: i64 }\n\
         impl Counter {\n\
             fn into_n(self) -> i64 { self.n }\n\
         }\n\
         #[derive(Copy)]\n\
         #[derive(Clone)]\n\
         struct CopyCounter { n: i64 }\n\
         impl CopyCounter {\n\
             fn into_n(self) -> i64 { self.n }\n\
         }\n\
         fn main() {\n\
             let c = CopyCounter { n: 1 };\n\
             let _ = c.into_n();\n\
             let _ = c.into_n();\n\
             let _ = c;\n\
         }",
    );
}

// ── Consume predicate: unsafe-block transparency (step 6) ──────

#[test]
fn test_consume_inside_unsafe_block_detected() {
    // A consume inside `unsafe { ... }` must be visible to the use-
    // predicate scan exactly like a consume inside an ordinary block.
    // (Note: ownership analysis does not "mask" consumes inside unsafe;
    // the unsafe escape hatch only suppresses the use-after-move
    // *rejection* on a separate pass — see design.md § Consume
    // Predicate step 6.)
    let errors = ownership_errors(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) {}\n\
         fn main() {\n\
             let d = Data { value: 1 };\n\
             unsafe { consume(d); }\n\
             let _ = d;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove from unsafe-block consume, got: {errors:?}"
    );
}

#[test]
fn test_unsafe_block_param_consume_infers_own() {
    // Param mode inference walks through `unsafe { ... }` — a param
    // consumed only inside an unsafe block is still classified Own.
    let result = ownership_ok(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) {}\n\
         fn drain(d: Data) {\n\
             unsafe { consume(d); }\n\
         }",
    );
    let modes = result.param_modes.get("drain").unwrap();
    assert_eq!(
        modes[0].1,
        OwnershipMode::Own,
        "expected param `d` of `drain` Own (consumed inside unsafe), got: {:?}",
        modes
    );
}

// ── Consume predicate: nesting-depth invariance (step 7) ───────

#[test]
fn test_deeply_nested_unconditional_consume_detected() {
    // A consume buried 4 levels deep in unconditional nested blocks
    // is classified the same as a top-level consume. The walker
    // recurses into all AST children unconditionally — depth is
    // irrelevant to the use-predicate scan.
    let errors = ownership_errors(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) {}\n\
         fn main() {\n\
             let d = Data { value: 1 };\n\
             { { { { consume(d); } } } }\n\
             let _ = d;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove from depth-4 nested-block consume, got: {errors:?}"
    );
}

#[test]
fn test_deeply_nested_param_consume_infers_own() {
    // Param consumed at depth 4 still infers Own — no path sensitivity,
    // no reachability analysis. Mixes blocks, if (with else for clean
    // merge), and another block.
    let result = ownership_ok(
        "struct Data { value: i64 }\n\
         fn consume(d: Data) {}\n\
         fn deep(d: Data, n: i64) {\n\
             {\n\
                 if n > 0 {\n\
                     { { consume(d); } }\n\
                 } else {\n\
                     { { consume(d); } }\n\
                 }\n\
             }\n\
         }",
    );
    let modes = result.param_modes.get("deep").unwrap();
    let d_mode = &modes
        .iter()
        .find(|(name, _)| name == "d")
        .expect("param d not found")
        .1;
    assert_eq!(
        *d_mode,
        OwnershipMode::Own,
        "expected param `d` of `deep` Own (deep consume), got: {modes:?}"
    );
}

// ── Consume predicate: return / tail expression consume ────────

#[test]
fn test_tail_expression_consumes_partial_move() {
    // `fn make(c: Container) -> String { c.inner }` — the tail-position
    // `c.inner` is a consume context (block.final_expr drives
    // check_expr_consuming), and the round-11.2 partial-move rule
    // pushes it to root `c`. The caller's use after `make(c)` is a
    // use-after-move.
    let errors = ownership_errors(
        "struct Container { inner: String }\n\
         fn make(c: Container) -> String {\n\
             c.inner\n\
         }\n\
         fn caller() {\n\
             let c = Container { inner: \"hi\" };\n\
             let _ = make(c);\n\
             let _ = c;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove on caller's `let _ = c` after make(c), got: {errors:?}"
    );
}

#[test]
fn test_tail_expression_param_inferred_own() {
    // The same `make(c)` body should infer the param mode as Own
    // because the body consumes c via tail-position partial move.
    let result = ownership_ok(
        "struct Container { inner: String }\n\
         fn make(c: Container) -> String {\n\
             c.inner\n\
         }",
    );
    let modes = result.param_modes.get("make").unwrap();
    assert_eq!(
        modes[0].1,
        OwnershipMode::Own,
        "expected param `c` of `make` to be inferred Own (tail consume), got: {:?}",
        modes
    );
}

#[test]
fn test_explicit_return_consumes_param() {
    // `fn drain(d: Data) -> Data { return d; }` — explicit return is a
    // consume context; param mode should be Own.
    let result = ownership_ok(
        "struct Data { value: i64 }\n\
         fn drain(d: Data) -> Data {\n\
             return d;\n\
         }",
    );
    let modes = result.param_modes.get("drain").unwrap();
    assert_eq!(
        modes[0].1,
        OwnershipMode::Own,
        "expected param `d` of `drain` to be inferred Own (return consume), got: {:?}",
        modes
    );
}

#[test]
fn test_tail_expression_in_nested_block_propagates() {
    // Nested block tail: `{ { d } }` — outer block's final_expr is the
    // inner block; inner block's final_expr is `d`. Tail consume must
    // propagate through the nesting.
    let result = ownership_ok(
        "struct Data { value: i64 }\n\
         fn make() -> Data {\n\
             let d = Data { value: 1 };\n\
             { d }\n\
         }",
    );
    // No assertion on errors (already ownership_ok). Just regression: a
    // body that double-uses `d` should still error.
    let errors = ownership_errors(
        "struct Data { value: i64 }\n\
         fn make() -> Data {\n\
             let d = Data { value: 1 };\n\
             let _ = d;\n\
             { d }\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove from nested-block tail re-use of d, got: {errors:?}"
    );
    let _ = result; // silence unused-binding lint
}

// ── Consume predicate: match scrutinee classification ──────────

#[test]
fn test_match_with_binding_consumes_non_copy_scrutinee() {
    // `match opt { Some(s) => ... }` where s is non-Copy String binds
    // the inner value by-move, so the scrutinee `opt` is consumed.
    let errors = ownership_errors(
        "fn main() {\n\
             let opt: Option[String] = Some(\"hi\");\n\
             let _ = match opt { Some(s) => s, None => \"\" };\n\
             let _ = opt;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove on `let _ = opt` after match-with-binding, got: {errors:?}"
    );
}

#[test]
fn test_match_with_all_wildcards_does_not_consume() {
    // `match opt { Some(_) | None => 0 }` has no bindings — scrutinee is
    // just read.
    ownership_ok(
        "fn main() {\n\
             let opt: Option[String] = Some(\"hi\");\n\
             let _ = match opt { Some(_) => 1, None => 0 };\n\
             let _ = opt;\n\
         }",
    );
}

#[test]
fn test_match_on_copy_scrutinee_with_binding_is_noop() {
    // Match on `Option[i64]` (Copy) with a binding — the consume of the
    // scrutinee is a no-op because Option[i64] is Copy.
    ownership_ok(
        "fn main() {\n\
             let opt: Option[i64] = Some(42);\n\
             let _ = match opt { Some(n) => n, None => 0 };\n\
             let _ = opt;\n\
         }",
    );
}

#[test]
fn test_match_at_binding_consumes_non_copy_scrutinee() {
    // `match opt { whole @ Some(_) => ... }` — the @-binding takes
    // ownership of the whole value, so the scrutinee is consumed.
    let errors = ownership_errors(
        "fn main() {\n\
             let opt: Option[String] = Some(\"hi\");\n\
             let _ = match opt { whole @ Some(_) => whole, None => None };\n\
             let _ = opt;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove after at-binding match, got: {errors:?}"
    );
}

#[test]
fn test_match_struct_pattern_with_field_binding_consumes() {
    // Struct pattern that binds a non-Copy field consumes the scrutinee.
    let errors = ownership_errors(
        "struct Container { name: String, n: i64 }\n\
         fn main() {\n\
             let c = Container { name: \"hi\", n: 1 };\n\
             let _ = match c { Container { name, n: _ } => name };\n\
             let _ = c;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove after struct-pattern match with field binding, got: {errors:?}"
    );
}

// ── Closure capture-mode prefix (Rule 2½ / K2 conflict table) ────

#[test]
fn test_capture_mode_ref_consume_is_error() {
    // `ref |...|` forbids consume of any captured name.
    let errors = ownership_errors(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = ref || { let _ = o; };\n\
             let _ = f;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::CaptureModeViolation
                && e.message.contains("`ref`")
                && e.message.contains("`o`")),
        "expected CaptureModeViolation naming `ref` and capture `o`, got: {errors:?}"
    );
}

#[test]
fn test_capture_mode_mut_ref_consume_is_error() {
    // `mut ref |...|` also forbids consume.
    let errors = ownership_errors(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = mut ref || { let _ = o; };\n\
             let _ = f;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::CaptureModeViolation
                && e.message.contains("`mut ref`")
                && e.message.contains("`o`")),
        "expected CaptureModeViolation naming `mut ref` and capture `o`, got: {errors:?}"
    );
}

#[test]
fn test_capture_mode_ref_read_only_is_ok() {
    // `ref |...|` accepts a read-only body.
    ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = ref || o.x + 1;\n\
             let _ = f();\n\
         }",
    );
}

#[test]
fn test_capture_mode_mut_ref_read_only_is_ok() {
    // `mut ref |...|` declared but body only reads — typechecks (perf note
    // for `unused-mut-capture` is a future enhancement; not asserted here).
    ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = mut ref || o.x + 1;\n\
             let _ = f();\n\
         }",
    );
}

#[test]
fn test_capture_mode_bare_consume_unchanged() {
    // Regression: bare `||` form continues to capture-and-consume normally
    // — once-callable inference still applies, no K2 violation. Program
    // typechecks and passes ownership (closure called once, captured value
    // not used after).
    ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = || { let _ = o; };\n\
             f();\n\
         }",
    );
}

#[test]
fn test_capture_mode_ref_does_not_taint_outer_binding() {
    // When `ref |...|` rejects a body consume, the outer binding `o` should
    // remain usable — the closure expression is the error site; the outer
    // value was never actually moved into a successful closure.
    let errors = ownership_errors(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let _f = ref || { let _ = o; };\n\
             let _g = o;\n\
         }",
    );
    // Exactly one CaptureModeViolation; no UseAfterMove cascading from it.
    let cmv = errors
        .iter()
        .filter(|e| e.kind == OwnershipErrorKind::CaptureModeViolation)
        .count();
    assert_eq!(
        cmv, 1,
        "expected exactly one CaptureModeViolation, got: {errors:?}"
    );
}

// ── Closure capture-mode prefix: unused-mut-capture perf note (round 12.5) ────
//
// Rule 2½ K2 conflict-table row "mut ref + reads only": when a closure
// declared `mut ref |...|` references a capture but never mutates it,
// the closure typechecks (declared mode is stronger than body usage —
// permitted under the stronger-or-equal rule), but a Tier 2 perf note
// fires suggesting `ref` instead.

#[test]
fn test_unused_mut_capture_note_fires_for_read_only_use() {
    // Canonical case: `mut ref` declared, body only reads `o.x`.
    let result = ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = mut ref || o.x + 1;\n\
             let _ = f();\n\
         }",
    );
    let notes: Vec<_> = result
        .notes
        .iter()
        .filter(|n| n.kind == OwnershipErrorKind::UnusedMutCaptureNote)
        .collect();
    assert_eq!(
        notes.len(),
        1,
        "expected exactly one UnusedMutCaptureNote, got notes: {:?}",
        result.notes,
    );
    assert!(
        notes[0].message.contains("`o`")
            && notes[0].message.contains("`mut ref`")
            && notes[0].message.contains("never mutated"),
        "unexpected note message: {}",
        notes[0].message
    );
    assert!(
        notes[0]
            .suggestion
            .as_ref()
            .is_some_and(|s| s.contains("`ref`")),
        "expected suggestion mentioning `ref`, got: {:?}",
        notes[0].suggestion
    );
}

#[test]
fn test_unused_mut_capture_note_does_not_fire_when_capture_mutated_via_field_assign() {
    // `mut ref` declared and the body assigns `o.x = ...` — declared mode
    // matches body usage, no perf note.
    let result = ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let mut o = Owned { x: 1 };\n\
             let f = mut ref || { o.x = 2; };\n\
             f();\n\
         }",
    );
    assert!(
        result
            .notes
            .iter()
            .all(|n| n.kind != OwnershipErrorKind::UnusedMutCaptureNote),
        "expected no UnusedMutCaptureNote, got: {:?}",
        result.notes
    );
}

#[test]
fn test_unused_mut_capture_note_does_not_fire_for_bare_closure() {
    // Bare `||` (owned default) + read-only body — the K2 row "owned + reads
    // only" is OK with no note (capture-for-ownership-extension idiom).
    let result = ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = || o.x + 1;\n\
             let _ = f();\n\
         }",
    );
    assert!(
        result
            .notes
            .iter()
            .all(|n| n.kind != OwnershipErrorKind::UnusedMutCaptureNote),
        "expected no UnusedMutCaptureNote for bare closure, got: {:?}",
        result.notes
    );
}

#[test]
fn test_unused_mut_capture_note_does_not_fire_for_ref_closure() {
    // `ref` declared + read-only body matches exactly — no perf note.
    let result = ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = ref || o.x + 1;\n\
             let _ = f();\n\
         }",
    );
    assert!(
        result
            .notes
            .iter()
            .all(|n| n.kind != OwnershipErrorKind::UnusedMutCaptureNote),
        "expected no UnusedMutCaptureNote for `ref` closure, got: {:?}",
        result.notes
    );
}

#[test]
fn test_unused_mut_capture_note_per_capture_when_body_mixes() {
    // Two captures: one mutated, one read-only. The note fires for the
    // read-only one and not for the mutated one. Both names appear in the
    // body so both are real captures from the K2 perspective.
    let result = ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let mut a = Owned { x: 1 };\n\
             let b = Owned { x: 2 };\n\
             let f = mut ref || { a.x = b.x + 1; };\n\
             f();\n\
         }",
    );
    let notes: Vec<_> = result
        .notes
        .iter()
        .filter(|n| n.kind == OwnershipErrorKind::UnusedMutCaptureNote)
        .collect();
    assert_eq!(
        notes.len(),
        1,
        "expected exactly one UnusedMutCaptureNote (for `b`), got: {:?}",
        result.notes
    );
    assert!(
        notes[0].message.contains("`b`"),
        "expected note to name `b`, got: {}",
        notes[0].message
    );
}

#[test]
fn test_unused_mut_capture_note_does_not_fire_for_mut_ref_self_method() {
    // `mut ref` declared and the body invokes a `mut ref self` method on
    // the capture — this is a mutation, no perf note. (Validates the
    // method-self-mode lookup path in classify_capture_body_uses.)
    let result = ownership_ok(
        "struct Counter { value: i64 }\n\
         impl Counter { fn bump(mut ref self) { self.value = self.value + 1; } }\n\
         fn main() {\n\
             let mut c = Counter { value: 0 };\n\
             let f = mut ref || { c.bump(); };\n\
             f();\n\
         }",
    );
    assert!(
        result
            .notes
            .iter()
            .all(|n| n.kind != OwnershipErrorKind::UnusedMutCaptureNote),
        "expected no UnusedMutCaptureNote for `mut ref self` method call, got: {:?}",
        result.notes
    );
}

#[test]
fn test_unused_mut_capture_note_does_not_fire_when_capture_unreferenced() {
    // `mut ref` declared but the body never references the would-be
    // capture — there's no capture at all, so no note. (The note is
    // specifically for captures that ARE referenced but only as reads.)
    let result = ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let _o = Owned { x: 1 };\n\
             let f = mut ref || 42;\n\
             let _ = f();\n\
         }",
    );
    assert!(
        result
            .notes
            .iter()
            .all(|n| n.kind != OwnershipErrorKind::UnusedMutCaptureNote),
        "expected no UnusedMutCaptureNote when body has no captures, got: {:?}",
        result.notes
    );
}

#[test]
fn test_unused_mut_capture_note_carries_machine_applicable_replacement() {
    // Round 12.31: the N0507 perf note gains the same machine-applicable
    // `replacement` metadata the resolver classes already carry. The
    // TextEdit covers exactly the `mut ref` prefix tokens — no closure
    // body, no surrounding whitespace — and replaces them with `ref`.
    // Applying the edit converts `mut ref || o.x + 1` to `ref || o.x + 1`
    // in place, ready for `karac fix`-style consumers and IDE quick-fix
    // UIs without further dispatcher work.
    let src = "struct Owned { x: i64 }\n\
               fn main() {\n\
                   let o = Owned { x: 1 };\n\
                   let f = mut ref || o.x + 1;\n\
                   let _ = f();\n\
               }";
    let result = ownership_ok(src);
    let note = result
        .notes
        .iter()
        .find(|n| n.kind == OwnershipErrorKind::UnusedMutCaptureNote)
        .expect("expected an UnusedMutCaptureNote");
    let edit = note
        .replacement
        .as_deref()
        .expect("N0507 should carry a TextEdit");
    assert_eq!(
        edit.replacement, "ref",
        "replacement text should be `ref`, got `{}`",
        edit.replacement
    );
    let original = &src[edit.offset..edit.offset + edit.length];
    assert_eq!(
        original, "mut ref",
        "edit span should cover only the prefix tokens, got `{original}`",
    );
    let mut rewritten = src.to_string();
    rewritten.replace_range(edit.offset..edit.offset + edit.length, &edit.replacement);
    assert_eq!(
        rewritten,
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = ref || o.x + 1;\n\
             let _ = f();\n\
         }",
        "applying the edit should swap `mut ref` for `ref` in place",
    );
}

#[test]
fn test_unused_mut_capture_note_replacement_per_note_when_multiple_unused() {
    // Two read-only captures in one `mut ref` closure produce two notes,
    // each carrying the same prefix-rewrite TextEdit. The dispatcher in
    // `cmd_fix` already dedupes overlapping edits, so applying produces
    // a single rewrite. Pinned because the note is emitted per-capture
    // in a loop and the replacement plumbing must populate every note
    // independently — not just the first.
    let src = "struct Owned { x: i64 }\n\
               fn main() {\n\
                   let a = Owned { x: 1 };\n\
                   let b = Owned { x: 2 };\n\
                   let f = mut ref || a.x + b.x;\n\
                   let _ = f();\n\
               }";
    let result = ownership_ok(src);
    let notes: Vec<_> = result
        .notes
        .iter()
        .filter(|n| n.kind == OwnershipErrorKind::UnusedMutCaptureNote)
        .collect();
    assert_eq!(notes.len(), 2, "expected one note per read-only capture");
    for n in &notes {
        let edit = n
            .replacement
            .as_deref()
            .expect("each N0507 note should carry a TextEdit");
        assert_eq!(edit.replacement, "ref");
        let original = &src[edit.offset..edit.offset + edit.length];
        assert_eq!(original, "mut ref");
    }
    // All notes target the same prefix, so the edits are byte-identical —
    // a sanity check on the per-note populate path.
    let first = notes[0].replacement.as_deref().unwrap();
    let second = notes[1].replacement.as_deref().unwrap();
    assert_eq!(first.offset, second.offset);
    assert_eq!(first.length, second.length);
}

// ── Closure calling through `ref` — repeatable-closure multi-call (round 12.6) ────
//
// Item 23: explicit `ref |...|` / `mut ref |...|` capture-mode prefixes
// guarantee a closure is *repeatable* — its captures are borrowed, not
// consumed, so calling it does not move the closure binding. Multiple
// invocations are valid. Bare `|...|` capturing an owned non-Copy value
// remains once-callable (existing behavior, regressed here for symmetry).

#[test]
fn test_ref_closure_multi_call_ok() {
    // `ref ||` borrows captures — closure is repeatable, multiple calls OK.
    ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = ref || o.x + 1;\n\
             let _ = f();\n\
             let _ = f();\n\
             let _ = f();\n\
         }",
    );
}

#[test]
fn test_mut_ref_closure_multi_call_ok() {
    // `mut ref ||` mutates borrowed captures — closure is repeatable.
    // Body uses a Copy field rebind (the perf note for "never mutated"
    // does not block the program; we explicitly check no errors).
    ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let mut o = Owned { x: 1 };\n\
             let f = mut ref || { o.x = o.x + 1; };\n\
             f();\n\
             f();\n\
         }",
    );
}

#[test]
fn test_ref_closure_keeps_outer_binding_usable_after_calls() {
    // After multiple `ref ||` invocations the outer binding `o` is still
    // usable — the captures are borrows, not moves.
    ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = ref || o.x + 1;\n\
             let _ = f();\n\
             let _ = f();\n\
             let _y = o.x;\n\
         }",
    );
}

#[test]
fn test_bare_once_callable_still_rejects_second_call() {
    // Regression for the contrast: bare `||` capturing owned non-Copy
    // remains once-callable; the second call is a use-after-move.
    let errors = ownership_errors(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = || { let _ = o; };\n\
             f();\n\
             f();\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseAfterMove),
        "expected UseAfterMove on second call of bare-form once-callable closure, got: {errors:?}"
    );
}

#[test]
fn test_ref_closure_called_in_loop_ok() {
    // The motivating pattern from design.md §3638: a repeatable closure
    // invoked many times — here, the same binding called inside a loop.
    ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 7 };\n\
             let f = ref || o.x + 1;\n\
             for _i in 0..3 {\n\
                 let _ = f();\n\
             }\n\
         }",
    );
}

// ── Uninitialized `let pat: T;` plumbing (round 12.1) ───────────
//
// 12.1 lands AST + parser + walker plumbing only. Definite-assignment
// semantics arrive in 12.2, so today the binding is registered as `Live`
// — meaning programs that read before initializing do NOT yet error.
// These tests pin the plumbing in place so 12.2 can swap the state
// machine without breaking the surface.

#[test]
fn test_let_uninit_then_assign_then_read_passes() {
    // Canonical first-assignment-is-init flow. Even with `let mut x: T;
    // x = ...; use(x);` the binding is registered cleanly through every
    // phase and ownership doesn't reject it.
    ownership_ok(
        "fn main() {\n\
            let mut x: i64;\n\
            x = 5;\n\
            let _y = x;\n\
        }",
    );
}

#[test]
fn test_let_uninit_array_plumbing() {
    // `let arr: Array[T, N];` is the form the design.md spec calls out for
    // the whole-value-assignment rule. Round 12.1 just plumbs the AST end
    // to end; the per-slot-write rule lands in 12.3.
    ownership_ok(
        "fn main() {\n\
            let mut arr: Array[i64, 4];\n\
            arr = [1, 2, 3, 4];\n\
            let _x = arr;\n\
        }",
    );
}

#[test]
fn test_let_uninit_immutable_first_assign_is_init() {
    // Per design.md §1689 the first assignment to `let x: T;` (no `mut`)
    // counts as initialization, not reassignment, so it succeeds even
    // though the binding wasn't declared `mut`.
    ownership_ok(
        "fn main() {\n\
            let x: i64;\n\
            x = 7;\n\
            let _y = x;\n\
        }",
    );
}

// ── Round 12.2: definite-assignment scalar flow ─────────────────

#[test]
fn test_read_uninit_errors() {
    // Reading a `let x: T;` binding before any assignment is a definite-
    // assignment failure.
    let errors = ownership_errors(
        "fn main() {\n\
            let x: i64;\n\
            let _ = x;\n\
        }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseOfUninitialized),
        "expected UseOfUninitialized, got {:?}",
        errors.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_read_uninit_in_rhs_of_first_assign_errors() {
    // `let x: T; x = f(x);` — the `x` inside the RHS is still uninit
    // when evaluated, so the read errors even though the assign would
    // otherwise initialize.
    let errors = ownership_errors(
        "fn main() {\n\
            let x: i64;\n\
            x = x + 1;\n\
        }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseOfUninitialized),
        "expected UseOfUninitialized, got {:?}",
        errors.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_second_assign_to_immutable_uninit_errors() {
    // `let x: T;` without `mut`: first assign is initialization (OK),
    // second assign requires `let mut x: T;`.
    let errors = ownership_errors(
        "fn main() {\n\
            let x: i64;\n\
            x = 5;\n\
            x = 6;\n\
        }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::ReassignToImmutable),
        "expected ReassignToImmutable, got {:?}",
        errors.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_let_mut_uninit_allows_reassign() {
    // `let mut x: T;` allows arbitrary reassignment after the first init.
    ownership_ok(
        "fn main() {\n\
            let mut x: i64;\n\
            x = 1;\n\
            x = 2;\n\
            x = 3;\n\
            let _y = x;\n\
        }",
    );
}

#[test]
fn test_init_in_both_branches_promotes() {
    // Both arms of the if/else assign — the join is initialized.
    ownership_ok(
        "fn main() {\n\
            let x: i64;\n\
            if true {\n\
                x = 1;\n\
            } else {\n\
                x = 2;\n\
            }\n\
            let _y = x;\n\
        }",
    );
}

#[test]
fn test_init_in_only_then_branch_does_not_promote() {
    // One-armed if (no else) cannot promote — the value would be uninit
    // on the falling-through path.
    let errors = ownership_errors(
        "fn main() {\n\
            let x: i64;\n\
            if true {\n\
                x = 1;\n\
            }\n\
            let _y = x;\n\
        }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseOfUninitialized),
        "expected UseOfUninitialized, got {:?}",
        errors.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_init_in_only_one_arm_of_if_else_does_not_promote() {
    // Only one branch of an else-bearing if assigns — still partial init.
    let errors = ownership_errors(
        "fn main() {\n\
            let x: i64;\n\
            if true {\n\
                x = 1;\n\
            } else {\n\
                let _ = 0;\n\
            }\n\
            let _y = x;\n\
        }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseOfUninitialized),
        "expected UseOfUninitialized, got {:?}",
        errors.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_init_in_loop_body_does_not_promote_after_loop() {
    // Loop body may run zero times; an assign inside the body cannot
    // satisfy DA after the loop.
    let errors = ownership_errors(
        "fn main() {\n\
            let x: i64;\n\
            while false {\n\
                x = 1;\n\
            }\n\
            let _y = x;\n\
        }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseOfUninitialized),
        "expected UseOfUninitialized, got {:?}",
        errors.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_init_then_read_inside_loop_body_passes() {
    // Inside the same iteration: assign-then-read on a `mut` binding is
    // fine. (Not the cross-iteration DA case.)
    ownership_ok(
        "fn main() {\n\
            let mut x: i64;\n\
            while false {\n\
                x = 1;\n\
                let _y = x;\n\
            }\n\
        }",
    );
}

#[test]
fn test_match_all_arms_init_promotes() {
    // Exhaustive match where every arm assigns — the join is initialized.
    ownership_ok(
        "fn main() {\n\
            let x: i64;\n\
            let n: i64 = 7;\n\
            match n {\n\
                0 => { x = 100; },\n\
                _ => { x = 200; },\n\
            }\n\
            let _y = x;\n\
        }",
    );
}

#[test]
fn test_match_one_arm_uninit_does_not_promote() {
    // If even one match arm leaves the binding uninit, the join is uninit.
    let errors = ownership_errors(
        "fn main() {\n\
            let x: i64;\n\
            let n: i64 = 7;\n\
            match n {\n\
                0 => { x = 100; },\n\
                _ => { let _ = 0; },\n\
            }\n\
            let _y = x;\n\
        }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == OwnershipErrorKind::UseOfUninitialized),
        "expected UseOfUninitialized, got {:?}",
        errors.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

// ── Round 12.3: Array per-slot DA rule ──────────────────────────

#[test]
fn test_read_uninit_array_errors_with_array_specific_message() {
    // Plain read of an uninit Array binding — error fires and the
    // diagnostic is array-specific (mentions whole-value-assignment and
    // the canonical fully-initialized constructors).
    let errors = ownership_errors(
        "fn main() {\n\
            let arr: Array[i64, 4];\n\
            let _x = arr;\n\
        }",
    );
    let array_err = errors
        .iter()
        .find(|e| e.kind == OwnershipErrorKind::UseOfUninitialized);
    let err = array_err.expect("expected UseOfUninitialized for array read");
    assert!(
        err.message.contains("uninitialized array"),
        "expected array-specific message, got {:?}",
        err.message
    );
    let suggestion = err.suggestion.as_deref().expect("expected a suggestion");
    assert!(
        suggestion.contains("whole value") && suggestion.contains("Array.from_fn"),
        "expected whole-value-assignment suggestion, got {:?}",
        suggestion
    );
}

#[test]
fn test_index_assign_to_uninit_array_does_not_promote() {
    // Per design.md §1097 the v1 DA analyser tracks whole-value assignment
    // only — `arr[0] = ...` on a `let mut arr: Array[T, N];` does NOT
    // promote `arr` to Live. The index assign reads `arr` to compute the
    // address, which fires UseOfUninitialized; a subsequent read of `arr`
    // is *also* uninit (the slot fill did not satisfy DA).
    let errors = ownership_errors(
        "fn main() {\n\
            let mut arr: Array[i64, 4];\n\
            arr[0] = 1;\n\
            let _x = arr;\n\
        }",
    );
    // At least the index-assign read should fire. The trailing read of
    // `arr` should also fire — i.e. `arr` was NOT promoted by the slot
    // assign.
    let array_uninit_count = errors
        .iter()
        .filter(|e| {
            e.kind == OwnershipErrorKind::UseOfUninitialized
                && e.message.contains("uninitialized array")
        })
        .count();
    assert!(
        array_uninit_count >= 2,
        "expected at least two array-uninit errors (index-assign + trailing read), got {} in {:?}",
        array_uninit_count,
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_whole_value_assign_to_uninit_array_promotes() {
    // The canonical happy path — whole-value assign promotes; subsequent
    // reads succeed.
    ownership_ok(
        "fn main() {\n\
            let mut arr: Array[i64, 4];\n\
            arr = [1, 2, 3, 4];\n\
            let _x = arr;\n\
        }",
    );
}

#[test]
fn test_index_assign_to_initialized_array_is_fine() {
    // After a whole-value init, index-assign is just a normal mutation.
    // No DA error here — the rule is about reaching the *initial* assign,
    // not about subsequent slot writes.
    ownership_ok(
        "fn main() {\n\
            let mut arr: Array[i64, 4];\n\
            arr = [0, 0, 0, 0];\n\
            arr[0] = 1;\n\
            arr[1] = 2;\n\
            let _x = arr;\n\
        }",
    );
}

#[test]
fn test_repeat_literal_assign_to_uninit_array_promotes() {
    // `Array[v; n]` repeat-literal as the whole-value RHS satisfies DA.
    ownership_ok(
        "fn main() {\n\
            let mut arr: Array[i64, 4];\n\
            arr = Array[0; 4];\n\
            let _x = arr;\n\
        }",
    );
}

#[test]
fn test_index_read_on_uninit_array_errors() {
    // Reading a slot (`arr[i]`) on an uninit array also reads `arr` and
    // fires the array-specific DA error.
    let errors = ownership_errors(
        "fn main() {\n\
            let arr: Array[i64, 4];\n\
            let _v = arr[0];\n\
        }",
    );
    assert!(
        errors.iter().any(|e| {
            e.kind == OwnershipErrorKind::UseOfUninitialized
                && e.message.contains("uninitialized array")
        }),
        "expected array-specific UseOfUninitialized, got {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_per_slot_init_all_slots_does_not_satisfy_da() {
    // The "every slot is eventually written" pattern is *explicitly*
    // rejected by §1097 — DA does not look at slot coverage; only the
    // whole-value assignment counts.
    let errors = ownership_errors(
        "fn main() {\n\
            let mut arr: Array[i64, 4];\n\
            arr[0] = 1;\n\
            arr[1] = 2;\n\
            arr[2] = 3;\n\
            arr[3] = 4;\n\
            let _x = arr;\n\
        }",
    );
    assert!(
        errors.iter().any(|e| {
            e.kind == OwnershipErrorKind::UseOfUninitialized
                && e.message.contains("uninitialized array")
        }),
        "expected array DA error even after per-slot fill, got {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

// ── Round 12.21: UAM-via-predicate routing sentinels ────────────────

#[test]
fn test_uam_predicate_emits_single_error_per_binding() {
    // Round 12.21 routes UseAfterMove through the predicate, which
    // returns one witness per binding. Pin that the legacy state
    // machine no longer double-emits for the same binding when it
    // encounters the binding's use site after the move.
    let errors = ownership_errors(
        "struct Data { value: i64 }
         fn consume(d: Data) { }
         fn main() {
             let d = Data { value: 1 };
             consume(d);
             consume(d);
         }",
    );
    let uam_count = errors
        .iter()
        .filter(|e| e.kind == OwnershipErrorKind::UseAfterMove && e.message.contains("'d'"))
        .count();
    assert_eq!(
        uam_count, 1,
        "expected exactly one UAM for binding 'd' under predicate routing; got {:?}",
        errors
    );
}

#[test]
fn test_uam_predicate_carries_consume_and_use_spans() {
    // The predicate's UamWitness uses (consume_span, other_use_span)
    // pairs. Pin that the emitted error's span points at the
    // OFFENDING USE (not the consume) — matching the legacy contract.
    let errors = ownership_errors(
        "struct Data { value: i64 }
         fn consume(d: Data) { }
         fn main() {
             let d = Data { value: 1 };
             consume(d);
             consume(d);
         }",
    );
    let uam = errors
        .iter()
        .find(|e| e.kind == OwnershipErrorKind::UseAfterMove)
        .expect("expected at least one UAM error");
    // The error span is on the second `consume(d)` (line 6 in this
    // source; line 1 is `struct ...`, line 4 is `let d = ...`,
    // line 5 is the first consume, line 6 is the use-after-move).
    assert_eq!(
        uam.span.line, 6,
        "UAM span should point at the second consume site (line 6), got line {}",
        uam.span.line
    );
    // The message includes the moved-at line (5) of the first consume.
    assert!(
        uam.message.contains("moved at line 5"),
        "expected message to reference line 5 (the consume); got: {}",
        uam.message
    );
}

// ── Round 12.23: Closure parameter mode inference (Step 1) ─────────
//
// Each closure expression `|x, y, z| body` has its parameters
// classified `own` / `ref` / `mut ref` by the same `ParamUsage`
// scan that drives fn-param inference. Inferred modes are surfaced
// per-closure via `OwnershipCheckResult::closure_param_modes`,
// keyed by the closure expression's `SpanKey`. These tests pin the
// classification rule for every variant of body usage.

/// Pull the inferred mode list for the single closure in `result`.
/// Asserts exactly one closure is recorded — useful for the
/// per-shape unit tests below where the source has one closure.
fn single_closure_modes(result: &OwnershipCheckResult) -> &Vec<(String, OwnershipMode)> {
    assert_eq!(
        result.closure_param_modes.len(),
        1,
        "expected exactly one closure in source; got {} entries: {:?}",
        result.closure_param_modes.len(),
        result.closure_param_modes.keys().collect::<Vec<_>>()
    );
    result.closure_param_modes.values().next().unwrap()
}

#[test]
fn closure_param_consumed_inferred_own() {
    // The closure's body consumes `x` (passes it owned to a
    // bare-`T` slot) → inferred mode is `own`.
    let result = ownership_ok(
        "struct Data { v: i64 }\n\
         fn take(d: Data) { }\n\
         fn main() {\n\
             let f = |x: Data| take(x);\n\
             let d = Data { v: 1 };\n\
             f(d);\n\
         }",
    );
    let modes = single_closure_modes(&result);
    assert_eq!(
        modes.as_slice(),
        &[("x".to_string(), OwnershipMode::Own)],
        "consume in body should infer own; got {:?}",
        modes
    );
}

#[test]
fn closure_param_read_only_inferred_ref() {
    // Body only reads through a Copy field projection → no consume,
    // mode is `ref`.
    let result = ownership_ok(
        "struct Config { value: i64 }\n\
         fn main() {\n\
             let f = |c: Config| c.value + 1;\n\
             let cfg = Config { value: 10 };\n\
             let _r = f(cfg);\n\
         }",
    );
    let modes = single_closure_modes(&result);
    assert_eq!(
        modes.as_slice(),
        &[("c".to_string(), OwnershipMode::Ref)],
        "read-only body should infer ref; got {:?}",
        modes
    );
}

#[test]
fn closure_param_unused_inferred_ref() {
    // Body never references the closure parameter → mode defaults
    // to `ref` (same as fn-param inference's `Unused`).
    let result = ownership_ok(
        "fn main() {\n\
             let f = |_x: i64| 42;\n\
             let _r = f(7);\n\
         }",
    );
    let modes = single_closure_modes(&result);
    assert_eq!(
        modes.as_slice(),
        &[("_x".to_string(), OwnershipMode::Ref)],
        "unused param should infer ref; got {:?}",
        modes
    );
}

#[test]
fn closure_multiple_params_each_classified_independently() {
    // First param consumed (own), second read-only (ref), third
    // unused (ref). Pins the per-param classification independence.
    let result = ownership_ok(
        "struct Data { v: i64 }\n\
         struct Config { value: i64 }\n\
         fn take(d: Data) { }\n\
         fn main() {\n\
             let f = |a: Data, b: Config, _c: i64| { take(a); b.value };\n\
             let _r = f(Data { v: 1 }, Config { value: 2 }, 3);\n\
         }",
    );
    let modes = single_closure_modes(&result);
    assert_eq!(
        modes.as_slice(),
        &[
            ("a".to_string(), OwnershipMode::Own),
            ("b".to_string(), OwnershipMode::Ref),
            ("_c".to_string(), OwnershipMode::Ref),
        ],
        "modes should be classified independently per param; got {:?}",
        modes
    );
}

#[test]
fn closure_param_does_not_pollute_outer_param_usage() {
    // A closure parameter that shadows an outer fn parameter must
    // not bleed its consumption signal back into the outer fn's
    // mode inference. Outer `x` is only read; the closure's `x` is
    // consumed. Outer must stay `ref`.
    let result = ownership_ok(
        "struct Data { v: i64 }\n\
         fn take(d: Data) { }\n\
         fn outer(x: i64) -> i64 {\n\
             let f = |x: Data| take(x);\n\
             f(Data { v: 0 });\n\
             x\n\
         }",
    );
    let outer_modes = result
        .param_modes
        .get("outer")
        .expect("outer must have inferred modes");
    assert_eq!(
        outer_modes.as_slice(),
        &[("x".to_string(), OwnershipMode::Ref)],
        "outer fn-param `x` must remain ref despite closure consuming its own `x`; got {:?}",
        outer_modes
    );
    // Closure's local `x` should still classify as own.
    let closure_modes = single_closure_modes(&result);
    assert_eq!(
        closure_modes.as_slice(),
        &[("x".to_string(), OwnershipMode::Own)],
        "closure-local `x` should infer own"
    );
}

#[test]
fn closure_param_modes_keyed_by_closure_expression_span() {
    // Two closures in the same function — each gets its own entry
    // keyed by the closure expression's span. Pins that the
    // closure_param_modes map is per-closure, not per-function.
    let src = "struct Data { v: i64 }\n\
               fn take(d: Data) { }\n\
               fn main() {\n\
                   let f = |a: Data| take(a);\n\
                   let g = |_b: i64| 0;\n\
                   f(Data { v: 1 });\n\
                   let _r = g(2);\n\
               }";
    let parsed = parse(src);
    let resolved = resolve(&parsed.program);
    let typed = typecheck(&parsed.program, &resolved);
    let result = ownershipcheck(&parsed.program, &typed);
    assert!(
        result.errors.is_empty(),
        "ownership errors: {:?}",
        result.errors
    );
    assert_eq!(
        result.closure_param_modes.len(),
        2,
        "expected two distinct closure entries; got {:?}",
        result.closure_param_modes.keys().collect::<Vec<_>>()
    );
    // Verify both are keyed by valid SpanKeys (non-default) and
    // their inferred modes match the body shapes.
    let modes_lists: Vec<_> = result.closure_param_modes.values().collect();
    let owns: Vec<_> = modes_lists
        .iter()
        .filter(|m| matches!(m.first(), Some((_, OwnershipMode::Own))))
        .collect();
    let refs: Vec<_> = modes_lists
        .iter()
        .filter(|m| matches!(m.first(), Some((_, OwnershipMode::Ref))))
        .collect();
    assert_eq!(owns.len(), 1, "exactly one closure should have own param");
    assert_eq!(refs.len(), 1, "exactly one closure should have ref param");
    // Sanity: SpanKey is real; constructing a fresh one should not
    // collide unless we passed the same span.
    let fresh = SpanKey(0, 0);
    assert!(!result.closure_param_modes.contains_key(&fresh));
}

// ── Round 12.24: Closure capture collection (Step 2) ────────────
//
// Each closure expression's free variables — outer-scope bindings
// referenced from inside the body — are surfaced via
// `OwnershipCheckResult::closure_captures`, keyed by the closure
// expression's span. Capture mode is derived from body usage:
// consume → `Own`, in-place mutation → `MutRef`, read-only → `Ref`.
// Names lexically shadowed by the closure's own parameters are NOT
// captured; the body's references to those names are to the
// closure-local. These tests pin the surface for each shape.

/// Pull the captures for the single closure in `result`.
fn single_closure_captures(result: &OwnershipCheckResult) -> &Vec<(String, OwnershipMode)> {
    assert_eq!(
        result.closure_captures.len(),
        1,
        "expected exactly one closure in source; got {} entries: {:?}",
        result.closure_captures.len(),
        result.closure_captures.keys().collect::<Vec<_>>()
    );
    result.closure_captures.values().next().unwrap()
}

#[test]
fn capture_consumed_in_body_is_own() {
    // Closure body moves the captured `cfg` into a consume-position
    // call → capture mode is `Own`.
    let result = ownership_ok(
        "struct Config { name: i64 }\n\
         fn apply(c: Config) { }\n\
         fn main() {\n\
             let cfg = Config { name: 1 };\n\
             let _f = || apply(cfg);\n\
         }",
    );
    let caps = single_closure_captures(&result);
    assert_eq!(
        caps.as_slice(),
        &[("cfg".to_string(), OwnershipMode::Own)],
        "consume-capture should fire Own; got {:?}",
        caps
    );
}

#[test]
fn capture_read_only_is_ref() {
    // Body only reads through a Copy field projection → capture
    // mode is `Ref`.
    let result = ownership_ok(
        "struct Config { value: i64 }\n\
         fn main() {\n\
             let cfg = Config { value: 1 };\n\
             let _f = || cfg.value + 1;\n\
         }",
    );
    let caps = single_closure_captures(&result);
    assert_eq!(
        caps.as_slice(),
        &[("cfg".to_string(), OwnershipMode::Ref)],
        "read-only capture should fire Ref; got {:?}",
        caps
    );
}

#[test]
fn capture_unreferenced_outer_is_not_captured() {
    // Outer binding `unused_v` is never referenced by the closure
    // body — it must NOT appear in the capture list. Outer `cfg` is
    // referenced.
    let result = ownership_ok(
        "struct Config { value: i64 }\n\
         fn main() {\n\
             let cfg = Config { value: 1 };\n\
             let unused_v = 42;\n\
             let _f = || cfg.value + 1;\n\
             let _u = unused_v;\n\
         }",
    );
    let caps = single_closure_captures(&result);
    let names: Vec<&str> = caps.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"cfg"),
        "cfg should be captured; got {:?}",
        names
    );
    assert!(
        !names.contains(&"unused_v"),
        "unused_v must not be captured; got {:?}",
        names
    );
}

#[test]
fn capture_excludes_shadowed_outer_name() {
    // Outer `x` shadowed by closure-local `x`. Body references `x`
    // (the closure-local). Outer `x` must NOT be in the capture
    // list. Pins the closure-param-set exclusion check.
    let result = ownership_ok(
        "struct Data { v: i64 }\n\
         fn take(d: Data) { }\n\
         fn outer(x: i64) -> i64 {\n\
             let _f = |x: Data| take(x);\n\
             x\n\
         }",
    );
    // Outer `x` is in pre_live but lexically shadowed by closure
    // param `x`. Capture list must be empty.
    let caps = single_closure_captures(&result);
    assert!(
        caps.is_empty(),
        "shadowed outer name must not appear as a capture; got {:?}",
        caps
    );
}

#[test]
fn capture_multiple_outer_bindings_each_classified() {
    // Two captures: one consumed, one read. Both should appear with
    // their respective modes; output is alphabetic by name.
    let result = ownership_ok(
        "struct Data { v: i64 }\n\
         struct Config { value: i64 }\n\
         fn take(d: Data) { }\n\
         fn main() {\n\
             let d = Data { v: 1 };\n\
             let cfg = Config { value: 2 };\n\
             let _f = || { take(d); cfg.value };\n\
         }",
    );
    let caps = single_closure_captures(&result);
    assert_eq!(
        caps.as_slice(),
        &[
            ("cfg".to_string(), OwnershipMode::Ref),
            ("d".to_string(), OwnershipMode::Own),
        ],
        "captures should list each outer binding with its body-derived mode; got {:?}",
        caps
    );
}

#[test]
fn capture_keyed_per_closure_expression() {
    // Two distinct closures in the same fn — each gets its own
    // capture list keyed by closure expression span.
    let src = "struct Config { value: i64 }\n\
               fn main() {\n\
                   let a = Config { value: 1 };\n\
                   let b = Config { value: 2 };\n\
                   let _f = || a.value;\n\
                   let _g = || b.value;\n\
               }";
    let parsed = parse(src);
    let resolved = resolve(&parsed.program);
    let typed = typecheck(&parsed.program, &resolved);
    let result = ownershipcheck(&parsed.program, &typed);
    assert!(
        result.errors.is_empty(),
        "ownership errors: {:?}",
        result.errors
    );
    assert_eq!(
        result.closure_captures.len(),
        2,
        "expected two closure entries; got {}",
        result.closure_captures.len()
    );
    let all_captured: Vec<String> = result
        .closure_captures
        .values()
        .flat_map(|caps| caps.iter().map(|(n, _)| n.clone()))
        .collect();
    assert!(all_captured.contains(&"a".to_string()));
    assert!(all_captured.contains(&"b".to_string()));
    // Each closure has exactly one capture (a or b, not both).
    for caps in result.closure_captures.values() {
        assert_eq!(
            caps.len(),
            1,
            "each closure has one capture; got {:?}",
            caps
        );
    }
}

#[test]
fn nested_closures_each_get_their_own_mode_entry() {
    // Closure inside a closure body — both should be recorded with
    // independently-inferred modes.
    let result = ownership_ok(
        "struct Data { v: i64 }\n\
         fn take(d: Data) { }\n\
         fn main() {\n\
             let outer = |x: Data| {\n\
                 let inner = |y: Data| take(y);\n\
                 inner(x);\n\
             };\n\
             outer(Data { v: 1 });\n\
         }",
    );
    assert_eq!(
        result.closure_param_modes.len(),
        2,
        "outer + inner closures should both register; got {} entries",
        result.closure_param_modes.len()
    );
    // Both bodies consume their respective params → both should be
    // own.
    for modes in result.closure_param_modes.values() {
        assert_eq!(modes.len(), 1, "each closure has one param");
        assert_eq!(
            modes[0].1,
            OwnershipMode::Own,
            "both closures consume their param → both should be own; got {:?}",
            modes
        );
    }
}

// ── Bare-form per-capture inference pins (Rule 2½ default) ──────
//
// design.md § Closures, Rule 2½: "A bare closure `|x| body` runs
// Rule 2's first-use scan to infer each capture's mode (read → `ref`,
// mutate → `mut ref`, consume → `own`)." `capture_consumed_in_body_is_own`
// and `capture_read_only_is_ref` above pin the Own / Ref legs;
// the MutRef leg is pinned here. Plus negative-space coverage for the
// `own` prefix (the only one without a K2 conflict row to fire on a
// stronger-than-declared body usage — declared `own` is the strongest
// mode in the `ref < mut ref < own` ordering).

#[test]
fn capture_mutated_in_body_is_mut_ref() {
    // Bare body assigns through a captured root → `body_usage.mutated`
    // is set on `o`; classification picks `MutRef`. The MutRef leg of
    // the Rule 2 inference table — completes the {Ref, MutRef, Own}
    // trio.
    let result = ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let mut o = Owned { x: 1 };\n\
             let _f = || { o.x = 2; };\n\
         }",
    );
    let caps = single_closure_captures(&result);
    assert_eq!(
        caps.as_slice(),
        &[("o".to_string(), OwnershipMode::MutRef)],
        "field-assign mutation of captured root should classify capture as MutRef; got {:?}",
        caps
    );
}

#[test]
fn capture_mode_own_prefix_accepts_consume_body() {
    // Explicit `own ||` + body consumes capture → declared mode and
    // usage agree (K2 row "own + consumes"). Mirrors
    // `test_capture_mode_bare_consume_unchanged` but with the explicit
    // prefix pinned.
    ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = own || { let _ = o; };\n\
             f();\n\
         }",
    );
}

#[test]
fn capture_mode_own_prefix_accepts_read_only_body() {
    // K2 row "own + reads only": OK — the "capture for ownership
    // extension" idiom (closure holds the value by value; body chose
    // not to consume it). No UnusedMutCaptureNote — that note is
    // specific to the `mut ref` declared / read-only used gap.
    let result = ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = own || o.x + 1;\n\
             let _ = f();\n\
         }",
    );
    assert!(
        result
            .notes
            .iter()
            .all(|n| n.kind != OwnershipErrorKind::UnusedMutCaptureNote),
        "no UnusedMutCaptureNote should fire for `own` + read-only; got: {:?}",
        result.notes
    );
}

#[test]
fn capture_mode_ref_consume_diagnostic_includes_spec_fix_wording() {
    // Pin the K2 conflict diagnostic's spec-mandated fix wording from
    // design.md § Closures Rule 2½ conflict table for `ref` + consume:
    //   "drop the `ref` prefix (use `own` or bare) or remove the consume"
    // The existing `test_capture_mode_ref_consume_is_error` checks the
    // error kind + key terms; this test pins the full guidance string
    // so a future diagnostic rewrite cannot silently drop the redirect
    // shape.
    let errors = ownership_errors(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = ref || { let _ = o; };\n\
             let _ = f;\n\
         }",
    );
    let cmv = errors
        .iter()
        .find(|e| e.kind == OwnershipErrorKind::CaptureModeViolation)
        .expect("expected at least one CaptureModeViolation");
    let fix = cmv.suggestion.as_deref().unwrap_or("");
    assert!(
        fix.contains("drop the `ref` prefix")
            && fix.contains("`own` or bare")
            && fix.contains("remove the consume"),
        "ref K2 fix wording missing required phrases; got suggestion: {fix:?}"
    );
}

#[test]
fn capture_mode_mut_ref_consume_diagnostic_includes_spec_fix_wording() {
    // Symmetric pin for the `mut ref` + consume row:
    //   "drop the `mut ref` prefix and use `own`"
    let errors = ownership_errors(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = mut ref || { let _ = o; };\n\
             let _ = f;\n\
         }",
    );
    let cmv = errors
        .iter()
        .find(|e| e.kind == OwnershipErrorKind::CaptureModeViolation)
        .expect("expected at least one CaptureModeViolation");
    let fix = cmv.suggestion.as_deref().unwrap_or("");
    assert!(
        fix.contains("drop the `mut ref` prefix") && fix.contains("use `own`"),
        "mut ref K2 fix wording missing required phrases; got suggestion: {fix:?}"
    );
}

#[test]
fn bare_closure_read_capture_leaves_outer_binding_usable() {
    // Bare-form inference picks `Ref` for `o` (body only reads). The
    // outer scope can continue to read `o` after the closure's last
    // use — pins that outer-scope availability tracks the inferred
    // per-capture mode, not a blanket "closure consumes everything"
    // approximation.
    ownership_ok(
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let _f = || o.x + 1;\n\
             let _u = o.x;\n\
         }",
    );
}

#[test]
fn bare_closure_consume_capture_with_outer_use_routes_through_rc_fallback() {
    // Bare-form inference picks `Own` for `o` (body consumes via a
    // value-taking call). The post-closure `let _u = o;` is an outer
    // use of the consumed capture — by design (Rule 2 sub-case (ii) +
    // Part 4 RC trigger 2), this does NOT fire UseAfterMove; the RC
    // dataflow pass tentatively marks `o` as `Rc` instead. Pins the
    // routing: outer-use-after-Own-capture is NOT a hard error, it is
    // an opt-in to RC fallback. Symmetric to the read-only test above
    // (Ref capture leaves outer-scope use trivially valid; Own
    // capture leaves it valid via RC promotion).
    let result = ownership_ok(
        "struct Owned { x: i64 }\n\
         fn take(o: Owned) { }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let _f = || take(o);\n\
             let _u = o;\n\
         }",
    );
    let main_rcs = result
        .rc_values
        .get("main")
        .expect("expected rc_values entry for `main`");
    let o_entry = main_rcs
        .get("o")
        .expect("expected `o` to be RC-promoted via closure-capture-with-outer-use trigger");
    assert!(
        matches!(o_entry.trigger, RcTrigger::ClosureCaptureWithOuterUse),
        "expected RC trigger ClosureCaptureWithOuterUse on `o`; got {:?}",
        o_entry.trigger
    );
    // Capture mode for `o` is `Own` (body consumes via the value-take
    // call). Pin alongside the RC trigger so the two halves of the
    // routing story are asserted together.
    let caps = single_closure_captures(&result);
    assert_eq!(
        caps.as_slice(),
        &[("o".to_string(), OwnershipMode::Own)],
        "consume in body → Own capture; got {:?}",
        caps
    );
}

// ── Disjoint closure capture — slice 1 (capture-path enumeration) ─
//
// Phase-5 § Disjoint closure capture (line 353) slice 1: the closure
// analyser produces a `CapturePath { root, projection }` set per
// closure expression in addition to the per-name capture-mode list.
// Empty projection means "captured whole" (bare identifier or a
// reference through a stopping construct — index, method call, or
// deref). Non-empty projection lists the field-chain root-to-leaf.
// Slice 1 surfaces only the set; mode inference is slice 2,
// borrow-checker integration is slice 3.
//
// These tests pin the path-set shape produced for the closure-body
// constructs the spec calls out in its test plan.

/// Pull the capture-path list for the single closure in `result`.
fn single_closure_capture_paths(result: &OwnershipCheckResult) -> &Vec<CapturePath> {
    assert_eq!(
        result.closure_capture_paths.len(),
        1,
        "expected exactly one closure in source; got {} entries",
        result.closure_capture_paths.len()
    );
    result.closure_capture_paths.values().next().unwrap()
}

fn path(root: &str, projection: &[&str]) -> CapturePath {
    CapturePath {
        root: root.to_string(),
        projection: projection.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn capture_path_bare_identifier_is_whole_root() {
    // `|| take(cfg)` — the body references `cfg` as a bare identifier
    // (call arg). Path-set is `{(cfg, [])}` — root captured whole.
    let result = ownership_ok(
        "struct Config { name: i64 }\n\
         fn take(c: Config) { }\n\
         fn main() {\n\
             let cfg = Config { name: 1 };\n\
             let _f = || take(cfg);\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("cfg", &[])],
        "bare identifier should register whole-root path; got {:?}",
        paths
    );
}

#[test]
fn capture_path_single_field_chain_records_projection() {
    // `|| cfg.value + 1` — body reads a single field projection. Path
    // is `(cfg, ["value"])` — root + one-segment projection. The root
    // is NOT additionally registered as a whole capture (the spec
    // walker extends the path through field accesses; only stopping
    // constructs commit the root as whole).
    let result = ownership_ok(
        "struct Config { value: i64 }\n\
         fn main() {\n\
             let cfg = Config { value: 1 };\n\
             let _f = || cfg.value + 1;\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("cfg", &["value"])],
        "field projection should record projection chain only; got {:?}",
        paths
    );
}

#[test]
fn capture_path_nested_field_chain_records_full_projection() {
    // `|| u.profile.name` — body reads through two field segments.
    // Path is `(u, ["profile", "name"])` — full root-to-leaf chain.
    let result = ownership_ok(
        "struct Profile { name: i64 }\n\
         struct User { profile: Profile }\n\
         fn main() {\n\
             let u = User { profile: Profile { name: 1 } };\n\
             let _f = || u.profile.name + 1;\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("u", &["profile", "name"])],
        "nested field chain should record full projection; got {:?}",
        paths
    );
}

#[test]
fn capture_path_disjoint_fields_under_same_root_record_distinct_paths() {
    // `|| { u.name; u.age }` — two distinct field projections under
    // one root. Path-set is `{(u, ["age"]), (u, ["name"])}` — both
    // siblings recorded, sorted lexicographically. The root `u` is
    // NOT registered as whole — neither projection hits a stopping
    // construct, so the path walker extends through each access
    // independently. Pins the foundation slice 2/3 will use to
    // accept outer-scope sibling access of `u.history` after the
    // closure captures only `u.name` and `u.age`.
    let result = ownership_ok(
        "struct User { name: i64, age: i64 }\n\
         fn main() {\n\
             let u = User { name: 1, age: 2 };\n\
             let _f = || u.name + u.age;\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("u", &["age"]), path("u", &["name"])],
        "disjoint sibling fields should record distinct paths; got {:?}",
        paths
    );
}

#[test]
fn capture_path_index_commits_root_whole() {
    // `|| vec[0]` — index is a stopping construct per spec. The
    // walker commits the root `vec` as captured whole regardless of
    // what the indexed result is used for. Path-set is `{(vec, [])}`.
    // Slice 3's borrow checker will use this to deny outer-scope
    // sibling access when the closure captured the whole vec.
    let result = ownership_ok(
        "fn main() {\n\
             let vec = [1, 2, 3];\n\
             let _f = || vec[0] + 1;\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("vec", &[])],
        "index expression should commit root whole; got {:?}",
        paths
    );
}

#[test]
fn capture_path_method_call_receiver_commits_root_whole() {
    // `|| u.length()` — method call on a captured root is a stopping
    // construct (the method may use any/all of the receiver's state
    // through its `self` parameter; the analyser cannot tell which
    // fields a method touches without inter-procedural inspection).
    // Path-set is `{(u, [])}`.
    let result = ownership_ok(
        "struct User { name: i64 }\n\
         impl User { fn length(ref self) -> i64 { 0 } }\n\
         fn main() {\n\
             let u = User { name: 1 };\n\
             let _f = || u.length();\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("u", &[])],
        "method call on captured root should commit root whole; got {:?}",
        paths
    );
}

#[test]
fn capture_path_index_into_field_chain_commits_root_whole() {
    // `|| vec[0].field` — index appears inside the projection chain.
    // The walker hits Index before completing the FieldAccess
    // extraction; the root `vec` commits as captured whole. The
    // outer `.field` access surrounding the index does not extend
    // the path (its object is no longer a pure field chain).
    let result = ownership_ok(
        "struct Item { field: i64 }\n\
         fn main() {\n\
             let items = [Item { field: 1 }];\n\
             let _f = || items[0].field + 1;\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("items", &[])],
        "index inside field chain should commit root whole; got {:?}",
        paths
    );
}

#[test]
fn capture_path_method_call_on_field_chain_commits_root_whole() {
    // `|| u.profile.method()` — the method-call receiver `u.profile`
    // is a captured-rooted place; the receiver commits the root `u`
    // as captured whole. The intermediate `profile` projection is
    // NOT recorded as a separate path — it was an in-progress field
    // chain when the stopping construct fired.
    let result = ownership_ok(
        "struct Profile { name: i64 }\n\
         impl Profile { fn length(ref self) -> i64 { 0 } }\n\
         struct User { profile: Profile }\n\
         fn main() {\n\
             let u = User { profile: Profile { name: 1 } };\n\
             let _f = || u.profile.length();\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("u", &[])],
        "method call on field chain should commit root whole; got {:?}",
        paths
    );
}

#[test]
fn capture_path_multiple_roots_each_recorded_independently() {
    // Two distinct outer bindings, each touched through its own path
    // shape (one via field, one via index). Output is sorted by root
    // then projection.
    let result = ownership_ok(
        "struct Config { value: i64 }\n\
         fn main() {\n\
             let cfg = Config { value: 1 };\n\
             let arr = [10, 20, 30];\n\
             let _f = || cfg.value + arr[1];\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("arr", &[]), path("cfg", &["value"])],
        "two roots should appear sorted with their respective shapes; got {:?}",
        paths
    );
}

#[test]
fn capture_path_excludes_shadowed_outer_name() {
    // Outer `x` lexically shadowed by the closure's own parameter
    // `x`. The closure body's `x.v` references the closure-local,
    // not the outer binding. Path-set must be empty — the outer `x`
    // is not captured.
    let result = ownership_ok(
        "struct Data { v: i64 }\n\
         fn take(d: Data) { }\n\
         fn outer(x: i64) -> i64 {\n\
             let _f = |x: Data| take(x);\n\
             x\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert!(
        paths.is_empty(),
        "shadowed outer name must not appear in path-set; got {:?}",
        paths
    );
}

#[test]
fn capture_path_unreferenced_outer_name_produces_no_path() {
    // `unused_v` is in the outer scope but the closure body never
    // touches it. Only `cfg.value` is captured, registering one
    // path. Pins the parity with `closure_captures` exclusion of
    // unreferenced outer bindings.
    let result = ownership_ok(
        "struct Config { value: i64 }\n\
         fn main() {\n\
             let cfg = Config { value: 1 };\n\
             let unused_v = 42;\n\
             let _f = || cfg.value + 1;\n\
             let _u = unused_v;\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("cfg", &["value"])],
        "unreferenced outer name should not appear in path-set; got {:?}",
        paths
    );
}

#[test]
fn capture_path_tuple_index_extends_projection() {
    // `|| t.0 + 1` — tuple-index access extends the path the same
    // way struct-field access does, with the index segment
    // stringified into the projection vector.
    let result = ownership_ok(
        "fn main() {\n\
             let t = (10, 20);\n\
             let _f = || t.0 + 1;\n\
         }",
    );
    let paths = single_closure_capture_paths(&result);
    assert_eq!(
        paths.as_slice(),
        &[path("t", &["0"])],
        "tuple-index access should extend projection with stringified index; got {:?}",
        paths
    );
}

// ── Step 7 sentinels: ref-captured value escape (E0508) ─────────
//
// Round 12.35 — design.md § Closures Rule 2 sub-case (iv):
// "A capture-by-reference that would outlive its source is a
// standard borrow-check error, caught at the closure creation site
// when the closure value is assigned into something that escapes."
// This v1 round detects the unambiguous escape-via-return cases
// (direct return, let-bound return, implicit tail-expression
// return). Other escape destinations (fn-arg pass to an `Fn(...)`
// slot, struct-field store) require richer escape-tracking and are
// tracked as a follow-up entry. The diagnostic fires at the closure
// expression, names the offending capture, and offers three concrete
// fixes via the `suggestion` field.

fn step7_e0508_errors(source: &str) -> Vec<OwnershipError> {
    ownership_errors(source)
        .into_iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::RefCaptureEscapesScope))
        .collect()
}

#[test]
fn step7_direct_return_of_closure_with_ref_capture_fires() {
    // Closure body reads `cfg.value` (Copy projection); capture mode
    // for `cfg` is `Ref`. The closure is the operand of an explicit
    // `return` statement, so it escapes the function. The captured
    // `cfg` is owned (parameter declared `cfg: Config`, not `ref
    // Config`), so the ref capture would outlive the source. E0508.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         fn make_handler(cfg: Config) -> Fn() -> i64 {\n\
             return || cfg.value;\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "expected one E0508 error, got {:?}",
        errors
    );
    assert!(
        errors[0].message.contains("`ref` capture of `cfg`"),
        "message should name capture mode and binding; got {:?}",
        errors[0].message
    );
    assert!(
        errors[0]
            .suggestion
            .as_ref()
            .is_some_and(|s| s.contains("clone") && s.contains("restructure") && s.contains("own")),
        "suggestion should offer all three fixes; got {:?}",
        errors[0].suggestion
    );
}

#[test]
fn step7_implicit_tail_return_fires() {
    // No explicit `return` — the closure expression is the function
    // body's tail expression (implicit return). Same outcome as the
    // explicit-return form: ref capture escapes via return.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         fn make_handler(cfg: Config) -> Fn() -> i64 {\n\
             || cfg.value\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "expected one E0508 error, got {:?}",
        errors
    );
}

#[test]
fn step7_let_bound_return_fires() {
    // `let h = || cfg.value;` registers `h` in the closure-let map;
    // `return h;` resolves the identifier back to the closure span.
    // Same diagnostic fires at the closure expression, not at the
    // identifier or return statement.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         fn make_handler(cfg: Config) -> Fn() -> i64 {\n\
             let h = || cfg.value;\n\
             return h;\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "expected one E0508 error, got {:?}",
        errors
    );
}

#[test]
fn step7_let_bound_implicit_tail_return_fires() {
    // Combination of the previous two: let-bound closure followed by
    // a tail-expression identifier (implicit return).
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         fn make_handler(cfg: Config) -> Fn() -> i64 {\n\
             let h = || cfg.value;\n\
             h\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "expected one E0508 error, got {:?}",
        errors
    );
}

#[test]
fn step7_owned_capture_clean_escape_does_not_fire() {
    // Closure body consumes `cfg` (`apply` takes by value). Capture
    // is `Own`, not `Ref`. Sub-case (i) clean escape — no error.
    // Mirrors `step5_escape_via_return_direct_clean` from
    // tests/rc_fallback.rs but checks specifically for E0508.
    let parsed = parse(
        "struct Config { value: i64 }\n\
         fn apply(c: Config) { }\n\
         fn make_handler(cfg: Config) -> Fn() -> () {\n\
             return || apply(cfg);\n\
         }",
    );
    let resolved = resolve(&parsed.program);
    let typed = typecheck(&parsed.program, &resolved);
    let result = ownershipcheck(&parsed.program, &typed);
    let e0508s: Vec<_> = result
        .errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::RefCaptureEscapesScope))
        .collect();
    assert!(
        e0508s.is_empty(),
        "owned-capture clean escape should not fire E0508; got {:?}",
        e0508s,
    );
}

#[test]
fn step7_local_use_does_not_fire() {
    // Closure stays local — invoked inside the function, not
    // returned. No escape, no error. Even though the capture is
    // `Ref` (read-only body), there's no escape destination.
    let parsed = parse(
        "struct Config { value: i64 }\n\
         fn use_cfg(cfg: Config) -> i64 {\n\
             let h = || cfg.value;\n\
             h()\n\
         }",
    );
    let resolved = resolve(&parsed.program);
    let typed = typecheck(&parsed.program, &resolved);
    let result = ownershipcheck(&parsed.program, &typed);
    let e0508s: Vec<_> = result
        .errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::RefCaptureEscapesScope))
        .collect();
    assert!(
        e0508s.is_empty(),
        "local-use closure should not fire E0508; got {:?}",
        e0508s,
    );
}

#[test]
fn step7_branch_divergent_returns_each_fire() {
    // `if c { return || cfg.value; } else { return || cfg.value + 1; }`
    // — both branches return a closure with a ref capture; each must
    // independently fire E0508. Two distinct closure expressions →
    // two errors.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         fn make_handler(cfg: Config, c: bool) -> Fn() -> i64 {\n\
             if c {\n\
                 return || cfg.value;\n\
             } else {\n\
                 return || cfg.value + 1;\n\
             }\n\
         }",
    );
    assert_eq!(
        errors.len(),
        2,
        "both branches' closures should each fire E0508; got {:?}",
        errors
    );
}

#[test]
fn step7_diagnostic_span_points_at_closure_expression() {
    // The diagnostic's `span` should point at the closure expression
    // (the creation site), not at the return statement or the
    // captured identifier. Match the line of the closure literal.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         fn make_handler(cfg: Config) -> Fn() -> i64 {\n\
             let h = || cfg.value;\n\
             return h;\n\
         }",
    );
    assert_eq!(errors.len(), 1);
    // The closure `|| cfg.value` is on line 3 of the source above.
    assert_eq!(
        errors[0].span.line, 3,
        "diagnostic span should point at the closure expression's line; got line {}",
        errors[0].span.line
    );
}

// ── Step 7 follow-up sentinels: composite literal escape (round 12.36) ──
//
// Round 12.36 extends `collect_escape_target` to recurse through
// composite literal expressions in the operand of an escaping return:
// struct literals, tuples, array literals, prefix-collection literals
// (`Vec[...]`, `Array[...]`, `Set[...]`, `Map[...]`), repeat literals
// (`[v; n]`), and map literals. A closure with ref captures sitting
// inside any of these as a sub-expression of a return statement (or
// the function-body's tail expression) escapes the function the same
// way a directly-returned closure does — the wrapping literal is
// constructed in the current scope and immediately handed off to the
// caller. Function-call escape (`return run_fn(h)`), field-access
// extraction (`return holder.f`), and let-bound-then-returned shapes
// remain deferred to a further follow-up.

#[test]
fn step7_struct_literal_in_return_fires() {
    // Closure embedded in a struct literal that's directly returned —
    // the struct's lifetime is the caller's scope, so the closure's
    // ref capture would outlive `cfg`.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         struct Holder { f: Fn() -> i64 }\n\
         fn make_holder(cfg: Config) -> Holder {\n\
             return Holder { f: || cfg.value };\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "expected one E0508 error, got {:?}",
        errors
    );
}

#[test]
fn step7_struct_literal_implicit_tail_return_fires() {
    // Same as above but the struct literal is the function body's
    // tail expression (implicit return).
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         struct Holder { f: Fn() -> i64 }\n\
         fn make_holder(cfg: Config) -> Holder {\n\
             Holder { f: || cfg.value }\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "expected one E0508 error, got {:?}",
        errors
    );
}

#[test]
fn step7_tuple_in_return_fires() {
    // Closure in a tuple literal returned from the function. Two
    // closures in the tuple → two errors (one per closure
    // expression).
    let errors = step7_e0508_errors(
        "struct Config { x: i64, y: i64 }\n\
         fn make_pair(cfg: Config) -> (Fn() -> i64, Fn() -> i64) {\n\
             return (|| cfg.x, || cfg.y);\n\
         }",
    );
    assert_eq!(
        errors.len(),
        2,
        "two closures in tuple → two errors; got {:?}",
        errors
    );
}

#[test]
fn step7_vec_literal_in_return_fires() {
    // Closure in a `Vec[...]` prefix-collection literal that's
    // returned. Same shape as struct/tuple.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         fn make_handlers(cfg: Config) -> Vec[Fn() -> i64] {\n\
             return Vec[|| cfg.value];\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "expected one E0508 error, got {:?}",
        errors
    );
}

#[test]
fn step7_struct_literal_with_owned_capture_does_not_fire() {
    // Negative: closure body consumes capture (own mode), so the
    // closure carries `cfg` by value. Storing the closure in a struct
    // and returning is the clean-escape sub-case (i) — no error.
    let parsed = parse(
        "struct Config { value: i64 }\n\
         struct Holder { f: Fn() -> () }\n\
         fn apply(c: Config) { }\n\
         fn make_holder(cfg: Config) -> Holder {\n\
             return Holder { f: || apply(cfg) };\n\
         }",
    );
    let resolved = resolve(&parsed.program);
    let typed = typecheck(&parsed.program, &resolved);
    let result = ownershipcheck(&parsed.program, &typed);
    let e0508s: Vec<_> = result
        .errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::RefCaptureEscapesScope))
        .collect();
    assert!(
        e0508s.is_empty(),
        "owned-capture in struct literal should not fire E0508; got {:?}",
        e0508s,
    );
}

#[test]
fn step7_nested_struct_literals_recurse() {
    // Closure two levels deep: outer struct holds inner struct; inner
    // struct holds the closure. Recursion through nested composite
    // literals should still find the closure.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         struct Inner { f: Fn() -> i64 }\n\
         struct Outer { inner: Inner }\n\
         fn make_outer(cfg: Config) -> Outer {\n\
             return Outer { inner: Inner { f: || cfg.value } };\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "nested struct literal should still surface the closure; got {:?}",
        errors
    );
}

// ── Step 7 follow-up sentinels: let-bound carrier escape (round 12.37) ──
//
// Round 12.37 generalises the round-12.35 closure-let map from
// `HashMap<String, SpanKey>` to `HashMap<String, Vec<SpanKey>>` and
// changes the registration walk to reuse `collect_escape_target` on
// the let-RHS — so a let-binding of a composite-literal-containing
// closures (`let holder = Holder { f: || cfg.x };`) registers the
// binding name against the union of closure spans inside. A
// subsequent `return holder;` resolves the identifier through the map
// and surfaces every embedded closure for the standard E0508 check.
//
// Identifier-to-identifier propagation also works (`let h2 = h;`
// extends `h2`'s span set with `h`'s) because `collect_escape_target`
// already handles the Identifier arm against `closure_lets`.

#[test]
fn step7_let_bound_struct_then_return_fires() {
    // Closure stored in struct via let, then the struct is returned.
    // Pre-12.37 this case missed because `closure_let_bindings` only
    // tracked direct `let h = closure;` forms.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         struct Holder { f: Fn() -> i64 }\n\
         fn make_holder(cfg: Config) -> Holder {\n\
             let holder = Holder { f: || cfg.value };\n\
             return holder;\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "let-bound struct then return should fire; got {:?}",
        errors
    );
}

#[test]
fn step7_let_bound_struct_implicit_tail_return_fires() {
    // Same as above but with implicit tail-expression return.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         struct Holder { f: Fn() -> i64 }\n\
         fn make_holder(cfg: Config) -> Holder {\n\
             let holder = Holder { f: || cfg.value };\n\
             holder\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "let-bound struct then tail return should fire; got {:?}",
        errors
    );
}

#[test]
fn step7_let_bound_tuple_of_closures_then_return_fires() {
    // Tuple of two closures bound to a let, then returned. Both
    // closures should fire — the let-RHS walk registers `pair`
    // against both closure spans, and the return resolves identifier
    // → both spans.
    let errors = step7_e0508_errors(
        "struct Config { x: i64, y: i64 }\n\
         fn make_pair(cfg: Config) -> (Fn() -> i64, Fn() -> i64) {\n\
             let pair = (|| cfg.x, || cfg.y);\n\
             return pair;\n\
         }",
    );
    assert_eq!(
        errors.len(),
        2,
        "two closures in let-bound tuple should each fire; got {:?}",
        errors
    );
}

#[test]
fn step7_identifier_propagation_through_lets_fires() {
    // `let h = || cfg.x; let h2 = h;` should propagate h's closure
    // span to h2; `return h2` then resolves through both layers.
    let errors = step7_e0508_errors(
        "struct Config { x: i64 }\n\
         fn make_handler(cfg: Config) -> Fn() -> i64 {\n\
             let h = || cfg.x;\n\
             let h2 = h;\n\
             return h2;\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "identifier propagation through lets should fire once; got {:?}",
        errors
    );
}

#[test]
fn step7_let_bound_struct_used_locally_does_not_fire() {
    // Negative: closure stored in struct via let, struct invoked
    // locally (not returned). No escape, no error.
    let parsed = parse(
        "struct Config { value: i64 }\n\
         struct Holder { f: Fn() -> i64 }\n\
         fn use_cfg(cfg: Config) -> i64 {\n\
             let holder = Holder { f: || cfg.value };\n\
             (holder.f)()\n\
         }",
    );
    let resolved = resolve(&parsed.program);
    let typed = typecheck(&parsed.program, &resolved);
    let result = ownershipcheck(&parsed.program, &typed);
    let e0508s: Vec<_> = result
        .errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::RefCaptureEscapesScope))
        .collect();
    assert!(
        e0508s.is_empty(),
        "let-bound struct used locally should not fire E0508; got {:?}",
        e0508s,
    );
}

#[test]
fn step7_let_bound_nested_struct_then_return_fires() {
    // Round 12.36 + 12.37 composition: two-level-nested struct
    // bound to a let, then returned. The let-RHS walk recurses
    // through both struct layers via composite-literal recursion.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         struct Inner { f: Fn() -> i64 }\n\
         struct Outer { inner: Inner }\n\
         fn make_outer(cfg: Config) -> Outer {\n\
             let outer = Outer { inner: Inner { f: || cfg.value } };\n\
             return outer;\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "let-bound nested struct then return should fire; got {:?}",
        errors
    );
}

// ── Step 7 follow-up sentinels: fn-arg pass conservative-fire (round 12.39) ──
//
// Round 12.39 closes the last Step 7 escape destination: a closure
// with `ref` / `mut ref` captures passed as a fn-arg to an Own-mode
// parameter slot. The receiving function MAY store the closure
// beyond its call (in a long-lived cell, in a struct field, by
// re-passing it elsewhere) — without inter-procedural analysis we
// cannot prove otherwise, so we conservatively fire E0508 on every
// such pass. Borrow-mode slots (`ref Fn(...)` / `mut ref Fn(...)`)
// are skipped — the callee borrows the closure for the duration of
// the call and structurally cannot store it. Method calls and
// indirect calls (through function-typed locals where we lack
// per-position mode info) are also skipped — those cases need
// their own follow-up.
//
// The function-level `#[allow(ref_capture_escape)]` attribute opts
// out of the conservative fire when the programmer knows the called
// functions are synchronous invoke-and-drop. This is the same shape
// as `#[allow(rc_fallback)]` elsewhere in the ownership pass.

#[test]
fn step7_fn_arg_pass_to_own_fn_slot_with_ref_capture_fires() {
    // Closure body reads `cfg.value` (Copy projection) → `cfg`
    // captured by `ref`. Closure passed to `run_fn(f: Fn() -> ())`
    // (Own-mode slot). The receiving function may or may not invoke
    // the closure synchronously — we conservatively fire.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         fn run_fn(f: Fn() -> i64) -> i64 { f() }\n\
         fn use_cfg(cfg: Config) -> i64 {\n\
             let h = || cfg.value;\n\
             run_fn(h)\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "ref-capture closure passed to Own-mode Fn slot should fire; got {:?}",
        errors
    );
}

#[test]
fn step7_fn_arg_pass_direct_closure_literal_fires() {
    // Same shape as above but with the closure as a direct literal
    // in the call-arg position (no let binding).
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         fn run_fn(f: Fn() -> i64) -> i64 { f() }\n\
         fn use_cfg(cfg: Config) -> i64 {\n\
             run_fn(|| cfg.value)\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "direct closure literal as Own-mode Fn arg should fire; got {:?}",
        errors
    );
}

#[test]
fn step7_fn_arg_pass_with_owned_capture_does_not_fire() {
    // Negative: closure body consumes `cfg` (`apply` takes by value)
    // → capture is `Own`, not `Ref`. Sub-case (i) clean-escape
    // through fn-arg pass — no error.
    let parsed = parse(
        "struct Config { value: i64 }\n\
         fn apply(c: Config) { }\n\
         fn run_fn(f: Fn() -> ()) { f() }\n\
         fn use_cfg(cfg: Config) {\n\
             let h = || apply(cfg);\n\
             run_fn(h);\n\
         }",
    );
    let resolved = resolve(&parsed.program);
    let typed = typecheck(&parsed.program, &resolved);
    let result = ownershipcheck(&parsed.program, &typed);
    let e0508s: Vec<_> = result
        .errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::RefCaptureEscapesScope))
        .collect();
    assert!(
        e0508s.is_empty(),
        "owned-capture passed to Fn slot should not fire E0508; got {:?}",
        e0508s,
    );
}

#[test]
fn step7_fn_arg_pass_to_ref_fn_slot_does_not_fire() {
    // Negative: `ref Fn(...)` slot — the callee borrows the closure
    // for the duration of the call and cannot store it beyond
    // return. Conservative-fire is structurally unnecessary here.
    let parsed = parse(
        "struct Config { value: i64 }\n\
         fn run_fn(f: ref Fn() -> i64) -> i64 { f() }\n\
         fn use_cfg(cfg: Config) -> i64 {\n\
             let h = || cfg.value;\n\
             run_fn(h)\n\
         }",
    );
    let resolved = resolve(&parsed.program);
    let typed = typecheck(&parsed.program, &resolved);
    let result = ownershipcheck(&parsed.program, &typed);
    let e0508s: Vec<_> = result
        .errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::RefCaptureEscapesScope))
        .collect();
    assert!(
        e0508s.is_empty(),
        "ref Fn slot pass should not fire E0508; got {:?}",
        e0508s,
    );
}

#[test]
fn step7_allow_ref_capture_escape_attribute_suppresses_fn_arg_fire() {
    // `#[allow(ref_capture_escape)]` on the enclosing function
    // suppresses the conservative fn-arg fire. Useful for the
    // synchronous invoke-and-drop pattern (`run_fn` only invokes,
    // doesn't store) until callee-side annotations land.
    let parsed = parse(
        "struct Config { value: i64 }\n\
         fn run_fn(f: Fn() -> i64) -> i64 { f() }\n\
         #[allow(ref_capture_escape)]\n\
         fn use_cfg(cfg: Config) -> i64 {\n\
             let h = || cfg.value;\n\
             run_fn(h)\n\
         }",
    );
    let resolved = resolve(&parsed.program);
    let typed = typecheck(&parsed.program, &resolved);
    let result = ownershipcheck(&parsed.program, &typed);
    let e0508s: Vec<_> = result
        .errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::RefCaptureEscapesScope))
        .collect();
    assert!(
        e0508s.is_empty(),
        "#[allow(ref_capture_escape)] should suppress fn-arg fire; got {:?}",
        e0508s,
    );
}

#[test]
fn step7_allow_attribute_does_not_suppress_return_fire() {
    // The opt-out is scoped to fn-arg-pass sub-case only — the
    // unambiguous return-escape sub-cases (round 12.35–12.37) still
    // fire even with the attribute. A closure with ref captures
    // returned from the function is always an iv violation.
    let errors = step7_e0508_errors(
        "struct Config { value: i64 }\n\
         #[allow(ref_capture_escape)]\n\
         fn make_handler(cfg: Config) -> Fn() -> i64 {\n\
             return || cfg.value;\n\
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "allow attribute should not suppress return-escape fire; got {:?}",
        errors
    );
}

// ── Slice borrow source attribution ─────────────────────────────
//
// Phase-5 Theme 1 Slice 1: `OwnershipCheckResult::slice_borrow_sources`
// records every slice creation site keyed by the slice expression's
// `SpanKey`. Each entry is `(PlaceExpr, mutable)` resolved to the
// original storage binding — slice-of-slice creations chain through to
// the root `Vec` / `Array` / `Slice`, never an intermediate slice.

#[test]
fn slice_from_as_slice_records_root_binding() {
    let result = ownership_ok(
        "fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             let _s = v.as_slice();
         }",
    );
    let entries: Vec<_> = result.slice_borrow_sources.values().collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one slice creation, got {:?}",
        entries
    );
    let (place, mutable) = entries[0];
    assert_eq!(place.root, "v");
    assert!(place.projections.is_empty());
    assert!(!mutable, ".as_slice() produces an immutable slice");
}

#[test]
fn slice_of_slice_records_root_not_parent() {
    // `s2 = s1[0..3]` chains through `s1`'s recorded source so `s2`'s
    // attribution names the original storage binding `v`, not `s1`.
    let result = ownership_ok(
        "fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             v.push(2);
             v.push(3);
             let s1 = v.as_slice();
             let _s2 = s1[0..2];
         }",
    );
    // Two slice creation sites: `v.as_slice()` and `s1[0..2]`. Both
    // resolve to root `v`.
    assert_eq!(
        result.slice_borrow_sources.len(),
        2,
        "expected two slice creations, got {:?}",
        result.slice_borrow_sources
    );
    for (place, _) in result.slice_borrow_sources.values() {
        assert_eq!(
            place.root, "v",
            "every entry should resolve to root v, got {:?}",
            place
        );
    }
}

#[test]
fn slice_from_temporary_escapes_rejected() {
    // `make_vec().as_slice()` bound to a let — the receiver is a
    // function-call temporary with no rooted attribution; the slice's
    // storage drops at end-of-statement.
    let errors = ownership_errors(
        "fn make_vec() -> Vec[i64] { Vec.new() }
         fn main() {
             let _s = make_vec().as_slice();
         }",
    );
    let escapes: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::SliceFromTemporaryEscapes))
        .collect();
    assert_eq!(
        escapes.len(),
        1,
        "expected one SliceFromTemporaryEscapes error, got {:?}",
        errors
    );
}

#[test]
fn slice_from_temporary_in_statement_accepted() {
    // `make_vec().as_slice().len()` — slice is a temp consumed
    // in-statement. No escape, should accept.
    ownership_ok(
        "fn make_vec() -> Vec[i64] { Vec.new() }
         fn main() {
             let _n = make_vec().as_slice().len();
         }",
    );
}

#[test]
fn slice_from_call_arg_coercion_records_root() {
    // Implicit `Vec[T]` → `mut Slice[T]` at call-arg coercion records
    // `(root: 'v', mutable: true)` keyed by the arg's span.
    let result = ownership_ok(
        "fn clear(xs: mut Slice[i64]) {}
         fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             clear(mut v);
         }",
    );
    let entries: Vec<_> = result.slice_borrow_sources.values().collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one slice creation, got {:?}",
        entries
    );
    let (place, mutable) = entries[0];
    assert_eq!(place.root, "v");
    assert!(place.projections.is_empty());
    assert!(*mutable, "mut Slice[T] formal records mutable=true");
}

// ── Slice borrow conflict detection ─────────────────────────────
//
// Phase-5 Theme 1 Slice 2: the conflict matrix scans `active_borrows`
// at every push and emits `SliceBorrowConflict { shape: ... }` for the
// four conflict shapes (A imm+mut, B mut+mut, C move-of-borrowed, D
// drop-of-borrowed) and `CrossBorrowConflict` for slice + ref of the
// same root. Borrows are scoped — drained at block-exit and at call
// return.

fn slice_conflict_errors(source: &str) -> Vec<OwnershipError> {
    let parsed = parse(source);
    assert!(parsed.errors.is_empty(), "Parse: {:?}", parsed.errors);
    let resolved = resolve(&parsed.program);
    assert!(resolved.errors.is_empty(), "Resolve: {:?}", resolved.errors);
    let typed = typecheck(&parsed.program, &resolved);
    let result = ownershipcheck(&parsed.program, &typed);
    result
        .errors
        .into_iter()
        .filter(|e| {
            matches!(
                e.kind,
                OwnershipErrorKind::SliceBorrowConflict { .. }
                    | OwnershipErrorKind::CrossBorrowConflict
            )
        })
        .collect()
}

#[test]
fn mut_slice_plus_imm_slice_same_source_rejected_shape_a() {
    let errors = slice_conflict_errors(
        "fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             let _s_mut = v.as_slice_mut();
             let _s_imm = v.as_slice();
         }",
    );
    let shape_a: Vec<_> = errors
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                OwnershipErrorKind::SliceBorrowConflict {
                    shape: SliceConflictShape::ImmSliceVsMutSlice
                }
            )
        })
        .collect();
    assert_eq!(
        shape_a.len(),
        1,
        "expected one shape A error, got {:?}",
        errors
    );
}

#[test]
fn two_imm_slices_same_source_accepted() {
    // Two read-only `Slice[T]` peers of the same source coexist —
    // shape A only fires for ImmSlice + MutSlice pairs.
    ownership_ok(
        "fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             let _s1 = v.as_slice();
             let _s2 = v.as_slice();
         }",
    );
}

#[test]
fn two_mut_slices_same_source_rejected_shape_b() {
    let errors = slice_conflict_errors(
        "fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             let _s1 = v.as_slice_mut();
             let _s2 = v.as_slice_mut();
         }",
    );
    let shape_b: Vec<_> = errors
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                OwnershipErrorKind::SliceBorrowConflict {
                    shape: SliceConflictShape::MutSliceVsMutSlice
                }
            )
        })
        .collect();
    assert_eq!(
        shape_b.len(),
        1,
        "expected one shape B error, got {:?}",
        errors
    );
}

#[test]
fn slice_then_move_source_rejected_shape_c() {
    // Slice into `v` is live, then `v` is consumed (moved into another
    // owned binding) — shape C: cannot move source while slice borrow
    // is live.
    let errors = slice_conflict_errors(
        "fn take(v: Vec[i64]) {}
         fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             let _s = v.as_slice();
             take(v);
         }",
    );
    let shape_c: Vec<_> = errors
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                OwnershipErrorKind::SliceBorrowConflict {
                    shape: SliceConflictShape::MoveOfBorrowed
                }
            )
        })
        .collect();
    assert_eq!(
        shape_c.len(),
        1,
        "expected one shape C error, got {:?}",
        errors
    );
}

#[test]
fn transitive_slice_of_slice_conflicts_via_root() {
    // `let s2 = s1[0..3];` chains through Slice 1's binding-source
    // map so the recorded root is `v`, not `s1`. The conflict matrix
    // scans `active_borrows[v]` and finds `s1`'s prior `mut` push,
    // firing shape A.
    let errors = slice_conflict_errors(
        "fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             v.push(2);
             v.push(3);
             let _s1 = v.as_slice_mut();
             let _s2 = v[0..2];
         }",
    );
    let shape_a: Vec<_> = errors
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                OwnershipErrorKind::SliceBorrowConflict {
                    shape: SliceConflictShape::ImmSliceVsMutSlice
                }
            )
        })
        .collect();
    assert_eq!(
        shape_a.len(),
        1,
        "expected one shape A error from transitive chain, got {:?}",
        errors
    );
}

#[test]
fn slice_borrow_ends_at_scope_exit_no_conflict() {
    // The first slice borrow lives only inside an inner block; once
    // the block exits, it drains. The second creation outside the
    // inner block sees no live borrow and accepts.
    ownership_ok(
        "fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             {
                 let _s1 = v.as_slice_mut();
             }
             let _s2 = v.as_slice_mut();
         }",
    );
}

#[test]
fn mut_slice_then_mutate_source_via_method_rejected_cross_borrow() {
    // A `mut Slice` into a struct's field is live, then an instance
    // method on the struct (`mut ref self`) is called. The receiver-
    // side ref push at MethodCall fires CrossBorrowConflict against
    // the live slice borrow because both target the same root binding.
    // Slice plan sub-step (g): cross-form (slice + ref) routes through
    // `CrossBorrowConflict`, distinct from slice-vs-slice's
    // `SliceBorrowConflict`.
    let errors = slice_conflict_errors(
        "struct Holder { val: Vec[i64] }
         impl Holder {
             fn check(mut ref self) {}
         }
         fn main() {
             let mut h = Holder { val: Vec.new() };
             let _s = h.val.as_slice_mut();
             h.check();
         }",
    );
    let cross: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::CrossBorrowConflict))
        .collect();
    assert_eq!(
        cross.len(),
        1,
        "expected one CrossBorrowConflict, got {:?}",
        errors
    );
}

#[test]
fn imm_slice_then_mut_ref_call_arg_rejected_cross_borrow() {
    // An immutable slice into `v` is live, then `take_mut` is called
    // with `mut v`. The `mut ref Vec[T]` formal pushes a transient
    // `MutRef` borrow at the call boundary; the conflict matrix sees
    // `ImmSlice` + `MutRef` against the same root and emits
    // `CrossBorrowConflict`. Slice 2 follow-up sub-step (c) — symmetric
    // to the receiver-side push for instance methods, but driven from
    // the `Call` arm via the formal's declared param mode.
    let errors = slice_conflict_errors(
        "fn take_mut(v: mut ref Vec[i64]) {}
         fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             let _s = v.as_slice();
             take_mut(mut v);
         }",
    );
    let cross: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::CrossBorrowConflict))
        .collect();
    assert_eq!(
        cross.len(),
        1,
        "expected one CrossBorrowConflict, got {:?}",
        errors
    );
}

#[test]
fn imm_slice_then_stdlib_mut_method_rejected_cross_borrow() {
    // A `Slice[T]` into `v` is live, then `v.push(99)` is called.
    // `Vec.push` has no user-side `impl Vec` block but `method_self_modes`
    // lookup falls through to the stdlib receiver-mode table, which
    // returns `BorrowKind::MutRef`. The receiver-side push at MethodCall
    // emits `CrossBorrowConflict` against the live ImmSlice. Slice 2
    // polish (b) — symmetric to the receiver-side push for user-defined
    // instance methods, but driven by the stdlib table when user
    // metadata is absent.
    let errors = slice_conflict_errors(
        "fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             let _s = v.as_slice();
             v.push(99);
         }",
    );
    let cross: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.kind, OwnershipErrorKind::CrossBorrowConflict))
        .collect();
    assert_eq!(
        cross.len(),
        1,
        "expected one CrossBorrowConflict via stdlib method table, got {:?}",
        errors
    );
}

#[test]
fn slice_outlives_source_drop_rejected_shape_d() {
    // Canonical drop-of-borrowed: a `LetUninit Slice[T]` outer binding
    // captures a slice taken inside an inner block. When the inner
    // block exits, the source `v` drops while `s_outer` still holds a
    // slice into freed storage. Shape D fires at the block-exit drain.
    //
    // This is the positive form of the v1 polish (D5) — earlier scoped
    // out because the LetUninit + Slice-typed assignment surface was
    // assumed unavailable; verified at probe time that
    // `let mut s: Slice[i64];` followed by `s = v.as_slice();`
    // typechecks cleanly. The Assign arm now propagates
    // `slice_binding_scope_depth` from the LHS's recorded scope
    // (captured at the LetUninit binding) so the drain matches.
    let errors = slice_conflict_errors(
        "fn main() {
             let mut s_outer: Slice[i64];
             {
                 let v: Vec[i64] = Vec.new();
                 s_outer = v.as_slice();
             }
         }",
    );
    let shape_d: Vec<_> = errors
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                OwnershipErrorKind::SliceBorrowConflict {
                    shape: SliceConflictShape::DropOfBorrowed
                }
            )
        })
        .collect();
    assert_eq!(
        shape_d.len(),
        1,
        "expected one shape D drop-of-borrowed error, got {:?}",
        errors
    );
}

#[test]
fn slice_well_scoped_no_shape_d_false_positive() {
    // Negative complement: when the slice's binding scope is at or
    // deeper than the source binding's, no shape D fires. Pins the
    // drain doesn't false-positive on well-scoped programs.
    ownership_ok(
        "fn main() {
             let mut v_outer: Vec[i64] = Vec.new();
             v_outer.push(1);
             let _s = v_outer.as_slice();
             {
                 let mut v_inner: Vec[i64] = Vec.new();
                 v_inner.push(99);
                 let _inner_s = v_inner.as_slice();
             }
         }",
    );
}

// ── Match Ergonomics: ref-scrutinee consume gate ─────────────────────
//
// design.md § Match Arm Binding Modes — when the scrutinee is
// `ref T` / `mut ref T`, arm bindings borrow rather than move, so
// the scrutinee itself is read (never consumed) regardless of what
// the arms bind. The owned-scrutinee path continues to consume per
// the prior rule. See `OwnershipChecker::is_borrow_typed_scrutinee`
// and `UseClassifier::is_borrow_typed_expr`.

#[test]
fn match_ref_struct_scrutinee_not_consumed_by_binding_arm() {
    // Binds the struct field `name` from a `ref Foo` scrutinee — under
    // match ergonomics, the scrutinee `val` stays Live so the post-match
    // `use_val(val)` does not trip `UseOfMoved`.
    ownership_ok(
        "struct Foo { name: String }
         fn use_str(s: ref String) -> i64 { 0 }
         fn use_val(v: ref Foo) -> i64 { 0 }
         fn g(val: ref Foo) -> i64 {
             let _ = match val { Foo { name } => use_str(name) };
             use_val(val)
         }
         fn main() { }",
    );
}

#[test]
fn match_owned_struct_scrutinee_consumed_by_binding_arm() {
    // Owned scrutinee + a binding arm still consumes the scrutinee.
    // Reusing `val` after the match flags as `UseOfMoved`.
    let errs = ownership_errors(
        "struct Foo { name: String }
         fn use_str(s: String) -> i64 { 0 }
         fn use_val(v: Foo) -> i64 { 0 }
         fn g(val: Foo) -> i64 {
             let _ = match val { Foo { name } => use_str(name) };
             use_val(val)
         }
         fn main() { }",
    );
    assert!(
        errs.iter().any(|e| format!("{}", e).contains("val")),
        "expected use-of-moved on val after owned-match consume, got: {:?}",
        errs.iter().map(|e| format!("{}", e)).collect::<Vec<_>>()
    );
}

#[test]
fn match_ref_option_payload_does_not_consume_scrutinee() {
    // Enum-variant payload binding under a `ref Option[String]`
    // scrutinee — the scrutinee remains usable after the match.
    ownership_ok(
        "fn use_str(s: ref String) -> i64 { 0 }
         fn use_val(v: ref Option[String]) -> i64 { 0 }
         fn g(val: ref Option[String]) -> i64 {
             let _ = match val {
                 Option.Some(s) => use_str(s),
                 Option.None => 0,
             };
             use_val(val)
         }
         fn main() { }",
    );
}

#[test]
fn match_mut_ref_scrutinee_not_consumed_by_binding_arm() {
    // Same exception applies to `mut ref T` scrutinees. Under
    // `mut ref Foo`, the field binding `name` is wrapped as
    // `mut ref String`; mirroring functions take the same form.
    ownership_ok(
        "struct Foo { name: String }
         fn use_mut(s: mut ref String) -> i64 { 0 }
         fn use_val(v: mut ref Foo) -> i64 { 0 }
         fn g(val: mut ref Foo) -> i64 {
             let _ = match val { Foo { name } => use_mut(name) };
             use_val(val)
         }
         fn main() { }",
    );
}
