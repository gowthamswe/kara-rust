// tests/typechecker.rs

use karac::typechecker::*;
use karac::{parse, resolve, typecheck};

// ── Test Helpers ────────────────────────────────────────────────

fn typecheck_ok(source: &str) -> TypeCheckResult {
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
    let result = typecheck(&parsed.program, &resolved);
    assert!(
        result.errors.is_empty(),
        "Type errors: {}",
        result
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    result
}

fn typecheck_errors(source: &str) -> Vec<TypeError> {
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
    let result = typecheck(&parsed.program, &resolved);
    assert!(
        !result.errors.is_empty(),
        "Expected type errors but got none"
    );
    result.errors
}

// ── Category 1: Literals and Basic Types ────────────────────────

#[test]
fn test_empty_program() {
    typecheck_ok("");
}

#[test]
fn test_integer_literal_type() {
    typecheck_ok("fn main() { let x: i64 = 42; }");
}

#[test]
fn test_bool_literal_type() {
    typecheck_ok("fn main() { let b: bool = true; }");
}

#[test]
fn test_string_literal_type() {
    typecheck_ok("fn main() { let s: String = \"hello\"; }");
}

#[test]
fn test_float_literal_type() {
    typecheck_ok("fn main() { let f: f64 = 3.14; }");
}

#[test]
fn test_int_to_float_widening() {
    typecheck_ok("fn f(x: i32) -> f64 { x }");
}

#[test]
fn test_uint_to_float_widening() {
    typecheck_ok("fn f(x: u32) -> f64 { x }");
}

#[test]
fn test_type_mismatch_let() {
    let errors = typecheck_errors("fn main() { let x: bool = 42; }");
    assert!(errors[0].kind == TypeErrorKind::TypeMismatch);
    assert!(errors[0].message.contains("bool"));
    assert!(errors[0].message.contains("i64"));
}

#[test]
fn test_integer_suffix_i32_ok() {
    typecheck_ok("fn main() { let x: i32 = 42i32; }");
}

#[test]
fn test_integer_suffix_u8_ok() {
    typecheck_ok("fn main() { let x: u8 = 255u8; }");
}

#[test]
fn test_float_suffix_f32_ok() {
    typecheck_ok("fn main() { let x: f32 = 1.5f32; }");
}

#[test]
fn test_integer_suffix_i128_rejected() {
    let errors = typecheck_errors("fn main() { let x = 1i128; }");
    assert!(errors[0].kind == TypeErrorKind::UnsupportedNumericSuffix);
    assert!(errors[0].message.contains("128"));
}

#[test]
fn test_integer_suffix_u128_rejected() {
    let errors = typecheck_errors("fn main() { let x = 1u128; }");
    assert!(errors[0].kind == TypeErrorKind::UnsupportedNumericSuffix);
}

#[test]
fn test_type_alias_resolves_to_primitive() {
    typecheck_ok(
        "
        type UserId = i64;
        fn f(x: UserId) -> UserId { x + 1 }
        fn main() { let _ = f(42); }
        ",
    );
}

#[test]
fn test_type_alias_transitive_forward_order() {
    // UserId declared before AdminId — already worked before CR-20.
    typecheck_ok(
        "
        type UserId = i64;
        type AdminId = UserId;
        fn f(x: AdminId) -> AdminId { x + 1 }
        fn main() { let _ = f(42); }
        ",
    );
}

#[test]
fn test_type_alias_transitive_backward_order() {
    // CR-20 fix: AdminId referenced before UserId is defined. Forward-ref
    // through the alias chain must still land on i64.
    typecheck_ok(
        "
        type AdminId = UserId;
        type UserId = i64;
        fn f(x: AdminId) -> AdminId { x + 1 }
        fn main() { let _ = f(42); }
        ",
    );
}

// ── Category 2: Function Signatures ─────────────────────────────

#[test]
fn test_function_return_type() {
    typecheck_ok("fn get_five() -> i64 { 5 }");
}

#[test]
fn test_function_wrong_return() {
    let errors = typecheck_errors("fn get_five() -> i64 { true }");
    assert!(errors[0].kind == TypeErrorKind::TypeMismatch);
}

#[test]
fn test_function_no_return_implicit_unit() {
    typecheck_ok("fn do_nothing() { let x = 1; }");
}

#[test]
fn test_function_params_available() {
    typecheck_ok("fn add(a: i64, b: i64) -> i64 { a + b }");
}

#[test]
fn test_function_wrong_arg_type() {
    let errors = typecheck_errors(
        "fn add(a: i64, b: i64) -> i64 { a + b }\n\
         fn main() { add(1, true); }",
    );
    assert!(errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch));
}

// ── Category 3: Operators ───────────────────────────────────────

#[test]
fn test_arithmetic_ops() {
    typecheck_ok("fn main() { let x: i64 = 1 + 2 * 3 - 4 / 2; }");
}

#[test]
fn test_comparison_ops() {
    typecheck_ok("fn main() { let b: bool = 1 < 2; }");
}

#[test]
fn test_logical_ops() {
    typecheck_ok("fn main() { let b: bool = true and false or true; }");
}

#[test]
fn test_arithmetic_type_mismatch() {
    let errors = typecheck_errors("fn main() { let x = 1 + true; }");
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TypeMismatch
                || e.kind == TypeErrorKind::InvalidBinaryOp)
    );
}

#[test]
fn test_logical_non_bool() {
    let errors = typecheck_errors("fn main() { let x = 1 and 2; }");
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::InvalidBinaryOp));
}

// ── Category 4: Control Flow ────────────────────────────────────

#[test]
fn test_if_else_same_type() {
    typecheck_ok("fn max(a: i64, b: i64) -> i64 { if a > b { a } else { b } }");
}

#[test]
fn test_if_else_type_mismatch() {
    let errors = typecheck_errors(
        "fn main() {\n\
             let x = if true { 1 } else { true };\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::BranchTypeMismatch));
}

#[test]
fn test_if_condition_must_be_bool() {
    let errors = typecheck_errors("fn main() { if 42 { } }");
    assert!(errors[0].kind == TypeErrorKind::ConditionNotBool);
}

#[test]
fn test_while_condition_bool() {
    let errors = typecheck_errors("fn main() { while 42 { } }");
    assert!(errors[0].kind == TypeErrorKind::ConditionNotBool);
}

#[test]
fn test_block_returns_final_expr() {
    typecheck_ok(
        "fn main() {\n\
             let x: i64 = {\n\
                 let a = 1;\n\
                 let b = 2;\n\
                 a + b\n\
             };\n\
         }",
    );
}

// ── Category 5: Function Calls ──────────────────────────────────

#[test]
fn test_function_call_correct() {
    typecheck_ok(
        "fn add(a: i64, b: i64) -> i64 { a + b }\n\
         fn main() { let result: i64 = add(1, 2); }",
    );
}

#[test]
fn test_function_call_wrong_count() {
    let errors = typecheck_errors(
        "fn add(a: i64, b: i64) -> i64 { a + b }\n\
         fn main() { add(1, 2, 3); }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs));
}

#[test]
fn test_function_call_wrong_type() {
    let errors = typecheck_errors(
        "fn negate(x: bool) -> bool { not x }\n\
         fn main() { negate(42); }",
    );
    assert!(errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch));
}

#[test]
fn test_recursive_function() {
    typecheck_ok(
        "fn countdown(n: i64) -> i64 {\n\
             if n < 1 { 0 } else { countdown(n - 1) }\n\
         }",
    );
}

// ── Category 6: Struct Operations ───────────────────────────────

#[test]
fn test_struct_literal_correct() {
    typecheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn main() { let p = Point { x: 1, y: 2 }; }",
    );
}

#[test]
fn test_struct_missing_field() {
    let errors = typecheck_errors(
        "struct Point { x: i64, y: i64 }\n\
         fn main() { let p = Point { x: 1 }; }",
    );
    assert!(errors.iter().any(|e| e.kind == TypeErrorKind::MissingField));
}

#[test]
fn test_struct_extra_field() {
    let errors = typecheck_errors(
        "struct Point { x: i64, y: i64 }\n\
         fn main() { let p = Point { x: 1, y: 2, z: 3 }; }",
    );
    assert!(errors.iter().any(|e| e.kind == TypeErrorKind::ExtraField));
}

#[test]
fn test_struct_field_type_mismatch() {
    let errors = typecheck_errors(
        "struct Point { x: i64, y: i64 }\n\
         fn main() { let p = Point { x: true, y: 2 }; }",
    );
    assert!(errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch));
}

#[test]
fn test_struct_field_access() {
    typecheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn main() {\n\
             let p = Point { x: 1, y: 2 };\n\
             let x: i64 = p.x;\n\
         }",
    );
}

#[test]
fn test_struct_unknown_field_access() {
    let errors = typecheck_errors(
        "struct Point { x: i64, y: i64 }\n\
         fn main() {\n\
             let p = Point { x: 1, y: 2 };\n\
             let z = p.z;\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::UndefinedField));
    assert!(errors[0].message.contains("z"));
}

// ── Category 7: Enum Operations ─────────────────────────────────

#[test]
fn test_enum_unit_variant() {
    typecheck_ok(
        "enum Color { Red, Green, Blue }\n\
         fn main() { let c = Red; }",
    );
}

#[test]
fn test_enum_tuple_variant() {
    typecheck_ok(
        "enum Option { Some(i64), None }\n\
         fn main() { let x = Some(42); }",
    );
}

#[test]
fn test_enum_variant_wrong_args() {
    let errors = typecheck_errors(
        "enum Option { Some(i64), None }\n\
         fn main() { let x = Some(1, 2); }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs));
}

#[test]
fn test_match_binds_correctly() {
    typecheck_ok(
        "enum Option { Some(i64), None }\n\
         fn unwrap(o: Option) -> i64 {\n\
             match o {\n\
                 Some(v) => v,\n\
                 None => 0,\n\
             }\n\
         }",
    );
}

// Generic enum variant constructors — prelude Option/Result thread type
// parameters through their constructor signatures, and call-site inference
// solves the substitution from argument types (CR-32).

#[test]
fn test_generic_enum_constructor_tuple_variant() {
    // `Some(5)` infers as `Option[i64]` — the prelude `Option[T]`
    // constructor's `T` is solved from the argument.
    typecheck_ok("fn main() { let x = Some(5); }");
}

#[test]
fn test_generic_enum_constructor_with_ascription() {
    typecheck_ok("fn main() { let x: Option[i64] = Some(5); }");
}

#[test]
fn test_generic_enum_unit_variant_with_ascription() {
    // Unit variant (`None`) has no argument to solve `T` from; falls through
    // to the unresolved-TypeParam permissive rule at `check_assignable`.
    typecheck_ok("fn main() { let x: Option[i64] = None; }");
}

#[test]
fn test_generic_enum_constructor_multi_param() {
    // `Result[T, E]` — `T` is solved from the `Ok` argument; `E` stays
    // unresolved (permissive against declared `String`).
    typecheck_ok("fn main() { let r: Result[i64, String] = Ok(5); }");
}

#[test]
fn test_generic_enum_constructor_qualified_path() {
    typecheck_ok("fn main() { let x: Option[i64] = Option.Some(5); }");
}

#[test]
fn test_generic_enum_constructor_nested() {
    // `Some(Some(5))` — inner solves `Option[i64]`, outer solves
    // `Option[Option[i64]]`.
    typecheck_ok("fn main() { let x = Some(Some(5)); }");
}

#[test]
fn test_generic_enum_constructor_type_mismatch() {
    // `Some("hello")` solves `T = String`; the binding annotated
    // `Option[i64]` then rejects `Option[String]` via the recursive
    // Named-args check in `types_compatible`.
    let errors = typecheck_errors("fn main() { let x: Option[i64] = Some(\"hello\"); }");
    assert!(
        errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch, got {:?}",
        errors
    );
}

// ── Map.entry / Entry[K, V] prelude enum (canonical: phase-8-stdlib-floor.md
//    "Map.entry(k) + Entry[K, V] enum") ────────────────────────────────────

#[test]
fn test_entry_kv_registered_as_prelude_type() {
    // Ascribing `Entry[i64, String]` is sufficient to verify the enum is
    // registered with two type params in the typechecker prelude. No
    // construction yet — `Entry` values only come out of `Map.entry(k)`.
    typecheck_ok(
        "fn use_entry(_e: Entry[i64, String]) {}\n\
         fn main() {}",
    );
}

#[test]
fn test_entry_kv_pattern_match_occupied() {
    // The `Occupied { value: mut ref V }` variant carries `mut ref V` —
    // pattern-matching binds the ref. Just typecheck — interpreter / codegen
    // dispatch lands in later subtasks.
    typecheck_ok(
        "fn classify(e: Entry[i64, String]) -> bool {\n\
             match e {\n\
                 Occupied { value: _ } => true,\n\
                 Vacant { key: _, map: _ } => false,\n\
             }\n\
         }\n\
         fn main() {}",
    );
}

#[test]
fn test_map_entry_returns_entry_kv() {
    // `m.entry(k)` returns `Entry[K, V]`. Type ascription confirms the
    // return-type plumbing.
    typecheck_ok(
        "fn main() {\n\
             let m: Map[i64, String] = Map.new();\n\
             let e: Entry[i64, String] = m.entry(1);\n\
         }",
    );
}

#[test]
fn test_map_entry_wrong_key_type_rejected() {
    // `entry(k: K)` checks the key against K — string into a Map[i64, ...]
    // is a TypeMismatch at the argument site.
    let errors = typecheck_errors(
        "fn main() {\n\
             let m: Map[i64, String] = Map.new();\n\
             let _e = m.entry(\"not an i64\");\n\
         }",
    );
    assert!(
        errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch for string-into-i64-key Map.entry, got {:?}",
        errors
    );
}

#[test]
fn test_entry_or_insert_returns_mut_ref_v() {
    // `or_insert(default: V) -> mut ref V`. Argument must match V.
    typecheck_ok(
        "fn main() {\n\
             let m: Map[i64, String] = Map.new();\n\
             let _slot: mut ref String = m.entry(1).or_insert(\"default\");\n\
         }",
    );
}

#[test]
fn test_entry_or_insert_wrong_default_type_rejected() {
    let errors = typecheck_errors(
        "fn main() {\n\
             let m: Map[i64, String] = Map.new();\n\
             let _ = m.entry(1).or_insert(42);\n\
         }",
    );
    assert!(
        errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch when or_insert default doesn't match V, got {:?}",
        errors
    );
}

#[test]
fn test_entry_or_insert_with_closure_returning_v() {
    // `or_insert_with(f: Fn() -> V) -> mut ref V`. Closure-pushdown solves
    // the closure's return type from the expected V.
    typecheck_ok(
        "fn main() {\n\
             let m: Map[i64, Vec[i64]] = Map.new();\n\
             let _slot: mut ref Vec[i64] = m.entry(1).or_insert_with(|| Vec.new());\n\
         }",
    );
}

#[test]
fn test_entry_and_modify_returns_entry_for_chaining() {
    // `and_modify(f: Fn(mut ref V)) -> Entry[K, V]`. The bare and_modify
    // (without further chaining) returns Entry[K, V] so subsequent
    // methods can chain on it.
    typecheck_ok(
        "fn main() {\n\
             let m: Map[i64, i64] = Map.new();\n\
             let _e: Entry[i64, i64] = m.entry(1).and_modify(|_v| {});\n\
         }",
    );
}

#[test]
fn test_entry_and_modify_chain_with_or_insert() {
    // The canonical pattern: and_modify chained into or_insert returns
    // `mut ref V` from the final or_insert.
    typecheck_ok(
        "fn main() {\n\
             let m: Map[i64, i64] = Map.new();\n\
             let _slot: mut ref i64 = m.entry(1).and_modify(|_v| {}).or_insert(0);\n\
         }",
    );
}

#[test]
fn test_entry_or_insert_arity_error() {
    let errors = typecheck_errors(
        "fn main() {\n\
             let m: Map[i64, i64] = Map.new();\n\
             let _ = m.entry(1).or_insert();\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs),
        "expected WrongNumberOfArgs on bare or_insert(), got {:?}",
        errors
    );
}

#[test]
fn test_map_entry_arity_error() {
    let errors = typecheck_errors(
        "fn main() {\n\
             let m: Map[i64, i64] = Map.new();\n\
             let _ = m.entry();\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs),
        "expected WrongNumberOfArgs on bare m.entry(), got {:?}",
        errors
    );
}

// ── Clone trait surface (canonical: phase-8-stdlib-floor.md
//    "Clone trait surface for collections") ─────────────────────────

#[test]
fn test_vec_clone_returns_self_type() {
    typecheck_ok(
        "fn main() {\n\
             let v: Vec[i64] = Vec.new();\n\
             let w: Vec[i64] = v.clone();\n\
         }",
    );
}

#[test]
fn test_string_clone_returns_string() {
    typecheck_ok(
        "fn main() {\n\
             let s: String = \"hello\";\n\
             let t: String = s.clone();\n\
         }",
    );
}

#[test]
fn test_map_clone_returns_self_type() {
    typecheck_ok(
        "fn main() {\n\
             let m: Map[i64, String] = Map.new();\n\
             let n: Map[i64, String] = m.clone();\n\
         }",
    );
}

#[test]
fn test_set_clone_returns_self_type() {
    typecheck_ok(
        "fn main() {\n\
             let s: Set[i64] = Set.new();\n\
             let t: Set[i64] = s.clone();\n\
         }",
    );
}

#[test]
fn test_sorted_set_clone_returns_self_type() {
    typecheck_ok(
        "fn main() {\n\
             let s: SortedSet[i64] = SortedSet.new();\n\
             let t: SortedSet[i64] = s.clone();\n\
         }",
    );
}

#[test]
fn test_clone_arity_error() {
    // `clone()` takes no arguments — passing one is rejected.
    let errors = typecheck_errors(
        "fn main() {\n\
             let v: Vec[i64] = Vec.new();\n\
             let _ = v.clone(42);\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs),
        "expected WrongNumberOfArgs on v.clone(42), got {:?}",
        errors
    );
}

#[test]
fn test_vec_clone_through_borrow_returns_owned() {
    // `clone()` on a `ref Vec[T]` borrow returns the owned `Vec[T]`.
    typecheck_ok(
        "fn dup(v: ref Vec[i64]) -> Vec[i64] { v.clone() }\n\
         fn main() {}",
    );
}

// ── Category 8: Pattern Exhaustiveness ──────────────────────────

#[test]
fn test_exhaustive_match_passes() {
    typecheck_ok(
        "enum Color { Red, Green, Blue }\n\
         fn name(c: Color) -> i64 {\n\
             match c {\n\
                 Red => 1,\n\
                 Green => 2,\n\
                 Blue => 3,\n\
             }\n\
         }",
    );
}

#[test]
fn test_non_exhaustive_match() {
    let errors = typecheck_errors(
        "enum Color { Red, Green, Blue }\n\
         fn name(c: Color) -> i64 {\n\
             match c {\n\
                 Red => 1,\n\
             }\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::NonExhaustiveMatch));
    // Should mention the missing variants
    let exhaust_err = errors
        .iter()
        .find(|e| e.kind == TypeErrorKind::NonExhaustiveMatch)
        .unwrap();
    assert!(exhaust_err.message.contains("Blue") || exhaust_err.message.contains("Green"));
}

#[test]
fn test_wildcard_makes_exhaustive() {
    typecheck_ok(
        "enum Color { Red, Green, Blue }\n\
         fn name(c: Color) -> i64 {\n\
             match c {\n\
                 Red => 1,\n\
                 _ => 0,\n\
             }\n\
         }",
    );
}

#[test]
fn test_binding_makes_exhaustive() {
    typecheck_ok(
        "enum Color { Red, Green, Blue }\n\
         fn name(c: Color) -> i64 {\n\
             match c {\n\
                 Red => 1,\n\
                 other => 0,\n\
             }\n\
         }",
    );
}

// ── Maranget field-level recursion (exhaustiveness slice 2) ──────

#[test]
fn test_exhaustive_some_literal_payloads_non_exhaustive() {
    // Pre-Maranget: Some(0) and Some(1) counted as covering all `Some`.
    // Under slice 2's field-level recursion they're distinct rows, so the
    // match is correctly flagged non-exhaustive on Option[i64] (no
    // `Some(_)` arm).
    let errors = typecheck_errors(
        "fn f(opt: Option[i64]) -> i64 {\n\
             match opt {\n\
                 Some(0) => 0,\n\
                 Some(1) => 1,\n\
                 None    => -1,\n\
             }\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::NonExhaustiveMatch),
        "expected NonExhaustiveMatch — Some(0)/Some(1) shouldn't cover all Some, got: {errors:?}"
    );
}

#[test]
fn test_exhaustive_some_binding_payload_is_exhaustive() {
    // `Some(x)` binds the payload as a wildcard; combined with `None`,
    // that's full coverage of Option.
    typecheck_ok(
        "fn f(opt: Option[i64]) -> i64 {\n\
             match opt {\n\
                 Some(x) => x,\n\
                 None    => 0,\n\
             }\n\
         }",
    );
}

#[test]
fn test_exhaustive_some_wildcard_payload_is_exhaustive() {
    // Same as above but with explicit `_` instead of a binding.
    typecheck_ok(
        "fn f(opt: Option[i64]) -> i64 {\n\
             match opt {\n\
                 Some(_) => 1,\n\
                 None    => 0,\n\
             }\n\
         }",
    );
}

// ── Maranget irrefutability (exhaustiveness slice 6) ────────────

#[test]
fn test_irrefutable_let_struct_destructure_passes() {
    // Plain struct destructure with all-binding fields — Maranget reports
    // irrefutable, agreeing with the legacy syntactic check. Ensures the
    // migration didn't regress this baseline.
    typecheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn main() {\n\
             let p = Point { x: 1, y: 2 };\n\
             let Point { x, y } = p;\n\
             let _ = x;\n\
             let _ = y;\n\
         }",
    );
}

#[test]
fn test_irrefutable_let_refutable_enum_variant_rejected_via_maranget() {
    // The `let Some(x) = opt;` case routes through Maranget — Option is a
    // handled scrutinee type — and the witness `None` proves refutability.
    let errors = typecheck_errors(
        "fn main() { let opt: Option[i32] = Option.None; let Option.Some(x) = opt; }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::RefutablePattern),
        "expected RefutablePattern error via Maranget irrefutability check, got: {errors:?}"
    );
}

// ── Maranget reachability (exhaustiveness slice 5) ──────────────

#[test]
fn test_reachability_duplicate_unguarded_arm_unreachable() {
    let result = typecheck_ok(
        "enum Color { Red, Green, Blue }\n\
         fn name(c: Color) -> i64 {\n\
             match c {\n\
                 Red   => 1,\n\
                 Red   => 2,\n\
                 Green => 3,\n\
                 Blue  => 4,\n\
             }\n\
         }",
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.kind == TypeErrorKind::UnreachableArm),
        "expected an UnreachableArm warning for the duplicate Red arm, got warnings: {:?}",
        result.warnings
    );
}

#[test]
fn test_reachability_arm_after_wildcard_unreachable() {
    let result = typecheck_ok(
        "enum Color { Red, Green, Blue }\n\
         fn name(c: Color) -> i64 {\n\
             match c {\n\
                 _     => 0,\n\
                 Red   => 1,\n\
             }\n\
         }",
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.kind == TypeErrorKind::UnreachableArm),
        "expected an UnreachableArm warning for Red after _, got warnings: {:?}",
        result.warnings
    );
}

#[test]
fn test_reachability_guarded_arm_does_not_cover_following() {
    // Guarded arm `Red if true` doesn't fully cover `Red`, so the second
    // unguarded `Red` is reachable. No warning.
    let result = typecheck_ok(
        "enum Color { Red, Green, Blue }\n\
         fn check(c: Color) -> i64 {\n\
             match c {\n\
                 Red if true => 1,\n\
                 Red         => 2,\n\
                 Green       => 3,\n\
                 Blue        => 4,\n\
             }\n\
         }",
    );
    assert!(
        result
            .warnings
            .iter()
            .all(|w| w.kind != TypeErrorKind::UnreachableArm),
        "expected no UnreachableArm warnings, got: {:?}",
        result.warnings
    );
}

#[test]
fn test_reachability_clean_match_no_warnings() {
    let result = typecheck_ok(
        "enum Color { Red, Green, Blue }\n\
         fn name(c: Color) -> i64 {\n\
             match c {\n\
                 Red   => 1,\n\
                 Green => 2,\n\
                 Blue  => 3,\n\
             }\n\
         }",
    );
    assert!(
        result.warnings.is_empty(),
        "expected zero warnings on a clean match, got: {:?}",
        result.warnings
    );
}

// ── Maranget witness construction (exhaustiveness slice 4) ──────

#[test]
fn test_witness_for_some_literal_payloads_is_compound() {
    // The witness for Some(0)/Some(1)/None should be a Some(_)-shaped
    // pattern, demonstrating that the recursion built up a structured
    // witness instead of just listing top-level missing constructors.
    let errors = typecheck_errors(
        "fn f(opt: Option[i64]) -> i64 {\n\
             match opt {\n\
                 Some(0) => 0,\n\
                 Some(1) => 1,\n\
                 None    => -1,\n\
             }\n\
         }",
    );
    let exhaust_err = errors
        .iter()
        .find(|e| e.kind == TypeErrorKind::NonExhaustiveMatch)
        .expect("expected NonExhaustiveMatch error");
    assert!(
        exhaust_err.message.contains("Some("),
        "expected witness to be a Some(_) pattern, got: {}",
        exhaust_err.message
    );
}

#[test]
fn test_witness_for_tuple_scrutinee_is_compound() {
    let errors = typecheck_errors(
        "fn check(t: (i32, i32)) -> i32 {\n\
             match t {\n\
                 (0, 0) => 0,\n\
                 (1, 1) => 1,\n\
             }\n\
         }",
    );
    let exhaust_err = errors
        .iter()
        .find(|e| e.kind == TypeErrorKind::NonExhaustiveMatch)
        .expect("expected NonExhaustiveMatch error");
    assert!(
        exhaust_err.message.contains('(') && exhaust_err.message.contains(')'),
        "expected witness to be a tuple pattern, got: {}",
        exhaust_err.message
    );
}

// ── Maranget type-specific handling (exhaustiveness slice 3) ─────

#[test]
fn test_exhaustive_integer_scrutinee_requires_wildcard() {
    // Pre-Maranget: silently exhaustive (skipped). Slice 3: open-domain
    // i32 demands a `_` arm.
    let errors = typecheck_errors(
        "fn classify(n: i32) -> i32 {\n\
             match n {\n\
                 0 => 0,\n\
                 1 => 1,\n\
             }\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::NonExhaustiveMatch),
        "expected NonExhaustiveMatch on integer match without wildcard, got: {errors:?}"
    );
}

#[test]
fn test_exhaustive_integer_scrutinee_with_wildcard_passes() {
    typecheck_ok(
        "fn classify(n: i32) -> i32 {\n\
             match n {\n\
                 0 => 0,\n\
                 1 => 1,\n\
                 _ => 99,\n\
             }\n\
         }",
    );
}

#[test]
fn test_exhaustive_string_scrutinee_requires_wildcard() {
    let errors = typecheck_errors(
        "fn classify(s: String) -> i32 {\n\
             match s {\n\
                 \"a\" => 1,\n\
                 \"b\" => 2,\n\
             }\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::NonExhaustiveMatch),
        "expected NonExhaustiveMatch on String match without wildcard, got: {errors:?}"
    );
}

#[test]
fn test_exhaustive_tuple_scrutinee_open_field_requires_wildcard() {
    // Single-ctor tuple type, but the integer field is open-domain.
    let errors = typecheck_errors(
        "fn check(t: (i32, i32)) -> i32 {\n\
             match t {\n\
                 (0, 0) => 0,\n\
                 (1, 1) => 1,\n\
             }\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::NonExhaustiveMatch),
        "expected NonExhaustiveMatch on tuple match with open-domain fields, got: {errors:?}"
    );
}

#[test]
fn test_exhaustive_tuple_scrutinee_with_wildcard_passes() {
    typecheck_ok(
        "fn check(t: (i32, i32)) -> i32 {\n\
             match t {\n\
                 (0, 0) => 0,\n\
                 _      => 99,\n\
             }\n\
         }",
    );
}

// ── Category 9: Method Calls and Impl Blocks ────────────────────

#[test]
fn test_method_self_access() {
    typecheck_ok(
        "struct Counter { value: i64 }\n\
         impl Counter {\n\
             fn get(self) -> i64 { self.value }\n\
         }",
    );
}

#[test]
fn test_self_type_in_constructor() {
    typecheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         impl Point {\n\
             fn origin() -> Point { Point { x: 0, y: 0 } }\n\
         }",
    );
}

// ── Category 10: Miscellaneous ──────────────────────────────────

#[test]
fn test_tuple_construction() {
    typecheck_ok("fn main() { let t = (1, true, \"hello\"); }");
}

#[test]
fn test_cast_numeric() {
    typecheck_ok("fn main() { let x = 42 as f64; }");
}

#[test]
fn test_cast_invalid() {
    let errors = typecheck_errors("fn main() { let x = \"hi\" as i64; }");
    assert!(errors.iter().any(|e| e.kind == TypeErrorKind::InvalidCast));
}

#[test]
fn test_unary_neg() {
    typecheck_ok("fn main() { let x: i64 = -5; }");
}

#[test]
fn test_unary_not() {
    typecheck_ok("fn main() { let b: bool = not true; }");
}

#[test]
fn test_unary_not_wrong_type() {
    let errors = typecheck_errors("fn main() { let x = not 42; }");
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::InvalidUnaryOp));
}

#[test]
fn test_deref_ref_param() {
    typecheck_ok(
        "fn read_val(r: ref i64) -> i64 { *r }\n\
         fn main() { }",
    );
}

#[test]
fn test_deref_mut_ref_param() {
    typecheck_ok(
        "fn read_val(r: mut ref i64) -> i64 { *r }\n\
         fn main() { }",
    );
}

#[test]
fn test_deref_non_ref_is_error() {
    let errors = typecheck_errors("fn main() { let x: i64 = 5; let y = *x; }");
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::InvalidUnaryOp),
        "expected InvalidUnaryOp for deref of non-ref"
    );
}

#[test]
fn test_deref_assign_through_mut_ref_ok() {
    typecheck_ok(
        "fn set_val(r: mut ref i64) { *r = 42; }\n\
         fn main() { }",
    );
}

#[test]
fn test_deref_assign_through_ref_is_error() {
    let errors = typecheck_errors(
        "fn try_set(r: ref i64) { *r = 42; }\n\
         fn main() { }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::InvalidUnaryOp),
        "expected error when assigning through shared ref"
    );
}

#[test]
fn test_return_type_mismatch() {
    let errors = typecheck_errors(
        "fn foo() -> i64 {\n\
             return true;\n\
         }",
    );
    assert!(errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch));
}

#[test]
fn test_const_type_checked() {
    typecheck_ok("const MAX: i64 = 100;");
}

#[test]
fn test_const_type_mismatch() {
    let errors = typecheck_errors("const MAX: i64 = true;");
    assert!(errors[0].kind == TypeErrorKind::TypeMismatch);
}

// ── Category 11: Error Recovery ─────────────────────────────────

#[test]
fn test_multiple_errors_reported() {
    let errors = typecheck_errors(
        "fn main() {\n\
             let x: bool = 42;\n\
             let y: i64 = true;\n\
         }",
    );
    assert!(errors.len() >= 2);
}

#[test]
fn test_error_does_not_cascade() {
    // Error in one let should not affect unrelated bindings
    let parsed = parse(
        "fn main() {\n\
             let x: bool = 42;\n\
             let y: i64 = 5;\n\
             let z: i64 = y + 1;\n\
         }",
    );
    assert!(parsed.errors.is_empty());
    let resolved = resolve(&parsed.program);
    assert!(resolved.errors.is_empty());
    let result = typecheck(&parsed.program, &resolved);
    // Only the first let should error
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors[0].message.contains("bool"));
}

// ── Category 12: Complex Programs ───────────────────────────────

#[test]
fn test_complex_program() {
    typecheck_ok(
        "struct User {\n\
             name: String,\n\
             age: i64,\n\
         }\n\
         \n\
         enum Status {\n\
             Active,\n\
             Inactive,\n\
         }\n\
         \n\
         impl User {\n\
             fn is_adult(self) -> bool {\n\
                 self.age > 18\n\
             }\n\
         }\n\
         \n\
         fn process(user: User) -> bool {\n\
             if user.age > 0 {\n\
                 true\n\
             } else {\n\
                 false\n\
             }\n\
         }\n\
         \n\
         fn check_status(s: Status) -> i64 {\n\
             match s {\n\
                 Active => 1,\n\
                 Inactive => 0,\n\
             }\n\
         }\n\
         \n\
         fn main() {\n\
             let u = User { name: \"Alice\", age: 30 };\n\
             let ok = process(u);\n\
             let s = Active;\n\
             let code = check_status(s);\n\
         }",
    );
}

#[test]
fn test_struct_pattern_binds_types() {
    typecheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn main() {\n\
             let p = Point { x: 1, y: 2 };\n\
             let Point { x, y } = p;\n\
             let sum: i64 = x + y;\n\
         }",
    );
}

#[test]
fn test_while_loop_types() {
    typecheck_ok(
        "fn main() {\n\
             let mut count: i64 = 0;\n\
             while count < 10 {\n\
                 count = count + 1;\n\
             }\n\
         }",
    );
}

#[test]
fn test_closure_types() {
    typecheck_ok(
        "fn main() {\n\
             let f = |x: i64, y: i64| x + y;\n\
         }",
    );
}

// ── Closure pushdown against expected `Type::Function` (round 10.1) ──
//
// `check_expr` seeds unannotated closure params from the expected
// `Type::Function { params, .. }` instead of `fresh_type_var()`. This is
// the receiving end of step 2's "closure parameter types become concrete"
// — without it, even a type-annotated `let` binding would type-check the
// closure body with fresh vars in place of the expected param types.

#[test]
fn test_closure_pushdown_unannotated_params() {
    // Closure params have no type annotation; expected `Fn(i64) -> i64`
    // pushdown gives `x` type `i64`, so `x + 1` resolves to `i64`.
    typecheck_ok(
        "fn main() {\n\
             let f: Fn(i64) -> i64 = |x| x + 1;\n\
         }",
    );
}

#[test]
fn test_closure_pushdown_partial_annotation() {
    // First param annotated `i64` (matches expected), second left unannotated
    // (filled from expected `i64`). Body `x + y` resolves on two `i64`s.
    typecheck_ok(
        "fn main() {\n\
             let f: Fn(i64, i64) -> i64 = |x: i64, y| x + y;\n\
         }",
    );
}

#[test]
fn test_closure_pushdown_explicit_annotation_wins() {
    // Explicit param annotation conflicts with expected: `bool` vs `i64`.
    // The annotation wins for the binding's type; the resulting
    // `Fn(bool) -> bool` then fails the structural assignability check
    // against the declared `Fn(i64) -> i64`.
    let errors = typecheck_errors(
        "fn main() {\n\
             let f: Fn(i64) -> i64 = |x: bool| x;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::TypeMismatch)),
        "expected TypeMismatch from explicit-annotation conflict, got: {errors:?}"
    );
}

#[test]
fn test_closure_pushdown_arity_mismatch_falls_through() {
    // Closure has 2 params; expected is `Fn(i64) -> i64` (1 param). The
    // pushdown branch's arity guard sends this through the synth path, so
    // `check_assignable` reports a normal type-mismatch.
    let errors = typecheck_errors(
        "fn main() {\n\
             let f: Fn(i64) -> i64 = |x, y| x;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::TypeMismatch)),
        "expected TypeMismatch from arity-mismatch closure, got: {errors:?}"
    );
}

// ── Two-pass arg inference at generic call sites (round 10.1) ──
//
// `infer_call`'s generic branch defers closure args to a second pass so
// that `T` is solved from non-closure args first. The substituted param
// type then flows into the closure via the `check_expr` pushdown above.
// Together these implement step 1 (constraint collection from
// non-closure args) and step 2 (type substitution before closure body
// check) of design.md § Monomorphization order for compound polymorphism.

#[test]
fn test_compound_call_closure_sees_concrete_type() {
    // Without the two-pass split, the closure body type-checks against
    // `?T0` (a fresh var). Field access on `?T0` cannot resolve, so `q.x`
    // fails. With the split, `T = Point` is solved from the first arg
    // before the closure is elaborated.
    typecheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn run[T](p: T, cb: Fn(T) -> i64) -> i64 { cb(p) }\n\
         fn main() {\n\
             let p = Point { x: 5, y: 7 };\n\
             let _r = run(p, |q| q.x);\n\
         }",
    );
}

#[test]
fn test_compound_call_closure_unannotated_int_param() {
    // `T = i64` solved from the first arg; closure's `y` then gets `i64`
    // and `y + 1` resolves.
    typecheck_ok(
        "fn run[T](x: T, cb: Fn(T) -> T) -> T { cb(x) }\n\
         fn main() {\n\
             let r: i64 = run(42, |y| y + 1);\n\
         }",
    );
}

#[test]
fn test_compound_call_type_error_reported_as_typemismatch_not_effect() {
    // Step 6 of design.md § Monomorphization order for compound polymorphism:
    // when a call site fails, type errors are reported before any effect
    // diagnostic. With `T = bool` solved from the first arg, the closure body
    // `touch_db(y)` fails because `y: bool` and `touch_db` takes `i64` — a
    // pure type-mismatch. No effect diagnostic should fire at this site.
    //
    // Architecturally this is enforced by phase ordering (typecheck aborts
    // before lower/effectcheck run), but the test pins the diagnostic kind so
    // a future re-architecture cannot regress the user-visible error to an
    // effect-shaped message.
    let errors = typecheck_errors(
        "fn pipeline[T, with E](x: T, cb: Fn(T) -> T with E) -> T with E { cb(x) }\n\
         fn touch_db(x: i64) -> i64 { x }\n\
         fn main() {\n\
             let _ = pipeline(true, |y| touch_db(y));\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::TypeMismatch)),
        "expected TypeMismatch on the closure body, got: {errors:?}"
    );
}

#[test]
fn test_compound_call_closure_body_type_error_still_reported() {
    // The split does not silence body-level type errors. `q.missing_field`
    // is invalid even after `T = Point` is substituted, so a normal
    // type-mismatch / unknown-field diagnostic must still fire.
    let errors = typecheck_errors(
        "struct Point { x: i64, y: i64 }\n\
         fn run[T](p: T, cb: Fn(T) -> i64) -> i64 { cb(p) }\n\
         fn main() {\n\
             let p = Point { x: 5, y: 7 };\n\
             let _r = run(p, |q| q.missing_field);\n\
         }",
    );
    assert!(
        !errors.is_empty(),
        "expected a body-level diagnostic for unknown field access in closure"
    );
}

#[test]
fn test_closure_pushdown_body_return_mismatch() {
    // Pushdown gives `x: i64`; body type is `i64` but expected return is `bool`.
    // The body's `check_expr` enforces the return-type match.
    let errors = typecheck_errors(
        "fn main() {\n\
             let f: Fn(i64) -> bool = |x| x;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::TypeMismatch)),
        "expected TypeMismatch from body-return mismatch, got: {errors:?}"
    );
}

// ── Method-call analogue of round 10.1 closure pushdown ────────
//
// The closure-pushdown logic in `infer_call`'s generic branch was lifted
// into a shared helper (`check_call_args_with_substitution`) so user-defined
// generic methods get the same two-pass arg inference. Without the helper,
// `r.pipeline(42, |y| y + 1)` left `y` typed as `?T0` and failed body
// elaboration even though `T = i64` was solvable from the eager arg.

#[test]
fn test_method_call_closure_pushdown_unannotated_int_param() {
    // Method-call analogue of `test_compound_call_closure_unannotated_int_param`.
    // `T = i64` is solved from the first arg before the closure is elaborated.
    typecheck_ok(
        "struct Runner {}\n\
         impl Runner {\n\
             fn pipeline[T](self, x: T, cb: Fn(T) -> T) -> T { cb(x) }\n\
         }\n\
         fn main() {\n\
             let r = Runner {};\n\
             let out: i64 = r.pipeline(42, |y| y + 1);\n\
         }",
    );
}

#[test]
fn test_method_call_closure_pushdown_struct_field_access() {
    // Method-call analogue of `test_compound_call_closure_sees_concrete_type`.
    // `T = Point` solved from the eager arg; `q.x` resolves on the substituted
    // closure parameter.
    typecheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         struct Runner {}\n\
         impl Runner {\n\
             fn run[T](self, p: T, cb: Fn(T) -> i64) -> i64 { cb(p) }\n\
         }\n\
         fn main() {\n\
             let r = Runner {};\n\
             let p = Point { x: 5, y: 7 };\n\
             let _r = r.run(p, |q| q.x);\n\
         }",
    );
}

#[test]
fn test_nested_function_calls() {
    typecheck_ok(
        "fn double(x: i64) -> i64 { x + x }\n\
         fn add(a: i64, b: i64) -> i64 { a + b }\n\
         fn main() {\n\
             let result: i64 = add(double(2), double(3));\n\
         }",
    );
}

#[test]
fn test_extern_function_types() {
    typecheck_ok(
        "effect resource FileSystem;\n\
         extern \"C\" fn write(fd: i32, buf: i64, count: i64) -> i64 writes(FileSystem);\n\
         fn main() {\n\
             let result: i64 = write(1, 0, 10);\n\
         }",
    );
}

#[test]
fn test_char_literal_type() {
    typecheck_ok("fn main() { let c: char = 'x'; }");
}

#[test]
fn test_char_type_mismatch() {
    let errors = typecheck_errors("fn main() { let c: char = 42; }");
    assert!(!errors.is_empty());
}

// ── Category: Eq Trait Enforcement (B3 fix) ────────────────────

#[test]
fn test_eq_on_primitives() {
    // Primitives support == implicitly — no #[derive(Eq)] needed.
    typecheck_ok("fn main() { let b: bool = 1 == 2; }");
    typecheck_ok("fn main() { let b: bool = true != false; }");
    typecheck_ok("fn main() { let b: bool = 'a' == 'b'; }");
}

#[test]
fn test_eq_on_struct_with_derive_eq() {
    typecheck_ok(
        "#[derive(Eq)]\n\
         struct Point { x: i64, y: i64 }\n\
         fn main() {\n\
             let a = Point { x: 1, y: 2 };\n\
             let b = Point { x: 3, y: 4 };\n\
             let same: bool = a == b;\n\
         }",
    );
}

#[test]
fn test_eq_on_struct_without_derive_eq() {
    // Struct without #[derive(Eq)] should produce an error on ==.
    let errors = typecheck_errors(
        "struct Point { x: i64, y: i64 }\n\
         fn main() {\n\
             let a = Point { x: 1, y: 2 };\n\
             let b = Point { x: 3, y: 4 };\n\
             let same: bool = a == b;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::InvalidBinaryOp),
        "Expected Eq trait error, got: {:?}",
        errors
    );
}

#[test]
fn test_noteq_on_struct_without_derive_eq() {
    // != also requires Eq.
    let errors = typecheck_errors(
        "struct Point { x: i64, y: i64 }\n\
         fn main() {\n\
             let a = Point { x: 1, y: 2 };\n\
             let b = Point { x: 3, y: 4 };\n\
             let diff: bool = a != b;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::InvalidBinaryOp),
        "Expected Eq trait error for !=, got: {:?}",
        errors
    );
}

#[test]
fn test_eq_on_enum_with_derive_eq() {
    typecheck_ok(
        "#[derive(Eq)]\n\
         enum Color { Red, Green, Blue }\n\
         fn main() {\n\
             let a = Color.Red;\n\
             let b = Color.Blue;\n\
             let same: bool = a == b;\n\
         }",
    );
}

#[test]
fn test_eq_on_enum_without_derive_eq() {
    let errors = typecheck_errors(
        "enum Color { Red, Green, Blue }\n\
         fn main() {\n\
             let a = Color.Red;\n\
             let b = Color.Blue;\n\
             let same: bool = a == b;\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::InvalidBinaryOp),
        "Expected Eq trait error, got: {:?}",
        errors
    );
}

// ── Destructuring in function/closure parameters ─────────────────

#[test]
fn test_tuple_destructuring_param_typechecks() {
    typecheck_ok("fn add((a, b): (i64, i64)) -> i64 { a + b }");
}

#[test]
fn test_struct_destructuring_param_typechecks() {
    typecheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn get_x(Point { x, y }: Point) -> i64 { x }",
    );
}

#[test]
fn test_wildcard_destructuring_param_typechecks() {
    typecheck_ok("fn y_only((_, y): (i64, i64)) -> i64 { y }");
}

#[test]
fn test_nested_tuple_destructuring_param_typechecks() {
    typecheck_ok("fn nested(((a, b), c): ((i64, i64), i64)) -> i64 { a + b + c }");
}

#[test]
fn test_refutable_enum_variant_param_error() {
    // A struct-style enum variant pattern in param position is refutable.
    let errors = typecheck_errors(
        "enum Shape { Circle { r: i64 }, Square { side: i64 } }\n\
         fn area(Circle { r: radius }: Shape) -> i64 { radius }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::RefutablePattern),
        "expected RefutablePattern error, got: {errors:?}"
    );
}

#[test]
fn test_tuple_arity_mismatch_param_error() {
    // Pattern has 3 elements but the declared type has 2.
    let errors = typecheck_errors("fn f((a, b, c): (i64, i64)) -> i64 { a + b }");
    assert!(
        errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch error for arity mismatch, got: {errors:?}"
    );
}

#[test]
fn test_tuple_pattern_on_non_tuple_type_error() {
    // Pattern is a tuple but the declared type is a scalar.
    let errors = typecheck_errors("fn f((a, b): i64) -> i64 { a }");
    assert!(
        errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch error for tuple-on-scalar, got: {errors:?}"
    );
}

#[test]
fn test_struct_pattern_unknown_field_error() {
    // Struct destructuring pattern references a field that doesn't exist.
    let errors = typecheck_errors(
        "struct Point { x: i64, y: i64 }\n\
         fn f(Point { x, z }: Point) -> i64 { x }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::UndefinedField),
        "expected UndefinedField error for missing struct field, got: {errors:?}"
    );
}

#[test]
fn test_closure_refutable_param_error() {
    // A struct-style enum variant pattern in a closure parameter is refutable.
    let errors = typecheck_errors(
        "enum Color { Red { r: i64 }, Blue { b: i64 } }\n\
         fn main() { let f = |Red { r: v }: Color| v; }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::RefutablePattern),
        "expected RefutablePattern error in closure, got: {errors:?}"
    );
}

// ── Named / Labeled Arguments ───────────────────────────────────

#[test]
fn test_labeled_args_correct() {
    typecheck_ok(
        "
        fn add(x: i64, y: i64) -> i64 { x + y }
        fn main() { let r = add(x: 1, y: 2); }
    ",
    );
}

#[test]
fn test_labeled_args_partial_suffix() {
    typecheck_ok(
        "
        fn add(x: i64, y: i64) -> i64 { x + y }
        fn main() { let r = add(1, y: 2); }
    ",
    );
}

#[test]
fn test_labeled_args_all_positional() {
    typecheck_ok(
        "
        fn add(x: i64, y: i64) -> i64 { x + y }
        fn main() { let r = add(1, 2); }
    ",
    );
}

#[test]
fn test_labeled_args_wrong_name() {
    let errors = typecheck_errors(
        "
        fn add(x: i64, y: i64) -> i64 { x + y }
        fn main() { let r = add(z: 1, y: 2); }
    ",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::LabelMismatch));
}

#[test]
fn test_labeled_args_wrong_order() {
    let errors = typecheck_errors(
        "
        fn add(x: i64, y: i64) -> i64 { x + y }
        fn main() { let r = add(y: 1, x: 2); }
    ",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::LabelMismatch));
}

#[test]
fn test_labeled_args_non_contiguous() {
    let errors = typecheck_errors(
        "
        fn f(a: i64, b: i64, c: i64) -> i64 { a + b + c }
        fn main() { let r = f(a: 1, 2, c: 3); }
    ",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::NonContiguousLabels));
}

#[test]
fn test_labeled_args_method_call() {
    typecheck_ok(
        "
        struct Point { x: i64, y: i64 }
        impl Point {
            fn translate(self, dx: i64, dy: i64) -> i64 { self.x + dx + dy }
        }
        fn main() {
            let p = Point { x: 1, y: 2 };
            let r = p.translate(dx: 3, dy: 4);
        }
    ",
    );
}

#[test]
fn test_labeled_args_method_wrong_name() {
    let errors = typecheck_errors(
        "
        struct Point { x: i64, y: i64 }
        impl Point {
            fn translate(self, dx: i64, dy: i64) -> i64 { self.x + dx + dy }
        }
        fn main() {
            let p = Point { x: 1, y: 2 };
            let r = p.translate(xx: 3, dy: 4);
        }
    ",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::LabelMismatch));
}

#[test]
fn test_labeled_args_destructuring_param_cannot_be_labeled() {
    let errors = typecheck_errors(
        "
        fn add((a, b): (i64, i64)) -> i64 { a + b }
        fn main() { let r = add(p: (1, 2)); }
    ",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::LabelMismatch));
}

// ── Category: Pipe Operator ────────────────────────────────────

#[test]
fn test_pipe_bare_function() {
    // a |> f desugars to f(a)
    typecheck_ok(
        "
        fn double(x: i64) -> i64 { x * 2 }
        fn main() { let r: i64 = 5 |> double; }
    ",
    );
}

#[test]
fn test_pipe_chained() {
    // a |> f |> g desugars to g(f(a))
    typecheck_ok(
        "
        fn double(x: i64) -> i64 { x * 2 }
        fn add_one(x: i64) -> i64 { x + 1 }
        fn main() { let r: i64 = 5 |> double |> add_one; }
    ",
    );
}

#[test]
fn test_pipe_with_extra_args() {
    // a |> f(b) desugars to f(a, b)
    typecheck_ok(
        "
        fn add(x: i64, y: i64) -> i64 { x + y }
        fn main() { let r: i64 = 5 |> add(10); }
    ",
    );
}

#[test]
fn test_pipe_with_placeholder() {
    // a |> f(_, b) desugars to f(a, b)
    typecheck_ok(
        "
        fn add(x: i64, y: i64) -> i64 { x + y }
        fn main() { let r: i64 = 5 |> add(_, 10); }
    ",
    );
}

#[test]
fn test_pipe_placeholder_non_first_position() {
    // a |> f(b, _) desugars to f(b, a)
    typecheck_ok(
        "
        fn sub(x: i64, y: i64) -> i64 { x - y }
        fn main() { let r: i64 = 5 |> sub(10, _); }
    ",
    );
}

#[test]
fn test_pipe_type_mismatch() {
    let errors = typecheck_errors(
        "
        fn double(x: i64) -> i64 { x * 2 }
        fn main() { let r = true |> double; }
    ",
    );
    assert!(errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch));
}

#[test]
fn test_pipe_wrong_arity() {
    let errors = typecheck_errors(
        "
        fn add(x: i64, y: i64) -> i64 { x + y }
        fn main() { let r = 5 |> add; }
    ",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs));
}

#[test]
fn test_pipe_multiple_placeholders_error() {
    let errors = typecheck_errors(
        "
        fn add3(x: i64, y: i64, z: i64) -> i64 { x + y + z }
        fn main() { let r = 5 |> add3(_, 10, _); }
    ",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::InvalidPipePlaceholder));
}

#[test]
fn test_pipe_placeholder_outside_pipe_error() {
    let errors = typecheck_errors(
        "
        fn double(x: i64) -> i64 { x * 2 }
        fn main() { let r = double(_); }
    ",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::InvalidPipePlaceholder));
}

#[test]
fn test_pipe_return_type_propagation() {
    // Pipe result type should be the return type of the last stage
    typecheck_ok(
        "
        fn to_bool(x: i64) -> bool { x > 0 }
        fn negate(b: bool) -> bool { not b }
        fn main() { let r: bool = 42 |> to_bool |> negate; }
    ",
    );
}

#[test]
fn test_pipe_not_callable_rhs() {
    let errors = typecheck_errors(
        "
        fn main() { let r = 5 |> 42; }
    ",
    );
    assert!(errors.iter().any(|e| e.kind == TypeErrorKind::NotCallable));
}

// ── Generic Type Inference ─────────────────────────────────────

#[test]
fn test_generic_function_body_typechecks() {
    // Generic function body with matching TypeParam types
    typecheck_ok("fn identity[T](x: T) -> T { x }");
}

#[test]
fn test_generic_struct_definition_typechecks() {
    // Generic struct with fields referencing type params
    typecheck_ok("struct Pair[A, B] { first: A, second: B }");
}

#[test]
fn test_generic_enum_definition_typechecks() {
    // Generic enum with variants referencing type params
    typecheck_ok("enum Maybe[T] { Just(T), Nothing }");
}

// ── Pattern Guards ─────────────────────────────────────────────

#[test]
fn test_match_guard_bool_ok() {
    typecheck_ok(
        "enum Color { Red, Green, Blue }\n\
         fn check(c: Color) -> i64 {\n\
             match c {\n\
                 Red if true => 1,\n\
                 Red => 2,\n\
                 Green => 3,\n\
                 Blue => 4,\n\
             }\n\
         }",
    );
}

#[test]
fn test_match_guard_non_bool_error() {
    let errors = typecheck_errors(
        "enum Color { Red, Green, Blue }\n\
         fn check(c: Color) -> i64 {\n\
             match c {\n\
                 Red if 42 => 1,\n\
                 Red => 2,\n\
                 Green => 3,\n\
                 Blue => 4,\n\
             }\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("guard") && e.message.contains("bool")));
}

#[test]
fn test_match_guarded_arm_excluded_from_exhaustiveness() {
    // All arms have guards, so exhaustiveness is not satisfied
    let errors = typecheck_errors(
        "enum Ab { A, B }\n\
         fn check(x: Ab) -> i64 {\n\
             match x {\n\
                 A if true => 1,\n\
                 B if true => 2,\n\
             }\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::NonExhaustiveMatch));
}

// ── todo() / unreachable() ─────────────────────────────────────

#[test]
fn test_todo_returns_never() {
    typecheck_ok(
        "fn placeholder() -> i64 {\n\
             todo()\n\
         }",
    );
}

#[test]
fn test_unreachable_returns_never() {
    typecheck_ok(
        "fn impossible() -> bool {\n\
             unreachable()\n\
         }",
    );
}

#[test]
fn test_todo_with_message() {
    typecheck_ok(
        "fn later() -> i64 {\n\
             todo(\"not yet implemented\")\n\
         }",
    );
}

#[test]
fn test_todo_non_string_message_error() {
    let errors = typecheck_errors(
        "fn later() -> i64 {\n\
             todo(42)\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("todo") && e.message.contains("str")));
}

#[test]
fn test_todo_too_many_args_error() {
    let errors = typecheck_errors(
        "fn later() -> i64 {\n\
             todo(\"a\", \"b\")\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs));
}

// ── Associated Types ───────────────────────────────────────────

#[test]
fn test_assoc_type_in_trait_declaration() {
    typecheck_ok(
        "trait Container {\n\
             type Item;\n\
             fn get(self) -> i64;\n\
         }",
    );
}

#[test]
fn test_assoc_type_binding_in_impl() {
    typecheck_ok(
        "trait Container {\n\
             type Item;\n\
         }\n\
         struct IntVec { data: i64 }\n\
         impl Container for IntVec {\n\
             type Item = i64;\n\
         }",
    );
}

#[test]
fn test_assoc_type_missing_in_impl_error() {
    let errors = typecheck_errors(
        "trait Container {\n\
             type Item;\n\
         }\n\
         struct IntVec { data: i64 }\n\
         impl Container for IntVec { }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("missing associated type") && e.message.contains("Item")));
}

// ── Where Clause Verification ──────────────────────────────────

#[test]
fn test_where_clause_known_trait_ok() {
    typecheck_ok(
        "trait Printable { fn print(self); }\n\
         fn show[T](x: T) where T: Printable { }",
    );
}

#[test]
fn test_where_clause_builtin_trait_ok() {
    typecheck_ok("fn sort[T](items: T) where T: Ord { }");
}

#[test]
fn test_where_clause_unknown_trait_error() {
    let errors = typecheck_errors("fn foo[T](x: T) where T: NonExistent { }");
    assert!(errors
        .iter()
        .any(|e| e.message.contains("unknown trait") && e.message.contains("NonExistent")));
}

#[test]
fn test_where_clause_unknown_type_param_error() {
    let errors = typecheck_errors("fn foo[T](x: T) where U: Eq { }");
    assert!(errors
        .iter()
        .any(|e| e.message.contains("unknown type parameter") && e.message.contains("U")));
}

#[test]
fn test_where_clause_stdlib_operator_traits_ok() {
    // Operator traits registered as built-ins must be accepted as bounds.
    for trait_name in [
        "Add", "Sub", "Mul", "Div", "Rem", "Neg", "BitAnd", "BitOr", "BitXor", "Shl", "Shr", "Not",
        "Index", "IndexMut", "Display",
    ] {
        let src = format!("fn f[T](x: T) where T: {} {{ }}", trait_name);
        typecheck_ok(&src);
    }
}

#[test]
fn test_where_clause_stdlib_conversion_traits_ok() {
    for trait_name in ["From", "Into", "TryFrom", "TryInto"] {
        let src = format!("fn f[T](x: T) where T: {} {{ }}", trait_name);
        typecheck_ok(&src);
    }
}

#[test]
fn test_where_clause_and_inline_bound_coexist_ok() {
    // Same type param with both an inline bound and a where-clause bound — both apply.
    typecheck_ok("fn f[T: Eq](x: T) where T: Ord { }");
}

#[test]
fn test_where_clause_struct_ok() {
    typecheck_ok("struct Wrapper[T] where T: Ord { value: T }");
}

#[test]
fn test_where_clause_struct_unknown_trait_error() {
    let errors = typecheck_errors("struct Wrapper[T] where T: Bogus { value: T }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("unknown trait") && e.message.contains("Bogus")),
        "expected unknown-trait error, got: {errors:?}"
    );
}

#[test]
fn test_where_clause_struct_unknown_type_param_error() {
    let errors = typecheck_errors("struct Foo[T] where U: Ord { value: T }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("unknown type parameter") && e.message.contains("U")),
        "expected unknown-type-param error, got: {errors:?}"
    );
}

#[test]
fn test_where_clause_enum_ok() {
    typecheck_ok("enum Pair[T] where T: Clone { Single(T), Double(T, T) }");
}

#[test]
fn test_where_clause_enum_unknown_trait_error() {
    let errors = typecheck_errors("enum Pair[T] where T: Nonexistent { Unit }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("unknown trait") && e.message.contains("Nonexistent")),
        "expected unknown-trait error, got: {errors:?}"
    );
}

#[test]
fn test_where_clause_impl_ok() {
    typecheck_ok(
        "struct Wrapper[T] { value: T }\n\
         impl[T] Wrapper[T] where T: Ord { }",
    );
}

#[test]
fn test_where_clause_impl_unknown_trait_error() {
    let errors = typecheck_errors(
        "struct Wrapper[T] { value: T }\n\
         impl[T] Wrapper[T] where T: Bogus { }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("unknown trait") && e.message.contains("Bogus")),
        "expected unknown-trait error, got: {errors:?}"
    );
}

#[test]
fn test_where_clause_trait_ok() {
    typecheck_ok("trait Sortable[T] where T: Ord { fn sort(self); }");
}

#[test]
fn test_where_clause_trait_unknown_trait_error() {
    let errors = typecheck_errors("trait Foo[T] where T: Mystery { fn bar(self); }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("unknown trait") && e.message.contains("Mystery")),
        "expected unknown-trait error, got: {errors:?}"
    );
}

#[test]
fn test_inline_bound_fn_ok() {
    typecheck_ok("fn max[T: Ord](a: T, b: T) -> T { a }");
}

#[test]
fn test_inline_bound_fn_unknown_trait_error() {
    let errors = typecheck_errors("fn f[T: Bogus](x: T) { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("unknown trait") && e.message.contains("Bogus")),
        "expected unknown-trait error for inline bound, got: {errors:?}"
    );
}

#[test]
fn test_inline_bound_struct_ok() {
    typecheck_ok("struct Sorted[T: Ord] { items: T }");
}

#[test]
fn test_inline_bound_struct_unknown_trait_error() {
    let errors = typecheck_errors("struct Foo[T: Mystery] { value: T }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("unknown trait") && e.message.contains("Mystery")),
        "expected unknown-trait error for struct inline bound, got: {errors:?}"
    );
}

// ── Default Parameter Values ───────────────────────────────────

#[test]
fn test_default_param_trailing_ok() {
    typecheck_ok("fn serve(host: String, port: i64 = 8080) { }");
}

#[test]
fn test_default_param_type_checked() {
    let errors = typecheck_errors("fn serve(host: String, port: i64 = true) { }");
    assert!(errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch));
}

#[test]
fn test_default_param_non_trailing_error() {
    let errors = typecheck_errors("fn bad(x: i64 = 1, y: i64) { }");
    assert!(errors.iter().any(|e| e.message.contains("defaulted")));
}

#[test]
fn test_multiple_defaults_ok() {
    typecheck_ok("fn config(host: String, port: i64 = 8080, timeout: i64 = 5000) { }");
}

#[test]
fn test_default_const_named_constant_ok() {
    typecheck_ok("const MAX: i64 = 100;\nfn f(x: i64 = MAX) { }");
}

#[test]
fn test_default_tuple_literal_ok() {
    typecheck_ok("fn f(x: (i64, i64) = (1, 2)) { }");
}

#[test]
fn test_default_call_expr_error() {
    // Function call in default is non-constant.
    let errors = typecheck_errors("fn get() -> i64 { 42 }\nfn f(x: i64 = get()) { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("constant expression")),
        "expected constant-expression error, got: {errors:?}"
    );
}

#[test]
fn test_default_references_sibling_param_error() {
    // Default expression references another parameter by name.
    let errors = typecheck_errors("fn f(n: i64, x: i64 = n) { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("another parameter") && e.message.contains("'n'")),
        "expected cross-param reference error, got: {errors:?}"
    );
}

#[test]
fn test_default_closure_error() {
    // Closure in default is non-constant.
    let errors = typecheck_errors("fn f(x: i64 = { 42 }) { }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("constant expression")),
        "expected constant-expression error for block/closure default, got: {errors:?}"
    );
}

// ── Copy trait validation ──────────────────────────────────────

#[test]
fn test_derive_copy_all_copy_fields_ok() {
    typecheck_ok(
        "#[derive(Copy, Clone)]\n\
         struct Point { x: i64, y: i64 }",
    );
}

#[test]
fn test_derive_copy_non_copy_field_error() {
    let errors = typecheck_errors(
        "#[derive(Copy)]\n\
         struct Wrapper { name: String }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("Copy") && e.message.contains("name")));
}

#[test]
fn test_derive_copy_nested_copy_struct_ok() {
    typecheck_ok(
        "#[derive(Copy, Clone)]\n\
         struct Inner { x: i64 }\n\
         #[derive(Copy, Clone)]\n\
         struct Outer { inner: Inner }",
    );
}

#[test]
fn test_derive_copy_nested_non_copy_struct_error() {
    let errors = typecheck_errors(
        "struct Inner { name: String }\n\
         #[derive(Copy)]\n\
         struct Outer { inner: Inner }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("Copy") && e.message.contains("inner")));
}

#[test]
fn test_derive_copy_array_field_ok() {
    typecheck_ok(
        "#[derive(Copy, Clone)]\n\
         struct Buf { data: Array[i32, 4] }",
    );
}

#[test]
fn test_derive_copy_option_field_ok() {
    typecheck_ok(
        "#[derive(Copy, Clone)]\n\
         struct Opt { value: Option[i64] }",
    );
}

#[test]
fn test_derive_copy_without_clone_error() {
    let errors = typecheck_errors(
        "#[derive(Copy)]\n\
         struct Point { x: i64, y: i64 }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("Copy") && e.message.contains("Clone")));
}

#[test]
fn test_derive_copy_with_clone_ok() {
    typecheck_ok(
        "#[derive(Copy, Clone)]\n\
         struct Point { x: i64, y: i64 }",
    );
}

#[test]
fn test_distinct_type_derive_copy_ok() {
    typecheck_ok(
        "#[derive(Copy, Clone)]\n\
         distinct type Meters = i64;",
    );
}

#[test]
fn test_distinct_type_derive_copy_non_copy_base_error() {
    let errors = typecheck_errors(
        "#[derive(Copy, Clone)]\n\
         distinct type NamedThing = String;",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("Copy") && e.message.contains("NamedThing")));
}

#[test]
fn test_distinct_type_derive_copy_without_clone_error() {
    let errors = typecheck_errors(
        "#[derive(Copy)]\n\
         distinct type Meters = i64;",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("Copy") && e.message.contains("Clone")));
}

// ── #[derive(Arithmetic)] on distinct types ────────────────────

#[test]
fn test_derive_arithmetic_on_distinct_type_accepted() {
    typecheck_ok(
        "#[derive(Arithmetic)]\n\
         distinct type FloorNum = i64;\n\
         fn f(a: FloorNum, b: FloorNum) -> FloorNum { a + b }",
    );
}

#[test]
fn test_derive_arithmetic_all_ops_accepted() {
    typecheck_ok(
        "#[derive(Arithmetic)]\n\
         distinct type Meters = i64;\n\
         fn f(a: Meters, b: Meters) -> Meters { a + b }\n\
         fn g(a: Meters, b: Meters) -> Meters { a - b }\n\
         fn h(a: Meters, b: Meters) -> Meters { a * b }\n\
         fn j(a: Meters, b: Meters) -> Meters { a / b }\n\
         fn k(a: Meters, b: Meters) -> Meters { a % b }\n\
         fn neg(a: Meters) -> Meters { -a }",
    );
}

#[test]
fn test_derive_arithmetic_cross_type_rejected() {
    let errors = typecheck_errors(
        "#[derive(Arithmetic)]\n\
         distinct type FloorNum = i64;\n\
         #[derive(Arithmetic)]\n\
         distinct type UserId = i64;\n\
         fn f(a: FloorNum, b: UserId) -> FloorNum { a + b }",
    );
    assert!(
        !errors.is_empty(),
        "expected a type error for cross-distinct-type arithmetic"
    );
}

#[test]
fn test_derive_arithmetic_on_struct_rejected() {
    let errors = typecheck_errors(
        "#[derive(Arithmetic)]\n\
         struct Point { x: i64, y: i64 }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("Arithmetic") && e.message.contains("struct")),
        "expected rejection of #[derive(Arithmetic)] on struct, got {:?}",
        errors
    );
}

#[test]
fn test_derive_arithmetic_on_enum_rejected() {
    let errors = typecheck_errors(
        "#[derive(Arithmetic)]\n\
         enum Dir { Up, Down }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("Arithmetic") && e.message.contains("enum")),
        "expected rejection of #[derive(Arithmetic)] on enum, got {:?}",
        errors
    );
}

#[test]
fn test_derive_arithmetic_non_numeric_base_rejected() {
    let errors = typecheck_errors(
        "#[derive(Arithmetic)]\n\
         distinct type Tag = String;",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("Arithmetic") && e.message.contains("numeric")),
        "expected rejection of Arithmetic on non-numeric base, got {:?}",
        errors
    );
}

// ── defer / errdefer ───────────────────────────────────────────

#[test]
fn test_defer_body_typechecked() {
    typecheck_ok(
        "fn cleanup() {\n\
             let x = 1;\n\
             defer { let y = x + 1; }\n\
         }",
    );
}

#[test]
fn test_question_in_defer_is_error() {
    let errors = typecheck_errors(
        "fn foo() -> i64 { 1 }\n\
         fn risky() {\n\
             defer { let x = foo()?; }\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("?") && e.message.contains("defer")));
}

#[test]
fn test_question_in_errdefer_is_error() {
    let errors = typecheck_errors(
        "fn foo() -> i64 { 1 }\n\
         fn risky() {\n\
             errdefer { let x = foo()?; }\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("?") && e.message.contains("defer")));
}

#[test]
fn test_errdefer_binding_in_scope() {
    // errdefer(e) should make `e` available in the body
    typecheck_ok(
        "fn may_fail() {\n\
             errdefer(e) { e; }\n\
         }",
    );
}

#[test]
fn test_question_outside_defer_allowed() {
    // ? outside defer should still work
    typecheck_ok(
        "fn foo() -> Result[i64, String] { Ok(1) }\n\
         fn main() -> Result[i64, String] {\n\
             let x = foo()?;\n\
             Ok(x)\n\
         }",
    );
}

// ── ? operator semantics (Step 5) ──────────────────────────────

#[test]
fn test_question_unwraps_result_ok_payload() {
    typecheck_ok(
        "fn produce() -> Result[i64, String] { Ok(7) }\n\
         fn main() -> Result[i64, String] {\n\
             let x: i64 = produce()?;\n\
             Ok(x)\n\
         }",
    );
}

#[test]
fn test_question_unwraps_option_some_payload() {
    typecheck_ok(
        "fn produce() -> Option[i64] { Some(7) }\n\
         fn main() -> Option[i64] {\n\
             let x: i64 = produce()?;\n\
             Some(x)\n\
         }",
    );
}

#[test]
fn test_question_on_non_result_rejected() {
    let errors = typecheck_errors(
        "fn produce() -> i64 { 1 }\n\
         fn main() -> Result[i64, String] {\n\
             let x = produce()?;\n\
             Ok(x)\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("requires `Result` or `Option`")));
}

#[test]
fn test_question_in_non_result_function_rejected() {
    let errors = typecheck_errors(
        "fn produce() -> Result[i64, String] { Ok(1) }\n\
         fn main() -> i64 {\n\
             let x = produce()?;\n\
             x\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("function to return")));
}

#[test]
fn test_question_mixing_result_and_option_rejected() {
    let errors = typecheck_errors(
        "fn produce() -> Option[i64] { Some(1) }\n\
         fn main() -> Result[i64, String] {\n\
             let x = produce()?;\n\
             Ok(x)\n\
         }",
    );
    assert!(errors.iter().any(|e| e.message.contains("cannot mix")));
}

#[test]
fn test_question_cross_error_with_from_impl() {
    typecheck_ok(
        "struct ParseError { msg: String }\n\
         struct AppError { msg: String }\n\
         impl From for AppError {\n\
             fn from(e: ParseError) -> AppError { AppError { msg: e.msg } }\n\
         }\n\
         fn produce() -> Result[i64, ParseError] { Ok(7) }\n\
         fn main() -> Result[i64, AppError] {\n\
             let x: i64 = produce()?;\n\
             Ok(x)\n\
         }",
    );
}

#[test]
fn test_question_cross_error_without_from_impl_rejected() {
    let errors = typecheck_errors(
        "struct ParseError { msg: String }\n\
         struct AppError { msg: String }\n\
         fn produce() -> Result[i64, ParseError] { Ok(7) }\n\
         fn main() -> Result[i64, AppError] {\n\
             let x: i64 = produce()?;\n\
             Ok(x)\n\
         }",
    );
    assert!(
        errors.iter().any(|e| e.message.contains("no `impl From")),
        "expected From-not-found diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

// ── Integration: Pattern Guards ────────────────────────────────

#[test]
fn test_guard_type_error_in_match() {
    // Guard expression must be bool — non-bool is a type error
    let errors = typecheck_errors(
        "enum Num { A(i64), B }\n\
         fn check(n: Num) -> i64 {\n\
             match n {\n\
                 A(x) if x => 1,\n\
                 A(_) => 2,\n\
                 B => 3,\n\
             }\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("guard") && e.message.contains("bool")));
}

#[test]
fn test_guarded_exhaustiveness_error_with_mixed_arms() {
    // Some arms guarded, some not — but a variant is only covered by guarded arms
    let errors = typecheck_errors(
        "enum Dir { Up, Down, Left, Right }\n\
         fn name(d: Dir) -> i64 {\n\
             match d {\n\
                 Up => 1,\n\
                 Down => 2,\n\
                 Left if true => 3,\n\
                 Right if true => 4,\n\
             }\n\
         }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == TypeErrorKind::NonExhaustiveMatch));
}

#[test]
fn test_guard_expression_uses_bound_variable() {
    // Guard can reference variables bound in the pattern
    typecheck_ok(
        "enum Opt { Some(i64), None }\n\
         fn positive(o: Opt) -> i64 {\n\
             match o {\n\
                 Some(x) if x > 0 => x,\n\
                 Some(_) => 0,\n\
                 None => 0,\n\
             }\n\
         }",
    );
}

// ── Integration: todo() / unreachable() ────────────────────────

#[test]
fn test_todo_in_match_arm() {
    // todo() as match arm body — should be compatible with any return type
    typecheck_ok(
        "enum Ab { A, B }\n\
         fn handle(x: Ab) -> i64 {\n\
             match x {\n\
                 A => 42,\n\
                 B => todo(),\n\
             }\n\
         }",
    );
}

#[test]
fn test_unreachable_in_if_else_branch() {
    // unreachable() in else branch — Never type compatible with any type
    typecheck_ok(
        "fn must_be_positive(x: i64) -> i64 {\n\
             if x > 0 { x } else { unreachable() }\n\
         }",
    );
}

#[test]
fn test_todo_in_let_binding() {
    // todo() as initializer — Never coerces to any type
    typecheck_ok(
        "fn placeholder() {\n\
             let x: i64 = todo();\n\
         }",
    );
}

// ── Integration: Named Arguments ───────────────────────────────

#[test]
fn test_labeled_args_closure_labels_are_outer_only() {
    // Labels apply to the outer call, not the closure's params
    typecheck_ok(
        "fn apply(f: Fn(i64) -> i64, value: i64) -> i64 { f(value) }\n\
         fn main() {\n\
             let result = apply(|x: i64| x + 1, value: 10);\n\
         }",
    );
}

#[test]
fn test_labeled_args_all_labeled_in_order() {
    // All arguments labeled in declaration order — valid
    typecheck_ok(
        "fn point(x: i64, y: i64, z: i64) -> i64 { x + y + z }\n\
         fn main() { point(x: 1, y: 2, z: 3); }",
    );
}

// ── IEEE 754 Floats & F32/F64 Total-Order Types ────────────────

#[test]
fn test_float_eq_still_works() {
    // f64 == f64 is valid (PartialEq) even though f64 doesn't implement Eq
    typecheck_ok("fn main() { let b: bool = 1.0 == 2.0; }");
}

#[test]
fn test_float_comparison_still_works() {
    // f64 < f64 is valid (PartialOrd)
    typecheck_ok("fn main() { let b: bool = 1.0 < 2.0; }");
}

#[test]
fn test_f64_type_resolves() {
    // F64 is a recognized type
    typecheck_ok("fn main() { let x: F64 = F64 { value: 1.0 }; }");
}

#[test]
fn test_f32_type_resolves() {
    // F32 is a recognized type
    typecheck_ok("fn main() { let x: F32 = F32 { value: 1.0 }; }");
}

// ── @no_rc Struct Annotation ───────────────────────────────────

#[test]
fn test_no_rc_struct_parsed() {
    // @no_rc attribute on struct is accepted by parser and typechecker
    typecheck_ok(
        "@no_rc\n\
         struct Particle { x: f64, y: f64 }\n\
         fn main() { let p = Particle { x: 1.0, y: 2.0 }; }",
    );
}

#[test]
fn test_no_rc_flag_recorded() {
    // Verify the no_rc flag is recorded in StructInfo
    let result = typecheck_ok(
        "@no_rc\n\
         struct Particle { x: f64, y: f64 }\n\
         fn main() {}",
    );
    let info = result
        .struct_info
        .get("Particle")
        .expect("Particle struct should exist");
    assert!(info.no_rc, "Particle should have no_rc = true");
}

#[test]
fn test_no_rc_false_by_default() {
    let result = typecheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         fn main() {}",
    );
    let info = result
        .struct_info
        .get("Point")
        .expect("Point struct should exist");
    assert!(!info.no_rc, "Point should have no_rc = false by default");
}

// ── Ordering / MemoryOrdering Enums ────────────────────────────

#[test]
fn test_ordering_enum_resolves() {
    // Comparison-Ordering variants (Less / Equal / Greater) — returned by Ord.cmp
    typecheck_ok(
        "fn main() {\n\
             let lt = Ordering.Less;\n\
             let eq = Ordering.Equal;\n\
             let gt = Ordering.Greater;\n\
         }",
    );
}

#[test]
fn test_memory_ordering_enum_resolves() {
    // MemoryOrdering variants — used by Atomic[T] operations
    typecheck_ok(
        "fn main() {\n\
             let ord = MemoryOrdering.Relaxed;\n\
         }",
    );
}

#[test]
fn test_ordering_helper_methods_typecheck() {
    // `impl Ordering { fn is_lt … }` (design.md lines 5162-5168) lives in
    // baked source. Both the typechecker (via `register_baked_stdlib`'s
    // `env_add_impl` walk) and the interpreter (via `register_impl_methods`
    // on STDLIB_PROGRAMS) read the methods from `runtime/stdlib/ordering.kara`.
    typecheck_ok(
        "fn main() {\n\
             let o = Ordering.Less;\n\
             let _lt: bool = o.is_lt();\n\
             let _le: bool = o.is_le();\n\
             let _gt: bool = o.is_gt();\n\
             let _ge: bool = o.is_ge();\n\
             let _eq: bool = o.is_eq();\n\
         }",
    );
}

#[test]
fn test_ord_cmp_returns_comparison_ordering() {
    // The original motivator for the disambiguation CR: `Ord.cmp(a, b)` on
    // primitives returns `Ordering`, and `Ordering.Less` is now a registered
    // variant of that type. Pre-rename this would have failed because
    // `Ordering` carried memory-ordering variants only.
    typecheck_ok(
        "fn main() {\n\
             let a: i32 = 1;\n\
             let b: i32 = 2;\n\
             let r = a.cmp(b);\n\
             let _eq: bool = r == Ordering.Less;\n\
         }",
    );
}

// ── process::exit ──────────────────────────────────────────────

#[test]
fn test_process_exit_resolves() {
    // process::exit is recognized — resolver doesn't error on the path
    let parsed = parse("fn main() { process.exit(0); }");
    assert!(parsed.errors.is_empty());
    let resolved = resolve(&parsed.program);
    assert!(
        resolved.errors.is_empty(),
        "Resolve errors: {:?}",
        resolved.errors
    );
}

// ── Array[T, N] fixed-size arrays ───────────────────────────────

#[test]
fn test_array_type_annotation() {
    typecheck_ok("fn main() { let a: Array[i32, 4] = [1, 2, 3, 4]; }");
}

#[test]
fn test_array_type_element_inferred() {
    typecheck_ok("fn main() { let a: Array[i64, 3] = [1, 2, 3]; }");
}

#[test]
fn test_array_size_in_parameter() {
    typecheck_ok(
        "fn first(a: Array[i32, 4]) -> i32 { a[0] } fn main() { let _x = first([1, 2, 3, 4]); }",
    );
}

#[test]
fn test_array_size_mismatch_is_error() {
    let errors = typecheck_errors("fn main() { let a: Array[i32, 4] = [1, 2, 3]; }");
    assert!(!errors.is_empty(), "Expected size-mismatch error, got none");
}

// ── Vec-default sequence literals ───────────────────────────────

#[test]
fn test_bare_array_literal_infers_vec() {
    // Unannotated `[...]` now defaults to Vec[T], not Array[T, N].
    typecheck_ok("fn main() { let v = [1, 2, 3]; let _x: i64 = v[0]; }");
}

#[test]
fn test_bare_array_literal_for_loop() {
    typecheck_ok(
        "fn main() {
             let v = [10, 20, 30];
             let mut sum = 0;
             for x in v { sum = sum + x; }
         }",
    );
}

#[test]
fn test_array_annotation_coerces_literal() {
    // With an explicit Array annotation, the `[...]` literal is checked as Array.
    typecheck_ok("fn main() { let a: Array[i32, 3] = [1, 2, 3]; }");
}

#[test]
fn test_repeat_literal_bare_infers_vec() {
    // Bare `[v; n]` in synthesis mode → Vec[T].
    typecheck_ok("fn main() { let v = [0; 8]; let _x: i64 = v[0]; }");
}

#[test]
fn test_repeat_literal_array_prefix() {
    // `Array[v; n]` → Array[T, N] with N taken from the literal.
    typecheck_ok("fn main() { let a: Array[i64, 256] = Array[0; 256]; }");
}

#[test]
fn test_repeat_literal_vec_prefix() {
    typecheck_ok("fn main() { let v: Vec[i64] = Vec[42; 10]; let _x: i64 = v[0]; }");
}

#[test]
fn test_repeat_literal_coerces_to_array_via_annotation() {
    // Bare `[v; n]` with an Array annotation coerces — count must equal N.
    typecheck_ok("fn main() { let a: Array[i32, 4] = [0; 4]; }");
}

#[test]
fn test_repeat_literal_array_size_mismatch_errors() {
    let errors = typecheck_errors("fn main() { let a: Array[i32, 4] = [0; 3]; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("does not match") || e.message.contains("count")),
        "expected count-mismatch diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_repeat_literal_array_requires_integer_literal_count() {
    // `Array[v; n]` requires `n` to be a non-negative integer literal —
    // a runtime expression in count is rejected.
    let errors = typecheck_errors(
        "fn main() {
             let n: i64 = 5;
             let a: Array[i32, 5] = Array[0; n];
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("integer literal") || e.message.contains("requires")),
        "expected integer-literal-required diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_repeat_literal_set_rejected() {
    // `Set[v; n]` doesn't make sense (set with N copies of one element); rejected.
    let errors = typecheck_errors("fn main() { let s = Set[0; 10]; }");
    assert!(
        errors.iter().any(|e| e.message.contains("Set")
            && (e.message.contains("not supported") || e.message.contains("only apply"))),
        "expected Set-rejection diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_set_prefix_literal_infers_i64() {
    // `Set[1_i64, 2_i64, 3_i64]` — element type unified from items.
    typecheck_ok("fn main() { let s: Set[i64] = Set[1_i64, 2_i64, 3_i64]; }");
}

#[test]
fn test_set_prefix_literal_infers_string() {
    typecheck_ok(r#"fn main() { let s: Set[String] = Set["alice", "bob"]; }"#);
}

#[test]
fn test_set_prefix_literal_mismatched_elements_rejected() {
    // First item types T; subsequent items must be assignable to T.
    let errors = typecheck_errors(r#"fn main() { let s: Set[i64] = Set[1_i64, "not an int"]; }"#);
    assert!(
        !errors.is_empty(),
        "expected element-type mismatch diagnostic for heterogeneous Set literal, got no errors"
    );
}

#[test]
fn test_set_prefix_literal_empty_with_annotation() {
    // Empty `Set[]` requires a type annotation to recover the element type.
    typecheck_ok("fn main() { let s: Set[i64] = Set[]; }");
}

// ── E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION ────────────────────────
//
// Per design.md § Collection Literals — an empty prefix-literal has no
// element type to infer. Synthesis-mode use (no enclosing annotation)
// emits a focused diagnostic; check-mode (annotated bindings, typed
// call args, typed struct-field initializers) still recovers via the
// expected type.

fn assert_one_typecheck_error_containing(source: &str, substrings: &[&str]) {
    let errors = typecheck_errors(source);
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one type error for source `{source}`; got {errors:#?}"
    );
    let msg = &errors[0].message;
    for s in substrings {
        assert!(
            msg.contains(s),
            "expected message to contain `{s}`; got `{msg}`"
        );
    }
}

#[test]
fn empty_vec_literal_without_annotation_emits_focused_diagnostic() {
    assert_one_typecheck_error_containing(
        "fn main() { let v = Vec[]; }",
        &[
            "E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION",
            "empty `Vec[]` literal",
            "let v: Vec[T] = Vec[]",
            "Vec.new()",
        ],
    );
}

#[test]
fn empty_set_literal_without_annotation_emits_focused_diagnostic() {
    assert_one_typecheck_error_containing(
        "fn main() { let s = Set[]; }",
        &[
            "E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION",
            "empty `Set[]` literal",
            "Set[T]",
            "Set.new()",
        ],
    );
}

#[test]
fn empty_map_literal_without_annotation_emits_focused_diagnostic() {
    assert_one_typecheck_error_containing(
        "fn main() { let m = Map[]; }",
        &[
            "E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION",
            "empty `Map[]` literal",
            "Map[K, V]",
            "Map.new()",
        ],
    );
}

#[test]
fn empty_array_literal_without_annotation_emits_focused_diagnostic() {
    let errors = typecheck_errors("fn main() { let a = Array[]; }");
    let any_match = errors.iter().any(|e| {
        e.message
            .contains("E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION")
            && e.message.contains("empty `Array[]` literal")
            && e.message.contains("Array[T, 0]")
    });
    assert!(
        any_match,
        "expected focused empty-array diagnostic, got: {errors:#?}"
    );
}

#[test]
fn empty_vec_literal_with_annotation_keeps_passing() {
    typecheck_ok("fn main() { let v: Vec[i64] = Vec[]; }");
}

#[test]
fn empty_map_literal_with_annotation_keeps_passing() {
    typecheck_ok("fn main() { let m: Map[i32, i32] = Map[]; }");
}

#[test]
fn empty_array_literal_with_annotation_keeps_passing() {
    typecheck_ok("fn main() { let a: Array[i64, 0] = Array[]; }");
}

#[test]
fn empty_vec_literal_at_typed_call_arg_keeps_passing() {
    typecheck_ok("fn take(v: Vec[i64]) -> i64 { 0 } fn main() { let _ = take(Vec[]); }");
}

#[test]
fn empty_set_literal_in_typed_struct_field_keeps_passing() {
    typecheck_ok(
        "struct Bag { items: Set[i64] } \
         fn main() { let b = Bag { items: Set[] }; }",
    );
}

// ── Trait alias use-site stub diagnostic ────────────────────────────
//
// Per design.md § Trait Aliases / phase-8 checklist: declarations parse,
// resolver registers the name, but every use site (bound / where-clause /
// future dyn position) emits `E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET` at v1.

#[test]
fn trait_alias_in_inline_bound_emits_v1_stub() {
    let errors = typecheck_errors(
        "trait Numeric = Copy + Clone; \
         fn need_copy[T: Numeric](x: T) -> T { x }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET")
                && e.message.contains("Numeric")
                && e.message.contains("Copy + Clone")),
        "expected v1 stub diagnostic with bound list, got: {errors:?}"
    );
}

#[test]
fn trait_alias_in_where_clause_emits_v1_stub() {
    let errors = typecheck_errors(
        "trait Numeric = Copy + Clone; \
         fn need_copy[T](x: T) -> T where T: Numeric { x }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET")
                && e.message.contains("Numeric")),
        "expected v1 stub diagnostic, got: {errors:?}"
    );
}

#[test]
fn impl_trait_alias_rejected() {
    let errors = typecheck_errors(
        "trait Numeric = Copy + Clone; \
         struct Foo { x: i64 } \
         impl Numeric for Foo { }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_IMPL_TRAIT_ALIAS")
                && e.message.contains("Numeric")
                && e.message.contains("Copy + Clone")),
        "expected E_IMPL_TRAIT_ALIAS with bound list, got: {errors:?}"
    );
}

#[test]
fn trait_alias_declaration_alone_typechecks() {
    // A declared but unused trait alias must not emit the v1 stub
    // diagnostic — only use sites do.
    typecheck_ok("trait Numeric = Copy + Clone;");
}

// ── Try block v1 stub diagnostic ────────────────────────────────────

// ── Marker trait impl-body rejection ────────────────────────────────

#[test]
fn impl_marker_trait_with_method_rejected() {
    let errors = typecheck_errors(
        "marker trait Pod; \
         struct Foo { x: i64 } \
         impl Pod for Foo { fn extra(self) { } }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_MARKER_IMPL_HAS_METHOD") && e.message.contains("Pod")),
        "expected E_MARKER_IMPL_HAS_METHOD, got: {errors:?}"
    );
}

#[test]
fn impl_marker_trait_empty_body_accepted() {
    typecheck_ok(
        "marker trait Pod; \
         struct Foo { x: i64 } \
         impl Pod for Foo { }",
    );
}

// ── Cast-pair rejections (slice 1 of saturating-float→int) ──────────

#[test]
fn cast_char_as_narrow_int_rejected() {
    let errors = typecheck_errors("fn main() { let _ = 'A' as u8; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_CHAR_AS_NARROW_INT")),
        "expected E_CHAR_AS_NARROW_INT, got: {errors:?}"
    );
}

#[test]
fn cast_char_as_i16_rejected() {
    let errors = typecheck_errors("fn main() { let _ = 'A' as i16; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_CHAR_AS_NARROW_INT")),
        "expected E_CHAR_AS_NARROW_INT, got: {errors:?}"
    );
}

#[test]
fn cast_char_as_u32_accepted() {
    typecheck_ok("fn main() { let _ = 'A' as u32; }");
}

#[test]
fn cast_char_as_i32_accepted() {
    typecheck_ok("fn main() { let _ = 'A' as i32; }");
}

#[test]
fn cast_int_as_char_rejected() {
    let errors = typecheck_errors("fn main() { let _ = 65u32 as char; }");
    assert!(
        errors.iter().any(|e| e.message.contains("E_INT_AS_CHAR")),
        "expected E_INT_AS_CHAR, got: {errors:?}"
    );
}

#[test]
fn cast_int_as_bool_rejected() {
    let errors = typecheck_errors("fn main() { let _ = 1i32 as bool; }");
    assert!(
        errors.iter().any(|e| e.message.contains("E_INT_AS_BOOL")),
        "expected E_INT_AS_BOOL, got: {errors:?}"
    );
}

#[test]
fn cast_float_as_bool_rejected() {
    let errors = typecheck_errors("fn main() { let _ = 1.0f64 as bool; }");
    assert!(
        errors.iter().any(|e| e.message.contains("E_FLOAT_AS_BOOL")),
        "expected E_FLOAT_AS_BOOL, got: {errors:?}"
    );
}

#[test]
fn cast_bool_as_int_accepted() {
    typecheck_ok("fn main() { let _ = false as u8; let _ = true as u32; }");
}

#[test]
fn cast_int_as_int_still_works() {
    typecheck_ok("fn main() { let _ = 300i32 as i8; let _ = (-1i8) as u8; }");
}

// ── @ binding semantics — typechecker coverage ──────────────────────

#[test]
fn at_binding_inherits_scrutinee_type() {
    // The @-bound name takes the type of the scrutinee at its position.
    typecheck_ok(
        "fn classify(n: i32) -> i32 { \
         match n { code @ 500..=599 => code, _ => 0 } \
         }",
    );
}

#[test]
fn at_binding_let_with_refutable_inner_rejected_at_typecheck() {
    // `let x @ Option.Some(y) = opt;` — the inner pattern is refutable
    // (Option could be None); the whole binding is refutable; the
    // existing irrefutable-pattern check fires.
    let errors = typecheck_errors(
        "fn main() { \
         let opt: Option[i32] = Option.None; \
         let x @ Option.Some(y) = opt; \
         }",
    );
    assert!(
        !errors.is_empty(),
        "expected refutable-pattern type error, got none"
    );
}

#[test]
fn at_binding_irrefutable_struct_pattern_in_let_accepted() {
    typecheck_ok(
        "struct Foo { a: i32 } \
         fn main() { let foo = Foo { a: 1 }; let outer @ Foo { a } = foo; \
         let _ = outer; let _ = a; }",
    );
}

#[test]
fn refutable_let_with_enum_variant_rejected() {
    let errors = typecheck_errors(
        "fn main() { let opt: Option[i32] = Option.None; let Option.Some(x) = opt; }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("refutable pattern")),
        "expected refutable-pattern error, got: {errors:?}"
    );
}

#[test]
fn refutable_let_with_literal_pattern_rejected() {
    let errors = typecheck_errors("fn main() { let n: i32 = 1; let 1 = n; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("refutable pattern")),
        "expected refutable-pattern error, got: {errors:?}"
    );
}

#[test]
fn match_with_full_range_pattern_set_typechecks() {
    // Five-form range pattern coverage: `..lo`, `lo..hi`, `lo..=hi`,
    // `lo..`, `..=hi`. Plus bare `_` for non-exhaustive recovery.
    typecheck_ok(
        "fn classify(n: i32) -> i32 { \
         match n { \
         ..0 => -1, \
         0..=9 => 1, \
         10..100 => 2, \
         100.. => 3, \
         _ => 0, \
         } \
         }",
    );
}

#[test]
fn cast_float_as_int_still_works() {
    typecheck_ok("fn main() { let _ = 3.7f64 as i32; let _ = 1.5f32 as u8; }");
}

#[test]
fn marker_trait_used_as_bound_works() {
    // Marker traits participate in bound resolution like ordinary traits.
    typecheck_ok(
        "marker trait Pod; \
         struct Foo { x: i64 } \
         impl Pod for Foo { } \
         fn need_pod[T: Pod](x: T) -> T { x }",
    );
}

#[test]
fn try_block_emits_v1_stub_diagnostic() {
    let errors = typecheck_errors("fn main() { let _ = try { 42 }; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_TRY_BLOCK_NOT_IMPLEMENTED_YET")
                && e.message.contains("extract the body into a helper")),
        "expected v1 stub, got: {errors:?}"
    );
}

// ── Built-in Display for collections ────────────────────────────────
//
// Regression: `f"{my_vec}"` and friends must accept `Vec[T]`, `Map[K, V]`,
// `Set[T]`, `Option[T]`, `Result[T, E]`, and arbitrary nesting when the
// leaf element types support Display. Verified via `type_supports_display`
// in the typechecker. Discovered while writing LeetCode group anagrams.

#[test]
fn fstring_accepts_vec_of_display_type() {
    typecheck_ok("fn main() { let v: Vec[i64] = Vec[]; let s = f\"{v}\"; }");
}

#[test]
fn fstring_accepts_nested_vec_of_display_type() {
    typecheck_ok("fn main() { let v: Vec[Vec[String]] = Vec[]; let s = f\"{v}\"; }");
}

#[test]
fn fstring_accepts_map_of_display_types() {
    typecheck_ok("fn main() { let m: Map[String, Vec[i32]] = Map[]; let s = f\"{m}\"; }");
}

#[test]
fn fstring_accepts_set_of_display_type() {
    typecheck_ok("fn main() { let s: Set[i64] = Set[]; let _ = f\"{s}\"; }");
}

#[test]
fn fstring_accepts_option_and_result_of_display_types() {
    typecheck_ok(
        "fn main() { \
         let o: Option[i64] = Option.Some(1); \
         let r: Result[i64, String] = Result.Ok(1); \
         let _ = f\"{o}\"; \
         let _ = f\"{r}\"; \
         }",
    );
}

#[test]
fn try_block_still_walks_body_for_inner_errors() {
    // The stub walks the body so unrelated errors inside still surface.
    // Here the body refers to an undefined identifier — we expect the
    // parse/resolve error PLUS the v1 stub. (Resolver would have flagged
    // `undefined_thing`, but resolver errors abort before the typechecker
    // sees the body — adapt the test to a typecheck-time inner error.)
    let errors = typecheck_errors("fn main() { let _ = try { 1 + true }; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("E_TRY_BLOCK_NOT_IMPLEMENTED_YET")),
        "expected v1 stub diagnostic, got: {errors:?}"
    );
    // The inner `1 + true` is also a type error.
    assert!(
        errors.len() >= 2,
        "expected both stub and inner error, got: {errors:?}"
    );
}

#[test]
fn test_repeat_literal_count_must_be_integer() {
    // Non-integer count emits a clear diagnostic.
    let errors = typecheck_errors("fn main() { let v = [0; \"five\"]; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("count must be an integer")),
        "expected integer-count diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_array_annotation_size_mismatch_still_errors() {
    let errors = typecheck_errors("fn main() { let a: Array[i32, 4] = [1, 2, 3]; }");
    assert!(!errors.is_empty(), "Expected size-mismatch error, got none");
}

#[test]
fn test_prefix_vec_literal() {
    // `Vec[e1, e2, ...]` is the explicit Vec prefix form.
    typecheck_ok("fn main() { let v = Vec[1, 2, 3]; let _x: i64 = v[0]; }");
}

#[test]
fn test_prefix_array_literal() {
    // `Array[e1, e2, e3]` produces Array[T, N] with N inferred from item count.
    typecheck_ok(
        "fn first(a: Array[i32, 3]) -> i32 { a[0] }
         fn main() { let _x = first(Array[10, 20, 30]); }",
    );
}

#[test]
fn test_prefix_vec_literal_passed_to_slice_param() {
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn main() { let _n = sum(Vec[1, 2, 3]); }",
    );
}

// ── Slice[T] borrowed-sequence views ────────────────────────────

#[test]
fn test_slice_type_annotation_in_parameter() {
    // Read-only Slice[T] parameter — will gain coercion from Vec/Array in a later step.
    // For now, just ensure the type parses, resolves, and lowers correctly.
    typecheck_ok(
        "fn first(xs: Slice[i64]) -> i64 { xs[0] }
         fn main() { }",
    );
}

#[test]
fn test_slice_element_indexing() {
    // Indexing a Slice[T] with an integer yields T.
    typecheck_ok(
        "fn head(xs: Slice[i32]) -> i32 { xs[0] }
         fn main() { }",
    );
}

#[test]
fn test_slice_for_loop_iteration() {
    // `for x in slice` iterates over the element type.
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 {
             let mut acc = 0;
             for x in xs { acc = acc + x; }
             acc
         }
         fn main() { }",
    );
}

#[test]
fn test_slice_type_generic_argument() {
    // Slice[T] inside a larger type position.
    typecheck_ok(
        "fn make() -> Option[Slice[i32]] { None }
         fn main() { }",
    );
}

#[test]
fn test_vec_coerces_to_slice_at_call_boundary() {
    // A Vec[T] value passed where Slice[T] is expected — the call-boundary
    // coercion inserts a slice view into the Vec's buffer.
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             let _n = sum(v);
         }",
    );
}

#[test]
fn test_array_coerces_to_slice_at_call_boundary() {
    typecheck_ok(
        "fn first(xs: Slice[i64]) -> i64 { xs[0] }
         fn main() {
             let a: Array[i64, 3] = [10, 20, 30];
             let _n = first(a);
         }",
    );
}

#[test]
fn test_ref_vec_parameter_coerces_to_slice() {
    // Function whose parameter is already `ref Vec[i64]` (a borrow) —
    // passes through to a downstream function that takes `Slice[i64]`.
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn first_of(v: ref Vec[i64]) -> i64 { sum(v) }
         fn main() { }",
    );
}

#[test]
fn test_ref_array_parameter_coerces_to_slice() {
    typecheck_ok(
        "fn first(xs: Slice[i64]) -> i64 { xs[0] }
         fn head(a: ref Array[i64, 3]) -> i64 { first(a) }
         fn main() { }",
    );
}

#[test]
fn test_range_indexing_on_array_yields_slice() {
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn main() {
             let a: Array[i64, 5] = [10, 20, 30, 40, 50];
             let _n = sum(a[1..3]);
         }",
    );
}

#[test]
fn test_range_indexing_on_vec_yields_slice() {
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             let _n = sum(v[0..1]);
         }",
    );
}

#[test]
fn test_range_indexing_on_slice_yields_slice() {
    // A slice can be re-sliced.
    typecheck_ok(
        "fn middle(xs: Slice[i64]) -> Slice[i64] { xs[1..3] }
         fn main() { }",
    );
}

#[test]
fn test_range_indexing_on_ref_array() {
    typecheck_ok(
        "fn first_two(a: ref Array[i64, 4]) -> Slice[i64] { a[0..2] }
         fn main() { }",
    );
}

#[test]
fn test_range_indexing_on_non_sequence_errors() {
    // Range indexing on a non-indexable type (e.g., bool) is a type error.
    let errors = typecheck_errors(
        "fn main() {
             let b: bool = true;
             let _x = b[0..1];
         }",
    );
    assert!(
        !errors.is_empty(),
        "Expected type error for range-indexing a bool, got none"
    );
}

#[test]
fn test_as_slice_on_vec_returns_slice() {
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn main() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
             let s = v.as_slice();
             let _n = sum(s);
         }",
    );
}

#[test]
fn test_as_slice_on_array_returns_slice() {
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn main() {
             let a: Array[i64, 3] = [1, 2, 3];
             let s: Slice[i64] = a.as_slice();
             let _n = sum(s);
         }",
    );
}

#[test]
fn test_mut_slice_parameter_parses_and_lowers() {
    // `mut Slice[T]` in a function parameter parses, resolves, and lowers.
    typecheck_ok(
        "fn clear(xs: mut Slice[i64]) { }
         fn main() { }",
    );
}

#[test]
fn test_mut_slice_accepts_ref_mut_vec() {
    typecheck_ok(
        "fn clear(xs: mut Slice[i64]) { }
         fn caller(v: mut ref Vec[i64]) { clear(v) }
         fn main() { }",
    );
}

#[test]
fn test_mut_slice_accepts_ref_mut_array() {
    typecheck_ok(
        "fn clear(xs: mut Slice[i64]) { }
         fn caller(a: mut ref Array[i64, 4]) { clear(a) }
         fn main() { }",
    );
}

#[test]
fn test_mut_slice_rejects_read_only_source() {
    // A `ref Vec[T]` (read-only borrow) cannot fill a `mut Slice[T]` slot.
    let errors = typecheck_errors(
        "fn clear(xs: mut Slice[i64]) { }
         fn caller(v: ref Vec[i64]) { clear(v) }
         fn main() { }",
    );
    assert!(
        !errors.is_empty(),
        "Expected type error passing ref Vec to mut Slice, got none"
    );
}

#[test]
fn test_slice_accepts_mut_slice_as_reborrow() {
    // A `mut Slice[T]` can be passed where a read-only `Slice[T]` is expected —
    // the mutable source reborrows as read-only.
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn caller(xs: mut Slice[i64]) -> i64 { sum(xs) }
         fn main() { }",
    );
}

#[test]
fn test_slice_rejects_incompatible_source_type() {
    // A bool value should not coerce to Slice[i64].
    let errors = typecheck_errors(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn main() {
             let b: bool = true;
             let _n = sum(b);
         }",
    );
    assert!(
        !errors.is_empty(),
        "Expected type error for bool → Slice[i64], got none"
    );
}

// ── From trait dispatch (Step 4) ──────────────────────────────

#[test]
fn test_from_numeric_widening_signed() {
    typecheck_ok(
        "fn main() {
             let x: i32 = 5;
             let y: i64 = i64.from(x);
         }",
    );
}

#[test]
fn test_from_numeric_widening_unsigned_to_signed() {
    typecheck_ok(
        "fn main() {
             let x: u8 = 5;
             let y: i32 = i32.from(x);
         }",
    );
}

#[test]
fn test_from_float_widening() {
    typecheck_ok(
        "fn main() {
             let x: f32 = 1.5;
             let y: f64 = f64.from(x);
         }",
    );
}

#[test]
fn test_from_narrowing_rejected() {
    // i64 → i32 is not in the lossless widening table; should error.
    let errors = typecheck_errors(
        "fn main() {
             let x: i64 = 5;
             let y: i32 = i32.from(x);
         }",
    );
    assert!(
        errors.iter().any(|e| e.message.contains("no `impl From")),
        "expected From-not-found diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_into_missing_from_impl_diagnoses() {
    // `.into()` with an expected target that has no `impl From[S] for T`
    // emits a targeted diagnostic.
    let errors = typecheck_errors(
        "struct Foo { n: i64 }
         fn main() {
             let x: i32 = 42;
             let y: Foo = x.into();
         }",
    );
    assert!(
        errors.iter().any(|e| e.message.contains("no `impl From")),
        "expected 'no impl From' diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_into_numeric_widening() {
    typecheck_ok(
        "fn main() {
             let x: i32 = 42;
             let y: i64 = x.into();
         }",
    );
}

#[test]
fn test_eq_ord_methods_directly_callable() {
    // `.ne`/`.lt`/`.le`/`.gt`/`.ge` now registered as Eq/Ord methods,
    // so user code can call them explicitly with the same shape as the
    // operator-lowered form.
    typecheck_ok(
        "fn main() {
             let a: i32 = 1;
             let b: i32 = 2;
             let _p: bool = i32.eq(a, b);
             let _q: bool = i32.ne(a, b);
             let _r: bool = i32.lt(a, b);
             let _s: bool = i32.le(a, b);
             let _t: bool = i32.gt(a, b);
             let _u: bool = i32.ge(a, b);
         }",
    );
}

#[test]
fn test_from_user_impl_dispatch() {
    // User-defined From impl resolves through the same dispatch path.
    typecheck_ok(
        "struct ParseError { msg: String }
         struct AppError { msg: String }
         impl From for AppError {
             fn from(e: ParseError) -> AppError { AppError { msg: e.msg } }
         }
         fn main() {
             let p: ParseError = ParseError { msg: \"bad\" };
             let a: AppError = AppError.from(p);
         }",
    );
}

// ── `.try_into()` desugar (round 7) ──────────────────────────────

#[test]
fn test_try_into_happy_path() {
    // `let y: Result[Target, E] = x.try_into()` resolves when an
    // `impl TryFrom for Target` matching the source type is in scope.
    typecheck_ok(
        "struct Raw { n: i64 }
         struct Validated { n: i64 }
         impl TryFrom for Validated {
             type Error = String;
             fn try_from(r: Raw) -> Result[Validated, String] {
                 Result.Ok(Validated { n: r.n })
             }
         }
         fn main() {
             let r: Raw = Raw { n: 42 };
             let v: Result[Validated, String] = r.try_into();
         }",
    );
}

#[test]
fn test_try_into_missing_tryfrom_impl_diagnoses() {
    // `.try_into()` with an expected `Result[Target, _]` and no matching
    // `impl TryFrom[Source] for Target` emits a targeted diagnostic.
    let errors = typecheck_errors(
        "struct Raw { n: i64 }
         struct Validated { n: i64 }
         fn main() {
             let r: Raw = Raw { n: 42 };
             let v: Result[Validated, String] = r.try_into();
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("no `impl TryFrom")),
        "expected 'no impl TryFrom' diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_try_into_multi_impl_disambiguates_by_source() {
    // Two `impl TryFrom for Target` blocks (one per source type). Each
    // `.try_into()` call site picks the right impl by source type — same
    // disambiguation rule as `find_from_impl`.
    typecheck_ok(
        "struct A { n: i64 }
         struct B { s: String }
         struct Out { n: i64 }
         impl TryFrom for Out {
             type Error = String;
             fn try_from(a: A) -> Result[Out, String] { Result.Ok(Out { n: a.n }) }
         }
         impl TryFrom for Out {
             type Error = String;
             fn try_from(b: B) -> Result[Out, String] { Result.Ok(Out { n: 0 }) }
         }
         fn main() {
             let a: A = A { n: 1 };
             let b: B = B { s: \"x\" };
             let oa: Result[Out, String] = a.try_into();
             let ob: Result[Out, String] = b.try_into();
         }",
    );
}

#[test]
fn test_try_into_non_result_expected_does_not_fire_recognizer() {
    // When the expected type isn't `Result[_, _]`, the recognizer must not
    // populate `try_into_conversions` — that side-table drives lowering, and
    // a spurious entry would rewrite a non-`.try_into()` site. (Unknown-
    // method calls are silently accepted by method-call inference today; that
    // language-quality issue is out of scope for this round. The assertion
    // below targets only the recognizer's own behavior.)
    let result = typecheck_ok(
        "struct Raw { n: i64 }
         struct Validated { n: i64 }
         impl TryFrom for Validated {
             type Error = String;
             fn try_from(r: Raw) -> Result[Validated, String] {
                 Result.Ok(Validated { n: r.n })
             }
         }
         fn use_result(r: Raw) -> Result[Validated, String] { r.try_into() }",
    );
    // Only one `.try_into()` call site exists, and its expected type is
    // `Result[Validated, String]`, so exactly one entry is expected.
    assert_eq!(
        result.try_into_conversions.len(),
        1,
        "expected one try_into rewrite entry, got: {:?}",
        result.try_into_conversions
    );
}

#[test]
fn test_try_into_source_type_mismatch_diagnoses() {
    // TryFrom impl exists for a different source type than the receiver.
    // The recognizer fires (expected is Result[Target, _]) but
    // `find_tryfrom_impl` returns None — the missing-impl diagnostic must
    // name the actual source type.
    let errors = typecheck_errors(
        "struct A { n: i64 }
         struct Other { s: String }
         struct Out { n: i64 }
         impl TryFrom for Out {
             type Error = String;
             fn try_from(a: A) -> Result[Out, String] { Result.Ok(Out { n: a.n }) }
         }
         fn main() {
             let o: Other = Other { s: \"x\" };
             let v: Result[Out, String] = o.try_into();
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("no `impl TryFrom") && e.message.contains("Other")),
        "expected diagnostic naming the source type 'Other', got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

// ── Call-site Mutation Markers (1A — design.md Part 1½) ──────────
//
// These tests exercise the call-site rule using `mut Slice[T]` as the
// mutating parameter form, since Vec/Array auto-coerce to Slice at call
// boundaries and `ref T` / `mut ref T` on primitives would require
// explicit borrows which the typechecker does not auto-insert.

#[test]
fn test_call_site_mut_marker_required_on_fresh_binding() {
    // Fresh owned binding passed to `mut Slice[T]` parameter must carry `mut`.
    let errors = typecheck_errors(
        "fn sort(xs: mut Slice[i64]) { }
         fn main() { let mut v: Array[i64, 4] = [3, 1, 4, 1]; sort(v); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::MissingMutMarker),
        "expected MissingMutMarker, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_call_site_mut_marker_accepted_on_fresh_binding() {
    typecheck_ok(
        "fn sort(xs: mut Slice[i64]) { }
         fn main() { let mut v: Array[i64, 4] = [3, 1, 4, 1]; sort(mut v); }",
    );
}

#[test]
fn test_call_site_array_to_mut_slice_requires_marker() {
    // Array → mut Slice coerces at call boundaries now; fresh binding still
    // requires the `mut` marker.
    let errors = typecheck_errors(
        "fn sort(xs: mut Slice[i64]) { }
         fn main() { let mut v: Array[i64, 4] = [3, 1, 4, 1]; sort(v); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::MissingMutMarker),
        "expected MissingMutMarker, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_call_site_mut_marker_rejected_on_owned_param() {
    // `mut` marker is not legal on an owned parameter.
    let errors = typecheck_errors(
        "fn take(xs: Array[i64, 4]) { }
         fn main() { let mut v: Array[i64, 4] = [1, 2, 3, 4]; take(mut v); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::InvalidMutMarker),
        "expected InvalidMutMarker, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_call_site_mut_marker_rejected_on_ref_slice_param() {
    // `mut` marker is not legal on a `Slice[T]` (read-only) parameter.
    let errors = typecheck_errors(
        "fn read(xs: Slice[i64]) { }
         fn main() { let mut v: Array[i64, 4] = [1, 2, 3, 4]; read(mut v); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::InvalidMutMarker),
        "expected InvalidMutMarker on read-only slice, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_call_site_forwarded_mut_slice_no_marker() {
    // Forwarding a `mut Slice[T]` binding through is unmarked — the mutation
    // surface was announced at the enclosing function's signature.
    typecheck_ok(
        "fn inner(xs: mut Slice[i64]) { }
         fn outer(ys: mut Slice[i64]) { inner(ys); }
         fn main() { }",
    );
}

#[test]
fn test_call_site_forwarded_mut_slice_marker_rejected() {
    // Marking a forwarded mut-slice is wrong — it's already a mut-ref.
    let errors = typecheck_errors(
        "fn inner(xs: mut Slice[i64]) { }
         fn outer(ys: mut Slice[i64]) { inner(mut ys); }
         fn main() { }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::InvalidMutMarker),
        "expected InvalidMutMarker on forwarded mut-slice, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_call_site_no_marker_ok_for_owned_param() {
    // Plain call to owned param: no marker.
    typecheck_ok(
        "fn take(xs: Array[i64, 4]) { }
         fn main() { let v: Array[i64, 4] = [1, 2, 3, 4]; take(v); }",
    );
}

#[test]
fn test_call_site_no_marker_ok_for_ref_slice_param() {
    // Plain call to read-only Slice param: no marker. Array → Slice coerces.
    typecheck_ok(
        "fn read(xs: Slice[i64]) { }
         fn main() { let v: Array[i64, 4] = [1, 2, 3, 4]; read(v); }",
    );
}

// ── Recursive derived-trait validation (CR-19) ──────────────────

#[test]
fn test_derive_eq_rejects_float_field() {
    let errors = typecheck_errors(
        "#[derive(Eq)]
         struct Bad { n: i64, f: f64 }",
    );
    assert!(
        errors.iter().any(|e| e.message.contains("derives Eq")
            && e.message.contains("'f'")
            && e.message.contains("f64")),
        "expected Eq/f64 diagnostic, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_derive_hash_rejects_float_field() {
    let errors = typecheck_errors(
        "#[derive(Hash)]
         struct Bad { f: f32 }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("derives Hash") && e.message.contains("f32")));
}

#[test]
fn test_derive_ord_rejects_float_field() {
    let errors = typecheck_errors(
        "#[derive(Ord)]
         struct Bad { f: f64 }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("derives Ord") && e.message.contains("f64")));
}

#[test]
fn test_derive_partial_eq_accepts_float_field() {
    // PartialEq admits floats — NaN != NaN is fine for partial equality.
    typecheck_ok(
        "#[derive(PartialEq)]
         struct Ok { a: f64 }",
    );
}

#[test]
fn test_derive_partial_ord_accepts_float_field() {
    typecheck_ok(
        "#[derive(PartialOrd)]
         struct Ok { a: f32 }",
    );
}

#[test]
fn test_derive_eq_rejects_recursive_non_eq_field() {
    // Inner lacks #[derive(Eq)], so Outer can't derive Eq through it.
    let errors = typecheck_errors(
        "struct Inner { f: i64 }
         #[derive(Eq)]
         struct Outer { inner: Inner }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("derives Eq") && e.message.contains("Inner")),
        "expected recursive Eq failure on Inner-typed field, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_derive_eq_accepts_recursive_eq_field() {
    typecheck_ok(
        "#[derive(Eq)]
         struct Inner { n: i64 }
         #[derive(Eq)]
         struct Outer { inner: Inner }",
    );
}

#[test]
fn test_derive_hash_rejects_enum_variant_float() {
    let errors = typecheck_errors(
        "#[derive(Hash)]
         enum Status { Active { id: i64 }, Disabled(f64) }",
    );
    assert!(errors
        .iter()
        .any(|e| e.message.contains("derives Hash") && e.message.contains("Disabled")));
}

#[test]
fn test_derive_eq_accepts_tuple_of_ints() {
    typecheck_ok(
        "#[derive(Eq)]
         struct Ok { xy: (i64, i64) }",
    );
}

// ── Supertrait parsing + enforcement ────────────────────────────

#[test]
fn test_supertrait_parsing_accepted() {
    typecheck_ok(
        "trait Base { fn base_method(ref self); }\n\
         trait Derived: Base { fn derived_method(ref self); }",
    );
}

#[test]
fn test_supertrait_multiple_accepted() {
    typecheck_ok(
        "trait A { fn a(ref self); }\n\
         trait B { fn b(ref self); }\n\
         trait C: A + B { fn c(ref self); }",
    );
}

#[test]
fn test_supertrait_impl_with_required_supertrait_ok() {
    typecheck_ok(
        "trait Base { fn base_method(ref self); }\n\
         trait Derived: Base { fn derived_method(ref self); }\n\
         struct MyType { x: i64 }\n\
         impl Base for MyType { fn base_method(ref self) { } }\n\
         impl Derived for MyType { fn derived_method(ref self) { } }",
    );
}

#[test]
fn test_supertrait_impl_missing_supertrait_rejected() {
    let errors = typecheck_errors(
        "trait Base { fn base_method(ref self); }\n\
         trait Derived: Base { fn derived_method(ref self); }\n\
         struct MyType { x: i64 }\n\
         impl Derived for MyType { fn derived_method(ref self) { } }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("requires impl Base")),
        "expected MissingSupertrait error, got {:?}",
        errors
    );
}

// ── Trait default methods (CR-33) ───────────────────────────────

#[test]
fn test_trait_default_method_self_method_call() {
    typecheck_ok(
        "trait Counter {
             fn count(ref self) -> i64;
             fn twice(ref self) -> i64 {
                 self.count() + self.count()
             }
         }",
    );
}

#[test]
fn test_trait_default_method_body_is_type_checked() {
    // Regression: default method body used to skip type checking entirely.
    let errors = typecheck_errors(
        "trait Counter {
             fn count(ref self) -> i64 {
                 let s: String = 42;
                 0
             }
         }",
    );
    assert!(
        errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch for `let s: String = 42` in trait default body, got: {:?}",
        errors.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_trait_self_type_in_return_position() {
    typecheck_ok("trait Default { fn make() -> Self; }");
}

#[test]
fn test_trait_self_type_in_default_body_return() {
    // `Self` in a default method body behaves like a type parameter —
    // consistent with Self in the signature.
    typecheck_ok(
        "trait Identity {
             fn me(self) -> Self {
                 self
             }
         }",
    );
}

#[test]
fn test_trait_with_generic_and_self_default_body() {
    typecheck_ok(
        "trait Container[T] {
             fn value(ref self) -> T;
             fn echo(ref self) -> T {
                 self.value()
             }
         }",
    );
}

// ── Method resolution: inherent-beats-trait priority ────────────
//
// Per design.md § Method Resolution Step 3, inherent methods have
// priority over trait methods on the same receiver candidate.

#[test]
fn test_method_resolution_inherent_wins_over_trait() {
    // Both impls declare `m`; the call must resolve to the inherent one
    // (return type `i64`), not the trait one (return type `bool`). If the
    // trait method were chosen, `let r: i64 = s.m()` would fail to typecheck.
    typecheck_ok(
        "struct S { x: i64 }\n\
         trait Foo { fn m(self) -> bool; }\n\
         impl S { fn m(self) -> i64 { 1 } }\n\
         impl Foo for S { fn m(self) -> bool { true } }\n\
         fn main() { let s = S { x: 0 }; let r: i64 = s.m(); }",
    );
}

#[test]
fn test_method_resolution_trait_method_when_no_inherent() {
    // Regression: when only a trait impl declares `m`, the call resolves
    // through the trait method (preserving pre-priority behavior).
    typecheck_ok(
        "struct S { x: i64 }\n\
         trait Foo { fn m(self) -> i64; }\n\
         impl Foo for S { fn m(self) -> i64 { 42 } }\n\
         fn main() { let s = S { x: 0 }; let r: i64 = s.m(); }",
    );
}

#[test]
fn test_method_resolution_type_prefixed_inherent_wins_over_trait() {
    // Type-prefixed `T.method(args)` form (parsed as MethodCall with
    // object = Identifier("T")) should also respect inherent priority.
    // `S.make()` resolves to the inherent constructor (returning S),
    // not the trait associated function (returning bool).
    typecheck_ok(
        "struct S { x: i64 }\n\
         trait Make { fn make() -> bool; }\n\
         impl S { fn make() -> S { S { x: 0 } } }\n\
         impl Make for S { fn make() -> bool { true } }\n\
         fn main() { let r: S = S.make(); }",
    );
}

// ── Method resolution: autoref through ref / mut ref ────────────
//
// Per design.md § Method Resolution Step 1, the receiver candidate list
// is `[T, ref T, mut ref T, ...]` — autoref candidates collapse to the
// same name lookup, so `r.method()` on a `ref T` receiver resolves to
// methods declared on `T`.

#[test]
fn test_method_resolution_through_ref_returns_real_type() {
    // Without autoref, `r.get()` returned `Type::Error` silently and the
    // type mismatch on `want_string(...)` would be missed. With autoref,
    // it returns `i64`, which fails to match the `String` parameter type.
    // `r` enters with type `ref S` because `read` declares it that way.
    let errors = typecheck_errors(
        "struct S { x: i64 }\n\
         impl S { fn get(ref self) -> i64 { self.x } }\n\
         fn want_string(s: String) {}\n\
         fn read(r: ref S) { want_string(r.get()); }",
    );
    assert!(
        errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch
            && e.message.contains("expected 'String'")
            && e.message.contains("found 'i64'")),
        "expected TypeMismatch for String/i64 from r.get() through ref, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_method_resolution_through_mut_ref_returns_real_type() {
    let errors = typecheck_errors(
        "struct S { x: i64 }\n\
         impl S { fn get(ref self) -> i64 { self.x } }\n\
         fn want_string(s: String) {}\n\
         fn read(r: mut ref S) { want_string(r.get()); }",
    );
    assert!(
        errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch
            && e.message.contains("expected 'String'")
            && e.message.contains("found 'i64'")),
        "expected TypeMismatch for String/i64 from r.get() through mut ref, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_method_resolution_no_method_diagnostic_fires_through_ref() {
    // The `no method named` diagnostic now reaches through `ref T`.
    let errors = typecheck_errors(
        "struct S { x: i64 }\n\
         impl S { fn length(ref self) -> i64 { 0 } }\n\
         fn read(r: ref S) { r.lenght(); }",
    );
    let msg = errors
        .iter()
        .find(|e| e.kind == TypeErrorKind::NoMethodFound)
        .map(|e| e.message.clone())
        .expect("expected NoMethodFound through ref");
    assert!(
        msg.contains("did you mean 'length'?"),
        "expected typo suggestion through ref, got: {msg}"
    );
}

// ── Method resolution: `no method named` diagnostic ─────────────
//
// Per design.md § Method Resolution Step 7, a method-call that fails to
// resolve at any receiver level emits a focused `no method named ... on
// type ...` diagnostic with an optional `did you mean ...?` tail when an
// edit-distance-≤2 candidate exists on the type's impls.

#[test]
fn test_method_resolution_no_method_diagnostic_uses_dedicated_kind() {
    // The diagnostic now uses `TypeErrorKind::NoMethodFound` (not the
    // historical `TypeMismatch`).
    let errors = typecheck_errors(
        "struct S { x: i64 }\n\
         impl S { fn len(self) -> i64 { 0 } }\n\
         fn main() { let s = S { x: 0 }; s.bogus(); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::NoMethodFound),
        "expected NoMethodFound, got: {:?}",
        errors.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

#[test]
fn test_method_resolution_typo_suggestion_appears() {
    // `s.lenght()` is one character off from `length` — diagnostic should
    // include the suggestion.
    let errors = typecheck_errors(
        "struct S { x: i64 }\n\
         impl S { fn length(self) -> i64 { 0 } }\n\
         fn main() { let s = S { x: 0 }; s.lenght(); }",
    );
    let msg = errors
        .iter()
        .find(|e| e.kind == TypeErrorKind::NoMethodFound)
        .map(|e| e.message.clone())
        .expect("expected NoMethodFound diagnostic");
    assert!(
        msg.contains("did you mean 'length'?"),
        "expected typo suggestion in: {msg}"
    );
}

#[test]
fn test_method_resolution_typo_suggestion_considers_trait_methods() {
    // The candidate list for typo suggestion includes both inherent and
    // trait-impl method names. Here only the trait declares `flush`; a
    // typo `flus()` should still find it.
    let errors = typecheck_errors(
        "struct S { x: i64 }\n\
         trait Output { fn flush(ref self); }\n\
         impl Output for S { fn flush(ref self) { } }\n\
         fn main() { let s = S { x: 0 }; s.flus(); }",
    );
    let msg = errors
        .iter()
        .find(|e| e.kind == TypeErrorKind::NoMethodFound)
        .map(|e| e.message.clone())
        .expect("expected NoMethodFound diagnostic");
    assert!(
        msg.contains("did you mean 'flush'?"),
        "expected typo suggestion in: {msg}"
    );
}

#[test]
fn test_method_resolution_no_suggestion_when_nothing_close() {
    // Nothing within edit distance 2 — diagnostic fires but carries no
    // `did you mean` tail.
    let errors = typecheck_errors(
        "struct S { x: i64 }\n\
         impl S { fn length(self) -> i64 { 0 } }\n\
         fn main() { let s = S { x: 0 }; s.completely_unrelated(); }",
    );
    let msg = errors
        .iter()
        .find(|e| e.kind == TypeErrorKind::NoMethodFound)
        .map(|e| e.message.clone())
        .expect("expected NoMethodFound diagnostic");
    assert!(
        !msg.contains("did you mean"),
        "did not expect a typo suggestion in: {msg}"
    );
}

// ── Method resolution: stdlib typo suggestions ──────────────────
//
// Each per-type `infer_*_method` arm in the typechecker now emits a
// typo-suggestion diagnostic when the typed name is close to a known
// method. Far-from-anything names stay silent (preserves the historical
// permissive behavior for runtime-only methods that the typechecker
// hasn't enumerated yet).

#[test]
fn test_method_resolution_iterator_typo_suggestion() {
    // `iter.colect()` → suggests `collect`
    let errors = typecheck_errors("fn main() { let v = [1, 2, 3]; v.iter().colect(); }");
    let msg = errors
        .iter()
        .find(|e| e.kind == TypeErrorKind::NoMethodFound)
        .map(|e| e.message.clone())
        .expect("expected NoMethodFound on Iterator");
    assert!(
        msg.contains("'Iterator'") && msg.contains("did you mean 'collect'?"),
        "expected Iterator typo suggestion, got: {msg}"
    );
}

#[test]
fn test_method_resolution_map_typo_suggestion() {
    // `m.contians_key(...)` → suggests `contains_key`
    let errors = typecheck_errors(
        "fn main() { let m: Map[String, i64] = Map.new(); m.contians_key(\"x\"); }",
    );
    let msg = errors
        .iter()
        .find(|e| e.kind == TypeErrorKind::NoMethodFound)
        .map(|e| e.message.clone())
        .expect("expected NoMethodFound on Map");
    assert!(
        msg.contains("'Map'") && msg.contains("did you mean 'contains_key'?"),
        "expected Map typo suggestion, got: {msg}"
    );
}

#[test]
fn test_method_resolution_slice_typo_suggestion() {
    // `s.firts()` → suggests `first`
    let errors = typecheck_errors("fn main() { let v = [1, 2, 3]; v.as_slice().firts(); }");
    let msg = errors
        .iter()
        .find(|e| e.kind == TypeErrorKind::NoMethodFound)
        .map(|e| e.message.clone())
        .expect("expected NoMethodFound on Slice");
    assert!(
        msg.contains("'Slice'") && msg.contains("did you mean 'first'?"),
        "expected Slice typo suggestion, got: {msg}"
    );
}

#[test]
fn test_method_resolution_stdlib_silent_for_runtime_only() {
    // `s.completely_unrelated()` on a String stays silent (no edit-distance
    // match to `sorted` / `sorted_by`). Preserves the permissive fall-through
    // for runtime-only methods like `len` that aren't yet typechecker-known.
    // `typecheck_ok` asserts there are no errors at all.
    typecheck_ok("fn main() { let s = \"hi\"; s.completely_unrelated(); }");
}

// ── Method resolution: conditional impl filtering ───────────────
//
// Slice 1 of the method-resolution CR (see phase-4-interpreter.md).
// `impl[T: Ord] Foo for Bar[T]` resolves at call sites where the
// receiver's `T` discharges the bound, and silently falls through to
// `no method named` (E0236) when it doesn't. The supertrait closure
// is walked, so `T: PartialOrd` discharges against any type that
// impls `Ord` directly (`Ord: PartialOrd + Eq` per stdlib).

#[test]
fn test_conditional_impl_satisfied_picks_method() {
    // i32 impls Ord (registered by register_stdlib_impls), so the
    // conditional impl applies; `b.method()` resolves through it. The
    // function-parameter form pins `b: Bar[i32]` concretely without
    // depending on struct-literal generic-argument inference.
    typecheck_ok(
        "struct Bar[T] { x: T }\n\
         trait Foo { fn method(self) -> i64; }\n\
         impl[T: Ord] Foo for Bar[T] { fn method(self) -> i64 { 0 } }\n\
         fn use_bar(b: Bar[i32]) -> i64 { b.method() }",
    );
}

#[test]
fn test_conditional_impl_unsatisfied_drops_silently() {
    // `NotOrd` has no `impl Ord for NotOrd` and no impl of any trait
    // whose supertrait closure reaches `Ord`, so the conditional
    // `impl[T: Ord] Foo for Bar[T]` discharges as false on
    // `Bar[NotOrd]`. `b.method()` should error with NoMethodFound.
    let errors = typecheck_errors(
        "struct NotOrd { x: i64 }\n\
         struct Bar[T] { x: T }\n\
         trait Foo { fn method(self) -> i64; }\n\
         impl[T: Ord] Foo for Bar[T] { fn method(self) -> i64 { 0 } }\n\
         fn main() { let b: Bar[NotOrd] = Bar { x: NotOrd { x: 0 } }; b.method(); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::NoMethodFound)),
        "expected NoMethodFound for conditional impl that fails to discharge, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_conditional_impl_where_clause_filter() {
    // Same shape as inline-bound case but using a `where` clause —
    // the discharge engine treats `where T: Ord` and `impl[T: Ord]`
    // equivalently.
    typecheck_ok(
        "struct Bar[T] { x: T }\n\
         trait Foo { fn method(self) -> i64; }\n\
         impl[T] Foo for Bar[T] where T: Ord { fn method(self) -> i64 { 0 } }\n\
         fn use_bar(b: Bar[i32]) -> i64 { b.method() }",
    );
}

#[test]
fn test_conditional_impl_two_impls_only_one_bound_satisfied() {
    // Two impls collide on `target_type=\"Bar\"` and `method=\"method\"`.
    // Only the impl whose bounds discharge survives the filter,
    // eliminating the would-be ambiguity. On `Bar[NotOrd]`, `FooOrd` is
    // dropped (NotOrd doesn't impl Ord) so only `FooAny` applies — its
    // `method` returns `bool`. Without the filter, the typechecker
    // would either ambiguity-error or pick the wrong impl. The
    // function-parameter form avoids the struct-literal generic-argument
    // inference gap that the let-annotation form trips on.
    typecheck_ok(
        "struct NotOrd { x: i64 }\n\
         struct Bar[T] { x: T }\n\
         trait FooOrd { fn method(self) -> i64; }\n\
         trait FooAny { fn method(self) -> bool; }\n\
         impl[T: Ord] FooOrd for Bar[T] { fn method(self) -> i64 { 0 } }\n\
         impl[T] FooAny for Bar[T] { fn method(self) -> bool { true } }\n\
         fn use_bar(b: Bar[NotOrd]) -> bool { b.method() }",
    );
}

// ── Method resolution: receiver-form generic call-site dispatch ─
//
// Slice 2 of the method-resolution CR (see phase-4-interpreter.md
// item 8). `t.method(args)` where `t: T` and `T: SomeTrait`
// declares `method` should dispatch through the bound trait's
// method, mirroring what `T.method(args)` already does for the
// type-prefixed form.

#[test]
fn test_receiver_form_typeparam_single_bound_dispatches() {
    // `T: Reader` declares `access` — `x.access()` resolves through
    // the Reader trait's method signature.
    typecheck_ok(
        "trait Reader { fn access(ref self) -> i64; }\n\
         fn use_reader[T: Reader](x: T) -> i64 { x.access() }",
    );
}

#[test]
fn test_receiver_form_typeparam_no_bound_no_method() {
    // `T` has no bounds, so `x.method()` finds no candidate trait.
    // Errors with NoMethodFound rather than the pre-slice-2 silent
    // fallthrough to Type::Error.
    let errors = typecheck_errors("fn use_anything[T](x: T) -> i64 { x.access() }");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::NoMethodFound)),
        "expected NoMethodFound for no-bound TypeParam receiver, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_receiver_form_typeparam_multi_bound_ambiguity() {
    // Both Reader and Writer declare `access`. `x.access()` is ambiguous
    // — emit AmbiguousAssocFn pointing at UFCS.
    let errors = typecheck_errors(
        "trait Reader { fn access(ref self) -> i64; }\n\
         trait Writer { fn access(ref self) -> i64; }\n\
         fn use_both[T: Reader + Writer](x: T) -> i64 { x.access() }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::AmbiguousAssocFn)),
        "expected AmbiguousAssocFn for multi-bound receiver, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_receiver_form_typeparam_multi_bound_disambiguates_by_method() {
    // Reader declares `read`, Writer declares `write`. `x.read()`
    // unambiguously resolves to Reader (only one bound declares it).
    typecheck_ok(
        "trait Reader { fn read(ref self) -> i64; }\n\
         trait Writer { fn write(ref self) -> i64; }\n\
         fn use_both[T: Reader + Writer](x: T) -> i64 { x.read() }",
    );
}

// ── Method resolution: ambiguity on receiver form ───────────────
//
// Slice 3 of the method-resolution CR (see phase-4-interpreter.md
// item 4). When more than one user-impl candidate of the same
// priority tier survives the conditional-impl filter at a
// receiver-form call (typically two trait impls when no inherent
// matches), emit `AmbiguousMethod` (E0239) listing each candidate
// with a UFCS-disambiguation hint instead of silently picking
// first-match. Inherent-beats-trait priority (item 3) is preserved
// — ambiguity only fires *between* candidates of the same tier.

#[test]
fn test_receiver_form_ambiguity_two_trait_impls_same_method() {
    // Two trait impls of `S` each declare `foo(self) -> i32` — no
    // inherent impl exists, so both trait candidates survive the
    // priority filter and `s.foo()` is ambiguous.
    let errors = typecheck_errors(
        "struct S { x: i32 }\n\
         trait A { fn foo(self) -> i32; }\n\
         trait B { fn foo(self) -> i32; }\n\
         impl A for S { fn foo(self) -> i32 { 1 } }\n\
         impl B for S { fn foo(self) -> i32 { 2 } }\n\
         fn use_s(s: S) -> i32 { s.foo() }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::AmbiguousMethod)),
        "expected AmbiguousMethod for two-trait-impl ambiguity, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
    let amb = errors
        .iter()
        .find(|e| matches!(e.kind, TypeErrorKind::AmbiguousMethod))
        .unwrap();
    assert!(
        amb.message.contains("`A.foo("),
        "diagnostic missing trait `A` UFCS hint: {}",
        amb.message
    );
    assert!(
        amb.message.contains("`B.foo("),
        "diagnostic missing trait `B` UFCS hint: {}",
        amb.message
    );
}

#[test]
fn test_receiver_form_ambiguity_inherent_wins_no_diagnostic() {
    // An inherent impl + a trait impl both declare `foo`. The
    // inherent-beats-trait priority filter short-circuits to the
    // inherent candidate, so ambiguity does not fire.
    typecheck_ok(
        "struct S { x: i32 }\n\
         trait A { fn foo(self) -> i32; }\n\
         impl S { fn foo(self) -> i32 { 1 } }\n\
         impl A for S { fn foo(self) -> i32 { 2 } }\n\
         fn use_s(s: S) -> i32 { s.foo() }",
    );
}

#[test]
fn test_receiver_form_ambiguity_filtered_by_conditional_impl() {
    // Two trait impls collide on method `method`, but only one's
    // bounds discharge against the call site's args (`Bar[NotOrd]`
    // doesn't satisfy `T: Ord`). Slice 1's discharge filter drops the
    // unsatisfied candidate so only `FooAny` survives — no ambiguity.
    typecheck_ok(
        "struct NotOrd { x: i64 }\n\
         struct Bar[T] { x: T }\n\
         trait FooOrd { fn method(self) -> i64; }\n\
         trait FooAny { fn method(self) -> i64; }\n\
         impl[T: Ord] FooOrd for Bar[T] { fn method(self) -> i64 { 0 } }\n\
         impl[T] FooAny for Bar[T] { fn method(self) -> i64 { 1 } }\n\
         fn use_bar(b: Bar[NotOrd]) -> i64 { b.method() }",
    );
}

#[test]
fn test_receiver_form_ambiguity_single_trait_no_diagnostic() {
    // Single trait impl in scope — no ambiguity, dispatches normally
    // through the one surviving candidate.
    typecheck_ok(
        "struct S { x: i32 }\n\
         trait A { fn foo(self) -> i32; }\n\
         impl A for S { fn foo(self) -> i32 { 1 } }\n\
         fn use_s(s: S) -> i32 { s.foo() }",
    );
}

// ── Public-signature visibility (CR-18) ─────────────────────────
//
// A `pub fn` / pub method / pub struct field / pub enum variant payload /
// pub type alias / pub const whose signature references a non-`pub` type
// leaks the private type across the package boundary; the typechecker
// rejects it with `PrivateTypeInPublicSignature`.

fn visibility_errors(source: &str) -> Vec<TypeError> {
    typecheck_errors(source)
        .into_iter()
        .filter(|e| matches!(e.kind, TypeErrorKind::PrivateTypeInPublicSignature))
        .collect()
}

#[test]
fn test_pub_fn_with_private_return_type_rejected() {
    let errs = visibility_errors(
        "struct Hidden { x: i64 }
         pub fn make() -> Hidden { Hidden { x: 0 } }",
    );
    assert_eq!(
        errs.len(),
        1,
        "expected one visibility error, got {:?}",
        errs
    );
    assert!(
        errs[0].message.contains("Hidden"),
        "message missing type name: {}",
        errs[0].message
    );
}

#[test]
fn test_pub_fn_with_private_param_type_rejected() {
    let errs = visibility_errors(
        "struct Secret { x: i64 }
         pub fn consume(s: Secret) -> i64 { s.x }",
    );
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("Secret"));
}

#[test]
fn test_pub_fn_with_pub_types_accepted() {
    typecheck_ok(
        "pub struct Exposed { pub x: i64 }
         pub fn make() -> Exposed { Exposed { x: 0 } }",
    );
}

#[test]
fn test_pub_fn_with_generic_param_ok() {
    // `T` is a generic parameter, not a private type name.
    typecheck_ok("pub fn identity[T](x: T) -> T { x }");
}

#[test]
fn test_pub_fn_with_private_type_in_generic_args_rejected() {
    let errs = visibility_errors(
        "enum Inner { A, B }
         pub fn make(v: Vec[Inner]) -> i64 { 0 }",
    );
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("Inner"));
}

#[test]
fn test_private_fn_with_private_types_ok() {
    // Only `pub` signatures are checked.
    typecheck_ok(
        "struct Hidden { x: i64 }
         fn make() -> Hidden { Hidden { x: 0 } }",
    );
}

#[test]
fn test_pub_struct_pub_field_with_private_type_rejected() {
    let errs = visibility_errors(
        "struct Inner { x: i64 }
         pub struct Outer { pub inner: Inner }",
    );
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("Inner"));
}

#[test]
fn test_pub_struct_private_field_with_private_type_ok() {
    // Non-pub fields on a pub struct are module-internal, so referencing a
    // private type in them does not leak anything.
    typecheck_ok(
        "struct Inner { x: i64 }
         pub struct Outer { inner: Inner }",
    );
}

#[test]
fn test_pub_enum_variant_payload_with_private_type_rejected() {
    let errs = visibility_errors(
        "struct Inner { x: i64 }
         pub enum E { Holds(Inner), None }",
    );
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("Inner"));
}

#[test]
fn test_pub_type_alias_with_private_rhs_rejected() {
    let errs = visibility_errors(
        "struct Hidden { x: i64 }
         pub type Exposed = Hidden;",
    );
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("Hidden"));
}

#[test]
fn test_pub_const_with_private_type_rejected() {
    let errs = visibility_errors(
        "struct Secret { x: i64 }
         pub const MARKER: Secret = Secret { x: 0 };",
    );
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("Secret"));
}

#[test]
fn test_pub_method_on_impl_with_private_return_rejected() {
    let errs = visibility_errors(
        "pub struct Outer { pub x: i64 }
         struct Hidden { y: i64 }
         impl Outer {
             pub fn peek(ref self) -> Hidden { Hidden { y: 0 } }
         }",
    );
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("Hidden"));
}

#[test]
fn test_private_method_with_private_types_ok() {
    typecheck_ok(
        "pub struct Outer { pub x: i64 }
         struct Hidden { y: i64 }
         impl Outer {
             fn peek(ref self) -> Hidden { Hidden { y: 0 } }
         }",
    );
}

#[test]
fn test_pub_trait_method_with_private_return_rejected() {
    let errs = visibility_errors(
        "struct Hidden { x: i64 }
         pub trait Peek {
             fn peek(ref self) -> Hidden;
         }",
    );
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("Hidden"));
}

#[test]
fn test_pub_fn_with_tuple_containing_private_rejected() {
    let errs = visibility_errors(
        "struct Hidden { x: i64 }
         pub fn split() -> (i64, Hidden) { (0, Hidden { x: 0 }) }",
    );
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("Hidden"));
}

#[test]
fn test_pub_fn_referencing_builtin_types_ok() {
    // `Vec`, `Option`, `Result`, and primitives are stdlib-registered outside
    // the user AST and are always treated as public.
    typecheck_ok("pub fn first(v: Vec[i64]) -> Option[i64] { None }");
}

// ── Half-open range expression types ────────────────────────────────────────

#[test]
fn test_range_from_type() {
    // `a..` should produce RangeFrom[i64] — no type error
    let result = typecheck_ok("fn main() { let n: i64 = 1; let _r = n..; }");
    assert!(result.errors.is_empty());
}

#[test]
fn test_range_to_exclusive_type() {
    // `..b` should produce RangeTo[i64] — no type error
    let result = typecheck_ok("fn main() { let n: i64 = 10; let _r = ..n; }");
    assert!(result.errors.is_empty());
}

#[test]
fn test_range_to_inclusive_type() {
    // `..=b` should produce RangeToInclusive[i64] — no type error
    let result = typecheck_ok("fn main() { let n: i64 = 10; let _r = ..=n; }");
    assert!(result.errors.is_empty());
}

#[test]
fn test_range_full_type() {
    // `..` should produce RangeFull — no type error
    let result = typecheck_ok("fn main() { let _r = ..; }");
    assert!(result.errors.is_empty());
}

#[test]
fn test_range_both_bounds_type() {
    // `a..b` — both bounds must have the same type; no error here
    let result = typecheck_ok("fn main() { let _r = 0..10; }");
    assert!(result.errors.is_empty());
}

#[test]
fn test_range_mismatched_bound_types_is_error() {
    // `0..true` — start is i64, end is bool → type error
    let errors = typecheck_errors("fn main() { let _r = 0..true; }");
    assert!(
        !errors.is_empty(),
        "Expected a type error for mismatched range bounds"
    );
}

#[test]
fn test_half_open_range_slice_index_from() {
    // `v[i..]` — slice from index i; no type error
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn main() {
             let a: Array[i64, 5] = [10, 20, 30, 40, 50];
             let _s = sum(a[2..]);
         }",
    );
}

#[test]
fn test_half_open_range_slice_index_to() {
    // `v[..n]` — slice up to n; no type error
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn main() {
             let a: Array[i64, 5] = [10, 20, 30, 40, 50];
             let _s = sum(a[..3]);
         }",
    );
}

#[test]
fn test_half_open_range_slice_index_full() {
    // `v[..]` — full slice; no type error
    typecheck_ok(
        "fn sum(xs: Slice[i64]) -> i64 { 0 }
         fn main() {
             let a: Array[i64, 5] = [10, 20, 30, 40, 50];
             let _s = sum(a[..]);
         }",
    );
}

// ── Standard I/O type checks ─────────────────────────────────────────────────

#[test]
fn test_io_error_variants_are_known() {
    // IoError variants can be matched without import
    typecheck_ok(
        "fn classify(e: IoError) -> String {
             match e {
                 IoError.NotFound => \"not found\",
                 IoError.PermissionDenied => \"permission denied\",
                 IoError.AlreadyExists => \"already exists\",
                 IoError.UnexpectedEof => \"unexpected eof\",
                 IoError.InvalidUtf8 => \"invalid utf-8\",
                 IoError.Interrupted => \"interrupted\",
                 IoError.Other(_) => \"other\",
             }
         }",
    );
}

#[test]
fn test_stdin_read_line_returns_result_str_io_error() {
    // Stdin.read_line() -> Result[str, IoError] — should typecheck
    typecheck_ok(
        "fn main() with reads(Stdin) {
             let r = Stdin.read_line();
         }",
    );
}

#[test]
fn test_filesystem_read_to_string_returns_result_str() {
    // FileSystem.read_to_string(path) -> Result[str, IoError]
    typecheck_ok(
        "fn main() with reads(FileSystem) {
             let r = FileSystem.read_to_string(\"file.txt\");
         }",
    );
}

#[test]
fn test_filesystem_write_returns_result_unit() {
    // FileSystem.write(path, contents) -> Result[Unit, IoError]
    typecheck_ok(
        "fn main() with writes(FileSystem) {
             let r = FileSystem.write(\"file.txt\", \"hello\");
         }",
    );
}

#[test]
fn test_stdout_flush_returns_unit() {
    // Stdout.flush() -> Unit
    typecheck_ok(
        "fn main() with writes(Stdout) {
             Stdout.flush();
         }",
    );
}

// ── env.args / env.var typechecker signatures ─────────────────────────────────

#[test]
fn test_env_args_returns_vec_string() {
    // env.args() -> Vec[String] with reads(Env)
    typecheck_ok(
        "fn main() with reads(Env) {
             let args = env.args();
         }",
    );
}

#[test]
fn test_env_args_capitalized_form_also_works() {
    // Env.args() capitalized form also typechecks
    typecheck_ok(
        "fn main() with reads(Env) {
             let args = Env.args();
         }",
    );
}

#[test]
fn test_env_var_returns_result_string_var_error() {
    // env.var(name) -> Result[String, VarError]
    typecheck_ok(
        "fn main() with reads(Env) {
             let r = env.var(\"HOME\");
         }",
    );
}

#[test]
fn test_env_var_wrong_arg_type_is_error() {
    // env.var expects a String, not an int
    let errors = typecheck_errors(
        "fn main() with reads(Env) {
             let r = env.var(42);
         }",
    );
    assert!(!errors.is_empty());
}

#[test]
fn test_var_error_not_unicode_variant_exists() {
    // VarError now has NotUnicode variant alongside NotPresent
    typecheck_ok(
        "fn main() {
             let e = VarError.NotUnicode;
         }",
    );
}

// ── Slice[T] method typechecking ──────────────────────────────────────────────

#[test]
fn test_slice_len_and_is_empty() {
    typecheck_ok(
        "fn main() {
             let v = [1, 2, 3];
             let s = v.as_slice();
             let n: i64 = s.len();
             let b: bool = s.is_empty();
         }",
    );
}

#[test]
fn test_slice_first_last_return_option() {
    typecheck_ok(
        "fn main() {
             let v = [10, 20, 30];
             let s = v.as_slice();
             let f = s.first();
             let l = s.last();
         }",
    );
}

#[test]
fn test_slice_get_returns_option() {
    typecheck_ok(
        "fn main() {
             let v = [1, 2, 3];
             let s = v.as_slice();
             let x = s.get(1);
         }",
    );
}

#[test]
fn test_slice_contains_returns_bool() {
    typecheck_ok(
        "fn main() {
             let v = [1, 2, 3];
             let s = v.as_slice();
             let b: bool = s.contains(2);
         }",
    );
}

#[test]
fn test_slice_binary_search_returns_option_i64() {
    typecheck_ok(
        "fn main() {
             let v = [1, 2, 3];
             let s = v.as_slice();
             let r = s.binary_search(2);
         }",
    );
}

#[test]
fn test_slice_split_at_returns_tuple() {
    typecheck_ok(
        "fn main() {
             let v = [1, 2, 3, 4];
             let s = v.as_slice();
             let (a, b) = s.split_at(2);
         }",
    );
}

#[test]
fn test_slice_chunks_returns_vec_slice() {
    typecheck_ok(
        "fn main() {
             let v = [1, 2, 3, 4];
             let s = v.as_slice();
             let c = s.chunks(2);
         }",
    );
}

#[test]
fn test_slice_sort_requires_mut_slice() {
    // sort() on a read-only Slice[T] is a type error
    let errors = typecheck_errors(
        "fn main() {
             let mut v = [3, 1, 2];
             let s = v.as_slice();
             s.sort();
         }",
    );
    assert!(!errors.is_empty());
}

#[test]
fn test_mut_slice_sort_and_reverse() {
    // sort() and reverse() are valid on mut Slice[T]
    typecheck_ok(
        "fn main() {
             let mut v = [3, 1, 2];
             let mut s = v.as_slice_mut();
             s.sort();
             s.reverse();
         }",
    );
}

// ── F-string expression type-checking ────────────────────────────────────────

#[test]
fn test_fstring_expression_typechecked() {
    // The embedded expression `x + 1` is now type-checked at parse time.
    typecheck_ok("fn main() { let x = 42; let _s = f\"value is {x}\"; }");
}

#[test]
fn test_fstring_arithmetic_expression() {
    typecheck_ok("fn main() { let x = 10; let _s = f\"double is {x * 2}\"; }");
}

// ── Debug trait derive (item 161) ────────────────────────────────────────────

#[test]
fn test_derive_debug_on_struct_ok() {
    typecheck_ok(
        "#[derive(Debug)]\n\
         struct Point { x: i64, y: i64 }\n\
         fn main() {}",
    );
}

#[test]
fn test_derive_debug_on_enum_ok() {
    typecheck_ok(
        "#[derive(Debug)]\n\
         enum Color { Red, Green, Blue }\n\
         fn main() {}",
    );
}

#[test]
fn test_derive_debug_with_other_traits_ok() {
    typecheck_ok(
        "#[derive(Debug, Clone, PartialEq)]\n\
         struct Pair { a: i64, b: i64 }\n\
         fn main() {}",
    );
}

// ── #[derive(Display)] on enums ──────────────────────────────────

#[test]
fn test_derive_display_unit_enum_ok() {
    // All-unit-variant enum accepts #[derive(Display)].
    typecheck_ok(
        "#[derive(Display)]\n\
         enum Direction { Up, Down, Left, Right }\n\
         fn main() {}",
    );
}

#[test]
fn test_derive_display_tuple_variant_rejected() {
    // Enum with a tuple variant rejects #[derive(Display)].
    let errors = typecheck_errors(
        "#[derive(Display)]\n\
         enum Wrapper { Value(i64), Empty }\n\
         fn main() {}",
    );
    assert!(
        !errors.is_empty(),
        "derive(Display) on enum with tuple variant should produce an error"
    );
    assert!(
        errors.iter().any(|e| e.message.contains("Wrapper")
            && e.message.contains("Value")
            && e.message.contains("unit variant")),
        "error should name the enum and offending variant, got: {:?}",
        errors
    );
}

#[test]
fn test_derive_display_snake_case_argument_parses() {
    // Display(snake_case) argument is accepted on a unit-variant enum.
    typecheck_ok(
        "#[derive(Display(snake_case))]\n\
         enum Status { Active, InProgress, Done }\n\
         fn main() {}",
    );
}

// ── bool exhaustiveness (F-072) ──────────────────────────────────

#[test]
fn test_bool_match_both_arms_exhaustive() {
    typecheck_ok(
        "fn describe(flag: bool) -> i64 {\n\
             match flag {\n\
                 true  => 1,\n\
                 false => 0,\n\
             }\n\
         }",
    );
}

#[test]
fn test_bool_match_missing_false_is_error() {
    let errors = typecheck_errors(
        "fn describe(flag: bool) -> i64 {\n\
             match flag {\n\
                 true => 1,\n\
             }\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::NonExhaustiveMatch),
        "missing false arm should produce NonExhaustiveMatch"
    );
}

#[test]
fn test_bool_match_missing_true_is_error() {
    let errors = typecheck_errors(
        "fn describe(flag: bool) -> i64 {\n\
             match flag {\n\
                 false => 0,\n\
             }\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::NonExhaustiveMatch),
        "missing true arm should produce NonExhaustiveMatch"
    );
}

#[test]
fn test_bool_match_wildcard_is_exhaustive() {
    typecheck_ok(
        "fn describe(flag: bool) -> i64 {\n\
             match flag {\n\
                 true => 1,\n\
                 _    => 0,\n\
             }\n\
         }",
    );
}

#[test]
fn test_bool_match_catch_all_binding_is_exhaustive() {
    typecheck_ok(
        "fn describe(flag: bool) -> i64 {\n\
             match flag {\n\
                 true  => 1,\n\
                 other => 0,\n\
             }\n\
         }",
    );
}

// ── SortedSet[T] method typechecking ──────────────────────────────────────────

#[test]
fn test_sorted_set_len_returns_i64() {
    typecheck_ok("fn f(s: SortedSet[i64]) -> i64 { s.len() }");
}

#[test]
fn test_sorted_set_is_empty_returns_bool() {
    typecheck_ok("fn f(s: SortedSet[i64]) -> bool { s.is_empty() }");
}

#[test]
fn test_sorted_set_contains_returns_bool() {
    typecheck_ok("fn f(s: SortedSet[i64]) -> bool { s.contains(42_i64) }");
}

#[test]
fn test_sorted_set_insert_returns_bool() {
    typecheck_ok("fn f(s: SortedSet[i64]) -> bool { s.insert(1_i64) }");
}

#[test]
fn test_sorted_set_remove_returns_bool() {
    typecheck_ok("fn f(s: SortedSet[i64]) -> bool { s.remove(1_i64) }");
}

#[test]
fn test_sorted_set_min_returns_option() {
    typecheck_ok("fn f(s: SortedSet[i64]) -> Option[i64] { s.min() }");
}

#[test]
fn test_sorted_set_max_returns_option() {
    typecheck_ok("fn f(s: SortedSet[i64]) -> Option[i64] { s.max() }");
}

#[test]
fn test_sorted_set_union_returns_sorted_set() {
    typecheck_ok("fn f(a: SortedSet[i64], b: SortedSet[i64]) -> SortedSet[i64] { a.union(b) }");
}

#[test]
fn test_sorted_set_intersection_returns_sorted_set() {
    typecheck_ok(
        "fn f(a: SortedSet[i64], b: SortedSet[i64]) -> SortedSet[i64] { a.intersection(b) }",
    );
}

#[test]
fn test_sorted_set_difference_returns_sorted_set() {
    typecheck_ok(
        "fn f(a: SortedSet[i64], b: SortedSet[i64]) -> SortedSet[i64] { a.difference(b) }",
    );
}

#[test]
fn test_sorted_set_type_in_annotation() {
    // SortedSet[T] is accepted as a type in let-annotations and parameter positions
    typecheck_ok(
        "fn f() -> i64 {\n\
             let s: SortedSet[i64] = SortedSet.new();\n\
             s.len()\n\
         }",
    );
}

#[test]
fn test_sorted_set_string_element() {
    typecheck_ok("fn f(s: SortedSet[String]) -> bool { s.is_empty() }");
}

// ── Channel[T] / Sender[T] / Receiver[T] typechecking ─────────────────────────

#[test]
fn test_sender_send_returns_unit() {
    typecheck_ok("fn f(s: Sender[i64]) { s.send(1_i64); }");
}

#[test]
fn test_receiver_recv_returns_element() {
    typecheck_ok("fn f(r: Receiver[i64]) -> i64 { r.recv() }");
}

#[test]
fn test_receiver_try_recv_returns_option() {
    typecheck_ok("fn f(r: Receiver[String]) -> Option[String] { r.try_recv() }");
}

#[test]
fn test_sender_clone_returns_sender() {
    typecheck_ok("fn f(s: Sender[bool]) -> Sender[bool] { s.clone() }");
}

#[test]
fn test_sender_in_let_annotation() {
    typecheck_ok(
        "fn f(s: Sender[i64]) {\n\
             let s2: Sender[i64] = s.clone();\n\
             s2.send(42_i64);\n\
         }",
    );
}

#[test]
fn test_channel_types_accepted_in_param_position() {
    typecheck_ok(
        "fn send_value(s: Sender[i64], v: i64) { s.send(v); }\n\
         fn recv_value(r: Receiver[i64]) -> i64 { r.recv() }",
    );
}

#[test]
fn test_sender_send_wrong_type_errors() {
    let errors = typecheck_errors("fn f(s: Sender[i64]) { s.send(true); }");
    assert!(!errors.is_empty());
}

// ── Map[K, V] method typechecking ──────────────────────────────────────────

#[test]
fn test_map_len_returns_i64() {
    typecheck_ok("fn f(m: Map[String, i64]) -> i64 { m.len() }");
}

#[test]
fn test_map_is_empty_returns_bool() {
    typecheck_ok("fn f(m: Map[String, i64]) -> bool { m.is_empty() }");
}

#[test]
fn test_map_contains_key_returns_bool() {
    typecheck_ok("fn f(m: Map[String, i64], k: String) -> bool { m.contains_key(k) }");
}

#[test]
fn test_map_get_returns_option_v() {
    typecheck_ok("fn f(m: Map[String, i64], k: String) -> Option[i64] { m.get(k) }");
}

#[test]
fn test_map_get_or_returns_v() {
    typecheck_ok("fn f(m: Map[String, i64], k: String) -> i64 { m.get_or(k, 0_i64) }");
}

#[test]
fn test_map_insert_returns_option_v() {
    typecheck_ok("fn f(m: Map[String, i64], k: String, v: i64) -> Option[i64] { m.insert(k, v) }");
}

#[test]
fn test_map_remove_returns_option_v() {
    typecheck_ok("fn f(m: Map[String, i64], k: String) -> Option[i64] { m.remove(k) }");
}

#[test]
fn test_map_keys_returns_vec_k() {
    typecheck_ok("fn f(m: Map[String, i64]) -> Vec[String] { m.keys() }");
}

#[test]
fn test_map_values_returns_vec_v() {
    typecheck_ok("fn f(m: Map[String, i64]) -> Vec[i64] { m.values() }");
}

#[test]
fn test_map_entries_returns_vec_tuple() {
    typecheck_ok("fn f(m: Map[String, i64]) -> Vec[(String, i64)] { m.entries() }");
}

#[test]
fn test_map_merge_returns_map() {
    typecheck_ok(
        "fn f(a: Map[String, i64], b: Map[String, i64]) -> Map[String, i64] { a.merge(b) }",
    );
}

#[test]
fn test_map_type_annotation_accepted() {
    typecheck_ok("fn f() -> Map[i64, bool] { Map.new() }");
}

#[test]
fn test_map_wrong_key_type_errors() {
    let errors = typecheck_errors("fn f(m: Map[String, i64]) -> bool { m.contains_key(42_i64) }");
    assert!(!errors.is_empty());
}

// ── Associated types in traits ────────────────────────────────────────────

#[test]
fn test_assoc_type_decl_in_trait_accepted() {
    typecheck_ok(
        "trait Container {\n\
             type Item;\n\
             fn get(ref self) -> Self.Item;\n\
         }",
    );
}

#[test]
fn test_assoc_type_with_bound_accepted() {
    typecheck_ok(
        "trait Printable {\n\
             type Output: Display;\n\
             fn render(ref self) -> Self.Output;\n\
         }",
    );
}

#[test]
fn test_assoc_type_binding_in_impl_accepted() {
    typecheck_ok(
        "trait Container {\n\
             type Item;\n\
             fn get(ref self) -> Self.Item;\n\
         }\n\
         struct Bag { }\n\
         impl Container for Bag {\n\
             type Item = i64;\n\
             fn get(ref self) -> i64 { 0_i64 }\n\
         }",
    );
}

#[test]
fn test_assoc_type_missing_in_impl_errors() {
    let errors = typecheck_errors(
        "trait Container {\n\
             type Item;\n\
             fn get(ref self) -> Self.Item;\n\
         }\n\
         struct Bag { }\n\
         impl Container for Bag {\n\
             fn get(ref self) -> i64 { 0_i64 }\n\
         }",
    );
    assert!(!errors.is_empty(), "missing assoc type should be an error");
}

#[test]
fn test_assoc_projection_in_function_signature() {
    typecheck_ok(
        "trait Container {\n\
             type Item;\n\
             fn get_item(ref self) -> Self.Item;\n\
         }\n\
         fn first_item[C: Container](c: ref C) -> C.Item {\n\
             c.get_item()\n\
         }",
    );
}

#[test]
fn test_assoc_type_resolved_through_impl() {
    typecheck_ok(
        "trait Mapper {\n\
             type Output;\n\
             fn map(ref self) -> Self.Output;\n\
         }\n\
         struct Doubler { }\n\
         impl Mapper for Doubler {\n\
             type Output = i64;\n\
             fn map(ref self) -> i64 { 42_i64 }\n\
         }",
    );
}

#[test]
fn test_where_assoc_type_equality_accepted() {
    typecheck_ok(
        "trait Container {\n\
             type Item;\n\
         }\n\
         fn sum_items[C: Container](c: ref C) -> i64\n\
             where C.Item = i64\n\
         { 0_i64 }",
    );
}

#[test]
fn test_multiple_assoc_types_in_trait() {
    typecheck_ok(
        "trait Converter {\n\
             type Input;\n\
             type Output;\n\
             fn convert(ref self, val: Self.Input) -> Self.Output;\n\
         }",
    );
}

#[test]
fn test_assoc_type_in_return_position() {
    typecheck_ok(
        "trait Source {\n\
             type Item;\n\
             fn next(ref self) -> Option[Self.Item];\n\
         }",
    );
}

// ── Iterator / IntoIterator trait registration + for-loop element typing ──

#[test]
fn test_iterator_trait_is_known() {
    // Iterator is a prelude trait — using it as a bound should not error.
    typecheck_ok(
        "trait MyIter: Iterator {\n\
             type Item;\n\
         }",
    );
}

#[test]
fn test_for_loop_vec_element_type() {
    typecheck_ok(
        "fn f(v: Vec[i64]) {\n\
             for x in v {\n\
                 let n: i64 = x;\n\
             }\n\
         }",
    );
}

#[test]
fn test_for_loop_sorted_set_element_type() {
    typecheck_ok(
        "fn f(s: SortedSet[i64]) {\n\
             for x in s {\n\
                 let n: i64 = x;\n\
             }\n\
         }",
    );
}

#[test]
fn test_for_loop_map_element_type_is_tuple() {
    typecheck_ok(
        "fn f(m: Map[String, i64]) {\n\
             for pair in m {\n\
                 let t: (String, i64) = pair;\n\
             }\n\
         }",
    );
}

#[test]
fn test_for_loop_range_element_type() {
    typecheck_ok(
        "fn f() {\n\
             for i in 0_i64..10_i64 {\n\
                 let n: i64 = i;\n\
             }\n\
         }",
    );
}

#[test]
fn test_impl_iterator_accepted() {
    typecheck_ok(
        "trait Iterator {\n\
             type Item;\n\
             fn next(ref self) -> Option[Self.Item];\n\
         }\n\
         struct Counter { value: i64 }\n\
         impl Iterator for Counter {\n\
             type Item = i64;\n\
             fn next(ref self) -> Option[i64] { Some(self.value) }\n\
         }",
    );
}

#[test]
fn test_impl_into_iterator_accepted() {
    typecheck_ok(
        "trait Iterator {\n\
             type Item;\n\
         }\n\
         trait IntoIterator {\n\
             type Item;\n\
             type IntoIter;\n\
         }\n\
         struct Counter { value: i64 }\n\
         impl IntoIterator for Counter {\n\
             type Item = i64;\n\
             type IntoIter = Counter;\n\
         }",
    );
}

#[test]
fn test_impl_into_iterator_missing_assoc_errors() {
    let errors = typecheck_errors(
        "trait IntoIterator {\n\
             type Item;\n\
             type IntoIter;\n\
         }\n\
         struct Foo { }\n\
         impl IntoIterator for Foo {\n\
             type Item = i64;\n\
         }",
    );
    assert!(
        !errors.is_empty(),
        "missing IntoIter assoc type should be an error"
    );
}

// ── Trait Bound Enforcement: SortedSet[T: Ord] and Map[K: Hash+Eq, V] ──

#[test]
fn test_sorted_set_ord_type_ok() {
    // Primitive types with total order are accepted as SortedSet elements.
    typecheck_ok(
        "fn f() {
             let s: SortedSet[i64] = SortedSet.new();
             s.insert(1);
         }",
    );
}

#[test]
fn test_sorted_set_string_key_ok() {
    typecheck_ok(
        "fn f() {
             let s: SortedSet[String] = SortedSet.new();
             s.insert(\"hello\");
         }",
    );
}

#[test]
fn test_sorted_set_float_key_rejects() {
    // f64 does not implement Ord (IEEE NaN breaks total order).
    let errors = typecheck_errors(
        "fn f() {
             let s: SortedSet[f64] = SortedSet.new();
             s.insert(1.0);
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for SortedSet[f64], got: {:?}",
        errors
    );
}

#[test]
fn test_sorted_set_struct_without_derive_ord_rejects() {
    let errors = typecheck_errors(
        "struct Point { x: i64, y: i64 }
         fn f() {
             let s: SortedSet[Point] = SortedSet.new();
             s.insert(Point { x: 1, y: 2 });
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for SortedSet[Point] without #[derive(Ord)], got: {:?}",
        errors
    );
}

#[test]
fn test_map_int_key_ok() {
    // Integers implement Hash + Eq — valid Map key type.
    typecheck_ok(
        "fn f() {
             let m: Map[i64, bool] = Map.new();
             m.insert(1, true);
         }",
    );
}

#[test]
fn test_map_string_key_ok() {
    typecheck_ok("fn f() -> Map[String, i64] { Map.new() }");
}

#[test]
fn test_map_float_key_rejects() {
    // f64 does not implement Hash (NaN != NaN breaks the contract).
    let errors = typecheck_errors(
        "fn f() {
             let m: Map[f64, i64] = Map.new();
             m.insert(1.0, 42);
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for Map[f64, _], got: {:?}",
        errors
    );
}

#[test]
fn test_map_struct_without_derive_hash_rejects() {
    let errors = typecheck_errors(
        "struct Key { id: i64 }
         fn f() {
             let m: Map[Key, i64] = Map.new();
             m.insert(Key { id: 1 }, 42);
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for Map[Key, _] without #[derive(Hash, Eq)], got: {:?}",
        errors
    );
}

#[test]
fn test_map_struct_with_derive_hash_eq_ok() {
    typecheck_ok(
        "#[derive(Hash, Eq)]
         struct Key { id: i64 }
         fn f() {
             let m: Map[Key, i64] = Map.new();
             m.insert(Key { id: 1 }, 42);
         }",
    );
}

#[test]
fn test_map_tuple_with_float_field_rejects() {
    // `Map[(String, f64), V]` — tuple Hash recurses per-field, and f64 fails
    // Hash. Locks in the recursive bound check so the codegen tuple path
    // never sees an unhashable element.
    let errors = typecheck_errors(
        "fn f() {
             let m: Map[(String, f64), i64] = Map.new();
             m.insert((\"k\", 1.0), 42);
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for Map[(String, f64), _], got: {:?}",
        errors
    );
}

// ── Regex ─────────────────────────────────────────────────────────

#[test]
fn test_regex_compile_ok() {
    typecheck_ok(r#"fn f() -> Result[Regex, RegexError] { Regex.compile("[0-9]+") }"#);
}

#[test]
fn test_regex_is_match_ok() {
    typecheck_ok(
        r#"fn f() {
             let r = Regex.compile("[0-9]+").unwrap();
             let b: bool = r.is_match("abc123");
         }"#,
    );
}

#[test]
fn test_regex_find_ok() {
    typecheck_ok(
        r#"fn f() -> Option[Match] {
             let r = Regex.compile("[0-9]+").unwrap();
             r.find("abc123")
         }"#,
    );
}

#[test]
fn test_regex_find_all_ok() {
    typecheck_ok(
        r#"fn f() -> Vec[Match] {
             let r = Regex.compile("[0-9]+").unwrap();
             r.find_all("abc 123 def 456")
         }"#,
    );
}

#[test]
fn test_regex_replace_all_ok() {
    typecheck_ok(
        r#"fn f() -> String {
             let r = Regex.compile("[0-9]+").unwrap();
             let _s = r.replace_all("abc 123", "NUM");
         }"#,
    );
}

// ── Stats namespace ───────────────────────────────────────────────

#[test]
fn test_stats_sum_ok() {
    typecheck_ok("fn f() -> f64 { let xs = [1.0_f64, 2.0_f64]; Stats.sum(xs) }");
}

#[test]
fn test_stats_mean_ok() {
    typecheck_ok("fn f() -> f64 { let xs = [1.0_f64, 2.0_f64]; Stats.mean(xs) }");
}

#[test]
fn test_stats_variance_ok() {
    typecheck_ok("fn f() -> f64 { let xs = [1.0_f64, 2.0_f64]; Stats.variance(xs) }");
}

#[test]
fn test_stats_stddev_ok() {
    typecheck_ok("fn f() -> f64 { let xs = [1.0_f64, 2.0_f64]; Stats.stddev(xs) }");
}

#[test]
fn test_stats_median_ok() {
    typecheck_ok("fn f() -> f64 { let xs = [1.0_f64, 3.0_f64, 2.0_f64]; Stats.median(xs) }");
}

#[test]
fn test_stats_min_ok() {
    typecheck_ok("fn f() -> Option[f64] { let xs = [1.0_f64, 2.0_f64]; Stats.min(xs) }");
}

#[test]
fn test_stats_max_ok() {
    typecheck_ok("fn f() -> Option[f64] { let xs = [1.0_f64, 2.0_f64]; Stats.max(xs) }");
}

// ── Set[T] ───────────────────────────────────────────────────────

#[test]
fn test_set_new_ok() {
    typecheck_ok("fn f() -> Set[i64] { Set.new() }");
}

#[test]
fn test_set_insert_ok() {
    typecheck_ok(
        "fn f() {
             let s: Set[i64] = Set.new();
             s.insert(42_i64);
         }",
    );
}

#[test]
fn test_set_contains_ok() {
    typecheck_ok(
        "fn f() -> bool {
             let s: Set[i64] = Set.new();
             s.contains(1_i64)
         }",
    );
}

#[test]
fn test_set_remove_ok() {
    typecheck_ok(
        "fn f() -> bool {
             let s: Set[i64] = Set.new();
             s.remove(1_i64)
         }",
    );
}

#[test]
fn test_set_len_ok() {
    typecheck_ok(
        "fn f() -> i64 {
             let s: Set[i64] = Set.new();
             s.len()
         }",
    );
}

#[test]
fn test_set_is_empty_ok() {
    typecheck_ok(
        "fn f() -> bool {
             let s: Set[i64] = Set.new();
             s.is_empty()
         }",
    );
}

#[test]
fn test_set_union_ok() {
    typecheck_ok(
        "fn f() {
             let a: Set[i64] = Set.new();
             let b: Set[i64] = Set.new();
             let c: Set[i64] = a.union(b);
         }",
    );
}

#[test]
fn test_set_float_key_rejects() {
    let errors = typecheck_errors(
        "fn f() {
             let s: Set[f64] = Set.new();
             s.insert(1.0);
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for Set[f64], got: {:?}",
        errors
    );
}

#[test]
fn test_set_struct_without_hash_eq_rejects() {
    let errors = typecheck_errors(
        "struct Pt { x: i64 }
         fn f() {
             let s: Set[Pt] = Set.new();
             s.insert(Pt { x: 1 });
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for Set[Pt] without #[derive(Hash, Eq)], got: {:?}",
        errors
    );
}

#[test]
fn test_set_struct_with_hash_eq_ok() {
    typecheck_ok(
        "#[derive(Hash, Eq)]
         struct Pt { x: i64 }
         fn f() {
             let s: Set[Pt] = Set.new();
             s.insert(Pt { x: 1 });
         }",
    );
}

// ── Display trait ────────────────────────────────────────────────

#[test]
fn test_println_with_str_ok() {
    typecheck_ok(r#"fn f() { println("hello"); }"#);
}

#[test]
fn test_println_with_i64_ok() {
    typecheck_ok("fn f() { println(42); }");
}

#[test]
fn test_println_with_bool_ok() {
    typecheck_ok("fn f() { println(true); }");
}

#[test]
fn test_println_no_args_ok() {
    typecheck_ok(r#"fn f() { println(); }"#);
}

#[test]
fn test_println_with_vec_display_ok() {
    typecheck_ok("fn f() { let v: Vec[i64] = Vec.new(); println(v); }");
}

#[test]
fn test_println_with_option_display_ok() {
    typecheck_ok("fn f() { let x: Option[i64] = Some(1); println(x); }");
}

#[test]
fn test_println_function_value_rejects() {
    let errors = typecheck_errors("fn g() {} fn f() { println(g); }");
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for println(fn), got: {:?}",
        errors
    );
}

#[test]
fn test_println_struct_without_derive_display_rejects() {
    let errors = typecheck_errors(
        "struct Point { x: i64, y: i64 }
         fn f() { println(Point { x: 1, y: 2 }); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for println(Point), got: {:?}",
        errors
    );
}

#[test]
fn test_println_struct_with_derive_display_ok() {
    typecheck_ok(
        "#[derive(Display)]
         struct Point { x: i64, y: i64 }
         fn f() { println(Point { x: 1, y: 2 }); }",
    );
}

#[test]
fn test_fstring_i64_interpolation_ok() {
    typecheck_ok(r#"fn f() { let n = 42; let s = f"value: {n}"; }"#);
}

#[test]
fn test_fstring_bool_interpolation_ok() {
    typecheck_ok(r#"fn f() { let b = true; let s = f"flag: {b}"; }"#);
}

#[test]
fn test_fstring_vec_interpolation_ok() {
    typecheck_ok(r#"fn f() { let v: Vec[i64] = Vec.new(); let s = f"items: {v}"; }"#);
}

#[test]
fn test_fstring_function_value_rejects() {
    let errors = typecheck_errors(r#"fn g() {} fn f() { let s = f"fn: {g}"; }"#);
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for f-string with fn, got: {:?}",
        errors
    );
}

#[test]
fn test_fstring_struct_without_display_rejects() {
    let errors = typecheck_errors(
        r#"struct Pt { x: i64 }
           fn f() { let p = Pt { x: 1 }; let s = f"point: {p}"; }"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TraitBoundNotSatisfied),
        "Expected TraitBoundNotSatisfied for f-string with non-Display struct, got: {:?}",
        errors
    );
}

#[test]
fn test_println_too_many_args_rejects() {
    let errors = typecheck_errors(r#"fn f() { println("a", "b"); }"#);
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs),
        "Expected WrongNumberOfArgs for println with 2 args, got: {:?}",
        errors
    );
}

#[test]
fn test_string_sorted_returns_string() {
    let result = typecheck_ok(r#"fn f() -> String { let s = "hello"; s.sorted() }"#);
    assert!(result.errors.is_empty());
}

#[test]
fn test_string_sorted_by_returns_string() {
    let result = typecheck_ok(
        r#"fn cmp(a: char, b: char) -> bool { a < b }
           fn f() -> String { let s = "hello"; s.sorted_by(cmp) }"#,
    );
    assert!(result.errors.is_empty());
}

#[test]
fn test_string_sorted_wrong_arity_rejects() {
    let errors = typecheck_errors(r#"fn f() { let s = "hello"; s.sorted("extra"); }"#);
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs),
        "Expected WrongNumberOfArgs for sorted with arg, got: {:?}",
        errors
    );
}

// ── Numeric literal promotion (Q4) ────────────────────────────────────────

#[test]
fn test_literal_promotion_int_right() {
    // x: i32 + 5  — literal on right promoted to i32
    typecheck_ok(r#"fn f(x: i32) -> i32 { x + 5 }"#);
}

#[test]
fn test_literal_promotion_int_left() {
    // 5 + x: i32  — literal on left promoted to i32
    typecheck_ok(r#"fn f(x: i32) -> i32 { 5 + x }"#);
}

#[test]
fn test_literal_promotion_float_right() {
    // x: f32 + 1.5  — float literal on right promoted to f32
    typecheck_ok(r#"fn f(x: f32) -> f32 { x + 1.5 }"#);
}

#[test]
fn test_literal_promotion_int_to_float() {
    // x: f64 + 5  — integer literal on right promoted to f64
    typecheck_ok(r#"fn f(x: f64) -> f64 { x + 5 }"#);
}

#[test]
fn test_literal_promotion_comparison() {
    // x: i32 < 10  — literal in comparison promoted to i32
    typecheck_ok(r#"fn f(x: i32) -> bool { x < 10 }"#);
}

#[test]
fn test_literal_promotion_equality() {
    // x: u8 == 0  — literal promoted to u8 for equality
    typecheck_ok(r#"fn f(x: u8) -> bool { x == 0 }"#);
}

#[test]
fn test_literal_promotion_does_not_apply_to_non_numeric() {
    // bool + int should still fail — promotion only applies to numeric types
    let errors = typecheck_errors(r#"fn f(x: bool) { let _z = x + 1; }"#);
    assert!(
        !errors.is_empty(),
        "Expected type error for bool + int literal, got none"
    );
}

#[test]
fn test_literal_promotion_string_concat_no_int_promo() {
    // "hello" + 5 should still fail — String + int is not a valid op
    let errors = typecheck_errors(r#"fn f() { let _z = "hello" + 5; }"#);
    assert!(
        !errors.is_empty(),
        "Expected type error for String + int literal, got none"
    );
}

// ── std.http ──────────────────────────────────────────────────────────────────

#[test]
fn test_http_client_new_ok() {
    typecheck_ok("fn f() -> Client { Client.new() }");
}

#[test]
fn test_http_client_get_ok() {
    typecheck_ok(
        r#"fn f(c: Client) -> Result[Response, HttpError] { c.get("https://example.com") }"#,
    );
}

#[test]
fn test_http_client_post_ok() {
    typecheck_ok(
        r#"fn f(c: Client) -> Result[Response, HttpError] { c.post("https://example.com", "body") }"#,
    );
}

#[test]
fn test_http_client_get_wrong_arg_count() {
    let errors = typecheck_errors(r#"fn f(c: Client) { c.get(); }"#);
    assert!(!errors.is_empty(), "Expected error for get() with no args");
}

#[test]
fn test_http_response_status_ok() {
    typecheck_ok(r#"fn f(r: Response) -> i64 { r.status() }"#);
}

#[test]
fn test_http_response_body_ok() {
    typecheck_ok(r#"fn f(r: Response) -> String { r.body() }"#);
}

#[test]
fn test_http_response_header_ok() {
    typecheck_ok(r#"fn f(r: Response) -> Option[String] { r.header("content-type") }"#);
}

#[test]
fn test_http_error_message_ok() {
    typecheck_ok(r#"fn f(e: HttpError) -> String { e.message() }"#);
}

#[test]
fn test_http_result_pattern_match_ok() {
    typecheck_ok(
        r#"
fn f(c: Client) {
    match c.get("https://example.com") {
        Ok(resp) => println(resp.status()),
        Err(e) => println(e.message()),
    }
}
"#,
    );
}

// ── Trait associated function dispatch on type parameters ──────

#[test]
fn test_typeparam_assoc_fn_dispatch_ok() {
    // `T.default()` resolves through `T: Default` to the trait's associated
    // function; the return type is `T`, the function-level generic param.
    typecheck_ok(
        r#"
trait Default {
    fn default() -> Self;
}

fn make[T: Default]() -> T {
    T.default()
}
"#,
    );
}

#[test]
fn test_typeparam_assoc_fn_wrong_arg_count() {
    let errors = typecheck_errors(
        r#"
trait Default {
    fn default() -> Self;
}

fn make[T: Default]() -> T {
    T.default(42)
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("default")
                && e.message.contains("0")
                && e.message.contains("1")),
        "expected wrong-arg-count error, got: {errors:?}"
    );
}

#[test]
fn test_typeparam_assoc_fn_ambiguous_traits() {
    let errors = typecheck_errors(
        r#"
trait A {
    fn m() -> Self;
}
trait B {
    fn m() -> Self;
}

fn make[T: A + B]() -> T {
    T.m()
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::AmbiguousAssocFn
                && e.message.contains("ambiguous")
                && e.message.contains("`A`")
                && e.message.contains("`B`")),
        "expected ambiguity error, got: {errors:?}"
    );
}

#[test]
fn test_typeparam_assoc_fn_via_where_clause() {
    // Where-clause bound works the same as inline bound.
    typecheck_ok(
        r#"
trait Default {
    fn default() -> Self;
}

fn make[T]() -> T where T: Default {
    T.default()
}
"#,
    );
}

#[test]
fn test_typeparam_assoc_fn_with_trait_arg() {
    // Trait method with a non-Self parameter — verify substitution lowers
    // the param type with `Self → T` so the arg type-checks.
    typecheck_ok(
        r#"
trait FromI64 {
    fn from_i64(n: i64) -> Self;
}

fn make[T: FromI64]() -> T {
    T.from_i64(42)
}
"#,
    );
}

#[test]
fn test_typeparam_assoc_fn_arg_type_mismatch() {
    let errors = typecheck_errors(
        r#"
trait FromI64 {
    fn from_i64(n: i64) -> Self;
}

fn make[T: FromI64]() -> T {
    T.from_i64("not an int")
}
"#,
    );
    assert!(
        errors.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected type-mismatch error, got: {errors:?}"
    );
}

#[test]
fn test_typeparam_assoc_fn_method_not_in_bound() {
    // `T: Default` does not declare `from_str`; the call should not resolve
    // through the typeparam dispatch path. Today, the call falls through to
    // the existing identifier-as-value path, which yields `Type::Error`
    // silently — no AmbiguousAssocFn diagnostic fires. Step 7 will wire a
    // dedicated no-match diagnostic; for now anchor only the absence of a
    // false-positive ambiguity error.
    let parsed = parse(
        r#"
trait Default {
    fn default() -> Self;
}

fn make[T: Default]() -> T {
    T.from_str()
}
"#,
    );
    assert!(parsed.errors.is_empty());
    let resolved = resolve(&parsed.program);
    assert!(resolved.errors.is_empty());
    let result = typecheck(&parsed.program, &resolved);
    assert!(
        !result
            .errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::AmbiguousAssocFn),
        "unexpected ambiguity error for non-matching method, got: {:?}",
        result.errors
    );
}

// ── Bare-call expected-type inference ────────────────────────────

#[test]
fn test_bare_call_let_annotation_concrete_type() {
    typecheck_ok(
        r#"
trait Default {
    fn default() -> Self;
}

struct Wrapper { value: i64 }

impl Default for Wrapper {
    fn default() -> Wrapper { Wrapper { value: 0 } }
}

fn main() {
    let w: Wrapper = default();
}
"#,
    );
}

#[test]
fn test_bare_call_let_annotation_generic_param() {
    typecheck_ok(
        r#"
trait Default {
    fn default() -> Self;
}

fn make[T: Default]() -> T {
    default()
}

struct Wrapper { value: i64 }
impl Default for Wrapper {
    fn default() -> Wrapper { Wrapper { value: 0 } }
}

fn main() {
    let w: Wrapper = make();
}
"#,
    );
}

#[test]
fn test_bare_call_arg_position() {
    typecheck_ok(
        r#"
trait Default {
    fn default() -> Self;
}

struct Wrapper { value: i64 }
impl Default for Wrapper {
    fn default() -> Wrapper { Wrapper { value: 0 } }
}

fn take(w: Wrapper) {}

fn main() {
    take(default());
}
"#,
    );
}

#[test]
fn test_bare_call_no_expected_type_errors() {
    let errors = typecheck_errors(
        r#"
trait Default {
    fn default() -> Self;
}

fn main() {
    let x = default();
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::CannotInferAssocFn
                && e.message.contains("cannot infer type")
                && e.message.contains("default")),
        "expected CannotInferAssocFn error, got: {errors:?}"
    );
}

#[test]
fn test_bare_call_via_where_clause_bound() {
    typecheck_ok(
        r#"
trait Default {
    fn default() -> Self;
}

fn make[T]() -> T where T: Default {
    default()
}

struct Wrapper { value: i64 }
impl Default for Wrapper {
    fn default() -> Wrapper { Wrapper { value: 0 } }
}

fn main() {
    let w: Wrapper = make();
}
"#,
    );
}

#[test]
fn test_bare_call_concrete_type_with_arg() {
    typecheck_ok(
        r#"
trait FromI64 {
    fn from_i64(n: i64) -> Self;
}

struct Wrapper { value: i64 }
impl FromI64 for Wrapper {
    fn from_i64(n: i64) -> Wrapper { Wrapper { value: n } }
}

fn main() {
    let w: Wrapper = from_i64(42);
}
"#,
    );
}

#[test]
fn test_bare_call_typeparam_with_arg() {
    typecheck_ok(
        r#"
trait FromI64 {
    fn from_i64(n: i64) -> Self;
}

fn make[T: FromI64]() -> T {
    from_i64(42)
}

struct Wrapper { value: i64 }
impl FromI64 for Wrapper {
    fn from_i64(n: i64) -> Wrapper { Wrapper { value: n } }
}

fn main() {
    let w: Wrapper = make();
}
"#,
    );
}

#[test]
fn test_bare_call_does_not_shadow_normal_function() {
    // A regular free function with the same name as a trait assoc fn must
    // still resolve to the free function — bare-call inference only fires
    // for unresolvable identifiers.
    typecheck_ok(
        r#"
trait Default {
    fn default() -> Self;
}

fn default() -> i32 { 0 }

fn main() {
    let x = default();
}
"#,
    );
}

#[test]
fn test_self_assoc_fn_via_supertrait_in_default_body() {
    // Inside a trait default body, `Self.method()` should dispatch through
    // the supertrait's associated function. Verifies that supertraits land
    // as bounds on `Self` and the typeparam dispatch fires for `Self`.
    typecheck_ok(
        r#"
trait Default {
    fn default() -> Self;
}

trait Resettable: Default {
    fn reset() -> Self {
        Self.default()
    }
}
"#,
    );
}

// ── Unknown method-call diagnostic on user-defined types ───────────

#[test]
fn test_unknown_method_on_user_struct_errors() {
    // Calling a method that doesn't exist on a user struct (no impl
    // declares it) emits a clear diagnostic — historical silent
    // fall-through tightened for user-defined types.
    let errors = typecheck_errors(
        "struct Raw { n: i64 }
         fn main() {
             let r: Raw = Raw { n: 42 };
             let v: i64 = r.totally_bogus_method();
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("no method 'totally_bogus_method'")
                && e.message.contains("Raw")),
        "expected 'no method' diagnostic naming the type, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_unknown_method_on_user_enum_errors() {
    // Same tightening for user-defined enums.
    let errors = typecheck_errors(
        "enum Direction { North, South, East, West }
         fn main() {
             let d: Direction = Direction.North;
             let _ = d.no_such_method();
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("no method 'no_such_method'")
                && e.message.contains("Direction")),
        "expected 'no method' diagnostic naming the enum, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_known_method_on_user_struct_still_works() {
    // Regression: a method that *is* declared in an impl block continues
    // to typecheck. Confirms the tightening only fires on actually-missing
    // methods.
    typecheck_ok(
        "struct Counter { n: i64 }
         impl Counter {
             fn bump(self) -> Counter { Counter { n: self.n + 1 } }
         }
         fn main() {
             let c: Counter = Counter { n: 0 };
             let c2: Counter = c.bump();
         }",
    );
}

#[test]
fn test_unknown_method_on_prelude_type_still_silent() {
    // Built-in prelude types (Result, Option, etc.) have a partially-
    // implicit method surface — `.unwrap()` / `.is_ok()` etc. have no
    // static dispatch entry yet. Keep the historical silent fall-through
    // for these so legitimate stdlib method calls don't break.
    typecheck_ok(
        r#"fn main() {
             let r: Result[i64, String] = Result.Ok(42);
             let _ = r.unwrap();
         }"#,
    );
}

// ── Encoding namespace (Base64 / Hex / Url) ───────────────────────

#[test]
fn test_base64_encode_returns_str() {
    typecheck_ok(r#"fn f() -> String { let bs = [1u8, 2u8, 3u8]; Base64.encode(bs) }"#);
}

#[test]
fn test_base64_encode_url_safe_returns_str() {
    typecheck_ok(r#"fn f() -> String { let bs = [1u8, 2u8, 3u8]; Base64.encode_url_safe(bs) }"#);
}

#[test]
fn test_base64_decode_returns_result_vec_u8() {
    typecheck_ok(r#"fn f() -> Result[Vec[u8], DecodeError] { Base64.decode("AQID") }"#);
}

#[test]
fn test_hex_encode_returns_str() {
    typecheck_ok(r#"fn f() -> String { let bs = [255u8, 0u8]; Hex.encode(bs) }"#);
}

#[test]
fn test_hex_encode_upper_returns_str() {
    typecheck_ok(r#"fn f() -> String { let bs = [255u8, 0u8]; Hex.encode_upper(bs) }"#);
}

#[test]
fn test_hex_decode_returns_result_vec_u8() {
    typecheck_ok(r#"fn f() -> Result[Vec[u8], DecodeError] { Hex.decode("ff00") }"#);
}

#[test]
fn test_url_encode_returns_str() {
    typecheck_ok(r#"fn f() -> String { Url.encode("hello world") }"#);
}

#[test]
fn test_url_decode_returns_result_str() {
    typecheck_ok(r#"fn f() -> Result[String, DecodeError] { Url.decode("hello%20world") }"#);
}

// ── Iterator: `iter()` / `into_iter()` / `next()` (wip-list2 subtask 1) ──

#[test]
fn test_iter_on_vec_then_next_returns_option_of_element() {
    // Vec.iter().next() returns Option[T]; unwrap yields T.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter();
             let _n: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_into_iter_on_vec_returns_same_iterator_type() {
    // into_iter() and iter() both produce Iterator[T] at this layer; the
    // borrow-vs-consume distinction is design.md-only.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.into_iter();
             let _n: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_on_map_yields_kv_tuple() {
    // Map[K, V].iter() yields (K, V) tuples per design.md § Iteration.
    typecheck_ok(
        "fn main() {
             let m: Map[String, i64] = Map.new();
             let mut it = m.iter();
             let _pair: (String, i64) = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_on_set_yields_element() {
    typecheck_ok(
        "fn main() {
             let s: Set[i64] = Set.new();
             let mut it = s.iter();
             let _n: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_on_sorted_set_yields_element() {
    typecheck_ok(
        "fn main() {
             let s: SortedSet[i64] = SortedSet.new();
             let mut it = s.iter();
             let _n: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_on_array_literal_yields_element() {
    // Array[T, N] is also iterable; .iter() returns Iterator[T].
    typecheck_ok(
        "fn main() {
             let a: Array[i64, 3] = [1, 2, 3];
             let mut it = a.iter();
             let _n: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_with_extra_argument_rejected() {
    // iter() / into_iter() are nullary; passing args is a hard error.
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter(42);
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("'iter' takes no arguments")),
        "expected WrongNumberOfArgs for iter() with arg, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_next_with_extra_argument_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter();
             let _n = it.next(99);
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.next() takes no arguments")),
        "expected WrongNumberOfArgs for next() with arg, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

// ── for-loop on iterator values (wip-list2 subtask 2) ────────────

#[test]
fn test_for_loop_on_vec_iter_binds_element_type() {
    // `for x in v.iter()` binds `x` to the Iterator's Item type. The iter()
    // call returns Iterator[T]; element_type_of resolves Item through the
    // impl_assoc_types entry registered for Iterator at subtask 2.
    typecheck_ok(
        "fn add_one(n: i64) -> i64 { n + 1 }
         fn main() {
             let v: Vec[i64] = Vec.new();
             for x in v.iter() {
                 let _ = add_one(x);
             }
         }",
    );
}

#[test]
fn test_for_loop_on_map_iter_destructures_kv_tuple() {
    // Map.iter() yields (K, V); for-loop tuple pattern destructures through
    // the Iterator's Item type.
    typecheck_ok(
        "fn main() {
             let m: Map[String, i64] = Map.new();
             for (k, v) in m.iter() {
                 let _key: String = k;
                 let _val: i64 = v;
             }
         }",
    );
}

#[test]
fn test_for_loop_on_set_iter_binds_element() {
    typecheck_ok(
        "fn double(n: i64) -> i64 { n * 2 }
         fn main() {
             let s: Set[i64] = Set.new();
             for x in s.iter() {
                 let _ = double(x);
             }
         }",
    );
}

// ── Iterator adaptors: map / filter (wip-list2 subtask 3) ────────

#[test]
fn test_iter_map_changes_element_type_to_closure_return() {
    // `Iterator[i64].map(|x| x > 0) -> Iterator[bool]`. The closure's
    // return type pushes through the fresh `__iter_map_U` type param via
    // `check_call_args_with_substitution`.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().map(|x| x > 0);
             let _b: bool = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_filter_preserves_element_type() {
    // filter never changes the element type — input and output Item are
    // the same.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().filter(|x| x > 0);
             let _n: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_map_chain_threads_types() {
    // Stacked maps thread types through: i64 → bool → String.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().map(|x| x > 0).map(|b| if b { "yes" } else { "no" });
             let _s: String = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_map_with_typed_closure_param_accepted() {
    // Explicit closure-param annotations work alongside check-mode pushdown.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().map(|x: i64| x * 2);
             let _n: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_filter_predicate_must_return_bool() {
    // The closure return type is checked against `bool` — non-bool
    // returns produce a type-mismatch diagnostic.
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().filter(|x| x + 1);
         }",
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on non-bool predicate, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_map_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().map();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.map()")),
        "expected WrongNumberOfArgs for map() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_count_returns_i64() {
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _n: i64 = v.iter().count();
         }",
    );
}

#[test]
fn test_iter_count_after_filter_returns_i64() {
    // count() composes with lazy adaptors — filter then count.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _n: i64 = v.iter().filter(|x| x > 0).count();
         }",
    );
}

#[test]
fn test_iter_count_with_arg_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _n = v.iter().count(1);
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.count()")),
        "expected WrongNumberOfArgs for count(arg), got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_collect_returns_vec_of_item() {
    // collect() v1 returns Vec[T] where T is the iterator's item type.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _xs: Vec[i64] = v.iter().collect();
         }",
    );
}

#[test]
fn test_iter_collect_after_map_uses_mapped_item_type() {
    // The Item type of a `map(...)` iterator is the closure return type;
    // collect() picks that up.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let _xs: Vec[String] = v.iter().map(|x| if x > 0 { "pos" } else { "neg" }).collect();
         }"#,
    );
}

#[test]
fn test_iter_collect_with_arg_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _xs = v.iter().collect(1);
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.collect()")),
        "expected WrongNumberOfArgs for collect(arg), got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_fold_returns_accumulator_type() {
    // `fold(init, f)` returns init's type; the closure must thread A, T -> A.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _sum: i64 = v.iter().fold(0, |acc, x| acc + x);
         }",
    );
}

#[test]
fn test_iter_fold_can_change_accumulator_type_from_item_type() {
    // The accumulator can have a different type than the item type:
    // walk Vec[i64] with a String accumulator.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let _s: String = v.iter().fold(String.new(), |acc, _x| acc);
         }"#,
    );
}

#[test]
fn test_iter_fold_closure_must_return_acc_type() {
    // If the closure's body type doesn't match the accumulator type,
    // the closure check fails with TypeMismatch.
    let errs = typecheck_errors(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let _r = v.iter().fold(0, |_acc, _x| "wrong");
         }"#,
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on fold closure return, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_fold_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _r = v.iter().fold(0);
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.fold()")),
        "expected WrongNumberOfArgs for fold(init) only, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_any_returns_bool() {
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _b: bool = v.iter().any(|x| x > 0);
         }",
    );
}

#[test]
fn test_iter_all_returns_bool() {
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _b: bool = v.iter().all(|x| x > 0);
         }",
    );
}

#[test]
fn test_iter_any_after_map_uses_mapped_item_type_for_predicate() {
    // Composes with map — predicate sees the mapped type.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let _b: bool = v.iter().map(|x| if x > 0 { "pos" } else { "neg" }).any(|s| s == "pos");
         }"#,
    );
}

#[test]
fn test_iter_any_predicate_must_return_bool() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _b = v.iter().any(|x| x + 1);
         }",
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on non-bool any predicate, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_all_predicate_must_return_bool() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _b = v.iter().all(|x| x + 1);
         }",
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on non-bool all predicate, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_any_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _b = v.iter().any();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.any()")),
        "expected WrongNumberOfArgs for any() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_all_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _b = v.iter().all();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.all()")),
        "expected WrongNumberOfArgs for all() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_enumerate_yields_indexed_tuples() {
    // enumerate() returns Iterator[(i64, T)] — indexable via tuple
    // destructuring on next().
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().enumerate();
             let (i, x): (i64, i64) = it.next().unwrap();
             let _ = i + x;
         }",
    );
}

#[test]
fn test_iter_enumerate_after_map_uses_mapped_type_in_tuple() {
    // Map first, then enumerate — the second slot of the tuple is the
    // mapped type (String here), not the source's i64.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().map(|x| if x > 0 { "pos" } else { "neg" }).enumerate();
             let (_i, _s): (i64, String) = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_enumerate_with_arg_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().enumerate(0);
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.enumerate()")),
        "expected WrongNumberOfArgs for enumerate(arg), got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_take_returns_same_item_type() {
    // take(n) bounds the iterator but doesn't change the item type.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().take(3);
             let _x: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_skip_returns_same_item_type() {
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().skip(2);
             let _x: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_take_argument_must_be_integer() {
    // The bound must be i64 — passing a String is a TypeMismatch.
    let errs = typecheck_errors(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().take("nope");
         }"#,
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on take(non-int), got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_take_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().take();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.take()")),
        "expected WrongNumberOfArgs for take() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_skip_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().skip();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.skip()")),
        "expected WrongNumberOfArgs for skip() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_chain_preserves_element_type() {
    // chain(other) yields Iterator[T] where T matches both sides.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let w: Vec[i64] = Vec.new();
             let mut it = v.iter().chain(w.iter());
             let _x: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_chain_mismatched_element_type_rejected() {
    // Left yields i64; right yields String — chain rejects the
    // element-type mismatch.
    let errs = typecheck_errors(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let w: Vec[String] = Vec.new();
             let _it = v.iter().chain(w.iter());
         }"#,
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on chain element-type mismatch, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_chain_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().chain();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.chain()")),
        "expected WrongNumberOfArgs for chain() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_zip_yields_tuple_of_two_element_types() {
    // zip(other) yields Iterator[(T, U)]. Different types on each side.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let w: Vec[String] = Vec.new();
             let mut it = v.iter().zip(w.iter());
             let (_a, _b): (i64, String) = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_zip_with_mapped_other_uses_mapped_type() {
    // zip composes with map on the other side — the U slot reflects
    // the mapped type.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let w: Vec[i64] = Vec.new();
             let mut it = v.iter().zip(w.iter().map(|x| if x > 0 { "pos" } else { "neg" }));
             let (_a, _b): (i64, String) = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_zip_with_non_iterator_arg_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().zip(42);
         }",
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on zip(non-iter), got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_zip_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().zip();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.zip()")),
        "expected WrongNumberOfArgs for zip() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_take_while_returns_same_item_type() {
    // take_while(pred) is bound-by-predicate but doesn't change item type.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().take_while(|x| x < 10);
             let _x: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_skip_while_returns_same_item_type() {
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().skip_while(|x| x < 5);
             let _x: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_take_while_after_map_predicate_sees_mapped_type() {
    // map then take_while — predicate's parameter is the post-map type.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().map(|x| if x > 0 { "pos" } else { "neg" }).take_while(|s| s == "pos");
             let _s: String = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_take_while_predicate_must_return_bool() {
    // Predicate must return bool — i64 return is rejected.
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().take_while(|x| x + 1);
         }",
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on non-bool take_while predicate, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_skip_while_predicate_must_return_bool() {
    let errs = typecheck_errors(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().skip_while(|x| "nope");
         }"#,
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on non-bool skip_while predicate, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_take_while_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().take_while();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.take_while()")),
        "expected WrongNumberOfArgs for take_while() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_skip_while_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().skip_while();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.skip_while()")),
        "expected WrongNumberOfArgs for skip_while() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_flat_map_uses_inner_element_type() {
    // flat_map(f: Fn(T) -> Iterator[U]) -> Iterator[U] — closure
    // returns an Iterator[String]; the result Item is String.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().flat_map(|n| {
                 let inner: Vec[String] = Vec.new();
                 inner.iter()
             });
             let _s: String = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_flat_map_preserves_outer_element_type_in_pred() {
    // The closure parameter is the OUTER's element type (i64 here),
    // not the inner's. Verifies closure-pushdown threads T correctly.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().flat_map(|n: i64| {
                 let inner: Vec[String] = Vec.new();
                 inner.iter()
             });
             let _s: String = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_flat_map_inner_can_chain_adaptors() {
    // The closure body can return an iterator that has its own
    // adaptor chain — its element type after the chain is what
    // flat_map's result reflects.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().flat_map(|n| {
                 let inner: Vec[i64] = Vec.new();
                 inner.iter().map(|x| x > 0)
             });
             let _b: bool = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_flat_map_non_iterator_return_rejected() {
    // Closure returns i64 instead of an iterator — TypeMismatch.
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().flat_map(|n| n + 1);
         }",
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on non-iterator flat_map return, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_flat_map_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().flat_map();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.flat_map()")),
        "expected WrongNumberOfArgs for flat_map() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_step_by_returns_same_item_type() {
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().step_by(2);
             let _x: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_step_by_after_map_uses_mapped_type() {
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().map(|x| if x > 0 { "pos" } else { "neg" }).step_by(3);
             let _s: String = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_step_by_argument_must_be_integer() {
    let errs = typecheck_errors(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().step_by("nope");
         }"#,
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on step_by(non-int), got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_step_by_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().step_by();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.step_by()")),
        "expected WrongNumberOfArgs for step_by() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_cycle_returns_same_item_type() {
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().cycle().take(5);
             let _x: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_cycle_after_map_uses_mapped_type() {
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().map(|x| if x > 0 { "pos" } else { "neg" }).cycle().take(3);
             let _s: String = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_cycle_with_arg_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().cycle(5);
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.cycle()")),
        "expected WrongNumberOfArgs for cycle(arg), got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_inspect_returns_same_item_type() {
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().inspect(|x| println(x));
             let _x: i64 = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_inspect_after_map_uses_mapped_type() {
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().map(|x| if x > 0 { "pos" } else { "neg" }).inspect(|s| println(s));
             let _s: String = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_inspect_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().inspect();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.inspect()")),
        "expected WrongNumberOfArgs for inspect() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_scan_yields_inner_value_type() {
    // scan(init: i64, f: |state, item| -> Option<(i64, String)>) →
    // Iterator[String]. The yielded item is the second tuple slot.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().scan(0, |state, item| {
                 let new_state = state + item;
                 Some((new_state, "tick"))
             });
             let _s: String = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_scan_state_type_inferred_from_init() {
    // The state type is locked from `init`. Closure's first
    // parameter must agree.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().scan("", |state, item| {
                 let new_state = state;
                 Some((new_state, item))
             });
             let _x: i64 = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_scan_non_option_return_rejected() {
    // Closure returns i64 directly instead of Option — TypeMismatch.
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().scan(0, |state, item| state + item);
         }",
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on non-Option scan return, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_scan_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().scan(0);
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.scan()")),
        "expected WrongNumberOfArgs for scan() with 1 arg, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_peekable_returns_peekable_of_same_item_type() {
    // peekable() on Iterator[T] yields Peekable[T]; both peek() and
    // next() then yield Option<T>.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut p = v.iter().peekable();
             let _peeked: i64 = p.peek().unwrap();
             let _consumed: i64 = p.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_peekable_after_map_uses_mapped_type() {
    // The Item type that flows through peekable is the post-map type.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut p = v.iter().map(|x| if x > 0 { "pos" } else { "neg" }).peekable();
             let _s: String = p.peek().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_peek_on_plain_iterator_rejected() {
    // peek() is only on Peekable[T] — calling it on a bare Iterator
    // should raise a type error rather than silently dispatching.
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter();
             let _x = it.peek();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::TypeMismatch && e.message.contains("peek()")),
        "expected TypeMismatch for peek() on non-Peekable, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_peek_after_adaptor_chain_loses_peekable() {
    // After .map() on a Peekable, the result is Iterator[U] (NOT
    // Peekable[U]) — so .peek() further down the chain is rejected.
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().peekable().map(|x| x * 2);
             let _x = it.peek();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::TypeMismatch && e.message.contains("peek()")),
        "expected TypeMismatch for peek() after map(), got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_peekable_supports_iterator_methods() {
    // Adaptor / terminal methods dispatch normally on Peekable since
    // it's still an iterator. count, collect, map, filter, etc.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _n: i64 = v.iter().peekable().count();
             let _xs: Vec[i64] = v.iter().peekable().filter(|x| x > 0).collect();
         }",
    );
}

#[test]
fn test_iter_peekable_with_arg_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _p = v.iter().peekable(1);
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.peekable()")),
        "expected WrongNumberOfArgs for peekable() with arg, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_peek_with_arg_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut p = v.iter().peekable();
             let _x = p.peek(1);
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Peekable.peek()")),
        "expected WrongNumberOfArgs for peek() with arg, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_chunk_by_yields_iterator_of_vec() {
    // chunk_by(key_fn: Fn(T) -> K) -> Iterator[Vec[T]]. The yielded
    // item per pull is a Vec[T] of grouped consecutive elements.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().chunk_by(|x| x % 2);
             let _g: Vec[i64] = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_chunk_by_after_map_uses_mapped_item_type() {
    // The Vec elements carry the post-map type; key_fn receives the
    // post-map element.
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter()
                 .map(|x| if x > 0 { "pos" } else { "neg" })
                 .chunk_by(|s| s);
             let _g: Vec[String] = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_chunk_by_key_fn_can_return_any_type() {
    // K is a free type parameter (TypeParam pushdown) — equality is
    // a runtime concern. Closure may return tuples / strings / etc.
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().chunk_by(|x| (x % 2, x > 10));
         }",
    );
}

#[test]
fn test_iter_chunk_by_chains_with_collect() {
    // Each group is Vec[T]; collecting yields Vec[Vec[T]].
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _groups: Vec[Vec[i64]] = v.iter().chunk_by(|x| x).collect();
         }",
    );
}

#[test]
fn test_iter_chunks_yields_iterator_of_vec() {
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().chunks(2);
             let _g: Vec[i64] = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_windows_yields_iterator_of_vec() {
    typecheck_ok(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter().windows(3);
             let _g: Vec[i64] = it.next().unwrap();
         }",
    );
}

#[test]
fn test_iter_chunks_after_map_uses_mapped_type() {
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter()
                 .map(|x| if x > 0 { "pos" } else { "neg" })
                 .chunks(2);
             let _g: Vec[String] = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_windows_after_map_uses_mapped_type() {
    typecheck_ok(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let mut it = v.iter()
                 .map(|x| if x > 0 { "pos" } else { "neg" })
                 .windows(2);
             let _g: Vec[String] = it.next().unwrap();
         }"#,
    );
}

#[test]
fn test_iter_chunks_argument_must_be_integer() {
    let errs = typecheck_errors(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().chunks("two");
         }"#,
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on string chunks() arg, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_windows_argument_must_be_integer() {
    let errs = typecheck_errors(
        r#"fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().windows("two");
         }"#,
    );
    assert!(
        errs.iter().any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on string windows() arg, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_chunks_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().chunks();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.chunks()")),
        "expected WrongNumberOfArgs for chunks() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_windows_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().windows();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.windows()")),
        "expected WrongNumberOfArgs for windows() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

#[test]
fn test_iter_chunk_by_wrong_arg_count_rejected() {
    let errs = typecheck_errors(
        "fn main() {
             let v: Vec[i64] = Vec.new();
             let _it = v.iter().chunk_by();
         }",
    );
    assert!(
        errs.iter()
            .any(|e| e.kind == TypeErrorKind::WrongNumberOfArgs
                && e.message.contains("Iterator.chunk_by()")),
        "expected WrongNumberOfArgs for chunk_by() with no args, got: {:?}",
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    );
}

// ── Bidirectional sub-step 1: branch check-mode pushdown (item 131) ─

#[test]
fn test_branch_pushdown_if_else_closure_at_typed_let() {
    // Both branches are unannotated closures; the typed let pushes
    // `Fn(i64) -> i64` into both, so each `|x| ...` body sees `x: i64`.
    // Without the If check arm, infer_expr would give each closure
    // fresh TypeVars and the trailing check_assignable would fail.
    typecheck_ok(
        "fn main() {\n\
             let cond: bool = true;\n\
             let f: Fn(i64) -> i64 = if cond { |x| x + 1 } else { |x| x - 1 };\n\
         }",
    );
}

#[test]
fn test_branch_pushdown_match_closure_at_typed_let() {
    // Same shape as the if/else test, but for match arms.
    typecheck_ok(
        "fn main() {\n\
             let n: i64 = 0;\n\
             let f: Fn(i64) -> i64 = match n {\n\
                 0 => |x| x + 1,\n\
                 _ => |x| x - 1,\n\
             };\n\
         }",
    );
}

#[test]
fn test_branch_pushdown_block_trailing_closure() {
    // Block at check position: the trailing closure literal should
    // see the let's expected type.
    typecheck_ok(
        "fn main() {\n\
             let f: Fn(i64) -> i64 = { |x| x + 1 };\n\
         }",
    );
}

#[test]
fn test_branch_pushdown_if_else_at_function_return() {
    // The function's declared return type flows into the tail
    // expression via check_block_against; from there, the new If
    // arm pushes it into both branches.
    typecheck_ok(
        "fn make_adder(cond: bool) -> Fn(i64) -> i64 {\n\
             if cond { |x| x + 1 } else { |x| x - 1 }\n\
         }",
    );
}

#[test]
fn test_branch_pushdown_match_at_call_argument() {
    // Match at a call-argument position; the parameter's type
    // pushes into each arm's body.
    typecheck_ok(
        "fn apply(f: Fn(i64) -> i64) -> i64 { f(10) }\n\
         fn main() {\n\
             let n: i64 = 1;\n\
             let _ = apply(match n {\n\
                 0 => |x| x + 1,\n\
                 _ => |x| x * 2,\n\
             });\n\
         }",
    );
}

#[test]
fn test_branch_pushdown_arm_mismatch_diagnoses_offending_arm() {
    // One match arm doesn't comply with the expected closure shape.
    // Each arm's check_expr emits its own TypeMismatch, so the
    // diagnostic names the offending arm (first/second-class via
    // span) rather than the synth path's aggregate
    // BranchTypeMismatch on the whole match expression.
    let errors = typecheck_errors(
        "fn main() {\n\
             let n: i64 = 1;\n\
             let _f: Fn(i64) -> i64 = match n {\n\
                 0 => |x| x + 1,\n\
                 _ => 42,\n\
             };\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::TypeMismatch)),
        "expected TypeMismatch from non-Fn arm, got: {errors:?}"
    );
}

#[test]
fn test_branch_pushdown_if_let_else_closure() {
    // IfLet at check position: both the then-block and the else
    // expression see the expected type.
    typecheck_ok(
        "fn main() {\n\
             let opt: Option[i64] = Some(7);\n\
             let f: Fn(i64) -> i64 = if let Some(_v) = opt { |x| x + 1 } else { |x| x };\n\
         }",
    );
}

// ── Bidirectional sub-step 2a: unsolved-T diagnostic (item 131) ─

#[test]
fn test_unsolved_t_generic_call_returning_vec_t() {
    // A user-defined generic that returns `Vec[T]` with no arg from
    // which to solve T. Lands cleanly at the synth let position.
    let errors = typecheck_errors(
        "fn empty_vec[T]() -> Vec[T] { todo() }\n\
         fn main() { let v = empty_vec(); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::CannotInferTypeParam)
                && e.message.contains("'T'")),
        "expected CannotInferTypeParam naming 'T', got: {errors:?}"
    );
}

#[test]
fn test_unsolved_t_none_at_unannotated_let() {
    // `let x = None;` — the `None` constructor produces
    // `Option[TypeParam("T")]` per CR-32. Without an annotation,
    // T cannot be inferred. Pre-fix: silently typed as Option[T]
    // and the binding becomes useless.
    let errors = typecheck_errors("fn main() { let x = None; }");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::CannotInferTypeParam)),
        "expected CannotInferTypeParam for unannotated None, got: {errors:?}"
    );
}

#[test]
fn test_unsolved_t_generic_id_no_arg_context() {
    // `let v = id();` — id is a generic identity-style helper whose
    // signature `[T]() -> T` has no argument from which to solve T.
    // This is the cleanest "no consumer" case.
    let errors = typecheck_errors(
        "fn id[T]() -> T { todo() }\n\
         fn main() { let v = id(); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::CannotInferTypeParam)
                && e.message.contains("'T'")),
        "expected CannotInferTypeParam, got: {errors:?}"
    );
}

#[test]
fn test_unsolved_t_annotation_silences_diagnostic() {
    // Same `id()` call, but with an annotation: check_expr's pushdown
    // pins T = i64 and no diagnostic should fire.
    typecheck_ok(
        "fn id[T]() -> T { todo() }\n\
         fn main() { let v: i64 = id(); }",
    );
}

#[test]
fn test_unsolved_t_concrete_arg_silences_diagnostic() {
    // Generic identity called with a concrete argument: the arg
    // pins T = i64 via solve_type_params; no unsolved metavar.
    typecheck_ok(
        "fn id[T](x: T) -> T { x }\n\
         fn main() { let v = id(7); }",
    );
}

#[test]
fn test_unsolved_t_only_at_synthesis_let() {
    // The diagnostic fires only at the let-without-annotation
    // position. A generic call inside a discarded statement
    // expression (e.g. wrapped in {}) produces no binding, so no
    // diagnostic is currently expected — the consuming check_expr
    // path either pins it or accepts the discarded result.
    typecheck_ok(
        "fn id[T]() -> T { todo() }\n\
         fn pin() -> i64 { id() }\n\
         fn main() { let _ = pin(); }",
    );
}

#[test]
fn test_unsolved_t_in_enclosing_generic_does_not_fire() {
    // Inside `fn outer[U]()`, `U` is a legitimately-unsolved type
    // param at the local site — it's bound by the enclosing function.
    // The diagnostic must skip names that match an enclosing generic.
    typecheck_ok(
        "fn id[T](x: T) -> T { x }\n\
         fn outer[U](u: U) {\n\
             let v = id(u);\n\
         }",
    );
}

// ── Bidirectional sub-step 2b: fresh-metavar instantiation (item 131) ─

#[test]
fn test_fresh_metavar_nested_id_calls_distinct_metavars() {
    // `id(id(7))` — outer T and inner T have the same name but each
    // call site instantiates a fresh metavariable (`?M_n`), so the
    // two never collide. Both resolve to `i64` from the literal `7`.
    typecheck_ok(
        "fn id[T](x: T) -> T { x }\n\
         fn main() { let v: i64 = id(id(7)); }",
    );
}

#[test]
fn test_fresh_metavar_closure_arg_sees_solved_slot() {
    // `apply` takes a `Fn(T) -> T` and a `T`. The `T` is solved
    // from the second arg (`5`); the closure's `Fn(T) -> T` slot
    // resolves to `Fn(i64) -> i64` and check_expr's pushdown gives
    // the closure param `x` type `i64`. Pre-sub-2b this worked
    // through `solve_type_params`; verifying it still works after
    // the fresh-metavar migration.
    typecheck_ok(
        "fn apply[T](f: Fn(T) -> T, x: T) -> T { f(x) }\n\
         fn main() {\n\
             let v: i64 = apply(|x| x + 1, 5);\n\
         }",
    );
}

#[test]
fn test_fresh_metavar_two_call_sites_independent() {
    // Two independent call sites of the same generic function.
    // Sub-step 2b's per-call instantiation guarantees the metavar
    // for site 1 (i64) doesn't pollute site 2 (String). Pre-2b
    // this also worked because `solutions` was a fresh HashMap per
    // call; preserved here.
    typecheck_ok(
        "fn id[T](x: T) -> T { x }\n\
         fn main() {\n\
             let a = id(7);\n\
             let b = id(\"hi\");\n\
         }",
    );
}

#[test]
fn test_fresh_metavar_unsolved_metavar_surfaces_via_2a() {
    // Unsolved metavar in the return type comes back as
    // `TypeParam(originating_name)` so slice 2a's
    // `find_unbound_type_param` still detects it. This verifies the
    // resolve_type_vars → TypeParam fallback works end-to-end.
    let errors = typecheck_errors(
        "fn empty[T]() -> Vec[T] { todo() }\n\
         fn main() { let v = empty(); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::CannotInferTypeParam)
                && e.message.contains("'T'")),
        "expected CannotInferTypeParam naming 'T' (from resolve_type_vars TypeVar→TypeParam fallback), got: {errors:?}"
    );
}

#[test]
fn test_fresh_metavar_multi_param_solved_independently() {
    // `swap[A, B](a: A, b: B) -> Tuple[B, A]` — each call instantiates
    // two distinct metavars; both are solved from their respective
    // args. Tuples thread generics correctly so the result-type
    // recovery is end-to-end testable here without depending on the
    // struct-literal generic-threading gap.
    typecheck_ok(
        "fn swap[A, B](a: A, b: B) -> (B, A) { (b, a) }\n\
         fn main() {\n\
             let p: (bool, i64) = swap(7, true);\n\
         }",
    );
}

// ── Bidirectional sub-step 3: function-type subsumption (item 131) ─

#[test]
fn test_subsume_function_into_oncefn_let_slot() {
    // A capture-free closure synthesizes as `Type::Function(i64) -> i64`.
    // The let annotation is `OnceFn(i64) -> i64`. Sub-step 3's `is_subtype`
    // admits this via the cross-arm Fn → OnceFn subsumption rule
    // (a repeatable callable trivially satisfies the callable-once contract).
    typecheck_ok(
        "fn main() {\n\
             let f: OnceFn(i64) -> i64 = |x| x + 1;\n\
             let _ = f(7);\n\
         }",
    );
}

#[test]
fn test_subsume_function_into_oncefn_call_arg_slot() {
    // Function-typed closure argument flows into a parameter slot typed as
    // `OnceFn(i64) -> i64`. The slot's contract permits a single call;
    // a Fn closure can be called once just fine.
    typecheck_ok(
        "fn run(f: OnceFn(i64) -> i64) -> i64 { f(5) }\n\
         fn main() {\n\
             let r = run(|x| x + 1);\n\
         }",
    );
}

#[test]
fn test_subsume_oncefn_into_fn_slot_still_rejects() {
    // Regression for round 12.45 / E0235: the reverse direction stays
    // rejected. A closure that consumes a captured non-Copy binding
    // synthesizes as OnceFunction; passing it into a `Fn(...)` slot must
    // still produce OnceFnIntoFnSlot, not slip through the new
    // subsumption admit-arm (which only fires in the upward direction).
    let errors = typecheck_errors(
        "struct Cfg { name: i64 }\n\
         fn run(f: Fn() -> i64) -> i64 { f() }\n\
         fn main() {\n\
             let c = Cfg { name: 7 };\n\
             let r = run(|| { let _ = c; 0 });\n\
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, TypeErrorKind::OnceFnIntoFnSlot)),
        "expected OnceFnIntoFnSlot for OnceFn → Fn (sub-step 3 must not weaken \
         the rejection direction); got: {errors:?}"
    );
}

#[test]
fn test_subsume_function_into_oncefn_through_intermediate_let() {
    // Sub-step 3 admits the upward direction across multi-step flow: a
    // closure stored in a `Fn(...)`-typed binding (so the value is a
    // first-class Function, not a closure literal) is then passed into an
    // OnceFn slot. The check_assignable on the call arg sees
    // (expected = OnceFunction, found = Function) and is_subtype admits.
    typecheck_ok(
        "fn run(f: OnceFn(i64) -> i64) -> i64 { f(5) }\n\
         fn main() {\n\
             let f: Fn(i64) -> i64 = |x| x + 1;\n\
             let r = run(f);\n\
         }",
    );
}

#[test]
fn test_subsume_function_identity_unchanged() {
    // Function → Function with matching signature still typechecks. Sanity
    // pin: the new is_subtype rule for the same-arm case (contravariant
    // params + covariant return) reduces to identity for primitives, so
    // existing Fn-to-Fn flows are not regressed.
    typecheck_ok(
        "fn run(f: Fn(i64) -> i64) -> i64 { f(5) }\n\
         fn main() {\n\
             let f: Fn(i64) -> i64 = |x| x + 1;\n\
             let r = run(f);\n\
         }",
    );
}

// ── #[compiler_builtin] dispatch (CR-202 slice 2) ───────────────
// Stdlib-source declarations of `#[compiler_builtin] fn foo(...)` register
// their signature in env.functions (the contract callers are checked
// against) and add their name to env.compiler_builtins (the marker that
// the body is replaced by Rust dispatch and should not be type-checked).
// Test fixtures load via `with_stdlib_source(true)` since slice 1's
// resolver gate (`E0237`) rejects the attribute outside stdlib source.

fn typecheck_stdlib_source(source: &str) -> TypeCheckResult {
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let resolved = karac::resolver::Resolver::new(&parsed.program)
        .with_stdlib_source(true)
        .resolve();
    assert!(
        resolved.errors.is_empty(),
        "resolve errors: {:?}",
        resolved
            .errors
            .iter()
            .map(|e| &e.message)
            .collect::<Vec<_>>()
    );
    typecheck(&parsed.program, &resolved)
}

#[test]
fn test_compiler_builtin_registers_signature_and_marks_intrinsic() {
    let result =
        typecheck_stdlib_source("#[compiler_builtin]\nfn id_intrinsic[T](v: T) -> T { v }");
    assert!(result.errors.is_empty(), "type errors: {:?}", result.errors);
    assert!(
        result.compiler_builtins.contains("id_intrinsic"),
        "compiler_builtins should contain id_intrinsic, got: {:?}",
        result.compiler_builtins
    );
}

#[test]
fn test_compiler_builtin_body_is_not_type_checked() {
    // Body returns a literal i64 (`42`) but the declared return type is `T`.
    // Without slice 2's body-skip, this would surface as a TypeMismatch.
    // With it, the body is treated as a placeholder that Rust dispatch
    // replaces, so the (deliberately wrong) body passes silently.
    let result =
        typecheck_stdlib_source("#[compiler_builtin]\nfn id_intrinsic[T](v: T) -> T { 42 }");
    assert!(
        result.errors.is_empty(),
        "expected no errors (body should be skipped), got: {:?}",
        result.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert!(result.compiler_builtins.contains("id_intrinsic"));
}

#[test]
fn test_compiler_builtin_signature_validates_caller() {
    // The stdlib-source declaration registers `id_intrinsic[T](T) -> T`.
    // A user-side caller that respects the signature (i64 in, i64 out)
    // should typecheck cleanly.
    let result = typecheck_stdlib_source(
        "#[compiler_builtin]\nfn id_intrinsic[T](v: T) -> T { v }\n\
         fn use_it() -> i64 { id_intrinsic(42) }",
    );
    assert!(
        result.errors.is_empty(),
        "expected no errors, got: {:?}",
        result.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_compiler_builtin_signature_rejects_mismatched_caller_return() {
    // Same intrinsic, caller declares `-> bool` but the call returns i64
    // (T solved to i64 from the argument). The signature-driven check
    // catches the return-type mismatch.
    let parsed = parse(
        "#[compiler_builtin]\nfn id_intrinsic[T](v: T) -> T { v }\n\
         fn use_it() -> bool { id_intrinsic(42) }",
    );
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let resolved = karac::resolver::Resolver::new(&parsed.program)
        .with_stdlib_source(true)
        .resolve();
    assert!(
        resolved.errors.is_empty(),
        "resolve errors: {:?}",
        resolved
            .errors
            .iter()
            .map(|e| &e.message)
            .collect::<Vec<_>>()
    );
    let result = typecheck(&parsed.program, &resolved);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::TypeMismatch),
        "expected TypeMismatch on bool/i64, got: {:?}",
        result
            .errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_compiler_builtin_existing_intrinsic_dbg_round_trip() {
    // Slice 2's stated equivalence check: declaring an existing intrinsic
    // (`dbg`) as `#[compiler_builtin]` in stdlib source should produce the
    // same observable typecheck behavior as today's `register_builtin_types`
    // shim path. The hardcoded typechecker path for `dbg` already accepts
    // any T → T call shape; the stdlib-source declaration adds the
    // signature to env.functions and marks it as a builtin, but the call
    // still typechecks via the existing path. This test pins the
    // no-regression property.
    let result = typecheck_stdlib_source(
        "#[compiler_builtin]\nfn dbg[T](v: T) -> T { v }\n\
         fn use_it() -> i64 { dbg(7) }",
    );
    assert!(
        result.errors.is_empty(),
        "expected no errors, got: {:?}",
        result.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert!(result.compiler_builtins.contains("dbg"));
}

#[test]
fn test_compiler_builtin_empty_in_user_only_program() {
    // Sanity pin: a user-only program (without with_stdlib_source) cannot
    // populate compiler_builtins — slice 1 rejects the attribute outright,
    // and the resolver-gate check fires before the typechecker sees it.
    // Confirm the registry is empty for an ordinary program.
    let result = typecheck_ok("fn ordinary() -> i64 { 0 }");
    assert!(
        result.compiler_builtins.is_empty(),
        "expected empty registry for user-only program, got: {:?}",
        result.compiler_builtins
    );
}

// ── Baked Option (CR-202 slice 3d verification) ─────────────────
// Pin that the source-of-truth swap (slice 3c) actually went through:
// `enum_info["Option"]` reads back the shape declared in
// `runtime/stdlib/option.kara`, not the legacy hardcoded shape from
// `register_builtin_types`. If a future refactor accidentally drops the
// baked source from `register_builtin_types`'s leading walk, this test
// fails — the legacy shape is gone, so `enum_info["Option"]` would be
// missing entirely.

#[test]
fn baked_option_registers_correct_variant_shape_in_enum_info() {
    let result = typecheck_ok("");
    let info = result
        .enum_info
        .get("Option")
        .expect("Option must be registered in enum_info from baked source");
    assert_eq!(info.generic_params, vec!["T".to_string()]);
    assert_eq!(info.variants.len(), 2, "Option has exactly two variants");

    let some = info
        .variants
        .iter()
        .find(|(n, _)| n == "Some")
        .expect("Some variant present");
    match &some.1 {
        VariantTypeInfo::Tuple(types) => {
            assert_eq!(types.len(), 1, "Some carries one payload type");
            assert_eq!(
                types[0],
                Type::TypeParam("T".to_string()),
                "Some(T) payload should be the generic param T"
            );
        }
        other => panic!("Some should be Tuple-shaped, got {:?}", other),
    }

    let none = info
        .variants
        .iter()
        .find(|(n, _)| n == "None")
        .expect("None variant present");
    assert_eq!(
        none.1,
        VariantTypeInfo::Unit,
        "None should be a Unit variant"
    );
}

#[test]
fn baked_option_carries_structural_derived_traits() {
    // The hardcoded path inserted these traits directly. After 3c, they
    // come from `#[derive(Eq, PartialEq, Hash, Ord, PartialOrd)]` on the
    // baked source. If the derive annotation is dropped (or
    // `extract_derived_traits` is skipped on baked items), Option's
    // `==`/`Hash`/`Ord` participation breaks and this catches it.
    let result = typecheck_ok("");
    let info = result.enum_info.get("Option").expect("Option registered");
    for trait_name in ["Eq", "PartialEq", "Hash", "Ord", "PartialOrd"] {
        assert!(
            info.derived_traits.contains(trait_name),
            "Option should derive {}, derived_traits = {:?}",
            trait_name,
            info.derived_traits
        );
    }
    assert!(
        !info.is_shared,
        "Option is not declared as shared (no `shared enum`)"
    );
}

#[test]
fn baked_option_user_code_still_typechecks() {
    // End-to-end behavioral pin: regular Option construction, pattern
    // matching, and method dispatch all continue to typecheck against
    // the baked declaration just as they did against the hardcoded
    // shape. If the swap left any caller path looking at the wrong
    // EnumInfo, this surfaces it.
    typecheck_ok(
        "fn use_option() -> i64 {\n\
             let x: Option[i64] = Some(42);\n\
             match x {\n\
                 Some(v) => v,\n\
                 None => 0,\n\
             }\n\
         }",
    );
}

// ── Baked Result (CR-202 slice 4a verification) ────────────────
// Mirrors the slice-3d Option pins. `Result[T, E]`'s shape now lives
// in `runtime/stdlib/result.kara`; the hardcoded `EnumInfo` arm in
// `register_builtin_types` is gone. These tests confirm that the swap
// preserved the contract the typechecker depends on — particularly
// the `Ok` / `Err` variant names, which several non-method paths
// (`?`-operator desugaring, `From`-on-error coercion) name-match.

#[test]
fn baked_result_registers_correct_variant_shape_in_enum_info() {
    let result = typecheck_ok("");
    let info = result
        .enum_info
        .get("Result")
        .expect("Result must be registered in enum_info from baked source");
    assert_eq!(info.generic_params, vec!["T".to_string(), "E".to_string()]);
    assert_eq!(info.variants.len(), 2, "Result has exactly two variants");

    let ok = info
        .variants
        .iter()
        .find(|(n, _)| n == "Ok")
        .expect("Ok variant present");
    match &ok.1 {
        VariantTypeInfo::Tuple(types) => {
            assert_eq!(types.len(), 1, "Ok carries one payload type");
            assert_eq!(
                types[0],
                Type::TypeParam("T".to_string()),
                "Ok(T) payload should be the generic param T"
            );
        }
        other => panic!("Ok should be Tuple-shaped, got {:?}", other),
    }

    let err = info
        .variants
        .iter()
        .find(|(n, _)| n == "Err")
        .expect("Err variant present");
    match &err.1 {
        VariantTypeInfo::Tuple(types) => {
            assert_eq!(types.len(), 1, "Err carries one payload type");
            assert_eq!(
                types[0],
                Type::TypeParam("E".to_string()),
                "Err(E) payload should be the generic param E"
            );
        }
        other => panic!("Err should be Tuple-shaped, got {:?}", other),
    }
}

#[test]
fn baked_result_carries_structural_derived_traits() {
    let result = typecheck_ok("");
    let info = result.enum_info.get("Result").expect("Result registered");
    for trait_name in ["Eq", "PartialEq", "Hash", "Ord", "PartialOrd"] {
        assert!(
            info.derived_traits.contains(trait_name),
            "Result should derive {}, derived_traits = {:?}",
            trait_name,
            info.derived_traits
        );
    }
    assert!(!info.is_shared);
}

#[test]
fn baked_result_user_code_still_typechecks() {
    // Construction + pattern-match end-to-end behavioral pin.
    typecheck_ok(
        "fn use_result() -> i64 {\n\
             let x: Result[i64, String] = Ok(42);\n\
             match x {\n\
                 Ok(v) => v,\n\
                 Err(_) => 0,\n\
             }\n\
         }",
    );
}

#[test]
fn baked_result_question_operator_still_desugars() {
    // The `?` operator name-matches `Ok` / `Err` variant names. If the
    // baked source used different names (or the swap registered them
    // wrong), `?` desugaring would fail to typecheck. Pin the contract.
    typecheck_ok(
        "fn inner() -> Result[i64, String] { Ok(7) }\n\
         fn outer() -> Result[i64, String] {\n\
             let v = inner()?;\n\
             Ok(v + 1)\n\
         }",
    );
}

// ── Baked Vec (CR-202 slice 4b verification) ───────────────────
// Vec moves from the collective `["Vec", "Array", ...]` loop in
// `register_builtin_types` to `runtime/stdlib/vec.kara`. The collective
// loop's `impl_assoc_types` insert for `("Vec", "Item")` is preserved
// explicitly outside the loop so `for x in v.iter()` element-type
// resolution keeps working. The legacy collective-loop entry set
// `derived_traits = empty`; the baked source has no `#[derive(...)]`
// to match.

#[test]
fn baked_vec_registers_correct_struct_shape() {
    let result = typecheck_ok("");
    let info = result
        .struct_info
        .get("Vec")
        .expect("Vec must be registered in struct_info from baked source");
    assert_eq!(info.generic_params, vec!["T".to_string()]);
    assert!(
        info.fields.is_empty(),
        "Vec[T] is opaque (no public fields)"
    );
    assert!(
        info.derived_traits.is_empty(),
        "Vec carries no structural derives at the type level (matches the legacy hardcoded path)"
    );
    assert!(!info.is_shared, "Vec is not declared `shared`");
    assert!(!info.no_rc, "Vec is not declared `@no_rc`");
}

#[test]
fn baked_vec_user_code_still_typechecks() {
    // Existing Vec methods (`new`, `push`, `len`, `iter`) dispatch
    // through the hardcoded `infer_vec_method`. They reference
    // `env.structs["Vec"].generic_params` for receiver-arity checks.
    // If the baked-source registration produced a different shape,
    // these calls would fail.
    typecheck_ok(
        "fn build() -> Vec[i64] {\n\
             let v: Vec[i64] = Vec.new();\n\
             v\n\
         }",
    );
}

#[test]
fn baked_vec_for_loop_resolves_element_type() {
    // `for x in v.iter()` walks `impl_assoc_types[("Vec", "Item")]` to
    // find T. CR-202 slice 4b restores this entry explicitly outside
    // the collective loop after pulling Vec out of it. This test fails
    // if that explicit insert is dropped.
    typecheck_ok(
        "fn sum(v: Vec[i64]) -> i64 {\n\
             let mut total = 0;\n\
             for x in v.iter() {\n\
                 total = total + x;\n\
             }\n\
             total\n\
         }",
    );
}

// ── Baked PartialEq (CR-202 slice 5a verification) ─────────────
// Slice 5a is the first baked stdlib *trait*. PartialEq isn't
// registered in `register_stdlib_traits` today (only Eq is), so this
// is a strictly additive change: `env.traits["PartialEq"]` becomes
// queryable, and user code can write `impl PartialEq for MyType` as a
// real trait impl rather than relying on `#[derive(PartialEq)]`.
//
// Tests are behavioral: a user struct that declares an impl block for
// PartialEq must typecheck against the registered trait. If the bake
// walk failed to register the trait, `impl PartialEq for ...` would
// fail with an unresolved-trait error.

#[test]
fn baked_partial_eq_user_impl_typechecks() {
    typecheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         impl PartialEq for Point {\n\
             fn eq(ref self, other: ref Point) -> bool {\n\
                 self.x == other.x and self.y == other.y\n\
             }\n\
         }",
    );
}

#[test]
fn baked_partial_eq_method_signature_uses_self_correctly() {
    // The method declares `other: ref Self`. Inside the impl, the user
    // writes the concrete receiver type (`ref Point`). If the trait
    // declaration's `Self` substitution is wrong, this fails.
    typecheck_ok(
        "struct Tag { value: i64 }\n\
         impl PartialEq for Tag {\n\
             fn eq(ref self, other: ref Tag) -> bool {\n\
                 self.value == other.value\n\
             }\n\
         }\n\
         fn check(a: ref Tag, b: ref Tag) -> bool { a.eq(b) }",
    );
}

// ── Baked Eq (CR-202 slice 5b verification) ────────────────────
// `Eq: PartialEq` is the design's supertrait edge. Pre-5b the hardcoded
// path registered `Eq` with empty supertraits; post-5b the bake walk
// reads the supertrait list from `runtime/stdlib/eq.kara`. The
// behavioral effect: user-written `impl Eq for X` now requires a
// companion `impl PartialEq for X` (typechecker `MissingSupertrait`).

#[test]
fn baked_eq_user_impl_with_partial_eq_companion_typechecks() {
    typecheck_ok(
        "struct Tag { value: i64 }\n\
         impl PartialEq for Tag {\n\
             fn eq(ref self, other: ref Tag) -> bool { self.value == other.value }\n\
         }\n\
         impl Eq for Tag {}",
    );
}

#[test]
fn baked_partial_ord_user_impl_typechecks() {
    typecheck_ok(
        "struct Tag { value: i64 }\n\
         impl PartialEq for Tag {\n\
             fn eq(ref self, other: ref Tag) -> bool { self.value == other.value }\n\
         }\n\
         impl PartialOrd for Tag {\n\
             fn partial_cmp(ref self, other: ref Tag) -> Option[Ordering] {\n\
                 Some(self.value.cmp(other.value))\n\
             }\n\
         }",
    );
}

#[test]
fn baked_ord_user_impl_with_full_companion_chain_typechecks() {
    // `Ord: PartialOrd + Eq` requires a longer companion chain than
    // PartialOrd alone. Pin that the slice-5d supertrait list reaches
    // both edges via env_add_trait.
    typecheck_ok(
        "struct Tag { value: i64 }\n\
         impl PartialEq for Tag {\n\
             fn eq(ref self, other: ref Tag) -> bool { self.value == other.value }\n\
         }\n\
         impl Eq for Tag {}\n\
         impl PartialOrd for Tag {\n\
             fn partial_cmp(ref self, other: ref Tag) -> Option[Ordering] { Some(self.value.cmp(other.value)) }\n\
         }\n\
         impl Ord for Tag {\n\
             fn cmp(ref self, other: ref Tag) -> Ordering { self.value.cmp(other.value) }\n\
         }",
    );
}

#[test]
fn baked_display_resolvable_as_trait_bound() {
    // `impl Display for X` for user types is rejected by the resolver
    // (`OperatorTraitImplRestricted` carve-out — Display is stdlib-only),
    // so this test confirms Display is a *resolvable* name in trait
    // bound position rather than at impl head. If the slice-5f swap
    // dropped Display from `env.traits`, this would fail with a
    // missing-trait diagnostic at the bound check.
    typecheck_ok("fn show[T: Display](v: T) -> String { v.to_string() }");
}

#[test]
fn baked_debug_user_impl_typechecks() {
    // Debug is not on the operator-trait restriction list, so user
    // impls go through.
    typecheck_ok(
        "struct Tag { value: i64 }\n\
         impl Debug for Tag {\n\
             fn fmt_debug(ref self) -> String { self.value.to_string() }\n\
         }",
    );
}

#[test]
fn baked_arithmetic_traits_resolvable_as_bounds() {
    // CR-202 slices 5h-5k: Add / Sub / Mul / Div migrated from
    // `register_stdlib_traits` to baked source. User impls of these
    // for non-stdlib types are still rejected by the resolver
    // (operator-trait carve-out), so the test pin is in trait-bound
    // position. If any of the four were accidentally dropped from
    // env.traits, the bound would fail to resolve.
    typecheck_ok(
        "fn use_add[T: Add](a: T, b: T) -> T { a.add(b) }\n\
         fn use_sub[T: Sub](a: T, b: T) -> T { a.sub(b) }\n\
         fn use_mul[T: Mul](a: T, b: T) -> T { a.mul(b) }\n\
         fn use_div[T: Div](a: T, b: T) -> T { a.div(b) }",
    );
}

#[test]
fn baked_extra_operator_traits_resolvable_as_bounds() {
    // CR-202 slice 5l: Rem / Neg / BitAnd / BitOr / BitXor / Shl / Shr
    // joined the baked surface (Not held back — `not` is a Kāra
    // keyword, so the method name in the trait declaration would not
    // parse). Pin that bound resolution still finds them.
    typecheck_ok(
        "fn use_rem[T: Rem](a: T, b: T) -> T { a.rem(b) }\n\
         fn use_neg[T: Neg](a: T) -> T { a.neg() }\n\
         fn use_bitand[T: BitAnd](a: T, b: T) -> T { a.bitand(b) }\n\
         fn use_bitor[T: BitOr](a: T, b: T) -> T { a.bitor(b) }\n\
         fn use_bitxor[T: BitXor](a: T, b: T) -> T { a.bitxor(b) }\n\
         fn use_shl[T: Shl](a: T, b: T) -> T { a.shl(b) }\n\
         fn use_shr[T: Shr](a: T, b: T) -> T { a.shr(b) }",
    );
}

#[test]
fn baked_hash_user_impl_typechecks() {
    // CR-202 slice 5e: `Hash` is now a real registered trait. The
    // method has a method-level generic param `H` for the hasher type.
    // Pin that user code can implement Hash and the generic-on-method
    // form lowers correctly.
    typecheck_ok(
        "struct FakeHasher { state: i64 }\n\
         struct Tag { value: i64 }\n\
         impl Hash for Tag {\n\
             fn hash[H](ref self, hasher: mut ref H) {}\n\
         }",
    );
}

#[test]
fn baked_ord_user_impl_without_partial_ord_companion_fails() {
    // Pre-5d an impl Ord without PartialOrd companion typechecked
    // clean; post-5d this fires MissingSupertrait.
    let parsed = parse(
        "struct Tag { value: i64 }\n\
         impl PartialEq for Tag {\n\
             fn eq(ref self, other: ref Tag) -> bool { self.value == other.value }\n\
         }\n\
         impl Eq for Tag {}\n\
         impl Ord for Tag {\n\
             fn cmp(ref self, other: ref Tag) -> Ordering { self.value.cmp(other.value) }\n\
         }",
    );
    assert!(parsed.errors.is_empty());
    let resolved = resolve(&parsed.program);
    assert!(resolved.errors.is_empty());
    let typed = typecheck(&parsed.program, &resolved);
    assert!(
        typed
            .errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::MissingSupertrait),
        "expected MissingSupertrait, got: {:?}",
        typed.errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn baked_eq_user_impl_without_partial_eq_companion_fails() {
    // The supertrait check fires here; pre-5b this snippet would have
    // typechecked clean.
    let parsed = parse(
        "struct Tag { value: i64 }\n\
         impl Eq for Tag {}",
    );
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let resolved = resolve(&parsed.program);
    assert!(
        resolved.errors.is_empty(),
        "resolve errors: {:?}",
        resolved.errors
    );
    let typed = typecheck(&parsed.program, &resolved);
    assert!(
        typed
            .errors
            .iter()
            .any(|e| e.kind == TypeErrorKind::MissingSupertrait),
        "expected MissingSupertrait, got: {:?}",
        typed
            .errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}
