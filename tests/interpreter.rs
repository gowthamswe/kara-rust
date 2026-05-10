// tests/interpreter.rs

use karac::interpreter::DbgOutputMode;
use karac::{
    run_program, run_program_full, run_program_with_dbg, run_program_with_drops,
    run_program_with_trace,
};

// ── Test Helpers ────────────────────────────────────────────────

fn run(source: &str) -> String {
    let output = run_program(source);
    output.join("")
}

fn runtime_errors(source: &str) -> Vec<karac::interpreter::RuntimeError> {
    let (_out, errors, _trace, _trunc) = run_program_full(source);
    errors
}

fn run_no_errors(source: &str) -> String {
    let (out, errors, _trace, _trunc) = run_program_full(source);
    assert!(
        errors.is_empty(),
        "Expected no runtime errors, got: {:?}",
        errors
    );
    out.join("")
}

// ── Arithmetic & Expressions ───────────────────────────────────

#[test]
fn test_integer_arithmetic() {
    assert_eq!(run("fn main() { println(1 + 2); }"), "3\n");
}

#[test]
fn test_integer_subtraction() {
    assert_eq!(run("fn main() { println(10 - 3); }"), "7\n");
}

#[test]
fn test_integer_multiplication() {
    assert_eq!(run("fn main() { println(4 * 5); }"), "20\n");
}

#[test]
fn test_integer_division() {
    assert_eq!(run("fn main() { println(15 / 4); }"), "3\n");
}

#[test]
fn test_integer_modulo() {
    assert_eq!(run("fn main() { println(17 % 5); }"), "2\n");
}

#[test]
fn test_float_arithmetic() {
    assert_eq!(run("fn main() { println(1.5 + 2.5); }"), "4\n");
}

#[test]
fn test_boolean_logic() {
    assert_eq!(run("fn main() { println(true and false); }"), "false\n");
    assert_eq!(run("fn main() { println(true or false); }"), "true\n");
}

#[test]
fn test_comparison() {
    assert_eq!(run("fn main() { println(3 > 2); }"), "true\n");
    assert_eq!(run("fn main() { println(3 < 2); }"), "false\n");
    assert_eq!(run("fn main() { println(5 == 5); }"), "true\n");
}

#[test]
fn test_comparison_and_equality_post_lowering() {
    // Exercises every comparison/equality op going through the lowered
    // `T.eq` / `T.lt` / etc. Call-path at runtime.
    assert_eq!(run("fn main() { println(3 != 4); }"), "true\n");
    assert_eq!(run("fn main() { println(3 != 3); }"), "false\n");
    assert_eq!(run("fn main() { println(4 <= 4); }"), "true\n");
    assert_eq!(run("fn main() { println(4 >= 5); }"), "false\n");
    assert_eq!(
        run(r#"fn main() { let a: String = "foo"; let b: String = "foo"; println(a == b); }"#),
        "true\n"
    );
    assert_eq!(
        run(r#"fn main() { let a: String = "foo"; let b: String = "bar"; println(a != b); }"#),
        "true\n"
    );
}

#[test]
fn test_bitwise_ops_post_lowering() {
    // `&`, `|`, `^`, `<<`, `>>` all flow through the lowered Call path.
    assert_eq!(run("fn main() { println(0b1100 & 0b1010); }"), "8\n");
    assert_eq!(run("fn main() { println(0b1100 | 0b1010); }"), "14\n");
    assert_eq!(run("fn main() { println(0b1100 ^ 0b1010); }"), "6\n");
    assert_eq!(run("fn main() { println(1 << 3); }"), "8\n");
    assert_eq!(run("fn main() { println(16 >> 2); }"), "4\n");
}

#[test]
fn test_bitnot_and_not_post_lowering() {
    // `~int` lowers to `int.not`, `not bool` lowers to `bool.not`.
    assert_eq!(run("fn main() { println(~0); }"), "-1\n");
    assert_eq!(run("fn main() { println(not false); }"), "true\n");
}

#[test]
fn test_user_impl_eq_drives_equality_operator() {
    // `impl Eq for Point` is registered, and `a == b` lowers to
    // `Point.eq(a, b)` — routed through the user-defined method rather than
    // any structural fallback. CR-202 slice 5b: companion
    // `impl PartialEq for Point` satisfies the new `Eq: PartialEq`
    // supertrait edge so the typecheck pass stays clean.
    assert_eq!(
        run("struct Point { x: i64, y: i64 }
             impl PartialEq for Point {
                 fn eq(ref self, other: ref Point) -> bool {
                     self.x == other.x and self.y == other.y
                 }
             }
             impl Eq for Point {
                 fn eq(self, other: Point) -> bool {
                     self.x == other.x and self.y == other.y
                 }
             }
             fn main() {
                 let a = Point { x: 1, y: 2 };
                 let b = Point { x: 1, y: 2 };
                 let c = Point { x: 9, y: 9 };
                 println(a == b);
                 println(a != c);
             }"),
        "true\ntrue\n"
    );
}

#[test]
fn test_user_impl_ord_drives_comparison_operators() {
    // `impl Ord for Point` with direct `lt`/`le`/`gt`/`ge` methods — `<`
    // lowers to `Point.lt(a, b)`, etc. Domain-specific ordering (here by x
    // only) rather than the interpreter's hardcoded primitive path.
    // CR-202 slice 5d: companion PartialEq/Eq/PartialOrd impls satisfy
    // the new `Ord: PartialOrd + Eq` supertrait edges (typecheck-clean;
    // interpreter execution behavior is unchanged).
    assert_eq!(
        run("struct Point { x: i64, y: i64 }
             impl PartialEq for Point {
                 fn eq(ref self, other: ref Point) -> bool { self.x == other.x and self.y == other.y }
             }
             impl Eq for Point {}
             impl PartialOrd for Point {
                 fn partial_cmp(ref self, other: ref Point) -> Option[Ordering] { Some(self.x.cmp(other.x)) }
             }
             impl Ord for Point {
                 fn lt(self, other: Point) -> bool { self.x < other.x }
                 fn le(self, other: Point) -> bool { self.x <= other.x }
                 fn gt(self, other: Point) -> bool { self.x > other.x }
                 fn ge(self, other: Point) -> bool { self.x >= other.x }
             }
             fn main() {
                 let a = Point { x: 1, y: 99 };
                 let b = Point { x: 5, y: 1 };
                 println(a < b);
                 println(a > b);
                 println(a <= b);
             }"),
        "true\nfalse\ntrue\n"
    );
}

#[test]
fn test_eq_ord_direct_method_calls() {
    // `i32.lt(a, b)` and peers are callable directly in user code, matching
    // the lowered form of the operator. Verifies the whole typecheck → lower
    // → interpret pipeline for the Eq/Ord method names.
    assert_eq!(
        run("fn main() {
                 let a: i32 = 3;
                 let b: i32 = 5;
                 println(i32.lt(a, b));
                 println(i32.ge(a, b));
                 println(i32.eq(a, a));
                 println(i32.ne(a, b));
             }"),
        "true\nfalse\ntrue\ntrue\n"
    );
}

#[test]
fn test_string_literal() {
    assert_eq!(
        run(r#"fn main() { println("hello world"); }"#),
        "hello world\n"
    );
}

#[test]
fn test_unary_negation() {
    assert_eq!(run("fn main() { println(-42); }"), "-42\n");
}

#[test]
fn test_unary_not() {
    assert_eq!(run("fn main() { println(not true); }"), "false\n");
}

// ── Variables & Let Bindings ───────────────────────────────────

#[test]
fn test_let_binding() {
    assert_eq!(run("fn main() { let x = 42; println(x); }"), "42\n");
}

#[test]
fn test_let_mut_reassign() {
    assert_eq!(
        run("fn main() { let mut x = 1; x = 2; println(x); }"),
        "2\n"
    );
}

#[test]
fn test_compound_assignment() {
    assert_eq!(
        run("fn main() { let mut x = 10; x += 5; println(x); }"),
        "15\n"
    );
}

#[test]
fn test_compound_assignment_int_subtract() {
    assert_eq!(
        run("fn main() { let mut x = 10; x -= 3; println(x); }"),
        "7\n"
    );
}

#[test]
fn test_compound_assignment_string_concat() {
    // `s += other` desugars to `s = s + other`. Confirms the desugar still
    // works for non-integer Add impls — guard for when Step 6 lowering lands.
    assert_eq!(
        run("fn main() { let mut s = \"hello \"; s += \"world\"; println(s); }"),
        "hello world\n"
    );
}

// ── Control Flow ───────────────────────────────────────────────

#[test]
fn test_if_true() {
    assert_eq!(run("fn main() { if true { println(1); } }"), "1\n");
}

#[test]
fn test_if_false_else() {
    assert_eq!(
        run("fn main() { if false { println(1); } else { println(2); } }"),
        "2\n"
    );
}

#[test]
fn test_if_else_expression() {
    assert_eq!(
        run("fn main() { let x = if true { 10 } else { 20 }; println(x); }"),
        "10\n"
    );
}

#[test]
fn test_while_loop() {
    assert_eq!(
        run("fn main() {\n\
                 let mut i = 0;\n\
                 while i < 3 {\n\
                     i += 1;\n\
                 }\n\
                 println(i);\n\
             }"),
        "3\n"
    );
}

#[test]
fn test_loop_break() {
    assert_eq!(
        run("fn main() {\n\
                 let mut i = 0;\n\
                 loop {\n\
                     i += 1;\n\
                     if i == 5 { break; }\n\
                 }\n\
                 println(i);\n\
             }"),
        "5\n"
    );
}

// ── Functions ──────────────────────────────────────────────────

#[test]
fn test_function_call() {
    assert_eq!(
        run("fn double(x: i64) -> i64 { x * 2 }\n\
             fn main() { println(double(21)); }"),
        "42\n"
    );
}

#[test]
fn test_recursive_function() {
    assert_eq!(
        run("fn factorial(n: i64) -> i64 {\n\
                 if n <= 1 { 1 } else { n * factorial(n - 1) }\n\
             }\n\
             fn main() { println(factorial(5)); }"),
        "120\n"
    );
}

#[test]
fn test_multiple_params() {
    assert_eq!(
        run("fn add(a: i64, b: i64) -> i64 { a + b }\n\
             fn main() { println(add(3, 4)); }"),
        "7\n"
    );
}

#[test]
fn test_early_return() {
    assert_eq!(
        run("fn abs(x: i64) -> i64 {\n\
                 if x < 0 { return -x; }\n\
                 x\n\
             }\n\
             fn main() { println(abs(-5)); }"),
        "5\n"
    );
}

// ── Closures ───────────────────────────────────────────────────

#[test]
fn test_closure_basic() {
    assert_eq!(
        run("fn apply(f: Fn(i64) -> i64, x: i64) -> i64 { f(x) }\n\
             fn main() { println(apply(|x: i64| x + 1, 41)); }"),
        "42\n"
    );
}

#[test]
fn test_closure_captures() {
    assert_eq!(
        run("fn apply(f: Fn(i64) -> i64, x: i64) -> i64 { f(x) }\n\
             fn main() {\n\
                 let offset = 10;\n\
                 let add_offset = |x: i64| x + offset;\n\
                 println(apply(add_offset, 32));\n\
             }"),
        "42\n"
    );
}

// ── Structs ────────────────────────────────────────────────────

#[test]
fn test_struct_construction_and_field_access() {
    assert_eq!(
        run("struct Point { x: i64, y: i64 }\n\
             fn main() {\n\
                 let p = Point { x: 3, y: 4 };\n\
                 println(p.x + p.y);\n\
             }"),
        "7\n"
    );
}

#[test]
fn test_struct_method() {
    assert_eq!(
        run("struct Counter { value: i64 }\n\
             impl Counter {\n\
                 fn get(self) -> i64 { self.value }\n\
             }\n\
             fn main() {\n\
                 let c = Counter { value: 42 };\n\
                 println(c.get());\n\
             }"),
        "42\n"
    );
}

// ── Enums & Match ──────────────────────────────────────────────

#[test]
fn test_enum_match_unit_variants() {
    assert_eq!(
        run("enum Color { Red, Green, Blue }\n\
             fn name(c: Color) -> i64 {\n\
                 match c {\n\
                     Red => 1,\n\
                     Green => 2,\n\
                     Blue => 3,\n\
                 }\n\
             }\n\
             fn main() { println(name(Red)); }"),
        "1\n"
    );
}

#[test]
fn test_enum_match_tuple_variant() {
    assert_eq!(
        run("enum Shape { Circle(i64), Rect(i64, i64) }\n\
             fn area(s: Shape) -> i64 {\n\
                 match s {\n\
                     Circle(r) => r * r,\n\
                     Rect(w, h) => w * h,\n\
                 }\n\
             }\n\
             fn main() { println(area(Rect(3, 4))); }"),
        "12\n"
    );
}

#[test]
fn test_match_wildcard() {
    assert_eq!(
        run("fn describe(x: i64) -> i64 {\n\
                 match x {\n\
                     0 => 100,\n\
                     _ => 200,\n\
                 }\n\
             }\n\
             fn main() { println(describe(5)); }"),
        "200\n"
    );
}

#[test]
fn test_match_with_guard() {
    assert_eq!(
        run("enum Opt { Some(i64), None }\n\
             fn check(o: Opt) -> i64 {\n\
                 match o {\n\
                     Some(x) if x > 10 => 1,\n\
                     Some(x) => 2,\n\
                     None => 3,\n\
                 }\n\
             }\n\
             fn main() { println(check(Some(5))); }"),
        "2\n"
    );
}

// ── Tuples ─────────────────────────────────────────────────────

#[test]
fn test_tuple_construction() {
    assert_eq!(
        run("fn main() {\n\
                 let t = (1, 2, 3);\n\
                 println(t.0 + t.1 + t.2);\n\
             }"),
        "6\n"
    );
}

// ── Arrays ─────────────────────────────────────────────────────

#[test]
fn test_array_literal() {
    assert_eq!(
        run("fn main() {\n\
                 let arr = [10, 20, 30];\n\
                 println(arr[1]);\n\
             }"),
        "20\n"
    );
}

// ── Pipe Operator ──────────────────────────────────────────────

#[test]
fn test_pipe_basic() {
    assert_eq!(
        run("fn double(x: i64) -> i64 { x * 2 }\n\
             fn main() { println(21 |> double); }"),
        "42\n"
    );
}

#[test]
fn test_pipe_chained() {
    assert_eq!(
        run("fn add1(x: i64) -> i64 { x + 1 }\n\
             fn double(x: i64) -> i64 { x * 2 }\n\
             fn main() { println(5 |> add1 |> double); }"),
        "12\n"
    );
}

// ── Integer Overflow ───────────────────────────────────────────

#[test]
fn test_integer_overflow_traps() {
    let errors = runtime_errors("fn main() { let x = 9223372036854775807 + 1; }");
    assert!(
        errors.iter().any(|e| e.message.contains("overflow")),
        "expected an overflow runtime error, got {:?}",
        errors
    );
}

#[test]
fn test_division_by_zero_records_runtime_error() {
    let errors = runtime_errors("fn main() { let x = 10; let y = 0; let z = x / y; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("division by zero")),
        "expected a division-by-zero runtime error, got {:?}",
        errors
    );
}

#[test]
fn test_modulo_by_zero_records_runtime_error() {
    let errors = runtime_errors("fn main() { let x = 10; let y = 0; let z = x % y; }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("division by zero")),
        "expected a division-by-zero runtime error for %, got {:?}",
        errors
    );
}

#[test]
fn test_index_out_of_bounds_records_runtime_error() {
    let errors = runtime_errors("fn main() { let a = [1, 2, 3]; let x = a[10]; }");
    assert!(
        errors.iter().any(|e| e.message.contains("out of bounds")),
        "expected an index-out-of-bounds runtime error, got {:?}",
        errors
    );
}

#[test]
fn test_todo_records_runtime_error() {
    let errors = runtime_errors(r#"fn main() { todo("finish this"); }"#);
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("not yet implemented") && e.message.contains("finish this")),
        "expected todo() to surface a runtime error, got {:?}",
        errors
    );
}

// ── Result/Option & ? operator ──────────────────────────────────

#[test]
fn test_option_some_unwrap() {
    assert_eq!(
        run("fn main() { let x = Some(42); println(x.unwrap()); }"),
        "42\n"
    );
}

#[test]
fn test_option_none_nil_coalesce() {
    assert_eq!(run("fn main() { let x = None; println(x ?? 99); }"), "99\n");
}

#[test]
fn test_result_ok_question_mark() {
    assert_eq!(
        run("fn get_value() -> i64 {\n\
                 let r = Ok(42);\n\
                 let v = r?;\n\
                 v\n\
             }\n\
             fn main() { println(get_value()); }"),
        "42\n"
    );
}

#[test]
fn test_result_err_question_mark_propagates() {
    assert_eq!(
        run("fn might_fail() -> i64 {\n\
                 let r = Err(0);\n\
                 let v = r?;\n\
                 v\n\
             }\n\
             fn check() -> i64 {\n\
                 let result = might_fail();\n\
                 match result {\n\
                     Ok(v) => v,\n\
                     Err(e) => -1,\n\
                 }\n\
             }\n\
             fn main() { println(check()); }"),
        "-1\n"
    );
}

#[test]
fn test_option_is_some_is_none() {
    assert_eq!(
        run("fn main() {\n\
                 let a = Some(1);\n\
                 let b = None;\n\
                 println(a.is_some());\n\
                 println(b.is_none());\n\
             }"),
        "true\ntrue\n"
    );
}

#[test]
fn test_unwrap_none_records_runtime_error() {
    let errors = runtime_errors("fn main() { let x = None; x.unwrap(); }");
    assert!(
        errors.iter().any(|e| e.message.contains("unwrap")),
        "expected an unwrap runtime error, got {:?}",
        errors
    );
}

#[test]
fn test_expect_with_message() {
    let errors = runtime_errors(r#"fn main() { let x = None; x.expect("value required"); }"#);
    assert!(
        errors.iter().any(|e| e.message.contains("value required")),
        "expected expect() message to surface in the runtime error, got {:?}",
        errors
    );
}

// ── defer/errdefer ─────────────────────────────────────────────

#[test]
fn test_defer_runs_on_scope_exit() {
    assert_eq!(
        run("fn main() {\n\
                 print(1);\n\
                 defer { print(3); }\n\
                 print(2);\n\
             }"),
        "123"
    );
}

#[test]
fn test_defer_lifo_order() {
    assert_eq!(
        run("fn main() {\n\
                 defer { print(3); }\n\
                 defer { print(2); }\n\
                 defer { print(1); }\n\
             }"),
        "123"
    );
}

// Unified drop+defer cleanup stack — design.md § Drop ordering within a branch.

#[test]
fn test_defer_registers_when_reached_not_at_block_start() {
    // The `defer` after the early `return` is never registered, so
    // its body must not fire. Pre-walk collection (the old bug)
    // would have run "late" anyway because the defer was hoisted to
    // block start.
    assert_eq!(
        run("fn early(go: bool) {\n\
                 print(\"a\");\n\
                 if go { return; }\n\
                 defer { print(\"late\"); }\n\
                 print(\"b\");\n\
             }\n\
             fn main() {\n\
                 early(true);\n\
                 print(\"|\");\n\
                 early(false);\n\
             }"),
        "a|ablate"
    );
}

#[test]
fn test_defer_runs_on_early_return() {
    // A defer registered before the early return must fire — the
    // pre-walk impl had a bug where the `?` in eval_stmt_cf
    // short-circuited past the cleanup drain.
    assert_eq!(
        run("fn body() {\n\
                 defer { print(\"d\"); }\n\
                 print(\"a\");\n\
                 return;\n\
                 print(\"unreached\");\n\
             }\n\
             fn main() { body(); }"),
        "ad"
    );
}

#[test]
fn test_errdefer_fires_on_err_return() {
    // Param-less errdefer fires on Err return; defer always fires.
    assert_eq!(
        run("fn body() -> Result[i64, String] {\n\
                 defer { print(\"d\"); }\n\
                 errdefer { print(\"e\"); }\n\
                 return Err(\"boom\");\n\
             }\n\
             fn main() { let _ = body(); }"),
        "ed"
    );
}

#[test]
fn test_errdefer_skipped_on_normal_return() {
    // errdefer must NOT fire on Ok; defer always fires.
    assert_eq!(
        run("fn body() -> Result[i64, String] {\n\
                 defer { print(\"d\"); }\n\
                 errdefer { print(\"e\"); }\n\
                 return Ok(1);\n\
             }\n\
             fn main() { let _ = body(); }"),
        "d"
    );
}

#[test]
fn test_errdefer_with_binding_sees_err_payload() {
    // errdefer(e) binds `e` to the Err payload during the errdefer phase.
    assert_eq!(
        run("fn body() -> Result[i64, String] {\n\
                 errdefer(e) { print(e); }\n\
                 return Err(\"oops\");\n\
             }\n\
             fn main() { let _ = body(); }"),
        "oops"
    );
}

#[test]
fn test_errdefer_runs_before_defer_on_error_path() {
    // Phase 1 (errdefer) drains LIFO, then phase 2 (drop+defer) drains LIFO.
    assert_eq!(
        run("fn body() -> Result[i64, String] {\n\
                 defer { print(\"d1\"); }\n\
                 errdefer { print(\"e1\"); }\n\
                 defer { print(\"d2\"); }\n\
                 errdefer { print(\"e2\"); }\n\
                 return Err(\"x\");\n\
             }\n\
             fn main() { let _ = body(); }"),
        "e2e1d2d1"
    );
}

#[test]
fn test_unified_stack_program_order_lifo() {
    // The drop+defer stack interleaves bindings and defers in
    // program order; LIFO drain means later items pop first.
    // Drop slots are no-ops today, but defer ordering relative to
    // them must respect program order. The test pins the defer-vs-defer
    // ordering across interleaved `let` statements.
    assert_eq!(
        run("fn main() {\n\
                 let _x = 1;\n\
                 defer { print(\"d1\"); }\n\
                 let _y = 2;\n\
                 defer { print(\"d2\"); }\n\
             }"),
        "d2d1"
    );
}

#[test]
fn test_par_cancellation_does_not_propagate_as_scope_error() {
    // Sub-step 4: a `par` sibling that observed cancel mid-execution
    // raises ControlFlow::Cancelled. `eval_par_block` must silence
    // that on the result side — the originating branch's real `Err`
    // is the scope's return value under fail-fast. Threading races
    // are tolerated by checking the OUTCOME (real Err propagates)
    // rather than asserting on whether observation actually fired.
    assert_eq!(
        run("fn attempt() -> Result[i64, String] {\n\
                 par {\n\
                     {\n\
                         let _a = 0;\n\
                         let _b = 0;\n\
                         let _c = 0;\n\
                         let _d = 0;\n\
                         let _e = 0;\n\
                         let _f = 0;\n\
                         let _g = 0;\n\
                         let _h = 0;\n\
                     }\n\
                     {\n\
                         return Err(\"r-fail\");\n\
                     }\n\
                 };\n\
                 Ok(0)\n\
             }\n\
             fn main() {\n\
                 match attempt() {\n\
                     Ok(_) => print(\"ok\"),\n\
                     Err(e) => print(e),\n\
                 }\n\
             }"),
        "r-fail"
    );
}

// ── Sub-step 3: NLL drop placement ─────────────────────────────

fn drops_in(source: &str) -> Vec<String> {
    let (_out, drops) = run_program_with_drops(source);
    drops
}

#[test]
fn test_nll_drop_fires_after_last_use_not_at_scope_exit() {
    // Per design.md § Drop ordering within a branch: NLL drops fire
    // at the binding's last-use program point, not at scope exit.
    // `x` is read on line 3; subsequent statements never reference it.
    // Drop(x) must fire after stmt 3, before the later prints — and
    // before Drop(y) which dies later.
    let drops = drops_in(
        "fn main() {\n\
             let x = 1;\n\
             let y = 2;\n\
             println(x);\n\
             println(y);\n\
             println(99);\n\
         }",
    );
    assert_eq!(drops, vec!["x", "y"]);
}

#[test]
fn test_nll_drop_orders_by_last_use_not_declaration_order() {
    // Declaration order: a, b, c. Last-use order: c (idx 1), a (idx 2),
    // b (idx 3). Drops fire in last-use order, NOT declaration LIFO.
    let drops = drops_in(
        "fn main() {\n\
             let a = 1;\n\
             let b = 2;\n\
             let c = 3;\n\
             println(c);\n\
             println(a);\n\
             println(b);\n\
         }",
    );
    assert_eq!(drops, vec!["c", "a", "b"]);
}

#[test]
fn test_nll_unread_binding_drops_at_its_let() {
    // A binding never read after its declaration drops immediately
    // (last_use == its own let-stmt index). Subsequent let-and-drop
    // chains the same way.
    let drops = drops_in(
        "fn main() {\n\
             let _a = 1;\n\
             let _b = 2;\n\
             let _c = 3;\n\
         }",
    );
    assert_eq!(drops, vec!["_a", "_b", "_c"]);
}

#[test]
fn test_nll_binding_used_in_final_expr_drops_at_scope_exit() {
    // A binding referenced by `final_expr` stays live until the
    // expression evaluates; its Drop drains via the unified LIFO at
    // scope exit (sentinel: `last_use == stmts.len()`).
    let drops = drops_in(
        "fn main() {\n\
             let result = {\n\
                 let x = 7;\n\
                 x + 1\n\
             };\n\
             println(result);\n\
         }",
    );
    // `x` drops at the inner block's scope exit; `result` is the
    // outer-block binding and drops at outer scope exit. Both reach
    // the LIFO drain because they're referenced after the let.
    assert_eq!(drops, vec!["x", "result"]);
}

#[test]
fn test_nll_defer_referencing_binding_extends_live_range() {
    // Per design.md § Drop ordering within a branch: a defer body
    // that references a binding extends the binding's live range to
    // scope exit. The defer fires first under LIFO, with the binding
    // still alive; Drop fires after.
    let drops = drops_in(
        "fn main() {\n\
             let x = 7;\n\
             defer { println(x); }\n\
             println(\"middle\");\n\
         }",
    );
    // `x` is referenced in the defer body, so its last_use is the
    // sentinel. Drop(x) drains at scope exit, after the defer body.
    assert_eq!(drops, vec!["x"]);
}

#[test]
fn test_nll_drop_ordering_with_defer_interleave() {
    // Bindings whose direct last-use is mid-block fire NLL early,
    // *before* any defer (registered later) drains at scope exit.
    // The unified LIFO ordering only kicks in for slots still in
    // `cleanup` at scope exit.
    let drops = drops_in(
        "fn main() {\n\
             let a = 1;\n\
             println(a);\n\
             defer { println(\"d\"); }\n\
             let b = 2;\n\
             println(b);\n\
         }",
    );
    // a's last use is stmt 1 → fires NLL after println(a).
    // b's last use is stmt 4 → fires NLL after println(b).
    // The defer body still drains at scope exit; it carries no
    // binding-name that would land on drop_trace.
    assert_eq!(drops, vec!["a", "b"]);
}

// ── Shared struct interior mutability ───────────────────────────

#[test]
fn test_shared_struct_aliasing_propagates_mutation() {
    // Per design.md § Part 5: Shared Types — `shared struct` values
    // have reference semantics. `let b = a` clones the Arc; mutations
    // through `b.field` are visible at `a.field` because both bindings
    // point to the same allocation.
    assert_eq!(
        run("shared struct Counter { mut value: i64 }\n\
             fn main() {\n\
                 let a = Counter { value: 1 };\n\
                 let b = a;\n\
                 b.value = 42;\n\
                 println(a.value);\n\
             }"),
        "42\n"
    );
}

#[test]
fn test_shared_struct_per_field_independence() {
    // Per design.md § Part 5: \"mutating `node.left` does not conflict
    // with reading `node.right`\". Per-field tracking is the entire
    // point of the spec choosing per-field over struct-wide.
    assert_eq!(
        run("shared struct Node { mut left: i64, mut right: i64 }\n\
             fn main() {\n\
                 let n = Node { left: 1, right: 2 };\n\
                 n.left = 10;\n\
                 println(n.left + n.right);\n\
             }"),
        "12\n"
    );
}

#[test]
fn test_shared_struct_immutable_field_persists() {
    // An immutable field set at construction is visible across all
    // holders and is never mutated afterwards.
    assert_eq!(
        run("shared struct Node { id: i64, mut value: i64 }\n\
             fn main() {\n\
                 let n = Node { id: 7, value: 0 };\n\
                 let m = n;\n\
                 m.value = 99;\n\
                 println(n.id);\n\
                 println(m.value);\n\
             }"),
        "7\n99\n"
    );
}

#[test]
fn test_shared_struct_mutation_through_method() {
    // Method dispatch on `shared struct` binds `self` to a SharedStruct
    // value (Arc clone). `self.field = x` inside the method writes
    // through the shared allocation.
    assert_eq!(
        run("shared struct Counter { mut value: i64 }\n\
             impl Counter {\n\
                 fn bump(ref self) { self.value = self.value + 1; }\n\
             }\n\
             fn main() {\n\
                 let c = Counter { value: 0 };\n\
                 c.bump();\n\
                 c.bump();\n\
                 c.bump();\n\
                 println(c.value);\n\
             }"),
        "3\n"
    );
}

#[test]
fn test_shared_struct_aliased_through_method_argument() {
    // Passing a `shared struct` to a function that mutates through it
    // is visible at the caller — same Arc allocation.
    assert_eq!(
        run("shared struct Box { mut value: i64 }\n\
             fn bump(b: ref Box) { b.value = b.value + 100; }\n\
             fn main() {\n\
                 let x = Box { value: 5 };\n\
                 bump(x);\n\
                 bump(x);\n\
                 println(x.value);\n\
             }"),
        "205\n"
    );
}

#[test]
fn test_shared_struct_per_field_independence_across_methods() {
    // Pins the canonical spec example from design.md:8186 —
    // \"mutating `node.left` does not conflict with reading `node.right`\".
    // Independent methods touching different fields never see a
    // borrow-conflict panic.
    assert_eq!(
        run("shared struct Node { mut left: i64, mut right: i64 }\n\
             impl Node {\n\
                 fn write_left(ref self, x: i64) { self.left = x; }\n\
                 fn read_right(ref self) -> i64 { self.right }\n\
             }\n\
             fn main() {\n\
                 let n = Node { left: 0, right: 7 };\n\
                 n.write_left(99);\n\
                 println(n.read_right());\n\
                 println(n.left);\n\
             }"),
        "7\n99\n"
    );
}

// ── Weak references on shared struct fields ────────────────────

#[test]
fn test_weak_field_alive_yields_some() {
    // Per design.md § Shared Types — Weak references: a `weak` field
    // read is the upgrade point. While a strong holder of the referent
    // is in scope, the upgrade succeeds and yields `Some(strong_ref)`.
    assert_eq!(
        run("shared struct Parent { id: i64 }\n\
             shared struct Child { id: i64, mut weak parent: Parent }\n\
             fn main() {\n\
                 let p = Parent { id: 7 };\n\
                 let c = Child { id: 2, parent: p };\n\
                 match c.parent {\n\
                     Some(parent_ref) => println(parent_ref.id),\n\
                     None => println(\"dangling\"),\n\
                 }\n\
             }"),
        "7\n"
    );
}

#[test]
fn test_weak_field_after_strong_drop_yields_none() {
    // After every strong holder of the referent is dropped, the
    // referent's allocation is freed and the weak field upgrade
    // returns `None`. `make_orphan` returns a Child whose only handle
    // to `Parent` is a weak reference; when the function frame exits,
    // the local strong `p` is dropped and the Arc count hits zero.
    assert_eq!(
        run("shared struct Parent { id: i64 }\n\
             shared struct Child { id: i64, mut weak parent: Parent }\n\
             fn make_orphan() -> Child {\n\
                 let p = Parent { id: 7 };\n\
                 Child { id: 2, parent: p }\n\
             }\n\
             fn main() {\n\
                 let c = make_orphan();\n\
                 match c.parent {\n\
                     Some(parent_ref) => println(parent_ref.id),\n\
                     None => println(\"dangling\"),\n\
                 }\n\
             }"),
        "dangling\n"
    );
}

#[test]
fn test_weak_field_multiple_aliases_all_see_drop() {
    // Two children weakly-reference the same parent. When the parent
    // is dropped, both children's weak fields upgrade to None.
    assert_eq!(
        run("shared struct Parent { id: i64 }\n\
             shared struct Child { id: i64, mut weak parent: Parent }\n\
             fn make_pair() -> (Child, Child) {\n\
                 let p = Parent { id: 1 };\n\
                 (Child { id: 10, parent: p }, Child { id: 11, parent: p })\n\
             }\n\
             fn main() {\n\
                 let pair = make_pair();\n\
                 let (a, b) = pair;\n\
                 match a.parent { Some(_) => println(\"a:alive\"), None => println(\"a:dangling\") }\n\
                 match b.parent { Some(_) => println(\"b:alive\"), None => println(\"b:dangling\") }\n\
             }"),
        "a:dangling\nb:dangling\n"
    );
}

#[test]
fn test_weak_field_reassignment_restores_some() {
    // A `mut weak` field can be reassigned after construction. The
    // assignment auto-downgrades the strong rhs. Reading after the
    // first assignment's referent dies yields None; assigning a fresh
    // live parent restores Some.
    assert_eq!(
        run("shared struct Parent { id: i64 }\n\
             shared struct Child { id: i64, mut weak parent: Parent }\n\
             fn main() {\n\
                 let p1 = Parent { id: 1 };\n\
                 let c = Child { id: 9, parent: p1 };\n\
                 let p2 = Parent { id: 2 };\n\
                 c.parent = p2;\n\
                 match c.parent {\n\
                     Some(parent_ref) => println(parent_ref.id),\n\
                     None => println(\"dangling\"),\n\
                 }\n\
             }"),
        "2\n"
    );
}

#[test]
fn test_weak_field_immutable_form_set_at_construction() {
    // `weak parent: Parent` (no `mut`) is set at construction and
    // never reassigned. While the strong parent lives, upgrade yields
    // Some; after the strong parent's frame exits, upgrade yields None.
    assert_eq!(
        run("shared struct Parent { id: i64 }\n\
             shared struct Child { id: i64, weak parent: Parent }\n\
             fn build() -> Child {\n\
                 let p = Parent { id: 42 };\n\
                 Child { id: 1, parent: p }\n\
             }\n\
             fn main() {\n\
                 let c = build();\n\
                 match c.parent {\n\
                     Some(parent_ref) => println(parent_ref.id),\n\
                     None => println(\"dangling\"),\n\
                 }\n\
             }"),
        "dangling\n"
    );
}

#[test]
fn test_weak_field_upgrade_observes_strong_field_data() {
    // The Some arm of a weak upgrade is a normal SharedStruct handle —
    // its other fields are reachable as usual. Pins that the Arc
    // returned by Weak::upgrade carries the full referent contents,
    // not a stub.
    assert_eq!(
        run("shared struct Parent { id: i64, mut count: i64 }\n\
             shared struct Child { mut weak parent: Parent }\n\
             fn main() {\n\
                 let p = Parent { id: 5, count: 100 };\n\
                 let c = Child { parent: p };\n\
                 match c.parent {\n\
                     Some(parent_ref) => { parent_ref.count = parent_ref.count + 1; println(p.count); },\n\
                     None => println(\"dangling\"),\n\
                 }\n\
             }"),
        "101\n"
    );
}

// ── Array methods ──────────────────────────────────────────────

#[test]
fn test_array_len() {
    assert_eq!(
        run("fn main() { let arr = [1, 2, 3]; println(arr.len()); }"),
        "3\n"
    );
}

// ── Vec-default and prefix collection literals ──────────────────

#[test]
fn test_bare_literal_defaults_to_vec_index() {
    assert_eq!(
        run("fn main() { let v = [10, 20, 30]; println(v[1]); }"),
        "20\n"
    );
}

#[test]
fn test_bare_literal_for_loop_sum() {
    assert_eq!(
        run("fn main() {\n\
                 let v = [1, 2, 3, 4];\n\
                 let mut s = 0;\n\
                 for x in v { s = s + x; }\n\
                 println(s);\n\
             }"),
        "10\n"
    );
}

#[test]
fn test_prefix_vec_literal_runtime() {
    assert_eq!(
        run("fn main() { let v = Vec[7, 8, 9]; println(v[2]); }"),
        "9\n"
    );
}

#[test]
fn test_prefix_array_literal_runtime() {
    assert_eq!(
        run("fn main() { let a = Array[4, 5, 6]; println(a[0]); }"),
        "4\n"
    );
}

#[test]
fn test_prefix_vec_len() {
    assert_eq!(
        run("fn main() { let v = Vec[1, 2, 3]; println(v.len()); }"),
        "3\n"
    );
}

#[test]
fn test_interpreter_vec_new_construct_push_len() {
    // `Vec.new()` returns an empty Vec through the interpreter's path-string
    // dispatch. Exercises construct → push → len-read → indexed read so a
    // regression in the dispatch arm fails this test rather than panicking
    // inside `karac run` with `path 'Vec.new' not found`.
    assert_eq!(
        run("fn main() {\n\
                 let mut v: Vec[i64] = Vec.new();\n\
                 v.push(10_i64);\n\
                 v.push(20_i64);\n\
                 v.push(30_i64);\n\
                 println(f\"{v.len()}\");\n\
                 println(f\"{v[0]}\");\n\
                 println(f\"{v[2]}\");\n\
             }"),
        "3\n10\n30\n"
    );
}

// ── Repeat literal `[v; n]` ────────────────────────────────────

#[test]
fn test_repeat_literal_bare_runtime() {
    // `[v; n]` allocates n copies of v and is iterable.
    assert_eq!(
        run("fn main() {
                 let v = [0; 5];
                 let mut sum = 0;
                 for x in v { sum = sum + x + 1; }
                 println(sum);
             }"),
        "5\n"
    );
}

#[test]
fn test_repeat_literal_array_prefix_runtime() {
    assert_eq!(
        run("fn main() {
                 let a = Array[42; 3];
                 println(a[0]);
                 println(a[2]);
             }"),
        "42\n42\n"
    );
}

#[test]
fn test_repeat_literal_vec_prefix_runtime() {
    assert_eq!(
        run("fn main() {
                 let v = Vec[7; 4];
                 println(v.len());
                 println(v[3]);
             }"),
        "4\n7\n"
    );
}

#[test]
fn test_repeat_literal_with_annotation_runtime() {
    // Bare `[v; n]` coerced to Array[T, N] via let annotation.
    assert_eq!(
        run("fn main() {
                 let a: Array[i64, 4] = [9; 4];
                 println(a[1]);
             }"),
        "9\n"
    );
}

// ── String Interpolation ───────────────────────────────────────

#[test]
fn test_string_interpolation_basic() {
    assert_eq!(
        run("fn main() {\n\
                 let x = 42;\n\
                 println(f\"the answer is {x}\");\n\
             }"),
        "the answer is 42\n"
    );
}

// ── E2E: Complete Programs ─────────────────────────────────────

#[test]
fn test_e2e_fizzbuzz() {
    assert_eq!(
        run("fn fizzbuzz(n: i64) -> i64 {\n\
                 if n % 15 == 0 { 0 }\n\
                 else if n % 3 == 0 { 3 }\n\
                 else if n % 5 == 0 { 5 }\n\
                 else { n }\n\
             }\n\
             fn main() {\n\
                 println(fizzbuzz(15));\n\
                 println(fizzbuzz(9));\n\
                 println(fizzbuzz(10));\n\
                 println(fizzbuzz(7));\n\
             }"),
        "0\n3\n5\n7\n"
    );
}

#[test]
fn test_e2e_fibonacci() {
    assert_eq!(
        run("fn fib(n: i64) -> i64 {\n\
                 if n <= 1 { n }\n\
                 else { fib(n - 1) + fib(n - 2) }\n\
             }\n\
             fn main() {\n\
                 println(fib(0));\n\
                 println(fib(1));\n\
                 println(fib(10));\n\
             }"),
        "0\n1\n55\n"
    );
}

#[test]
fn test_e2e_struct_with_methods() {
    assert_eq!(
        run("struct Rect { width: i64, height: i64 }\n\
             impl Rect {\n\
                 fn area(self) -> i64 { self.width * self.height }\n\
                 fn is_square(self) -> bool { self.width == self.height }\n\
             }\n\
             fn main() {\n\
                 let r = Rect { width: 3, height: 4 };\n\
                 println(r.area());\n\
                 println(r.is_square());\n\
                 let s = Rect { width: 5, height: 5 };\n\
                 println(s.is_square());\n\
             }"),
        "12\nfalse\ntrue\n"
    );
}

#[test]
fn test_e2e_enum_state_machine() {
    assert_eq!(
        run("enum State { Idle, Running(i64), Done }\n\
             fn step(s: State) -> State {\n\
                 match s {\n\
                     Idle => Running(0),\n\
                     Running(n) => {\n\
                         if n >= 3 { Done } else { Running(n + 1) }\n\
                     },\n\
                     Done => Done,\n\
                 }\n\
             }\n\
             fn is_done(s: State) -> bool {\n\
                 match s {\n\
                     Done => true,\n\
                     _ => false,\n\
                 }\n\
             }\n\
             fn main() {\n\
                 let s1 = step(Idle);\n\
                 println(is_done(s1));\n\
                 let s2 = step(s1);\n\
                 let s3 = step(s2);\n\
                 let s4 = step(s3);\n\
                 let s5 = step(s4);\n\
                 println(is_done(s5));\n\
             }"),
        "false\ntrue\n"
    );
}

#[test]
fn test_e2e_higher_order_functions() {
    assert_eq!(
        run("fn apply_twice(f: Fn(i64) -> i64, x: i64) -> i64 {\n\
                 f(f(x))\n\
             }\n\
             fn add3(x: i64) -> i64 { x + 3 }\n\
             fn main() {\n\
                 println(apply_twice(add3, 10));\n\
                 println(apply_twice(|x: i64| x * 2, 5));\n\
             }"),
        "16\n20\n"
    );
}

#[test]
fn test_e2e_error_handling() {
    assert_eq!(
        run("fn divide(a: i64, b: i64) -> i64 {\n\
                 if b == 0 { return Err(0); }\n\
                 Ok(a / b)\n\
             }\n\
             fn main() {\n\
                 let r1 = divide(10, 2);\n\
                 println(r1.unwrap());\n\
                 let r2 = divide(10, 0);\n\
                 println(r2.is_err());\n\
             }"),
        "5\ntrue\n"
    );
}

// ── Edge Cases: Scoping ────────────────────────────────────────

#[test]
fn test_nested_scopes_shadow() {
    assert_eq!(
        run("fn main() {\n\
                 let x = 1;\n\
                 let y = {\n\
                     let x = 2;\n\
                     x\n\
                 };\n\
                 println(x);\n\
                 println(y);\n\
             }"),
        "1\n2\n"
    );
}

#[test]
fn test_variable_not_leaked_from_block() {
    // After a block, variables defined inside it should not be accessible
    // (the interpreter panics on undefined variable, so we test indirectly)
    assert_eq!(
        run("fn main() {\n\
                 let before = 10;\n\
                 { let inner = 20; }\n\
                 println(before);\n\
             }"),
        "10\n"
    );
}

#[test]
fn test_function_scope_isolation() {
    // Variables defined in one function call should not leak to the next
    assert_eq!(
        run("fn f() -> i64 {\n\
                 let local = 42;\n\
                 local\n\
             }\n\
             fn main() {\n\
                 println(f());\n\
                 println(f());\n\
             }"),
        "42\n42\n"
    );
}

// ── Edge Cases: Recursion & Control Flow ───────────────────────

#[test]
fn test_mutual_recursion() {
    assert_eq!(
        run("fn is_even(n: i64) -> bool {\n\
                 if n == 0 { true } else { is_odd(n - 1) }\n\
             }\n\
             fn is_odd(n: i64) -> bool {\n\
                 if n == 0 { false } else { is_even(n - 1) }\n\
             }\n\
             fn main() {\n\
                 println(is_even(4));\n\
                 println(is_odd(5));\n\
             }"),
        "true\ntrue\n"
    );
}

#[test]
fn test_nested_if_else() {
    assert_eq!(
        run("fn classify(x: i64) -> i64 {\n\
                 if x > 100 {\n\
                     3\n\
                 } else if x > 10 {\n\
                     2\n\
                 } else if x > 0 {\n\
                     1\n\
                 } else {\n\
                     0\n\
                 }\n\
             }\n\
             fn main() {\n\
                 println(classify(200));\n\
                 println(classify(50));\n\
                 println(classify(5));\n\
                 println(classify(-1));\n\
             }"),
        "3\n2\n1\n0\n"
    );
}

#[test]
fn test_break_with_variable_value() {
    assert_eq!(
        run("fn main() {\n\
                 let mut i = 0;\n\
                 let x = loop {\n\
                     i += 1;\n\
                     if i == 3 {\n\
                         break i;\n\
                     }\n\
                 };\n\
                 println(x);\n\
             }"),
        "3\n"
    );
}

#[test]
fn test_while_with_return() {
    assert_eq!(
        run("fn count_to_return(limit: i64) -> i64 {\n\
                 let mut i = 0;\n\
                 while true {\n\
                     i += 1;\n\
                     if i >= limit { return i; }\n\
                 }\n\
                 0\n\
             }\n\
             fn main() { println(count_to_return(5)); }"),
        "5\n"
    );
}

// ── Edge Cases: Struct Patterns ────────────────────────────────

#[test]
fn test_struct_destructuring_in_let() {
    assert_eq!(
        run("struct Point { x: i64, y: i64 }\n\
             fn main() {\n\
                 let p = Point { x: 10, y: 20 };\n\
                 let Point { x, y } = p;\n\
                 println(x + y);\n\
             }"),
        "30\n"
    );
}

#[test]
fn test_tuple_destructuring_in_let() {
    assert_eq!(
        run("fn main() {\n\
                 let t = (1, 2, 3);\n\
                 let (a, b, c) = t;\n\
                 println(a + b + c);\n\
             }"),
        "6\n"
    );
}

#[test]
fn test_nested_match_patterns() {
    assert_eq!(
        run("enum Outer { A(i64), B }\n\
             fn check(o: Outer) -> i64 {\n\
                 match o {\n\
                     A(0) => 100,\n\
                     A(n) => n,\n\
                     B => -1,\n\
                 }\n\
             }\n\
             fn main() {\n\
                 println(check(A(0)));\n\
                 println(check(A(42)));\n\
                 println(check(B));\n\
             }"),
        "100\n42\n-1\n"
    );
}

// ── Edge Cases: Closures ───────────────────────────────────────

#[test]
fn test_closure_as_return_value() {
    assert_eq!(
        run("fn make_adder(n: i64) -> Fn(i64) -> i64 {\n\
                 |x: i64| x + n\n\
             }\n\
             fn main() {\n\
                 let add5 = make_adder(5);\n\
                 println(add5(10));\n\
                 let add100 = make_adder(100);\n\
                 println(add100(1));\n\
             }"),
        "15\n101\n"
    );
}

// ── Closure calling through `ref` (round 12.6) ──────────────────
//
// Item 23: explicit `ref |...|` / `mut ref |...|` capture-mode prefix
// guarantees the closure is repeatable. The interpreter dispatches
// each invocation through the same Value::Function — multi-call
// patterns (loop, vec slot, repeated direct calls) must produce
// the expected per-call results without consuming the closure binding.

#[test]
fn test_ref_closure_invokes_multiple_times() {
    // `ref ||` body reads a captured field — three calls return the
    // same value (snapshot-at-creation-time semantics for the cloned
    // env, which is the interpreter's existing closure behavior).
    assert_eq!(
        run("struct Owned { x: i64 }\n\
             fn main() {\n\
                 let o = Owned { x: 7 };\n\
                 let f = ref || o.x + 1;\n\
                 println(f());\n\
                 println(f());\n\
                 println(f());\n\
             }"),
        "8\n8\n8\n"
    );
}

#[test]
fn test_ref_closure_called_in_for_loop() {
    // Motivating pattern from design.md §3638: a repeatable closure
    // invoked many times by a loop. The closure value is read from
    // the binding once per iteration.
    assert_eq!(
        run("struct Owned { x: i64 }\n\
             fn main() {\n\
                 let o = Owned { x: 10 };\n\
                 let f = ref || o.x;\n\
                 for _i in 0..3 {\n\
                     println(f());\n\
                 }\n\
             }"),
        "10\n10\n10\n"
    );
}

#[test]
fn test_repeatable_closure_in_vec_dispatched_via_index_call() {
    // Storing a repeatable closure in a Vec and invoking it through
    // `vec[i]()` exercises the interpreter's "callee evaluates to a
    // Value::Function" dispatch path (design.md §3638 — the
    // multi-callable case where `vec[i]()` is permitted).
    assert_eq!(
        run("fn main() {\n\
                 let n = 5;\n\
                 let f = ref || n + 1;\n\
                 let g = ref || n + 2;\n\
                 let callbacks = Vec[f, g];\n\
                 println(callbacks[0]());\n\
                 println(callbacks[1]());\n\
                 println(callbacks[0]());\n\
             }"),
        "6\n7\n6\n"
    );
}

// ── `mut ref |...|` capture-mutation propagation (round 12.48) ──
//
// Mutations made by a `mut ref` closure to a captured outer binding
// must persist across invocations and be observable via the outer
// binding after the calls return. Implemented by promoting each
// captured slot to `Value::SharedCell` at closure construction so
// reads/writes on either side route through the same Mutex<Value>.
// Bare and `ref ||` closures do NOT alias — those captures keep the
// snapshot-at-construction behavior the existing tests above pin.

#[test]
fn test_mut_ref_closure_assignment_propagates() {
    assert_eq!(
        run("fn main() {\n\
                 let mut counter = 0_i64;\n\
                 let bump = mut ref || { counter = counter + 1; };\n\
                 bump();\n\
                 bump();\n\
                 bump();\n\
                 println(counter);\n\
             }"),
        "3\n"
    );
}

#[test]
fn test_mut_ref_closure_compound_assign_propagates() {
    assert_eq!(
        run("fn main() {\n\
                 let mut counter = 10_i64;\n\
                 let bump = mut ref || { counter += 5; };\n\
                 bump();\n\
                 bump();\n\
                 println(counter);\n\
             }"),
        "20\n"
    );
}

#[test]
fn test_mut_ref_closure_vec_push_propagates() {
    assert_eq!(
        run("fn main() {\n\
                 let mut v = Vec[1_i64, 2, 3];\n\
                 let push5 = mut ref || { v.push(5); };\n\
                 push5();\n\
                 push5();\n\
                 println(v.len());\n\
             }"),
        "5\n"
    );
}

#[test]
fn test_mut_ref_closure_field_mutation_propagates() {
    // Field-level mutation through a captured struct binding routes
    // through `set_field` → `Env::set` → SharedCell write-through.
    assert_eq!(
        run("struct Counter { n: i64 }\n\
             fn main() {\n\
                 let mut c = Counter { n: 0 };\n\
                 let bump = mut ref || { c.n = c.n + 1; };\n\
                 bump();\n\
                 bump();\n\
                 bump();\n\
                 println(c.n);\n\
             }"),
        "3\n"
    );
}

#[test]
fn test_mut_ref_closure_observes_outer_change_between_calls() {
    // Aliasing is bidirectional — between calls the outer binding can
    // be mutated and the next invocation sees the updated value.
    assert_eq!(
        run("fn main() {\n\
                 let mut x = 1_i64;\n\
                 let print_x = mut ref || { println(x); x = x + 10; };\n\
                 print_x();\n\
                 x = 100_i64;\n\
                 print_x();\n\
                 println(x);\n\
             }"),
        "1\n100\n110\n"
    );
}

#[test]
fn test_mut_ref_closure_forwarded_to_higher_order_fn() {
    // The Value::Function clones when passed across function boundaries,
    // but the SharedCell aliases inside `closure_env` are Arc-based so
    // every clone shares the same backing cell — mutations made by the
    // higher-order function's invocations are still visible at main's
    // outer binding.
    assert_eq!(
        run("fn run_thrice(f: ref Fn()) { f(); f(); f(); }\n\
             fn main() {\n\
                 let mut counter = 0_i64;\n\
                 let bump = mut ref || { counter = counter + 1; };\n\
                 run_thrice(bump);\n\
                 println(counter);\n\
             }"),
        "3\n"
    );
}

#[test]
fn test_bare_closure_does_not_propagate_mutation() {
    // Pinning the negative case: a bare `|...|` closure (default — captures
    // by ownership) snapshots the captured value, so mutations stay local
    // to the body and the outer binding is untouched.
    assert_eq!(
        run("fn main() {\n\
                 let mut x = 0_i64;\n\
                 let f = || { let _y = x + 1; };\n\
                 f();\n\
                 f();\n\
                 println(x);\n\
             }"),
        "0\n"
    );
}

// ── Edge Cases: Method + Enum Interaction ──────────────────────

#[test]
fn test_static_method_constructor() {
    assert_eq!(
        run("struct Vec2 { x: i64, y: i64 }\n\
             impl Vec2 {\n\
                 fn new(x: i64, y: i64) -> Vec2 { Vec2 { x: x, y: y } }\n\
                 fn dot(self, other: Vec2) -> i64 { self.x * other.x + self.y * other.y }\n\
             }\n\
             fn main() {\n\
                 let a = Vec2.new(3, 4);\n\
                 let b = Vec2.new(1, 2);\n\
                 println(a.dot(b));\n\
             }"),
        "11\n"
    );
}

// ── Edge Cases: Multiple Return Paths ──────────────────────────

#[test]
fn test_multiple_returns() {
    assert_eq!(
        run("fn sign(x: i64) -> i64 {\n\
                 if x > 0 { return 1; }\n\
                 if x < 0 { return -1; }\n\
                 0\n\
             }\n\
             fn main() {\n\
                 println(sign(42));\n\
                 println(sign(-7));\n\
                 println(sign(0));\n\
             }"),
        "1\n-1\n0\n"
    );
}

// ── Generic Functions ──────────────────────────────────────────

#[test]
fn test_generic_identity_function() {
    assert_eq!(
        run("fn identity[T](x: T) -> T { x }\n\
             fn main() {\n\
                 println(identity(42));\n\
                 println(identity(true));\n\
             }"),
        "42\ntrue\n"
    );
}

#[test]
fn test_generic_pair() {
    assert_eq!(
        run("struct Pair[A, B] { first: A, second: B }\n\
             fn main() {\n\
                 let p = Pair { first: 1, second: true };\n\
                 println(p.first);\n\
                 println(p.second);\n\
             }"),
        "1\ntrue\n"
    );
}

// ── Print ──────────────────────────────────────────────────────

#[test]
fn test_print_no_newline() {
    assert_eq!(run(r#"fn main() { print("a"); print("b"); }"#), "ab");
}

#[test]
fn test_println_multiple() {
    assert_eq!(
        run("fn main() {\n\
                 println(1);\n\
                 println(2);\n\
                 println(3);\n\
             }"),
        "1\n2\n3\n"
    );
}

// ── Seq block ───────────────────────────────────────────────────

#[test]
fn test_seq_block_empty() {
    // seq {} evaluates to unit
    assert_eq!(run("fn main() { let x = seq { }; println(0); }"), "0\n");
}

#[test]
fn test_seq_block_value() {
    // seq { let x = 42; x } evaluates to 42
    assert_eq!(
        run("fn main() { let result = seq { let x = 42; x }; println(result); }"),
        "42\n"
    );
}

#[test]
fn test_seq_block_scoping() {
    // Variables inside seq are scoped to the block
    assert_eq!(
        run("fn main() {\n\
                 let a = 1;\n\
                 let b = seq { let inner = 10; inner + a };\n\
                 println(b);\n\
             }"),
        "11\n"
    );
}

// ── Labeled Loops ──────────────────────────────────────────────

#[test]
fn test_labeled_break_exits_outer_loop() {
    // `break label expr` syntax is required; `break label;` alone parses as break-with-value
    assert_eq!(
        run("fn main() {\n\
                 let mut count = 0;\n\
                 outer: for x in [1, 2, 3] {\n\
                     for y in [10, 20, 30] {\n\
                         count = count + 1;\n\
                         break outer ();\n\
                     }\n\
                 }\n\
                 println(count);\n\
             }"),
        "1\n"
    );
}

#[test]
fn test_labeled_continue_skips_outer_iteration() {
    assert_eq!(
        run("fn main() {\n\
                 let mut count = 0;\n\
                 outer: for x in [1, 2, 3] {\n\
                     for y in [10, 20] {\n\
                         count = count + 1;\n\
                         continue outer;\n\
                     }\n\
                 }\n\
                 println(count);\n\
             }"),
        "3\n"
    );
}

#[test]
fn test_labeled_break_with_value() {
    assert_eq!(
        run("fn main() {\n\
                 let result = outer: loop {\n\
                     loop {\n\
                         break outer 42;\n\
                     }\n\
                 };\n\
                 println(result);\n\
             }"),
        "42\n"
    );
}

#[test]
fn test_labeled_break_while_loop() {
    assert_eq!(
        run("fn main() {\n\
                 let mut i = 0;\n\
                 outer: while i < 5 {\n\
                     let mut j = 0;\n\
                     while j < 5 {\n\
                         if j == 2 {\n\
                             break outer ();\n\
                         }\n\
                         j = j + 1;\n\
                     }\n\
                     i = i + 1;\n\
                 }\n\
                 println(i);\n\
             }"),
        "0\n"
    );
}

#[test]
fn test_unlabeled_break_still_works() {
    assert_eq!(
        run("fn main() {\n\
                 let mut count = 0;\n\
                 outer: for x in [1, 2, 3] {\n\
                     for y in [10, 20, 30] {\n\
                         count = count + 1;\n\
                         break;\n\
                     }\n\
                 }\n\
                 println(count);\n\
             }"),
        "3\n"
    );
}

// ── Labeled blocks runtime ───────────────────────────────────
//
// Sibling slice to `tests/codegen.rs` "Labeled blocks runtime" — the
// interpreter side of the LBC4 design choice. The labeled-block expr
// arm in `eval_expr_inner` matches `ControlFlow::Break { label, value
// }` against its own label; non-matching labels propagate. See
// `docs/implementation_checklist/phase-5-diagnostics.md` § 5.2.

#[test]
fn test_interpreter_labeled_block_break_with_value() {
    // Mirror of codegen test 1: `lbl: { break lbl 42; -1 }` evaluates
    // to 42 through the tree-walk path.
    assert_eq!(
        run("fn main() {\n\
             let x: i64 = lbl: { break lbl 42; -1 };\n\
             println(x);\n\
         }"),
        "42\n"
    );
}

#[test]
fn test_interpreter_labeled_block_bare_break_unit() {
    // Bare `break label` exits with `Value::Unit`. Observable check:
    // post-block println runs.
    assert_eq!(
        run("fn main() {\n\
             lbl: { break lbl; };\n\
             println(7);\n\
         }"),
        "7\n"
    );
}

#[test]
fn test_interpreter_labeled_block_tail_expression() {
    // No break: the block falls through normally and the tail value
    // becomes the labeled block's value.
    assert_eq!(
        run("fn main() {\n\
             let x: i64 = lbl: { 99 };\n\
             println(x);\n\
         }"),
        "99\n"
    );
}

#[test]
fn test_interpreter_nested_labeled_break_outer() {
    // Mirror of codegen latent-bug regression gate: `outer: while {
    // inner: while { break outer; } }` exits the outer loop. Pre-slice
    // interpreter already routed through `ControlFlow::Break.label`
    // correctly, but the test pins the contract to prevent a future
    // regression that flattens the label match.
    assert_eq!(
        run("fn main() {\n\
             let mut count = 0;\n\
             outer: while true {\n\
                 inner: while true {\n\
                     count = count + 1;\n\
                     break outer ();\n\
                 }\n\
                 count = count + 100;\n\
             }\n\
             println(count);\n\
         }"),
        "1\n"
    );
}

// ── Error Return Trace Tests ──────────────────────────────────

#[test]
fn test_error_trace_single_question_mark() {
    let (_output, trace, truncated) = run_program_with_trace(
        "fn might_fail() {\n\
             let r = Err(42);\n\
             r?\n\
         }\n\
         fn main() {\n\
             let result = might_fail();\n\
             match result {\n\
                 Ok(v) => println(v),\n\
                 Err(e) => println(e),\n\
             }\n\
         }",
    );
    assert!(
        !trace.is_empty(),
        "Error trace should have at least one frame"
    );
    assert!(!truncated);
    assert_eq!(trace.len(), 1);
    // The ? is on line 3
    assert_eq!(trace[0].line, 3);
}

#[test]
fn test_error_trace_multi_level_propagation() {
    let (_output, trace, truncated) = run_program_with_trace(
        "fn inner() {\n\
             let r = Err(99);\n\
             r?\n\
         }\n\
         fn middle() {\n\
             let v = inner()?;\n\
             v\n\
         }\n\
         fn outer() {\n\
             let v = middle()?;\n\
             v\n\
         }\n\
         fn main() {\n\
             let result = outer();\n\
             match result {\n\
                 Ok(v) => println(v),\n\
                 Err(e) => println(e),\n\
             }\n\
         }",
    );
    assert!(!truncated);
    // inner's ?, middle's ?, outer's ? = 3 frames
    assert_eq!(trace.len(), 3, "Expected 3 trace frames, got {:?}", trace);
}

#[test]
fn test_error_trace_cleared_on_ok() {
    let (_output, trace, _truncated) = run_program_with_trace(
        "fn try_thing() {\n\
             let r = Ok(10);\n\
             r?\n\
         }\n\
         fn main() {\n\
             let v = try_thing();\n\
             println(v);\n\
         }",
    );
    assert!(
        trace.is_empty(),
        "Trace should be empty after Ok(?), got {:?}",
        trace
    );
}

#[test]
fn test_error_trace_none_propagation() {
    let (_output, trace, truncated) = run_program_with_trace(
        "fn find() {\n\
             let r = None;\n\
             r?\n\
         }\n\
         fn search() {\n\
             let v = find()?;\n\
             v\n\
         }\n\
         fn main() {\n\
             let result = search();\n\
             match result {\n\
                 Some(v) => println(v),\n\
                 None => println(\"not found\"),\n\
             }\n\
         }",
    );
    assert!(!truncated);
    assert_eq!(
        trace.len(),
        2,
        "Expected 2 trace frames for None propagation, got {:?}",
        trace
    );
}

#[test]
fn test_error_trace_truncation_at_64() {
    // Build a deeply nested chain of ? propagation (65 levels).
    // Needs a larger stack for deep recursion in debug builds.
    let result = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let mut source = String::new();
            source.push_str("fn func0() {\n    let r = Err(1);\n    r?\n}\n");
            for i in 1..=64 {
                source.push_str(&format!(
                    "fn func{i}() {{\n    let v = func{}()?;\n    v\n}}\n",
                    i - 1
                ));
            }
            source.push_str(
                "fn main() {\n\
                     let result = func64();\n\
                     match result {\n\
                         Ok(v) => println(v),\n\
                         Err(e) => println(e),\n\
                     }\n\
                 }",
            );
            let (_output, trace, truncated) = run_program_with_trace(&source);
            // 65 frames total, but max is 64, so oldest is dropped
            assert!(truncated, "Trace should be truncated at 64 frames");
            assert_eq!(trace.len(), 64, "Trace should have exactly 64 frames");
        })
        .expect("failed to spawn thread")
        .join();
    result.expect("test thread panicked");
}

// ── Par Block Tests ────────────────────────────────────────────

#[test]
fn test_par_block_basic() {
    let output = run("fn a() -> i32 { println(\"A\"); 1 }
         fn b() -> i32 { println(\"B\"); 2 }
         fn main() {
             par {
                 let x = a();
                 let y = b();
             };
             println(\"done\");
         }");
    // Both tasks run, output is merged in source order
    assert!(output.contains("A"));
    assert!(output.contains("B"));
    assert!(output.contains("done"));
}

#[test]
fn test_par_block_single_statement() {
    // Single statement in par — runs without threading
    let output = run("fn main() {
             par {
                 println(\"solo\");
             };
         }");
    assert!(output.contains("solo"));
}

#[test]
fn test_par_block_empty() {
    // Empty par block is valid
    let output = run("fn main() { par { }; println(\"ok\"); }");
    assert!(output.contains("ok"));
}

// ── IEEE 754 Float Semantics ──────────────────────────────────

#[test]
fn test_float_nan_not_equal() {
    // IEEE 754: NaN != NaN
    let output = run("fn main() { let nan = 0.0 / 0.0; println(nan == nan); }");
    assert_eq!(output, "false\n");
}

// ── F64/F32 Total-Order Types ─────────────────────────────────

#[test]
fn test_f64_from_constructor() {
    let output = run("fn main() { let x = F64.from(3.14); println(x); }");
    assert_eq!(output, "F64(3.14)\n");
}

#[test]
fn test_f32_from_constructor() {
    let output = run("fn main() { let x = F32.from(2.5); println(x); }");
    assert!(output.starts_with("F32(2.5"));
}

// ── Numeric primitive From (Step 4) ────────────────────────────

#[test]
fn test_int_from_widening() {
    let output = run("fn main() { let x: i32 = 42; let y: i64 = i64.from(x); println(y); }");
    assert_eq!(output, "42\n");
}

// ── ? cross-error From propagation (Step 5) ────────────────────

#[test]
fn test_question_cross_error_calls_from_impl() {
    let output = run("struct ParseError { msg: String }\n\
         struct AppError { msg: String }\n\
         impl From for AppError {\n\
             fn from(e: ParseError) -> AppError {\n\
                 AppError { msg: e.msg }\n\
             }\n\
         }\n\
         fn produce() -> Result[i64, ParseError] { Err(ParseError { msg: \"bad\" }) }\n\
         fn run_it() -> Result[i64, AppError] {\n\
             let x: i64 = produce()?;\n\
             Ok(x)\n\
         }\n\
         fn main() {\n\
             match run_it() {\n\
                 Ok(_) => println(\"ok\"),\n\
                 Err(e) => println(e.msg),\n\
             }\n\
         }");
    assert_eq!(output, "bad\n");
}

#[test]
fn test_float_from_widening() {
    let output = run("fn main() { let x: f32 = 1.5; let y: f64 = f64.from(x); println(y); }");
    assert_eq!(output, "1.5\n");
}

// ── .into() expected-type threading (Slice 3a) ────────────────

#[test]
fn test_into_at_let_annotation_uses_from_impl() {
    // `let y: i64 = x.into()` should dispatch through the `i64.from(x)`
    // widening impl — same semantics as calling `i64.from(x)` directly.
    let output = run("fn main() { let x: i32 = 42; let y: i64 = x.into(); println(y); }");
    assert_eq!(output, "42\n");
}

#[test]
fn test_into_at_return_position() {
    let output = run("fn widen(x: i32) -> i64 { x.into() }\n\
         fn main() { println(widen(7)); }");
    assert_eq!(output, "7\n");
}

#[test]
fn test_into_at_call_argument_position() {
    let output = run("fn takes(y: i64) { println(y); }\n\
         fn main() { let x: i32 = 99; takes(x.into()); }");
    assert_eq!(output, "99\n");
}

#[test]
fn test_into_drives_user_from_impl() {
    // User-defined `impl From[Inches] for Cm` should drive `.into()` at
    // a `let: Cm` position.
    let output = run("struct Inches { n: i64 }\n\
         struct Cm { n: i64 }\n\
         impl From for Cm {\n\
             fn from(i: Inches) -> Cm { Cm { n: i.n * 254 / 100 } }\n\
         }\n\
         fn main() {\n\
             let i: Inches = Inches { n: 10 };\n\
             let c: Cm = i.into();\n\
             println(c.n);\n\
         }");
    assert_eq!(output, "25\n");
}

// ── Atomic[T] Runtime ─────────────────────────────────────────

#[test]
fn test_atomic_new_and_load() {
    let output = run("fn main() {\n\
             let a = Atomic.new(42);\n\
             println(a.load(MemoryOrdering.SeqCst));\n\
         }");
    assert_eq!(output, "42\n");
}

#[test]
fn test_atomic_store_and_load() {
    let output = run("fn main() {\n\
             let mut a = Atomic.new(0);\n\
             a.store(99, MemoryOrdering.Relaxed);\n\
             println(a.load(MemoryOrdering.Relaxed));\n\
         }");
    assert_eq!(output, "99\n");
}

#[test]
fn test_atomic_bool() {
    let output = run("fn main() {\n\
             let flag = Atomic.new(false);\n\
             println(flag.load(MemoryOrdering.SeqCst));\n\
         }");
    assert_eq!(output, "false\n");
}

// ── Ordering / MemoryOrdering Enums ───────────────────────────

#[test]
fn test_ordering_variants() {
    // Comparison-Ordering variants (Less / Equal / Greater)
    let output = run("fn main() {\n\
             let lt = Ordering.Less;\n\
             let eq = Ordering.Equal;\n\
             let gt = Ordering.Greater;\n\
             println(lt);\n\
             println(eq);\n\
             println(gt);\n\
         }");
    assert!(output.contains("Less"));
    assert!(output.contains("Equal"));
    assert!(output.contains("Greater"));
}

#[test]
fn test_ordering_helper_methods() {
    // `impl Ordering { fn is_lt … }` per design.md § Comparison Traits
    // (lines 5162-5168). Lives in baked source `runtime/stdlib/ordering.kara`;
    // requires the interpreter to walk baked impl blocks.
    let output = run("fn main() {\n\
             let lt = Ordering.Less;\n\
             let eq = Ordering.Equal;\n\
             let gt = Ordering.Greater;\n\
             println(lt.is_lt());\n\
             println(lt.is_le());\n\
             println(lt.is_gt());\n\
             println(lt.is_ge());\n\
             println(lt.is_eq());\n\
             println(eq.is_lt());\n\
             println(eq.is_le());\n\
             println(eq.is_gt());\n\
             println(eq.is_ge());\n\
             println(eq.is_eq());\n\
             println(gt.is_lt());\n\
             println(gt.is_le());\n\
             println(gt.is_gt());\n\
             println(gt.is_ge());\n\
             println(gt.is_eq());\n\
         }");
    assert_eq!(
        output,
        "true\ntrue\nfalse\nfalse\nfalse\n\
         false\ntrue\nfalse\ntrue\ntrue\n\
         false\nfalse\ntrue\ntrue\nfalse\n",
    );
}

#[test]
fn test_memory_ordering_variants() {
    let output = run("fn main() {\n\
             let r = MemoryOrdering.Relaxed;\n\
             let a = MemoryOrdering.Acquire;\n\
             let rel = MemoryOrdering.Release;\n\
             println(r);\n\
             println(a);\n\
             println(rel);\n\
         }");
    assert!(output.contains("Relaxed"));
    assert!(output.contains("Acquire"));
    assert!(output.contains("Release"));
}

// ── Slice[T] end-to-end ────────────────────────────────────────

#[test]
fn test_slice_sum_over_array_coercion() {
    let output = run("fn sum(xs: Slice[i64]) -> i64 {
             let mut acc = 0;
             for x in xs { acc = acc + x; }
             acc
         }
         fn main() {
             let a: Array[i64, 4] = [1, 2, 3, 4];
             println(sum(a));
         }");
    assert_eq!(output, "10\n");
}

#[test]
fn test_slice_range_indexing_on_array() {
    let output = run("fn sum(xs: Slice[i64]) -> i64 {
             let mut acc = 0;
             for x in xs { acc = acc + x; }
             acc
         }
         fn main() {
             let a: Array[i64, 5] = [10, 20, 30, 40, 50];
             let s = a[1..4];
             println(sum(s));
         }");
    assert_eq!(output, "90\n");
}

#[test]
fn test_slice_element_indexing_runtime() {
    let output = run("fn second(xs: Slice[i64]) -> i64 { xs[1] }
         fn main() {
             let a: Array[i64, 3] = [7, 8, 9];
             println(second(a));
         }");
    assert_eq!(output, "8\n");
}

#[test]
fn test_as_slice_on_array() {
    let output = run("fn first(xs: Slice[i64]) -> i64 { xs[0] }
         fn main() {
             let a: Array[i64, 3] = [42, 2, 3];
             let s = a.as_slice();
             println(first(s));
         }");
    assert_eq!(output, "42\n");
}

#[test]
fn test_slice_of_slice_via_range() {
    let output = run("fn sum(xs: Slice[i64]) -> i64 {
             let mut acc = 0;
             for x in xs { acc = acc + x; }
             acc
         }
         fn main() {
             let a: Array[i64, 5] = [1, 2, 3, 4, 5];
             let outer = a[0..5];
             let inner = outer[1..4];
             println(sum(inner));
         }");
    assert_eq!(output, "9\n");
}

// ── Slice[T] stdlib methods ───────────────────────────────────────

#[test]
fn test_slice_is_empty_and_len() {
    let output = run("fn main() {
         let v = [1, 2, 3];
         let s = v.as_slice();
         println(s.is_empty());
         println(s.len());
     }");
    assert_eq!(output, "false\n3\n");
}

#[test]
fn test_slice_first_and_last() {
    let output = run("fn main() {
         let v = [10, 20, 30];
         let s = v.as_slice();
         match s.first() { Some(x) => println(x), None => println(\"none\") }
         match s.last()  { Some(x) => println(x), None => println(\"none\") }
     }");
    assert_eq!(output, "10\n30\n");
}

#[test]
fn test_slice_first_last_empty_slice() {
    let output = run("fn main() {
         let v: Array[i64, 0] = [];
         let s = v.as_slice();
         match s.first() { Some(x) => println(x), None => println(\"none\") }
         match s.last()  { Some(x) => println(x), None => println(\"none\") }
     }");
    assert_eq!(output, "none\nnone\n");
}

#[test]
fn test_slice_get_in_bounds_and_out_of_bounds() {
    let output = run("fn main() {
         let v = [100, 200, 300];
         let s = v.as_slice();
         match s.get(1) { Some(x) => println(x), None => println(\"oob\") }
         match s.get(5) { Some(x) => println(x), None => println(\"oob\") }
     }");
    assert_eq!(output, "200\noob\n");
}

#[test]
fn test_slice_contains() {
    let output = run("fn main() {
         let v = [1, 2, 3, 4];
         let s = v.as_slice();
         println(s.contains(3));
         println(s.contains(9));
     }");
    assert_eq!(output, "true\nfalse\n");
}

#[test]
fn test_slice_binary_search_found_and_not_found() {
    let output = run("fn main() {
         let v = [1, 2, 3, 4, 5];
         let s = v.as_slice();
         match s.binary_search(3) { Some(i) => println(i), None => println(\"not found\") }
         match s.binary_search(9) { Some(i) => println(i), None => println(\"not found\") }
     }");
    assert_eq!(output, "2\nnot found\n");
}

#[test]
fn test_slice_split_at() {
    let output = run("fn main() {
         let v = [1, 2, 3, 4];
         let s = v.as_slice();
         let (a, b) = s.split_at(2);
         println(a.len());
         println(b.len());
     }");
    assert_eq!(output, "2\n2\n");
}

#[test]
fn test_slice_chunks() {
    let output = run("fn main() {
         let v = [1, 2, 3, 4, 5];
         let s = v.as_slice();
         let cs = s.chunks(2);
         println(cs.len());
     }");
    assert_eq!(output, "3\n");
}

#[test]
fn test_slice_windows() {
    let output = run("fn main() {
         let v = [1, 2, 3, 4];
         let s = v.as_slice();
         let ws = s.windows(3);
         println(ws.len());
     }");
    assert_eq!(output, "2\n");
}

#[test]
fn test_slice_sort_and_reverse() {
    let output = run("fn main() {
         let mut v = [3, 1, 4, 1, 5];
         let mut s = v.as_slice_mut();
         s.sort();
         println(s.len());
         s.reverse();
         println(s.len());
     }");
    assert_eq!(output, "5\n5\n");
}

#[test]
fn test_slice_fill() {
    let output = run("fn main() {
         let mut v = [1, 2, 3];
         let mut s = v.as_slice_mut();
         s.fill(0);
         println(s.is_empty());
         println(s.len());
     }");
    assert_eq!(output, "false\n3\n");
}

#[test]
fn test_slice_swap() {
    let output = run("fn main() {
         let mut v = [10, 20, 30];
         let mut s = v.as_slice_mut();
         s.swap(0, 2);
         match s.get(0) { Some(x) => println(x), None => {} }
         match s.get(2) { Some(x) => println(x), None => {} }
     }");
    assert_eq!(output, "30\n10\n");
}

// ── env.args / env.var ────────────────────────────────────────────

#[test]
fn test_env_var_missing_key_returns_err() {
    let output = run("fn main() {
         match env.var(\"__KARAC_NO_SUCH_VAR_XYZ__\") {
             Ok(v) => println(v),
             Err(e) => println(\"not found\"),
         }
     }");
    assert_eq!(output, "not found\n");
}

#[test]
fn test_env_args_returns_array() {
    // env.args() returns a Vec[String]; len() ≥ 1 (includes binary path)
    let output = run("fn main() {
         let args = env.args();
         println(args.len() > 0);
     }");
    assert_eq!(output, "true\n");
}

#[test]
fn test_env_set_round_trips_via_var() {
    // env.set("X", "v") then env.var("X") observes "v". Confirms the new
    // stdlib method writes to the same process-environment block that
    // `env.var` reads from. Use a unique name to avoid collisions with
    // other tests running in the same process.
    std::env::remove_var("KARAC_ENV_SET_ROUND_TRIP_TEST");
    let output = run("fn main() {
         env.set(\"KARAC_ENV_SET_ROUND_TRIP_TEST\", \"hello-set\");
         match env.var(\"KARAC_ENV_SET_ROUND_TRIP_TEST\") {
             Ok(v) => println(v),
             Err(_) => println(\"unset\"),
         }
     }");
    std::env::remove_var("KARAC_ENV_SET_ROUND_TRIP_TEST");
    assert_eq!(output, "hello-set\n");
}

// ── impl From[VarError] for IoError — variant mapping ────────────────────

#[test]
fn test_var_error_not_present_maps_to_io_error_not_found() {
    // VarError.NotPresent → IoError.NotFound via the baked stdlib impl.
    let output = run("fn main() {
         let io: IoError = IoError.from(VarError.NotPresent);
         match io {
             IoError.NotFound => println(\"not_found\"),
             IoError.PermissionDenied => println(\"perm_denied\"),
             IoError.AlreadyExists => println(\"already_exists\"),
             IoError.UnexpectedEof => println(\"eof\"),
             IoError.InvalidUtf8 => println(\"invalid_utf8\"),
             IoError.Interrupted => println(\"interrupted\"),
             IoError.Other(_) => println(\"other\"),
         }
     }");
    assert_eq!(output, "not_found\n");
}

#[test]
fn test_var_error_not_unicode_maps_to_io_error_invalid_utf8() {
    // VarError.NotUnicode → IoError.InvalidUtf8 via the baked stdlib impl.
    let output = run("fn main() {
         let io: IoError = IoError.from(VarError.NotUnicode);
         match io {
             IoError.NotFound => println(\"not_found\"),
             IoError.PermissionDenied => println(\"perm_denied\"),
             IoError.AlreadyExists => println(\"already_exists\"),
             IoError.UnexpectedEof => println(\"eof\"),
             IoError.InvalidUtf8 => println(\"invalid_utf8\"),
             IoError.Interrupted => println(\"interrupted\"),
             IoError.Other(_) => println(\"other\"),
         }
     }");
    assert_eq!(output, "invalid_utf8\n");
}

#[test]
fn test_var_error_question_propagation_produces_io_error_not_found() {
    // End-to-end: `env.var(missing)?` in a function returning
    // `Result[String, IoError]` must propagate as `IoError.NotFound`
    // (because `env.var` returns `VarError.NotPresent` for an unset key,
    // and the `?` operator desugars through `IoError.from(...)`).
    std::env::remove_var("__KARAC_FROM_VAR_TO_IO_NO_SUCH_VAR__");
    let output = run("fn read_var() -> Result[String, IoError] with reads(Env) {
             let s: String = env.var(\"__KARAC_FROM_VAR_TO_IO_NO_SUCH_VAR__\")?;
             Ok(s)
         }
         fn main() {
             match read_var() {
                 Ok(v) => println(v),
                 Err(IoError.NotFound) => println(\"not_found\"),
                 Err(IoError.InvalidUtf8) => println(\"invalid_utf8\"),
                 Err(_) => println(\"other_io_err\"),
             }
         }");
    assert_eq!(output, "not_found\n");
}

// ── `with_provider` runtime + resource method dispatch ───────────

#[test]
fn test_with_provider_dispatches_resource_method_to_top_of_stack() {
    let output = run("effect resource UserDB;
         struct FakeDB { data: i64 }
         impl FakeDB { fn query(self, n: i64) -> i64 { self.data + n } }
         fn main() {
             with_provider[UserDB](FakeDB { data: 100 }, || {
                 println(UserDB.query(5));
             });
         }");
    assert_eq!(output, "105\n");
}

#[test]
fn test_with_provider_nested_same_resource_inner_shadows_outer_restored_on_pop() {
    let output = run("effect resource UserDB;
         struct Db { tag: i64 }
         impl Db { fn id(self) -> i64 { self.tag } }
         fn main() {
             with_provider[UserDB](Db { tag: 1 }, || {
                 println(UserDB.id());
                 with_provider[UserDB](Db { tag: 2 }, || {
                     println(UserDB.id());
                 });
                 println(UserDB.id());
             });
         }");
    assert_eq!(output, "1\n2\n1\n");
}

#[test]
fn test_with_provider_multiple_resources_resolve_independently() {
    let output = run("effect resource UserDB;
         effect resource AuditLog;
         struct Db { tag: i64 }
         impl Db { fn id(self) -> i64 { self.tag } }
         struct Log { count: i64 }
         impl Log { fn count(self) -> i64 { self.count } }
         fn main() {
             with_provider[UserDB](Db { tag: 42 }, || {
                 with_provider[AuditLog](Log { count: 7 }, || {
                     println(UserDB.id());
                     println(AuditLog.count());
                 });
             });
         }");
    assert_eq!(output, "42\n7\n");
}

#[test]
fn test_resource_method_without_provider_raises_runtime_error() {
    let errors = runtime_errors(
        "effect resource UserDB;
         struct Db { tag: i64 }
         impl Db { fn id(self) -> i64 { self.tag } }
         fn main() {
             println(UserDB.id());
         }",
    );
    assert_eq!(
        errors.len(),
        1,
        "expected one runtime error, got {:?}",
        errors
    );
    assert!(
        errors[0]
            .message
            .contains("no provider bound for resource 'UserDB'"),
        "message did not mention missing provider: {:?}",
        errors[0].message
    );
}

#[test]
fn test_with_provider_value_of_closure_is_returned() {
    let output = run("effect resource UserDB;
         struct Db { tag: i64 }
         impl Db { fn id(self) -> i64 { self.tag } }
         fn main() {
             let x = with_provider[UserDB](Db { tag: 99 }, || {
                 UserDB.id()
             });
             println(x);
         }");
    assert_eq!(output, "99\n");
}

#[test]
fn test_with_provider_frame_popped_after_closure_even_when_body_returns() {
    // After the `with_provider` block exits, the resource is unbound
    // again — the second bare `UserDB.id()` should fail with the same
    // "no provider" runtime error as the cold-start case.
    let errors = runtime_errors(
        "effect resource UserDB;
         struct Db { tag: i64 }
         impl Db { fn id(self) -> i64 { self.tag } }
         fn main() {
             with_provider[UserDB](Db { tag: 1 }, || {
                 println(UserDB.id());
             });
             println(UserDB.id());
         }",
    );
    assert!(
        errors.iter().any(|e| e
            .message
            .contains("no provider bound for resource 'UserDB'")),
        "expected one missing-provider error after block exit, got {:?}",
        errors
    );
}

// ── Ambient program-rooted resources (CR-A slice 3) ──────────────

#[test]
fn test_ambient_clock_now_returns_positive_timestamp() {
    // Bare `Clock.now()` outside any `with_provider` uses the ambient
    // default provider installed in the base frame. The system time is
    // well past the Unix epoch for all plausible test environments, so
    // a non-zero result is enough to prove the default fired.
    let output = run("fn main() {\n\
                          let t = Clock.now();\n\
                          println(t > 1_000_000_000);\n\
                      }");
    assert_eq!(output, "true\n");
}

#[test]
fn test_ambient_clock_with_provider_overrides_default() {
    // `with_provider[Clock]` pushes a fake on top of the ambient default.
    // Inside the scope the fake wins; after exit the ambient default is
    // still present (the frame popped back to the base frame, not past it).
    let output = run("struct FakeClock {}\n\
                      impl FakeClock { fn now(self) -> i64 { 42 } }\n\
                      fn main() {\n\
                          with_provider[Clock](FakeClock {}, || {\n\
                              println(Clock.now());\n\
                          });\n\
                          let t = Clock.now();\n\
                          println(t > 1_000_000_000);\n\
                      }");
    assert_eq!(output, "42\ntrue\n");
}

#[test]
fn test_ambient_clock_not_required_to_declare_effect_resource() {
    // `Clock` is a prelude effect resource — user code doesn't need
    // `effect resource Clock;` to call `Clock.now()` or wrap it in
    // `with_provider[Clock](...)`.
    let output = run("fn main() {\n\
                          let t = Clock.now();\n\
                          if t > 0 { println(\"ok\"); }\n\
                      }");
    assert_eq!(output, "ok\n");
}

#[test]
fn test_ambient_random_source_next_u64_advances_state() {
    // Two consecutive draws from the default `RandomSource` must return
    // different values — the xorshift state advances on every call. We
    // can't assert any specific number (seeded from wall-clock nanoseconds)
    // but inequality is a sharp witness that state advanced.
    let output = run("fn main() {\n\
                          let a = RandomSource.next_u64();\n\
                          let b = RandomSource.next_u64();\n\
                          println(a != b);\n\
                      }");
    assert_eq!(output, "true\n");
}

#[test]
fn test_ambient_random_source_with_provider_overrides_default() {
    // `with_provider[RandomSource]` shadows the ambient xorshift. Inside
    // the scope the fake wins (returns `7` twice); after exit the
    // interpreter is back on the ambient default (two fresh non-equal
    // draws).
    let output = run("struct FakeRandom {}\n\
                      impl FakeRandom { fn next_u64(self) -> i64 { 7 } }\n\
                      fn main() {\n\
                          with_provider[RandomSource](FakeRandom {}, || {\n\
                              println(RandomSource.next_u64());\n\
                              println(RandomSource.next_u64());\n\
                          });\n\
                          let a = RandomSource.next_u64();\n\
                          let b = RandomSource.next_u64();\n\
                          println(a != b);\n\
                      }");
    assert_eq!(output, "7\n7\ntrue\n");
}

#[test]
fn test_ambient_random_source_not_required_to_declare_effect_resource() {
    // `RandomSource` is a prelude effect resource — user code doesn't need
    // `effect resource RandomSource;` to call `RandomSource.next_u64()`.
    let output = run("fn main() {\n\
                          let _ = RandomSource.next_u64();\n\
                          println(\"ok\");\n\
                      }");
    assert_eq!(output, "ok\n");
}

#[test]
fn test_ambient_env_args_returns_nonempty_array() {
    // `Env.args()` returns process argv as `Vec[String]`. Under `cargo
    // test`, argv[0] is the test binary path, so the array is guaranteed
    // non-empty. We don't assert on exact contents (too environment-
    // dependent) — the length check is a sharp witness the default fired.
    let output = run("fn main() {\n\
                          let a = Env.args();\n\
                          println(a.len() > 0);\n\
                      }");
    assert_eq!(output, "true\n");
}

#[test]
fn test_ambient_env_with_provider_overrides_default() {
    // `with_provider[Env]` shadows the ambient default. The fake's
    // `args()` returns a controlled Vec; after exit the ambient default
    // (real argv) is back.
    let output = run("struct FakeEnv {}\n\
                      impl FakeEnv { fn args(self) -> Vec[String] { [\"a\", \"b\"] } }\n\
                      fn main() {\n\
                          with_provider[Env](FakeEnv {}, || {\n\
                              let a = Env.args();\n\
                              println(a.len());\n\
                              println(a[0]);\n\
                              println(a[1]);\n\
                          });\n\
                          let outer = Env.args();\n\
                          println(outer.len() > 0);\n\
                      }");
    assert_eq!(output, "2\na\nb\ntrue\n");
}

#[test]
fn test_ambient_env_not_required_to_declare_effect_resource() {
    // `Env` is a prelude effect resource — user code doesn't need
    // `effect resource Env;` to call `Env.args()`.
    let output = run("fn main() {\n\
                          let _ = Env.args();\n\
                          println(\"ok\");\n\
                      }");
    assert_eq!(output, "ok\n");
}

#[test]
fn test_ambient_env_var_present_returns_ok() {
    // `Env.var(name)` returns `Ok(value)` for a set environment variable.
    // We set the var via `std::env::set_var` from the test harness, then
    // observe it through the interpreter's ambient default provider.
    std::env::set_var("KARAC_ENV_VAR_PRESENT_TEST", "hello");
    let output = run("fn main() {\n\
                          let r = Env.var(\"KARAC_ENV_VAR_PRESENT_TEST\");\n\
                          match r {\n\
                              Ok(v) => println(v),\n\
                              Err(_) => println(\"unset\"),\n\
                          }\n\
                      }");
    std::env::remove_var("KARAC_ENV_VAR_PRESENT_TEST");
    assert_eq!(output, "hello\n");
}

#[test]
fn test_ambient_env_var_missing_returns_err_not_present() {
    // Missing var returns `Err(VarError.NotPresent)`. We don't name the
    // variant explicitly (it's not in `PRELUDE_VARIANTS` per v49 Q2=B);
    // the `Err(_)` arm proves the Err branch fired.
    std::env::remove_var("KARAC_ENV_VAR_MISSING_TEST_XYZ");
    let output = run("fn main() {\n\
                          let r = Env.var(\"KARAC_ENV_VAR_MISSING_TEST_XYZ\");\n\
                          match r {\n\
                              Ok(_) => println(\"set\"),\n\
                              Err(_) => println(\"unset\"),\n\
                          }\n\
                      }");
    assert_eq!(output, "unset\n");
}

#[test]
fn test_ambient_env_var_with_provider_overrides_default() {
    // `with_provider[Env]` shadows the ambient default — the FakeEnv's
    // `var` method wins inside the scope. FakeEnv uses `Result[String,
    // String]` as its return type to avoid pinning the test on `VarError`
    // resolving as a user-visible name (the interpreter's resource-method
    // dispatch is duck-typed at runtime).
    let output = run("struct FakeEnv {}\n\
                      impl FakeEnv {\n\
                          fn var(self, name: ref String) -> Result[String, String] {\n\
                              if name == \"FOO\" { Ok(\"fake-foo\") } else { Err(\"\") }\n\
                          }\n\
                      }\n\
                      fn main() {\n\
                          with_provider[Env](FakeEnv {}, || {\n\
                              match Env.var(\"FOO\") {\n\
                                  Ok(v) => println(v),\n\
                                  Err(_) => println(\"err\"),\n\
                              }\n\
                              match Env.var(\"BAR\") {\n\
                                  Ok(_) => println(\"ok\"),\n\
                                  Err(_) => println(\"err\"),\n\
                              }\n\
                          });\n\
                      }");
    assert_eq!(output, "fake-foo\nerr\n");
}

#[test]
fn test_ambient_stdout_println_resource_method_writes() {
    // Direct `Stdout.println(s)` dispatches to the BuiltinDefault arm,
    // which routes through `write_stdout` and lands in the harness's
    // `captured_output` buffer. Same path the routed free `println`
    // takes — this just exercises the user-visible Stdout surface.
    let output = run("fn main() {\n\
                          Stdout.println(\"hello\");\n\
                          Stdout.println(\"world\");\n\
                      }");
    assert_eq!(output, "hello\nworld\n");
}

#[test]
fn test_ambient_stdout_print_resource_method_writes_without_newline() {
    // `Stdout.print(s)` does NOT append a newline — the test asserts
    // two `print` calls concatenate cleanly, then a trailing `println`
    // closes the line so the captured buffer ends with `\n`.
    let output = run("fn main() {\n\
                          Stdout.print(\"a\");\n\
                          Stdout.print(\"b\");\n\
                          Stdout.println(\"c\");\n\
                      }");
    assert_eq!(output, "abc\n");
}

#[test]
fn test_free_println_routes_through_stdout_provider() {
    // The free `println(x)` is routed through the `Stdout` provider
    // stack — installing a `with_provider[Stdout]` fake intercepts the
    // call. The Mute fake swallows everything; the inner `println` has
    // no observable effect, while the outer `println` (after the scope
    // pops back to the BuiltinDefault) still writes normally.
    let output = run("struct Mute {}\n\
                      impl Mute {\n\
                          fn println(self, s: String) { }\n\
                          fn print(self, s: String) { }\n\
                      }\n\
                      fn main() {\n\
                          with_provider[Stdout](Mute {}, || {\n\
                              println(\"hidden\");\n\
                              println(\"also hidden\");\n\
                          });\n\
                          println(\"visible\");\n\
                      }");
    assert_eq!(output, "visible\n");
}

#[test]
fn test_ambient_stdout_not_required_to_declare_effect_resource() {
    // `Stdout` is a prelude effect resource — user code doesn't need
    // `effect resource Stdout;` to call `Stdout.println(...)`.
    let output = run("fn main() {\n\
                          Stdout.println(\"ok\");\n\
                      }");
    assert_eq!(output, "ok\n");
}

#[test]
fn test_eprintln_routes_through_stderr_provider() {
    // `eprintln(x)` previously panicked with "variable 'eprintln' not
    // found" — it was in PRELUDE_FUNCTIONS but had no interpreter arm.
    // Now it routes through `Stderr.println` like `println` routes
    // through `Stdout.println`. We can't assert the stderr contents
    // (the test harness only captures stdout) but we can prove the
    // call succeeds and a subsequent `println` still writes stdout.
    let output = run("fn main() {\n\
                          eprintln(\"to stderr\");\n\
                          println(\"to stdout\");\n\
                      }");
    assert_eq!(output, "to stdout\n");
}

#[test]
fn test_ambient_stdin_with_provider_overrides_default() {
    // Symmetric to the Stdout interception test: a `with_provider[Stdin]`
    // install routes `Stdin.read_line()` through the user's fake instead
    // of pulling from the real stdin. CannedStdin returns a fixed line;
    // the test asserts that line came back through the provider stack.
    // Uses `Result[String, String]` to dodge `IoError` name resolution
    // (the dispatch is duck-typed at runtime — same trick as the Env test).
    let output = run("struct CannedStdin {}\n\
                      impl CannedStdin {\n\
                          fn read_line(self) -> Result[String, String] {\n\
                              Ok(\"piped line\")\n\
                          }\n\
                      }\n\
                      fn main() {\n\
                          with_provider[Stdin](CannedStdin {}, || {\n\
                              match Stdin.read_line() {\n\
                                  Ok(s)  => println(s),\n\
                                  Err(_) => println(\"err\"),\n\
                              }\n\
                          });\n\
                      }");
    assert_eq!(output, "piped line\n");
}

// ── `providers { R => p, ... } in { body }` block ───────────────

#[test]
fn test_providers_block_binds_each_resource() {
    let output = run("effect resource UserDB;
         effect resource AuditLog;
         struct Db { tag: i64 }
         impl Db { fn id(self) -> i64 { self.tag } }
         struct Log { count: i64 }
         impl Log { fn count(self) -> i64 { self.count } }
         fn main() {
             providers {
                 UserDB   => Db { tag: 7 },
                 AuditLog => Log { count: 3 },
             } in {
                 println(UserDB.id());
                 println(AuditLog.count());
             }
         }");
    assert_eq!(output, "7\n3\n");
}

#[test]
fn test_providers_block_returns_body_value() {
    let output = run("effect resource UserDB;
         struct Db { tag: i64 }
         impl Db { fn id(self) -> i64 { self.tag } }
         fn main() {
             let r = providers {
                 UserDB => Db { tag: 42 },
             } in {
                 UserDB.id()
             };
             println(r);
         }");
    assert_eq!(output, "42\n");
}

#[test]
fn test_providers_block_single_trailing_comma_accepted() {
    let output = run("effect resource UserDB;
         struct Db { tag: i64 }
         impl Db { fn id(self) -> i64 { self.tag } }
         fn main() {
             providers {
                 UserDB => Db { tag: 9 },
             } in {
                 println(UserDB.id());
             }
         }");
    assert_eq!(output, "9\n");
}

#[test]
fn test_providers_block_frames_popped_after_body() {
    // After the block exits, all bindings are released — the trailing
    // `UserDB.id()` should fail with a missing-provider error, confirming
    // every pushed frame was popped.
    let errors = runtime_errors(
        "effect resource UserDB;
         effect resource AuditLog;
         struct Db { tag: i64 }
         impl Db { fn id(self) -> i64 { self.tag } }
         struct Log { count: i64 }
         impl Log { fn count(self) -> i64 { self.count } }
         fn main() {
             providers {
                 UserDB   => Db { tag: 1 },
                 AuditLog => Log { count: 2 },
             } in {
                 println(UserDB.id());
             }
             println(UserDB.id());
         }",
    );
    assert!(
        errors.iter().any(|e| e
            .message
            .contains("no provider bound for resource 'UserDB'")),
        "expected missing-provider error after block exit, got {:?}",
        errors
    );
}

#[test]
fn test_providers_block_evaluates_all_expressions_before_scope() {
    // Evaluate-all-then-scope semantics: every provider expression runs in
    // source order before any resource scope opens. We observe this by
    // threading a captured `i` counter through side-effecting constructors
    // that each append a trace string; the body ties off the sequence.
    let output = run("effect resource R1;
         effect resource R2;
         struct P { n: i64 }
         impl P { fn n(self) -> i64 { self.n } }
         fn mk1() -> P { println(\"eval1\"); P { n: 1 } }
         fn mk2() -> P { println(\"eval2\"); P { n: 2 } }
         fn main() {
             providers {
                 R1 => mk1(),
                 R2 => mk2(),
             } in {
                 println(\"body\");
                 println(R1.n());
                 println(R2.n());
             }
         }");
    assert_eq!(output, "eval1\neval2\nbody\n1\n2\n");
}

#[test]
fn test_providers_block_inner_binding_overrides_outer_with_provider() {
    // Nesting a `providers { R => ... }` block inside an outer
    // `with_provider[R]` should shadow the outer binding for the body's
    // duration and restore it on exit — same stack semantics as
    // nested `with_provider`.
    let output = run("effect resource UserDB;
         struct Db { tag: i64 }
         impl Db { fn id(self) -> i64 { self.tag } }
         fn main() {
             with_provider[UserDB](Db { tag: 1 }, || {
                 println(UserDB.id());
                 providers {
                     UserDB => Db { tag: 2 },
                 } in {
                     println(UserDB.id());
                 }
                 println(UserDB.id());
             });
         }");
    assert_eq!(output, "1\n2\n1\n");
}

// ── Standard I/O interpreter tests ──────────────────────────────────────────

#[test]
fn test_filesystem_write_and_read_roundtrip() {
    let tmp = std::env::temp_dir().join("karac_test_fs_roundtrip.txt");
    // Escape backslashes so Windows paths (C:\Users\...) are valid inside a Kāra string literal.
    let path = tmp.to_str().unwrap().replace('\\', "\\\\");
    // Write via the host OS directly, then read back through the interpreter.
    std::fs::write(&tmp, "hello kara").expect("temp write");
    let src = format!(
        "fn main() {{
             let r = FileSystem.read_to_string(\"{path}\");
             match r {{
                 Ok(contents) => println(contents),
                 Err(_) => println(\"read error\"),
             }}
         }}"
    );
    let out = run_no_errors(&src);
    assert_eq!(out, "hello kara\n");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn test_filesystem_read_nonexistent_file_returns_err() {
    let src = "fn main() {
                   let r = FileSystem.read_to_string(\"/nonexistent_karac_test_xyz.txt\");
                   match r {
                       Ok(_) => println(\"ok\"),
                       Err(e) => match e {
                           IoError.NotFound => println(\"not found\"),
                           _ => println(\"other error\"),
                       },
                   }
               }";
    let out = run_no_errors(src);
    assert_eq!(out, "not found\n");
}

#[test]
fn test_stdout_flush_is_callable() {
    // Stdout.flush() should return Unit without error.
    let out = run_no_errors("fn main() { Stdout.flush(); println(\"ok\"); }");
    assert_eq!(out, "ok\n");
}

#[test]
fn test_stderr_flush_is_callable() {
    let out = run_no_errors("fn main() { Stderr.flush(); println(\"ok\"); }");
    assert_eq!(out, "ok\n");
}

// ── std.runtime introspection (Debugger Contract slice 5) ─────────────────────
//
// The tree-walk interpreter has its own par-block evaluation path and does
// NOT construct `KaracFrame` / `ACTIVE_FRAMES` state, so all three APIs
// return the empty / false form per design.md's "try-then-degrade" contract.
// Real values flow through codegen in compiled binaries; the interpreter
// returns degraded results that are still well-typed (empty Vec / false
// bool). When/if interpreter parity for active-frame enumeration ships
// (post-v1), these tests upgrade to assert real values.

#[test]
fn test_runtime_has_debug_metadata_returns_false_in_interpreter() {
    let out = run_no_errors(
        "fn main() {
             let dbg = Runtime.has_debug_metadata();
             if dbg {
                 println(1);
             } else {
                 println(0);
             }
         }",
    );
    assert_eq!(out, "0\n");
}

#[test]
fn test_runtime_list_par_blocks_returns_empty_in_interpreter() {
    let out = run_no_errors(
        "fn main() {
             let pbs = Runtime.list_par_blocks();
             println(pbs.len());
         }",
    );
    assert_eq!(out, "0\n");
}

#[test]
fn test_runtime_list_tasks_returns_empty_in_interpreter() {
    let out = run_no_errors(
        "fn main() {
             let tasks = Runtime.list_tasks();
             println(tasks.len());
         }",
    );
    assert_eq!(out, "0\n");
}

// ── Default parameter values ──────────────────────────────────────

#[test]
fn test_default_param_used_when_arg_omitted() {
    let out = run_no_errors(
        "fn greet(name: String, greeting: String = \"Hello\") -> String { greeting + \", \" + name }\n\
         fn main() { println(greet(\"world\")); }",
    );
    assert_eq!(out, "Hello, world\n");
}

#[test]
fn test_default_param_overridden_by_explicit_arg() {
    let out = run_no_errors(
        "fn greet(name: String, greeting: String = \"Hello\") -> String { greeting + \", \" + name }\n\
         fn main() { println(greet(\"world\", \"Hi\")); }",
    );
    assert_eq!(out, "Hi, world\n");
}

#[test]
fn test_default_param_integer() {
    let out = run_no_errors(
        "fn add(x: i64, step: i64 = 1) -> i64 { x + step }\n\
         fn main() { println(add(10)); println(add(10, 5)); }",
    );
    assert_eq!(out, "11\n15\n");
}

#[test]
fn test_multiple_defaults_substitution() {
    // Both trailing params omitted — both defaults fire.
    let out = run_no_errors(
        "fn pair(a: i64, b: i64 = 10, c: i64 = 20) -> i64 { a + b + c }\n\
         fn main() {\n\
             println(pair(1));\n\
             println(pair(1, 5));\n\
             println(pair(1, 5, 7));\n\
         }",
    );
    assert_eq!(out, "31\n26\n13\n");
}

// ── Debug trait format (item 161) ─────────────────────────────────────────────

#[test]
fn test_assert_eq_failure_shows_debug_format_for_strings() {
    // assert_eq failure left/right fields show strings with quotes.
    let errors = runtime_errors("fn main() { assert_eq(\"hello\", \"world\"); }");
    assert!(!errors.is_empty(), "expected a runtime error");
    let e = &errors[0];
    assert_eq!(e.left.as_deref(), Some("\"hello\""));
    assert_eq!(e.right.as_deref(), Some("\"world\""));
}

#[test]
fn test_assert_ne_failure_shows_debug_format() {
    // assert_ne failure for equal strings shows debug format.
    let errors = runtime_errors("fn main() { assert_ne(\"same\", \"same\"); }");
    assert!(!errors.is_empty(), "expected a runtime error");
    let e = &errors[0];
    assert_eq!(e.left.as_deref(), Some("\"same\""));
    assert_eq!(e.right.as_deref(), Some("\"same\""));
}

#[test]
fn test_assert_eq_failure_integer_values() {
    let errors = runtime_errors("fn main() { assert_eq(1i64, 2i64); }");
    assert!(!errors.is_empty(), "expected a runtime error");
    let e = &errors[0];
    assert_eq!(e.left.as_deref(), Some("1"));
    assert_eq!(e.right.as_deref(), Some("2"));
}

// ── #[derive(Display)] on unit enums ────────────────────────────

#[test]
fn test_derive_display_to_string_returns_variant_name() {
    // `#[derive(Display)]` — `.to_string()` returns the PascalCase variant name.
    let output = run("#[derive(Display)]\n\
         enum Direction { Up, Down, Left, Right }\n\
         fn main() {\n\
             let d = Direction.Up;\n\
             println(d.to_string());\n\
         }");
    assert_eq!(output, "Up\n");
}

#[test]
fn test_derive_display_snake_case_lowercases_variant() {
    // `#[derive(Display(snake_case))]` — `.to_string()` returns the lower_snake_case name.
    let output = run("#[derive(Display(snake_case))]\n\
         enum Status { Active, InProgress, Done }\n\
         fn main() {\n\
             let s = Status.InProgress;\n\
             println(s.to_string());\n\
         }");
    assert_eq!(output, "in_progress\n");
}

#[test]
fn test_derive_display_fstring_interpolation() {
    // Enum with derived Display works inside an f-string.
    let output = run("#[derive(Display)]\n\
         enum Color { Red, Green, Blue }\n\
         fn main() {\n\
             let c = Color.Green;\n\
             println(f\"color={c.to_string()}\");\n\
         }");
    assert_eq!(output, "color=Green\n");
}

// ── SortedSet[T] stdlib methods ────────────────────────────────────────────

#[test]
fn test_sorted_set_new_and_len() {
    let output = run("fn main() {\n\
             let s: SortedSet[i64] = SortedSet.new();\n\
             println(f\"{s.len()}\");\n\
         }");
    assert_eq!(output, "0\n");
}

#[test]
fn test_sorted_set_insert_and_contains() {
    let output = run("fn main() {\n\
             let s: SortedSet[i64] = SortedSet.new();\n\
             let inserted = s.insert(3_i64);\n\
             println(f\"{inserted}\");\n\
             let again = s.insert(3_i64);\n\
             println(f\"{again}\");\n\
             println(f\"{s.contains(3_i64)}\");\n\
             println(f\"{s.contains(99_i64)}\");\n\
         }");
    assert_eq!(output, "true\nfalse\ntrue\nfalse\n");
}

#[test]
fn test_sorted_set_remove() {
    let output = run("fn main() {\n\
             let s: SortedSet[i64] = SortedSet.new();\n\
             s.insert(5_i64);\n\
             let removed = s.remove(5_i64);\n\
             println(f\"{removed}\");\n\
             let again = s.remove(5_i64);\n\
             println(f\"{again}\");\n\
             println(f\"{s.is_empty()}\");\n\
         }");
    assert_eq!(output, "true\nfalse\ntrue\n");
}

#[test]
fn test_sorted_set_min_max() {
    let output = run("fn main() {\n\
             let s: SortedSet[i64] = SortedSet.new();\n\
             s.insert(5_i64);\n\
             s.insert(1_i64);\n\
             s.insert(9_i64);\n\
             match s.min() {\n\
                 Some(v) => println(f\"{v}\"),\n\
                 None    => println(\"empty\"),\n\
             }\n\
             match s.max() {\n\
                 Some(v) => println(f\"{v}\"),\n\
                 None    => println(\"empty\"),\n\
             }\n\
         }");
    assert_eq!(output, "1\n9\n");
}

#[test]
fn test_sorted_set_min_max_empty() {
    let output = run("fn main() {\n\
             let s: SortedSet[i64] = SortedSet.new();\n\
             match s.min() {\n\
                 Some(v) => println(f\"{v}\"),\n\
                 None    => println(\"empty\"),\n\
             }\n\
         }");
    assert_eq!(output, "empty\n");
}

#[test]
fn test_sorted_set_ordered_iteration() {
    let output = run("fn main() {\n\
             let s: SortedSet[i64] = SortedSet.new();\n\
             s.insert(7_i64);\n\
             s.insert(2_i64);\n\
             s.insert(5_i64);\n\
             for x in s {\n\
                 println(f\"{x}\");\n\
             }\n\
         }");
    assert_eq!(output, "2\n5\n7\n");
}

#[test]
fn test_sorted_set_union() {
    let output = run("fn main() {\n\
             let a: SortedSet[i64] = SortedSet.new();\n\
             a.insert(1_i64);\n\
             a.insert(2_i64);\n\
             let b: SortedSet[i64] = SortedSet.new();\n\
             b.insert(2_i64);\n\
             b.insert(3_i64);\n\
             let u = a.union(b);\n\
             println(f\"{u.len()}\");\n\
             for x in u {\n\
                 println(f\"{x}\");\n\
             }\n\
         }");
    assert_eq!(output, "3\n1\n2\n3\n");
}

#[test]
fn test_sorted_set_intersection() {
    let output = run("fn main() {\n\
             let a: SortedSet[i64] = SortedSet.new();\n\
             a.insert(1_i64);\n\
             a.insert(2_i64);\n\
             a.insert(3_i64);\n\
             let b: SortedSet[i64] = SortedSet.new();\n\
             b.insert(2_i64);\n\
             b.insert(3_i64);\n\
             b.insert(4_i64);\n\
             let i = a.intersection(b);\n\
             for x in i {\n\
                 println(f\"{x}\");\n\
             }\n\
         }");
    assert_eq!(output, "2\n3\n");
}

#[test]
fn test_sorted_set_difference() {
    let output = run("fn main() {\n\
             let a: SortedSet[i64] = SortedSet.new();\n\
             a.insert(1_i64);\n\
             a.insert(2_i64);\n\
             a.insert(3_i64);\n\
             let b: SortedSet[i64] = SortedSet.new();\n\
             b.insert(2_i64);\n\
             let d = a.difference(b);\n\
             for x in d {\n\
                 println(f\"{x}\");\n\
             }\n\
         }");
    assert_eq!(output, "1\n3\n");
}

#[test]
fn test_sorted_set_dedup_on_insert() {
    let output = run("fn main() {\n\
             let s: SortedSet[i64] = SortedSet.new();\n\
             s.insert(4_i64);\n\
             s.insert(4_i64);\n\
             s.insert(4_i64);\n\
             println(f\"{s.len()}\");\n\
         }");
    assert_eq!(output, "1\n");
}

#[test]
fn test_sorted_set_string_elements() {
    let output = run("fn main() {\n\
             let s: SortedSet[String] = SortedSet.new();\n\
             s.insert(\"banana\");\n\
             s.insert(\"apple\");\n\
             s.insert(\"cherry\");\n\
             for x in s {\n\
                 println(x);\n\
             }\n\
         }");
    assert_eq!(output, "apple\nbanana\ncherry\n");
}

// ── Channel[T] / Sender[T] / Receiver[T] ──────────────────────────────────────

#[test]
fn test_channel_send_recv_round_trip() {
    let output = run("fn main() {\n\
             let (sender, receiver) = Channel.new();\n\
             sender.send(42_i64);\n\
             let val = receiver.recv();\n\
             println(f\"{val}\");\n\
         }");
    assert_eq!(output, "42\n");
}

#[test]
fn test_channel_try_recv_non_empty() {
    let output = run("fn main() {\n\
             let (sender, receiver) = Channel.new();\n\
             sender.send(7_i64);\n\
             match receiver.try_recv() {\n\
                 Some(v) => println(f\"{v}\"),\n\
                 None    => println(\"empty\"),\n\
             }\n\
         }");
    assert_eq!(output, "7\n");
}

#[test]
fn test_channel_try_recv_empty_returns_none() {
    let output = run("fn main() {\n\
             let (_sender, receiver) = Channel.new();\n\
             match receiver.try_recv() {\n\
                 Some(v) => println(f\"{v}\"),\n\
                 None    => println(\"empty\"),\n\
             }\n\
         }");
    assert_eq!(output, "empty\n");
}

#[test]
fn test_channel_multiple_sends_fifo() {
    let output = run("fn main() {\n\
             let (sender, receiver) = Channel.new();\n\
             sender.send(1_i64);\n\
             sender.send(2_i64);\n\
             sender.send(3_i64);\n\
             println(f\"{receiver.recv()}\");\n\
             println(f\"{receiver.recv()}\");\n\
             println(f\"{receiver.recv()}\");\n\
         }");
    assert_eq!(output, "1\n2\n3\n");
}

#[test]
fn test_channel_cloned_sender() {
    let output = run("fn main() {\n\
             let (sender, receiver) = Channel.new();\n\
             let s2 = sender.clone();\n\
             sender.send(10_i64);\n\
             s2.send(20_i64);\n\
             println(f\"{receiver.recv()}\");\n\
             println(f\"{receiver.recv()}\");\n\
         }");
    assert_eq!(output, "10\n20\n");
}

#[test]
fn test_channel_string_values() {
    let output = run("fn main() {\n\
             let (sender, receiver) = Channel.new();\n\
             sender.send(\"hello\");\n\
             let msg = receiver.recv();\n\
             println(msg);\n\
         }");
    assert_eq!(output, "hello\n");
}

// ── #[derive(Arithmetic)] on distinct types ────────────────────

#[test]
fn test_derive_arithmetic_addition() {
    let output = run_no_errors(
        "#[derive(Arithmetic)]\n\
         distinct type Meters = i64;\n\
         fn main() {\n\
             let a: Meters = 10;\n\
             let b: Meters = 3;\n\
             let sum: Meters = a + b;\n\
             println(sum);\n\
         }",
    );
    assert_eq!(output, "13\n");
}

#[test]
fn test_derive_arithmetic_negation() {
    let output = run_no_errors(
        "#[derive(Arithmetic)]\n\
         distinct type Offset = i64;\n\
         fn main() {\n\
             let x: Offset = 7;\n\
             let neg: Offset = -x;\n\
             println(neg);\n\
         }",
    );
    assert_eq!(output, "-7\n");
}

#[test]
fn test_derive_arithmetic_all_ops() {
    let output = run_no_errors(
        "#[derive(Arithmetic)]\n\
         distinct type Score = i64;\n\
         fn main() {\n\
             let a: Score = 10;\n\
             let b: Score = 3;\n\
             println(a + b);\n\
             println(a - b);\n\
             println(a * b);\n\
             println(a / b);\n\
             println(a % b);\n\
         }",
    );
    assert_eq!(output, "13\n7\n30\n3\n1\n");
}

// ── Map[K, V] interpreter tests ────────────────────────────────────────────

#[test]
fn test_map_new_and_len() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             println(m.len());\n\
         }");
    assert_eq!(output, "0\n");
}

#[test]
fn test_map_insert_and_get() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"a\", 1_i64);\n\
             m.insert(\"b\", 2_i64);\n\
             let v = m.get(\"a\");\n\
             match v {\n\
                 Some(x) => println(x),\n\
                 None => println(\"missing\"),\n\
             }\n\
         }");
    assert_eq!(output, "1\n");
}

#[test]
fn test_map_get_missing_returns_none() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             let v = m.get(\"z\");\n\
             match v {\n\
                 Some(x) => println(x),\n\
                 None => println(\"none\"),\n\
             }\n\
         }");
    assert_eq!(output, "none\n");
}

#[test]
fn test_map_contains_key() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"x\", 10_i64);\n\
             println(m.contains_key(\"x\"));\n\
             println(m.contains_key(\"y\"));\n\
         }");
    assert_eq!(output, "true\nfalse\n");
}

#[test]
fn test_map_remove() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"a\", 42_i64);\n\
             let old = m.remove(\"a\");\n\
             match old {\n\
                 Some(v) => println(v),\n\
                 None => println(\"none\"),\n\
             }\n\
             println(m.len());\n\
         }");
    assert_eq!(output, "42\n0\n");
}

#[test]
fn test_map_get_or() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"k\", 7_i64);\n\
             println(m.get_or(\"k\", 0_i64));\n\
             println(m.get_or(\"missing\", 99_i64));\n\
         }");
    assert_eq!(output, "7\n99\n");
}

#[test]
fn test_map_is_empty() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             println(m.is_empty());\n\
             m.insert(\"a\", 1_i64);\n\
             println(m.is_empty());\n\
         }");
    assert_eq!(output, "true\nfalse\n");
}

#[test]
fn test_map_keys_and_values() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"a\", 1_i64);\n\
             m.insert(\"b\", 2_i64);\n\
             let ks = m.keys();\n\
             let vs = m.values();\n\
             println(ks.len());\n\
             println(vs.len());\n\
         }");
    assert_eq!(output, "2\n2\n");
}

#[test]
fn test_map_entries_iteration() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"x\", 10_i64);\n\
             let es = m.entries();\n\
             println(es.len());\n\
         }");
    assert_eq!(output, "1\n");
}

#[test]
fn test_map_merge() {
    let output = run("fn main() {\n\
             let a: Map[String, i64] = Map.new();\n\
             a.insert(\"p\", 1_i64);\n\
             let b: Map[String, i64] = Map.new();\n\
             b.insert(\"q\", 2_i64);\n\
             let c = a.merge(b);\n\
             println(c.len());\n\
             println(c.contains_key(\"p\"));\n\
             println(c.contains_key(\"q\"));\n\
         }");
    assert_eq!(output, "2\ntrue\ntrue\n");
}

#[test]
fn test_map_prefix_literal_string_keys() {
    // Map["a": 1, "b": 2] prefix-literal form — parses + type-checks +
    // produces a Map with the entries.
    let output = run("fn main() {\n\
             let m = Map[\"a\": 1_i64, \"b\": 2_i64, \"c\": 3_i64];\n\
             println(m.len());\n\
             match m.get(\"b\") {\n\
                 Some(v) => println(v),\n\
                 None => println(0_i64),\n\
             }\n\
         }");
    assert_eq!(output, "3\n2\n");
}

#[test]
fn test_map_prefix_literal_int_keys() {
    let output = run("fn main() {\n\
             let m = Map[1_i64: 100_i64, 2_i64: 200_i64];\n\
             println(m.len());\n\
             match m.get(2_i64) {\n\
                 Some(v) => println(v),\n\
                 None => println(0_i64),\n\
             }\n\
         }");
    assert_eq!(output, "2\n200\n");
}

#[test]
fn test_map_clear() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"a\", 1_i64);\n\
             m.insert(\"b\", 2_i64);\n\
             println(m.len());\n\
             m.clear();\n\
             println(m.len());\n\
             println(m.is_empty());\n\
             m.insert(\"c\", 3_i64);\n\
             println(m.contains_key(\"a\"));\n\
             println(m.contains_key(\"c\"));\n\
         }");
    assert_eq!(output, "2\n0\ntrue\nfalse\ntrue\n");
}

#[test]
fn test_map_merge_overwrite() {
    let output = run("fn main() {\n\
             let a: Map[String, i64] = Map.new();\n\
             a.insert(\"k\", 1_i64);\n\
             let b: Map[String, i64] = Map.new();\n\
             b.insert(\"k\", 99_i64);\n\
             let c = a.merge(b);\n\
             println(c.len());\n\
             match c.get(\"k\") {\n\
                 Some(v) => println(v),\n\
                 None => println(\"none\"),\n\
             }\n\
         }");
    assert_eq!(output, "1\n99\n");
}

#[test]
fn test_map_insert_update_returns_old() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"k\", 5_i64);\n\
             let old = m.insert(\"k\", 10_i64);\n\
             match old {\n\
                 Some(v) => println(v),\n\
                 None => println(\"none\"),\n\
             }\n\
         }");
    assert_eq!(output, "5\n");
}

// ── Map.entry / Entry[K, V] (canonical: phase-8-stdlib-floor.md
//    "Map.entry(k) + Entry[K, V] enum") ────────────────────────────

#[test]
fn test_map_entry_or_insert_vacant_inserts_default() {
    // Vacant key — or_insert pushes (key, default) and returns the default.
    // Verify the map state by re-fetching.
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             let v = m.entry(\"a\").or_insert(7_i64);\n\
             println(v);\n\
             match m.get(\"a\") {\n\
                 Some(x) => println(x),\n\
                 None => println(\"missing\"),\n\
             }\n\
         }");
    assert_eq!(output, "7\n7\n");
}

#[test]
fn test_map_entry_or_insert_occupied_returns_existing() {
    // Occupied key — or_insert is a no-op write; returns the existing value.
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"a\", 42_i64);\n\
             let v = m.entry(\"a\").or_insert(0_i64);\n\
             println(v);\n\
             match m.get(\"a\") {\n\
                 Some(x) => println(x),\n\
                 None => println(\"missing\"),\n\
             }\n\
         }");
    assert_eq!(output, "42\n42\n");
}

#[test]
fn test_map_entry_or_insert_with_vacant_invokes_closure() {
    // Vacant — closure runs to produce the default. Counter pattern via
    // a side variable confirms the closure fired exactly once.
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             let v = m.entry(\"x\").or_insert_with(|| 99_i64);\n\
             println(v);\n\
         }");
    assert_eq!(output, "99\n");
}

#[test]
fn test_map_entry_or_insert_with_occupied_skips_closure() {
    // Occupied — closure does NOT fire. Returning a sentinel that would
    // overwrite if the closure ran lets the test detect a regression.
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"k\", 5_i64);\n\
             let v = m.entry(\"k\").or_insert_with(|| 999_i64);\n\
             println(v);\n\
         }");
    assert_eq!(output, "5\n");
}

#[test]
fn test_map_entry_and_modify_runs_when_occupied() {
    // and_modify's closure fires only on Occupied; it receives the slot
    // value as a mut ref and can mutate through (the interpreter aliases
    // the slot via SharedCell for the duration of the call).
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"k\", 5_i64);\n\
             m.entry(\"k\").and_modify(|v| { v += 1; });\n\
             match m.get(\"k\") {\n\
                 Some(x) => println(x),\n\
                 None => println(\"missing\"),\n\
             }\n\
         }");
    assert_eq!(output, "6\n");
}

#[test]
fn test_map_entry_and_modify_skips_when_vacant() {
    // Vacant — closure does not fire; map state is unchanged.
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.entry(\"k\").and_modify(|v| { v += 1; });\n\
             println(m.is_empty());\n\
         }");
    assert_eq!(output, "true\n");
}

#[test]
fn test_map_entry_and_modify_chain_with_or_insert() {
    // The canonical chain: and_modify on Occupied increments; on Vacant
    // the trailing or_insert provides the seed value.
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.entry(\"a\").and_modify(|v| { v += 1; }).or_insert(1_i64);\n\
             m.entry(\"a\").and_modify(|v| { v += 1; }).or_insert(1_i64);\n\
             m.entry(\"a\").and_modify(|v| { v += 1; }).or_insert(1_i64);\n\
             match m.get(\"a\") {\n\
                 Some(x) => println(x),\n\
                 None => println(\"missing\"),\n\
             }\n\
         }");
    // First call: vacant → or_insert(1) sets a=1.
    // Second:    occupied → and_modify(+1) → a=2; or_insert no-op.
    // Third:     occupied → and_modify(+1) → a=3; or_insert no-op.
    assert_eq!(output, "3\n");
}

// ── Clone trait surface (canonical: phase-8-stdlib-floor.md
//    "Clone trait surface for collections") ────────────────────────

#[test]
fn test_vec_clone_preserves_contents() {
    // Cloning a Vec produces an equal Vec; both contain the same elements
    // in the same order.
    let output = run("fn main() {\n\
             let v: Vec[i64] = [1_i64, 2_i64, 3_i64];\n\
             let w: Vec[i64] = v.clone();\n\
             println(w[0]);\n\
             println(w[1]);\n\
             println(w[2]);\n\
         }");
    assert_eq!(output, "1\n2\n3\n");
}

#[test]
fn test_vec_clone_independent_after_push() {
    // Mutating the source after cloning does not affect the clone — they
    // own independent buffers.
    let output = run("fn main() {\n\
             let v: Vec[i64] = [1_i64, 2_i64];\n\
             let w: Vec[i64] = v.clone();\n\
             v.push(99_i64);\n\
             println(v.len());\n\
             println(w.len());\n\
         }");
    assert_eq!(output, "3\n2\n");
}

#[test]
fn test_string_clone_preserves_value() {
    let output = run("fn main() {\n\
             let s = \"hello\";\n\
             let t = s.clone();\n\
             println(t);\n\
         }");
    assert_eq!(output, "hello\n");
}

#[test]
fn test_map_clone_preserves_entries() {
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"k\", 7_i64);\n\
             let n: Map[String, i64] = m.clone();\n\
             match n.get(\"k\") {\n\
                 Some(v) => println(v),\n\
                 None => println(\"missing\"),\n\
             }\n\
         }");
    assert_eq!(output, "7\n");
}

#[test]
fn test_map_clone_independent_after_source_insert() {
    // Inserting into the source after cloning leaves the clone unchanged.
    let output = run("fn main() {\n\
             let m: Map[String, i64] = Map.new();\n\
             m.insert(\"k\", 1_i64);\n\
             let n: Map[String, i64] = m.clone();\n\
             m.insert(\"k\", 99_i64);\n\
             match n.get(\"k\") {\n\
                 Some(v) => println(v),\n\
                 None => println(\"missing\"),\n\
             }\n\
         }");
    assert_eq!(output, "1\n");
}

#[test]
fn test_set_clone_preserves_membership() {
    let output = run("fn main() {\n\
             let s: Set[i64] = Set.new();\n\
             s.insert(5_i64);\n\
             let t: Set[i64] = s.clone();\n\
             println(t.contains(5_i64));\n\
             println(t.contains(99_i64));\n\
         }");
    assert_eq!(output, "true\nfalse\n");
}

#[test]
fn test_set_clone_independent_after_source_insert() {
    let output = run("fn main() {\n\
             let s: Set[i64] = Set.new();\n\
             s.insert(1_i64);\n\
             let t: Set[i64] = s.clone();\n\
             s.insert(2_i64);\n\
             println(t.len());\n\
             println(s.len());\n\
         }");
    assert_eq!(output, "1\n2\n");
}

// ── Regex ─────────────────────────────────────────────────────────

#[test]
fn test_regex_compile_ok_result() {
    let output = run(r#"fn main() {
         match Regex.compile("[0-9]+") {
             Ok(r) => println("ok"),
             Err(e) => println("err"),
         }
     }"#);
    assert_eq!(output, "ok\n");
}

#[test]
fn test_regex_compile_invalid_err() {
    let output = run(r#"fn main() {
         match Regex.compile("[invalid") {
             Ok(r) => println("ok"),
             Err(e) => println("err"),
         }
     }"#);
    assert_eq!(output, "err\n");
}

#[test]
fn test_regex_is_match_true() {
    let output = run(r#"fn main() {
         let r = Regex.compile("[0-9]+").unwrap();
         println(r.is_match("abc123"));
     }"#);
    assert_eq!(output, "true\n");
}

#[test]
fn test_regex_is_match_false() {
    let output = run(r#"fn main() {
         let r = Regex.compile("[0-9]+").unwrap();
         println(r.is_match("abc"));
     }"#);
    assert_eq!(output, "false\n");
}

#[test]
fn test_regex_find_some() {
    let output = run(r#"fn main() {
         let r = Regex.compile("[0-9]+").unwrap();
         match r.find("abc123def") {
             Some(m) => println(m.text),
             None => println("none"),
         }
     }"#);
    assert_eq!(output, "123\n");
}

#[test]
fn test_regex_find_none() {
    let output = run(r#"fn main() {
         let r = Regex.compile("[0-9]+").unwrap();
         match r.find("abcdef") {
             Some(m) => println(m.text),
             None => println("none"),
         }
     }"#);
    assert_eq!(output, "none\n");
}

#[test]
fn test_regex_find_all() {
    let output = run(r#"fn main() {
         let r = Regex.compile("[0-9]+").unwrap();
         let ms = r.find_all("abc 123 def 456");
         println(ms.len());
     }"#);
    assert_eq!(output, "2\n");
}

#[test]
fn test_regex_replace_all() {
    let output = run(r#"fn main() {
         let r = Regex.compile("[0-9]+").unwrap();
         println(r.replace_all("abc 123 def 456", "NUM"));
     }"#);
    assert_eq!(output, "abc NUM def NUM\n");
}

// ── Stats namespace ───────────────────────────────────────────────

#[test]
fn test_stats_sum() {
    let output = run("fn main() { let xs = [1.0_f64, 2.0_f64, 3.0_f64]; println(Stats.sum(xs)); }");
    assert_eq!(output, "6\n");
}

#[test]
fn test_stats_prod() {
    let output =
        run("fn main() { let xs = [2.0_f64, 3.0_f64, 4.0_f64]; println(Stats.prod(xs)); }");
    assert_eq!(output, "24\n");
}

#[test]
fn test_stats_mean() {
    let output =
        run("fn main() { let xs = [1.0_f64, 2.0_f64, 3.0_f64]; println(Stats.mean(xs)); }");
    assert_eq!(output, "2\n");
}

#[test]
fn test_stats_variance() {
    let output = run("fn main() { let xs = [2.0_f64, 4.0_f64, 4.0_f64, 4.0_f64, 5.0_f64, 5.0_f64, 7.0_f64, 9.0_f64]; println(Stats.variance(xs)); }");
    assert_eq!(output, "4\n");
}

#[test]
fn test_stats_stddev() {
    let output = run("fn main() { let xs = [2.0_f64, 4.0_f64, 4.0_f64, 4.0_f64, 5.0_f64, 5.0_f64, 7.0_f64, 9.0_f64]; println(Stats.stddev(xs)); }");
    assert_eq!(output, "2\n");
}

#[test]
fn test_stats_median_odd() {
    let output =
        run("fn main() { let xs = [3.0_f64, 1.0_f64, 2.0_f64]; println(Stats.median(xs)); }");
    assert_eq!(output, "2\n");
}

#[test]
fn test_stats_median_even() {
    let output = run(
        "fn main() { let xs = [1.0_f64, 2.0_f64, 3.0_f64, 4.0_f64]; println(Stats.median(xs)); }",
    );
    assert_eq!(output, "2.5\n");
}

#[test]
fn test_stats_min_nonempty() {
    let output = run("fn main() {\n\
         let xs = [3.0_f64, 1.0_f64, 2.0_f64];\n\
         match Stats.min(xs) {\n\
             Some(v) => println(v),\n\
             None => println(\"none\"),\n\
         }\n\
     }");
    assert_eq!(output, "1\n");
}

#[test]
fn test_stats_max_nonempty() {
    let output = run("fn main() {\n\
         let xs = [3.0_f64, 1.0_f64, 2.0_f64];\n\
         match Stats.max(xs) {\n\
             Some(v) => println(v),\n\
             None => println(\"none\"),\n\
         }\n\
     }");
    assert_eq!(output, "3\n");
}

#[test]
fn test_stats_min_empty() {
    let output = run("fn main() {\n\
         let xs: Vec[f64] = Vec[0.0_f64];\n\
         let ys = xs[1..];\n\
         match Stats.min(ys) {\n\
             Some(v) => println(v),\n\
             None => println(\"none\"),\n\
         }\n\
     }");
    // empty slice → None
    assert_eq!(output, "none\n");
}

// ── Encoding namespace (Base64 / Hex / Url) ───────────────────────

#[test]
fn test_base64_encode_basic() {
    // RFC 4648 vector: "foobar" → "Zm9vYmFy"
    let output = run("fn main() {\n\
             let bs = [102u8, 111u8, 111u8, 98u8, 97u8, 114u8];\n\
             println(Base64.encode(bs));\n\
         }");
    assert_eq!(output, "Zm9vYmFy\n");
}

#[test]
fn test_base64_encode_padding_one_byte() {
    // RFC 4648 vector: "f" → "Zg=="
    let output = run("fn main() {\n\
             let bs = [102u8];\n\
             println(Base64.encode(bs));\n\
         }");
    assert_eq!(output, "Zg==\n");
}

#[test]
fn test_base64_encode_padding_two_bytes() {
    // RFC 4648 vector: "fo" → "Zm8="
    let output = run("fn main() {\n\
             let bs = [102u8, 111u8];\n\
             println(Base64.encode(bs));\n\
         }");
    assert_eq!(output, "Zm8=\n");
}

#[test]
fn test_base64_encode_url_safe_no_padding() {
    // Bytes 0xfb 0xff 0xbf encode to "+/+/" in standard alphabet,
    // "-_-_" in URL-safe alphabet. URL-safe omits padding.
    let output = run("fn main() {\n\
             let bs = [251u8, 255u8, 191u8];\n\
             println(Base64.encode_url_safe(bs));\n\
         }");
    assert_eq!(output, "-_-_\n");
}

#[test]
fn test_base64_decode_roundtrip() {
    let output = run("fn main() {\n\
             match Base64.decode(\"Zm9vYmFy\") {\n\
                 Ok(bs) => println(bs.len()),\n\
                 Err(_) => println(\"err\"),\n\
             }\n\
         }");
    assert_eq!(output, "6\n");
}

#[test]
fn test_base64_decode_invalid_char() {
    let output = run("fn main() {\n\
             match Base64.decode(\"!!!!\") {\n\
                 Ok(_) => println(\"ok\"),\n\
                 Err(_) => println(\"err\"),\n\
             }\n\
         }");
    assert_eq!(output, "err\n");
}

#[test]
fn test_hex_encode_lowercase() {
    let output = run("fn main() {\n\
             let bs = [255u8, 0u8, 16u8];\n\
             println(Hex.encode(bs));\n\
         }");
    assert_eq!(output, "ff0010\n");
}

#[test]
fn test_hex_encode_upper() {
    let output = run("fn main() {\n\
             let bs = [255u8, 0u8, 16u8];\n\
             println(Hex.encode_upper(bs));\n\
         }");
    assert_eq!(output, "FF0010\n");
}

#[test]
fn test_hex_decode_mixed_case() {
    let output = run("fn main() {\n\
             match Hex.decode(\"FfaA\") {\n\
                 Ok(bs) => println(bs.len()),\n\
                 Err(_) => println(\"err\"),\n\
             }\n\
         }");
    assert_eq!(output, "2\n");
}

#[test]
fn test_hex_decode_odd_length() {
    let output = run("fn main() {\n\
             match Hex.decode(\"abc\") {\n\
                 Ok(_) => println(\"ok\"),\n\
                 Err(_) => println(\"err\"),\n\
             }\n\
         }");
    assert_eq!(output, "err\n");
}

#[test]
fn test_url_encode_reserved() {
    let output = run("fn main() {\n\
             println(Url.encode(\"hello world\"));\n\
         }");
    assert_eq!(output, "hello%20world\n");
}

#[test]
fn test_url_encode_preserves_unreserved() {
    // RFC 3986 unreserved set: A-Za-z0-9-._~ — must round-trip unchanged.
    let output = run("fn main() {\n\
             println(Url.encode(\"abcXYZ123-._~\"));\n\
         }");
    assert_eq!(output, "abcXYZ123-._~\n");
}

#[test]
fn test_url_decode_roundtrip() {
    let output = run("fn main() {\n\
             match Url.decode(\"a%20b%2Fc\") {\n\
                 Ok(s) => println(s),\n\
                 Err(_) => println(\"err\"),\n\
             }\n\
         }");
    assert_eq!(output, "a b/c\n");
}

#[test]
fn test_url_decode_invalid_percent() {
    let output = run("fn main() {\n\
             match Url.decode(\"a%2\") {\n\
                 Ok(_) => println(\"ok\"),\n\
                 Err(_) => println(\"err\"),\n\
             }\n\
         }");
    assert_eq!(output, "err\n");
}

// ── Set[T] ────────────────────────────────────────────────────────

#[test]
fn test_set_new_and_len() {
    let output = run("fn main() {\n\
         let s: Set[i64] = Set.new();\n\
         println(s.len());\n\
     }");
    assert_eq!(output, "0\n");
}

#[test]
fn test_set_insert_and_contains() {
    let output = run("fn main() {\n\
         let s: Set[i64] = Set.new();\n\
         s.insert(1_i64);\n\
         s.insert(2_i64);\n\
         println(s.contains(1_i64));\n\
         println(s.contains(3_i64));\n\
     }");
    assert_eq!(output, "true\nfalse\n");
}

#[test]
fn test_set_insert_dedup() {
    let output = run("fn main() {\n\
         let s: Set[i64] = Set.new();\n\
         s.insert(5_i64);\n\
         s.insert(5_i64);\n\
         println(s.len());\n\
     }");
    assert_eq!(output, "1\n");
}

#[test]
fn test_set_remove() {
    let output = run("fn main() {\n\
         let s: Set[i64] = Set.new();\n\
         s.insert(10_i64);\n\
         let was_present = s.remove(10_i64);\n\
         println(was_present);\n\
         println(s.len());\n\
     }");
    assert_eq!(output, "true\n0\n");
}

#[test]
fn test_set_is_empty() {
    let output = run("fn main() {\n\
         let s: Set[i64] = Set.new();\n\
         println(s.is_empty());\n\
         s.insert(1_i64);\n\
         println(s.is_empty());\n\
     }");
    assert_eq!(output, "true\nfalse\n");
}

#[test]
fn test_set_union() {
    let output = run("fn main() {\n\
         let a: Set[i64] = Set.new();\n\
         a.insert(1_i64);\n\
         a.insert(2_i64);\n\
         let b: Set[i64] = Set.new();\n\
         b.insert(2_i64);\n\
         b.insert(3_i64);\n\
         let c = a.union(b);\n\
         println(c.len());\n\
         println(c.contains(1_i64));\n\
         println(c.contains(3_i64));\n\
     }");
    assert_eq!(output, "3\ntrue\ntrue\n");
}

#[test]
fn test_set_intersection() {
    let output = run("fn main() {\n\
         let a: Set[i64] = Set.new();\n\
         a.insert(1_i64);\n\
         a.insert(2_i64);\n\
         let b: Set[i64] = Set.new();\n\
         b.insert(2_i64);\n\
         b.insert(3_i64);\n\
         let c = a.intersection(b);\n\
         println(c.len());\n\
         println(c.contains(2_i64));\n\
     }");
    assert_eq!(output, "1\ntrue\n");
}

#[test]
fn test_set_difference() {
    let output = run("fn main() {\n\
         let a: Set[i64] = Set.new();\n\
         a.insert(1_i64);\n\
         a.insert(2_i64);\n\
         let b: Set[i64] = Set.new();\n\
         b.insert(2_i64);\n\
         let c = a.difference(b);\n\
         println(c.len());\n\
         println(c.contains(1_i64));\n\
     }");
    assert_eq!(output, "1\ntrue\n");
}

#[test]
fn test_set_for_loop() {
    let output = run("fn main() {\n\
         let s: Set[i64] = Set.new();\n\
         s.insert(42_i64);\n\
         for x in s {\n\
             println(x);\n\
         }\n\
     }");
    assert_eq!(output, "42\n");
}

// ── Display / to_string ───────────────────────────────────────────

#[test]
fn test_to_string_i64() {
    let output = run("fn main() { let n: i64 = 42; println(n.to_string()); }");
    assert_eq!(output, "42\n");
}

#[test]
fn test_to_string_bool() {
    let output = run("fn main() { println(true.to_string()); }");
    assert_eq!(output, "true\n");
}

#[test]
fn test_to_string_str() {
    let output = run(r#"fn main() { let s = "hello"; println(s.to_string()); }"#);
    assert_eq!(output, "hello\n");
}

#[test]
fn test_to_string_vec() {
    let output = run("fn main() {\n\
         let v = Vec[1_i64, 2_i64];\n\
         println(v.to_string());\n\
     }");
    assert_eq!(output, "[1, 2]\n");
}

#[test]
fn test_to_string_option_some() {
    let output = run("fn main() {\n\
         let x: Option[i64] = Some(99_i64);\n\
         println(x.to_string());\n\
     }");
    assert_eq!(output, "Some(99)\n");
}

#[test]
fn test_to_string_option_none() {
    let output = run("fn main() {\n\
         let x: Option[i64] = None;\n\
         println(x.to_string());\n\
     }");
    assert_eq!(output, "None\n");
}

#[test]
fn test_fstring_interpolates_i64() {
    let output = run(r#"fn main() { let n: i64 = 7; println(f"n is {n}"); }"#);
    assert_eq!(output, "n is 7\n");
}

#[test]
fn test_fstring_interpolates_vec() {
    let output = run("fn main() {\n\
         let v = Vec[10_i64];\n\
         println(f\"v={v}\");\n\
     }");
    assert_eq!(output, "v=[10]\n");
}

#[test]
fn test_println_i64_direct() {
    let output = run("fn main() { println(123_i64); }");
    assert_eq!(output, "123\n");
}

#[test]
fn test_println_float_direct() {
    let output = run("fn main() { println(3.14_f64); }");
    assert_eq!(output, "3.14\n");
}

#[test]
fn test_string_sorted_basic() {
    let output = run(r#"fn main() { let s = "cba"; println(s.sorted()); }"#);
    assert_eq!(output, "abc\n");
}

#[test]
fn test_string_sorted_already_sorted() {
    let output = run(r#"fn main() { let s = "abc"; println(s.sorted()); }"#);
    assert_eq!(output, "abc\n");
}

#[test]
fn test_string_sorted_empty() {
    let output = run(r#"fn main() { let s = ""; println(s.sorted()); }"#);
    assert_eq!(output, "\n");
}

#[test]
fn test_string_sorted_by_fallback() {
    let output = run(r#"fn main() { let s = "dcba"; println(s.sorted_by(|a, b| a < b)); }"#);
    assert_eq!(output, "abcd\n");
}

// ── Prefix dereference operator ───────────────────────────────────────────────

#[test]
fn test_deref_read_ref_param() {
    let output = run("fn read_val(r: ref i64) -> i64 { *r }\nfn main() { let x = 42_i64; println(read_val(x)); }");
    assert_eq!(output, "42\n");
}

#[test]
fn test_deref_read_mut_ref_param() {
    let output = run("fn read_val(r: mut ref i64) -> i64 { *r }\nfn main() { let x = 7_i64; println(read_val(x)); }");
    assert_eq!(output, "7\n");
}

#[test]
fn test_deref_write_through_mut_ref() {
    let output = run("fn set_val(r: mut ref i64) { *r = 99; }\nfn main() { let mut x = 1_i64; set_val(mut x); println(x); }");
    assert_eq!(output, "99\n");
}

#[test]
fn test_deref_double() {
    let output = run("fn double_in_place(r: mut ref i64) { *r = *r * 2; }\nfn main() { let mut n = 5_i64; double_in_place(mut n); println(n); }");
    assert_eq!(output, "10\n");
}

// ── std.http ──────────────────────────────────────────────────────────────────

#[test]
fn test_http_client_new() {
    // Client.new() should return a Client struct — no network needed.
    let output = run(r#"
fn main() {
    let c = Client.new();
    println("ok");
}
"#);
    assert_eq!(output, "ok\n");
}

#[test]
#[ignore = "requires network access"]
fn test_http_client_get_ok() {
    let output = run(r#"
fn main() {
    let c = Client.new();
    match c.get("http://httpbin.org/status/200") {
        Ok(resp) => println(resp.status()),
        Err(e) => println(e.message()),
    }
}
"#);
    assert_eq!(output, "200\n");
}

#[test]
fn test_http_client_get_invalid_url() {
    // A clearly invalid URL should produce Err, not panic.
    let output = run(r#"
fn main() {
    let c = Client.new();
    match c.get("not-a-url") {
        Ok(_) => println("ok"),
        Err(_) => println("err"),
    }
}
"#);
    assert_eq!(output, "err\n");
}

#[test]
#[ignore = "requires network access"]
fn test_http_client_post_ok() {
    let output = run(r#"
fn main() {
    let c = Client.new();
    match c.post("http://httpbin.org/post", "hello") {
        Ok(resp) => println(resp.status()),
        Err(e) => println(e.message()),
    }
}
"#);
    assert_eq!(output, "200\n");
}

#[test]
fn test_http_response_methods() {
    // Invalid URL → Err — verify the error path and HttpError.message() work.
    let output = run(r#"
fn make_client() -> Client { Client.new() }
fn main() {
    let c = make_client();
    match c.get("not-a-url") {
        Ok(resp) => println(resp.status()),
        Err(e) => println("error"),
    }
}
"#);
    assert_eq!(output, "error\n");
}

// ── Trait associated function dispatch (List 1, item 5) ─────────

#[test]
fn test_bare_assoc_fn_with_concrete_expected_type() {
    // `let w: Foo = default()` resolves through `impl Default for Foo`.
    let output = run(r#"
trait Default {
    fn default() -> Self;
}

struct Foo { value: i64 }

impl Default for Foo {
    fn default() -> Foo { Foo { value: 42 } }
}

fn main() {
    let f: Foo = default();
    println(f.value);
}
"#);
    assert_eq!(output, "42\n");
}

#[test]
fn test_typeparam_assoc_fn_dispatch() {
    // `T.default()` inside a generic function dispatches to the impl
    // matching the runtime binding of `T` (driven by the caller's
    // expected type at the outer call site).
    let output = run(r#"
trait Default {
    fn default() -> Self;
}

struct Foo { value: i64 }

impl Default for Foo {
    fn default() -> Foo { Foo { value: 42 } }
}

fn make[T: Default]() -> T {
    T.default()
}

fn main() {
    let f: Foo = make();
    println(f.value);
}
"#);
    assert_eq!(output, "42\n");
}

#[test]
fn test_bare_assoc_fn_with_typeparam_expected() {
    // Bare `default()` inside a generic function with typeparam expected
    // type uses lowering + runtime substitution stack to dispatch.
    let output = run(r#"
trait Default {
    fn default() -> Self;
}

struct Foo { value: i64 }

impl Default for Foo {
    fn default() -> Foo { Foo { value: 99 } }
}

fn make[T: Default]() -> T {
    default()
}

fn main() {
    let f: Foo = make();
    println(f.value);
}
"#);
    assert_eq!(output, "99\n");
}

#[test]
fn test_assoc_fn_with_arg_through_typeparam() {
    // Trait method with a non-Self parameter dispatches through typeparam.
    let output = run(r#"
trait FromI64 {
    fn from_i64(n: i64) -> Self;
}

struct Wrap { v: i64 }

impl FromI64 for Wrap {
    fn from_i64(n: i64) -> Wrap { Wrap { v: n } }
}

fn make[T: FromI64](n: i64) -> T {
    T.from_i64(n)
}

fn main() {
    let w: Wrap = make(7);
    println(w.v);
}
"#);
    assert_eq!(output, "7\n");
}

#[test]
fn test_concrete_type_prefix_assoc_fn_dispatch() {
    // `Foo.default()` directly dispatches to the impl method.
    let output = run(r#"
trait Default {
    fn default() -> Self;
}

struct Foo { value: i64 }

impl Default for Foo {
    fn default() -> Foo { Foo { value: 5 } }
}

fn main() {
    let f = Foo.default();
    println(f.value);
}
"#);
    assert_eq!(output, "5\n");
}

// ── Item 7: broad integration coverage ──────────────────────────

#[test]
fn test_integration_from_str_user_trait() {
    // FromStr-style factory trait taking a String argument. Verifies
    // dispatch when the trait method has a non-Self parameter and the
    // dispatch goes through the bare-call expected-type lowering.
    let output = run(r#"
trait FromStr {
    fn from_str(s: String) -> Self;
}

struct Tag { label: String }

impl FromStr for Tag {
    fn from_str(s: String) -> Tag { Tag { label: s } }
}

fn main() {
    let t: Tag = from_str("hello");
    println(t.label);
}
"#);
    assert_eq!(output, "hello\n");
}

#[test]
fn test_integration_two_traits_one_type() {
    // A single concrete type implementing two factory traits — verify
    // both dispatch correctly in the same program.
    let output = run(r#"
trait Default {
    fn default() -> Self;
}
trait FromI64 {
    fn from_i64(n: i64) -> Self;
}

struct Counter { value: i64 }

impl Default for Counter {
    fn default() -> Counter { Counter { value: 0 } }
}
impl FromI64 for Counter {
    fn from_i64(n: i64) -> Counter { Counter { value: n } }
}

fn main() {
    let a: Counter = default();
    let b: Counter = from_i64(7);
    println(a.value);
    println(b.value);
}
"#);
    assert_eq!(output, "0\n7\n");
}

#[test]
fn test_integration_generic_helper_chain() {
    // A generic helper calls another generic helper that dispatches via
    // typeparam — verifies the runtime substitution stack chains transitively
    // through multiple call frames.
    let output = run(r#"
trait Default {
    fn default() -> Self;
}

struct Box { value: i64 }

impl Default for Box {
    fn default() -> Box { Box { value: 100 } }
}

fn make[T: Default]() -> T {
    T.default()
}

fn outer[T: Default]() -> T {
    make()
}

fn main() {
    let b: Box = outer();
    println(b.value);
}
"#);
    assert_eq!(output, "100\n");
}

#[test]
fn test_integration_where_clause_bound_e2e() {
    // Full end-to-end with a where-clause bound (instead of inline) —
    // dispatch should land at the same impl method.
    let output = run(r#"
trait Default {
    fn default() -> Self;
}

struct Slot { v: i64 }

impl Default for Slot {
    fn default() -> Slot { Slot { v: 13 } }
}

fn make[T]() -> T where T: Default {
    default()
}

fn main() {
    let s: Slot = make();
    println(s.v);
}
"#);
    assert_eq!(output, "13\n");
}

#[test]
fn test_integration_assoc_fn_in_arg_position() {
    // Bare assoc fn call passed as a function argument. The expected type
    // is the parameter type at the call site.
    let output = run(r#"
trait Default {
    fn default() -> Self;
}

struct Foo { v: i64 }

impl Default for Foo {
    fn default() -> Foo { Foo { v: 21 } }
}

fn take(f: Foo) -> i64 { f.v * 2 }

fn main() {
    println(take(default()));
}
"#);
    assert_eq!(output, "42\n");
}

#[test]
fn test_for_in_vec_string_calls_len_interp() {
    // Interpreter parity for List 2 / item 3 (codegen for-loop element-type
    // propagation). The interpreter already binds runtime-tagged Values, so
    // dispatch on the bound name routes through Value::String automatically.
    let output = run(r#"
fn main() {
    let v = ["alice", "bobby"];
    for s in v {
        println(s.len());
    }
}
"#);
    assert_eq!(output, "5\n5\n");
}

// ── Iterator: `iter()` / `into_iter()` / `next()` (wip-list2 subtask 1) ──

#[test]
fn test_iter_next_drains_vec_in_order() {
    // Calling next() repeatedly walks the source elements then yields None.
    // The cursor advance writes back through the binding, so successive
    // calls observe the new state.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30];
    let mut it = v.iter();
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    assert_eq!(output, "10\n20\n30\ndone\n");
}

#[test]
fn test_into_iter_matches_iter_at_runtime() {
    // The interpreter is type-erased; iter() and into_iter() produce the
    // same Value::Iterator. Verifying observable equivalence pins the
    // contract before laziness adaptors land.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    let mut a = v.iter();
    let mut b = v.into_iter();
    println(a.next().unwrap());
    println(b.next().unwrap());
    println(a.next().unwrap());
    println(b.next().unwrap());
}
"#,
    );
    assert_eq!(output, "1\n1\n2\n2\n");
}

#[test]
fn test_iter_on_set_yields_each_element_once() {
    // Set iterates in insertion order at the interpreter (storage backed
    // by Vec<Value>); next() yields each element exactly once before None.
    let output = run_no_errors(
        r#"
fn main() {
    let mut s: Set[i64] = Set.new();
    s.insert(7);
    s.insert(3);
    s.insert(11);
    let mut it = s.iter();
    let mut sum = 0;
    let mut count = 0;
    let mut more = true;
    while more {
        match it.next() {
            Some(n) => { sum = sum + n; count = count + 1; },
            None    => { more = false; },
        }
    }
    println(sum);
    println(count);
}
"#,
    );
    assert_eq!(output, "21\n3\n");
}

#[test]
fn test_iter_on_sorted_set_yields_ascending() {
    // SortedSet iterates ascending — verify next() honors that order.
    let output = run_no_errors(
        r#"
fn main() {
    let mut s: SortedSet[i64] = SortedSet.new();
    s.insert(11);
    s.insert(3);
    s.insert(7);
    let mut it = s.iter();
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
}
"#,
    );
    assert_eq!(output, "3\n7\n11\n");
}

#[test]
fn test_iter_on_map_yields_kv_tuples() {
    // Map.iter() yields (K, V) tuples in insertion order at the interpreter.
    let output = run_no_errors(
        r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("a", 1);
    m.insert("b", 2);
    let mut it = m.iter();
    let (k1, v1) = it.next().unwrap();
    let (k2, v2) = it.next().unwrap();
    println(k1);
    println(v1);
    println(k2);
    println(v2);
}
"#,
    );
    assert_eq!(output, "a\n1\nb\n2\n");
}

#[test]
fn test_iter_on_empty_vec_yields_none_immediately() {
    // First next() on an empty source returns None; no Some preceded it.
    let output = run_no_errors(
        r#"
fn main() {
    let v: Vec[i64] = Vec[];
    let mut it = v.iter();
    match it.next() {
        Some(_) => println("had value"),
        None => println("empty"),
    }
}
"#,
    );
    assert_eq!(output, "empty\n");
}

// ── for-loop on iterator values (wip-list2 subtask 2) ────────────

#[test]
fn test_for_loop_on_vec_iter_walks_elements() {
    // Direct iteration over an iterator value — `for x in v.iter() { ... }`
    // walks the source elements in order.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30];
    for x in v.iter() {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "10\n20\n30\n");
}

#[test]
fn test_for_loop_on_iter_resumes_from_cursor() {
    // The iterator is bound, advanced manually with next(), then dropped
    // into a for-loop — the loop must resume from the cursor's current
    // position rather than restarting from the beginning.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let mut it = v.iter();
    let _ = it.next();
    let _ = it.next();
    for x in it {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "3\n4\n");
}

#[test]
fn test_for_loop_on_map_iter_destructures_kv() {
    // Map.iter() yields (K, V) tuples; for-loop binds via tuple pattern.
    let output = run_no_errors(
        r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("x", 1);
    m.insert("y", 2);
    for (k, v) in m.iter() {
        println(k);
        println(v);
    }
}
"#,
    );
    assert_eq!(output, "x\n1\ny\n2\n");
}

#[test]
fn test_for_loop_break_inside_iterator_loop() {
    // break exits the for-loop early; downstream code still executes.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    for x in v.iter() {
        if x > 2 {
            break;
        }
        println(x);
    }
    println(99);
}
"#,
    );
    assert_eq!(output, "1\n2\n99\n");
}

#[test]
fn test_for_loop_continue_inside_iterator_loop() {
    // continue skips to the next iteration without exiting the loop.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    for x in v.iter() {
        if x == 2 {
            continue;
        }
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n3\n4\n");
}

// ── map / filter (wip-list2 subtask 3) ───────────────────────────

#[test]
fn test_iter_map_transforms_each_element() {
    // `.map(|x| x * 10)` rewrites every element through the closure.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    for n in v.iter().map(|x| x * 10) {
        println(n);
    }
}
"#,
    );
    assert_eq!(output, "10\n20\n30\n");
}

#[test]
fn test_iter_filter_keeps_matching_elements() {
    // `.filter(pred)` yields only elements where pred returns true.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    for n in v.iter().filter(|x| x > 2) {
        println(n);
    }
}
"#,
    );
    assert_eq!(output, "3\n4\n5\n");
}

#[test]
fn test_iter_map_then_filter_chain() {
    // Adaptors chain: map first, then filter on the mapped values.
    // 1,2,3,4 → *3 → 3,6,9,12 → > 5 → 6,9,12.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    for n in v.iter().map(|x| x * 3).filter(|y| y > 5) {
        println(n);
    }
}
"#,
    );
    assert_eq!(output, "6\n9\n12\n");
}

#[test]
fn test_iter_filter_then_map_chain() {
    // Order matters — filter first, then map on filtered elements.
    // 1,2,3,4,5 → > 2 → 3,4,5 → +100 → 103,104,105.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    for n in v.iter().filter(|x| x > 2).map(|x| x + 100) {
        println(n);
    }
}
"#,
    );
    assert_eq!(output, "103\n104\n105\n");
}

#[test]
fn test_iter_map_via_next_step_by_step() {
    // map is lazy — each next() pull invokes the closure exactly once.
    // No more, no fewer (verified by counting println side effects).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    let mut it = v.iter().map(|x| x + 100);
    println(it.next().unwrap());
    println(it.next().unwrap());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    assert_eq!(output, "101\n102\ndone\n");
}

#[test]
fn test_iter_filter_drops_all_when_predicate_always_false() {
    // When the predicate rejects every element, next() returns None on
    // the first pull (after walking the entire source internally).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let mut it = v.iter().filter(|x| x > 100);
    match it.next() {
        Some(_) => println("had value"),
        None => println("empty"),
    }
}
"#,
    );
    assert_eq!(output, "empty\n");
}

#[test]
fn test_iter_map_on_map_kv_tuples() {
    // Map.iter() yields (K, V) tuples; .map(|pair| ...) gets the tuple.
    let output = run_no_errors(
        r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("a", 1);
    m.insert("b", 2);
    for s in m.iter().map(|(k, v)| f"{k}={v}") {
        println(s);
    }
}
"#,
    );
    assert_eq!(output, "a=1\nb=2\n");
}

#[test]
fn test_iter_count_returns_element_count() {
    // count() drains the iterator and returns the element count as i64.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30, 40];
    let n: i64 = v.iter().count();
    println(n);
}
"#,
    );
    assert_eq!(output, "4\n");
}

#[test]
fn test_iter_count_empty_returns_zero() {
    let output = run_no_errors(
        r#"
fn main() {
    let v: Vec[i64] = Vec[];
    let n: i64 = v.iter().count();
    println(n);
}
"#,
    );
    assert_eq!(output, "0\n");
}

#[test]
fn test_iter_count_after_filter_counts_kept_elements() {
    // count() composes with filter — only elements that pass the
    // predicate contribute to the count.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    let n: i64 = v.iter().filter(|x| x > 2).count();
    println(n);
}
"#,
    );
    assert_eq!(output, "3\n");
}

#[test]
fn test_iter_collect_yields_vec_in_order() {
    // collect() v1 returns a Vec[T] preserving iterator order.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let xs: Vec[i64] = v.iter().collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n2\n3\n");
}

#[test]
fn test_iter_collect_after_map_collects_mapped_values() {
    // map then collect — closure runs once per element during collect's drain.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let xs: Vec[i64] = v.iter().map(|x| x * 10).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "10\n20\n30\n");
}

#[test]
fn test_iter_collect_after_filter_drops_rejected_elements() {
    // filter then collect — only elements that pass the predicate land
    // in the resulting Vec.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    let xs: Vec[i64] = v.iter().filter(|x| x > 2).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "3\n4\n5\n");
}

#[test]
fn test_iter_fold_sums_elements() {
    // Canonical fold use — sum a Vec[i64] starting from 0.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    let s: i64 = v.iter().fold(0, |acc, x| acc + x);
    println(s);
}
"#,
    );
    assert_eq!(output, "15\n");
}

#[test]
fn test_iter_fold_empty_returns_init_unchanged() {
    let output = run_no_errors(
        r#"
fn main() {
    let v: Vec[i64] = Vec[];
    let s: i64 = v.iter().fold(42, |acc, x| acc + x);
    println(s);
}
"#,
    );
    assert_eq!(output, "42\n");
}

#[test]
fn test_iter_fold_threads_string_accumulator() {
    // Accumulator type can differ from element type.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let s: String = v.iter().fold("", |acc, x| f"{acc}{x},");
    println(s);
}
"#,
    );
    assert_eq!(output, "1,2,3,\n");
}

#[test]
fn test_iter_fold_after_filter_only_visits_kept_elements() {
    // Adaptors fire during fold's drain — filter rejects 1 and 2,
    // so the closure only runs for 3 + 4 + 5 = 12 (init 0).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    let s: i64 = v.iter().filter(|x| x > 2).fold(0, |acc, x| acc + x);
    println(s);
}
"#,
    );
    assert_eq!(output, "12\n");
}

#[test]
fn test_iter_any_returns_true_on_first_match() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    let b: bool = v.iter().any(|x| x > 3);
    println(b);
}
"#,
    );
    assert_eq!(output, "true\n");
}

#[test]
fn test_iter_any_returns_false_when_no_match() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let b: bool = v.iter().any(|x| x > 100);
    println(b);
}
"#,
    );
    assert_eq!(output, "false\n");
}

#[test]
fn test_iter_any_on_empty_returns_false() {
    let output = run_no_errors(
        r#"
fn main() {
    let v: Vec[i64] = Vec[];
    let b: bool = v.iter().any(|x| x > 0);
    println(b);
}
"#,
    );
    assert_eq!(output, "false\n");
}

#[test]
fn test_iter_any_short_circuits_on_first_match() {
    // any() should stop iterating the moment the predicate returns true.
    // The closure prints each element it sees; with input 1..5 and
    // pred `x > 2`, only the first three elements should print before
    // any() returns. Tree-walk closures snapshot captures so we can't
    // count via mutated outer bindings — the println side-effect
    // ordering is the visible signal.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    let b: bool = v.iter().any(|x| {
        println(x);
        x > 2
    });
    println(b);
}
"#,
    );
    assert_eq!(output, "1\n2\n3\ntrue\n");
}

#[test]
fn test_iter_all_returns_true_when_every_element_matches() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [2, 4, 6];
    let b: bool = v.iter().all(|x| x > 0);
    println(b);
}
"#,
    );
    assert_eq!(output, "true\n");
}

#[test]
fn test_iter_all_returns_false_on_first_mismatch() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [2, 4, -1, 6];
    let b: bool = v.iter().all(|x| x > 0);
    println(b);
}
"#,
    );
    assert_eq!(output, "false\n");
}

#[test]
fn test_iter_all_on_empty_returns_true() {
    // Vacuously true — no element violates the predicate.
    let output = run_no_errors(
        r#"
fn main() {
    let v: Vec[i64] = Vec[];
    let b: bool = v.iter().all(|x| x > 100);
    println(b);
}
"#,
    );
    assert_eq!(output, "true\n");
}

#[test]
fn test_iter_all_short_circuits_on_first_mismatch() {
    // all() should stop the moment the predicate returns false. With
    // input 1..5 and pred `x < 3`, the predicate sees 1, 2, 3 — and
    // bails on 3 (the first failing element). Element 3 is still
    // printed because the closure body runs to completion before its
    // boolean is consulted.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    let b: bool = v.iter().all(|x| {
        println(x);
        x < 3
    });
    println(b);
}
"#,
    );
    assert_eq!(output, "1\n2\n3\nfalse\n");
}

#[test]
fn test_iter_any_after_map_predicate_sees_mapped_values() {
    // Composes with map — the predicate sees mapped i64 (x * 10), so
    // it returns true once the running x*10 exceeds 25 (i.e. on x=3).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let b: bool = v.iter().map(|x| x * 10).any(|y| y > 25);
    println(b);
}
"#,
    );
    assert_eq!(output, "true\n");
}

#[test]
fn test_iter_enumerate_yields_index_and_item_pairs() {
    // enumerate() yields (idx, item) tuples; idx starts at 0.
    let output = run_no_errors(
        r#"
fn main() {
    let v = ["a", "b", "c"];
    for (i, s) in v.iter().enumerate() {
        println(f"{i}:{s}");
    }
}
"#,
    );
    assert_eq!(output, "0:a\n1:b\n2:c\n");
}

#[test]
fn test_iter_enumerate_persists_index_across_next_calls() {
    // Verifies state writeback — the Enumerate(idx) counter has to
    // survive between separate next() calls on the same iterator.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30];
    let mut it = v.iter().enumerate();
    let (a, ax) = it.next().unwrap();
    let (b, bx) = it.next().unwrap();
    println(f"{a}:{ax}");
    println(f"{b}:{bx}");
}
"#,
    );
    assert_eq!(output, "0:10\n1:20\n");
}

#[test]
fn test_iter_take_yields_only_first_n_elements() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    for x in v.iter().take(3) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n2\n3\n");
}

#[test]
fn test_iter_take_zero_yields_nothing() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let mut it = v.iter().take(0);
    match it.next() {
        Some(_) => println("had value"),
        None => println("empty"),
    }
}
"#,
    );
    assert_eq!(output, "empty\n");
}

#[test]
fn test_iter_take_more_than_length_yields_all_elements() {
    // take(n) where n > len yields all elements without error.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    let n: i64 = v.iter().take(100).count();
    println(n);
}
"#,
    );
    assert_eq!(output, "2\n");
}

#[test]
fn test_iter_skip_drops_first_n_elements() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30, 40, 50];
    for x in v.iter().skip(2) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "30\n40\n50\n");
}

#[test]
fn test_iter_skip_more_than_length_yields_nothing() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let mut it = v.iter().skip(100);
    match it.next() {
        Some(_) => println("had value"),
        None => println("empty"),
    }
}
"#,
    );
    assert_eq!(output, "empty\n");
}

#[test]
fn test_iter_skip_then_take_window() {
    // skip + take composed forms a slice — drop 1, take next 2 → [2, 3].
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    for x in v.iter().skip(1).take(2) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "2\n3\n");
}

#[test]
fn test_iter_take_with_filter_yields_first_n_passing() {
    // filter then take(2) — only the first two elements that pass the
    // predicate are yielded; the source still walks past rejected items.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5, 6];
    for x in v.iter().filter(|x| x > 2).take(2) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "3\n4\n");
}

#[test]
fn test_iter_take_state_persists_across_next_calls() {
    // The Take(remaining) counter has to survive between next() calls
    // for the bound to actually limit total yields.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30, 40];
    let mut it = v.iter().take(2);
    println(it.next().unwrap());
    println(it.next().unwrap());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    assert_eq!(output, "10\n20\ndone\n");
}

#[test]
fn test_iter_enumerate_after_map_indexes_mapped_values() {
    // Map first, then enumerate — index counts mapped output positions
    // (same as source positions here, but enumerate sees the mapped item).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    for (i, y) in v.iter().map(|x| x * 100).enumerate() {
        println(f"{i}={y}");
    }
}
"#,
    );
    assert_eq!(output, "0=100\n1=200\n2=300\n");
}

#[test]
fn test_iter_chain_yields_left_then_right() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    let w = [10, 20, 30];
    for x in v.iter().chain(w.iter()) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n2\n10\n20\n30\n");
}

#[test]
fn test_iter_chain_left_empty_yields_only_right() {
    let output = run_no_errors(
        r#"
fn main() {
    let v: Vec[i64] = Vec[];
    let w = [7, 8];
    for x in v.iter().chain(w.iter()) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "7\n8\n");
}

#[test]
fn test_iter_chain_right_empty_yields_only_left() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [3, 4];
    let w: Vec[i64] = Vec[];
    for x in v.iter().chain(w.iter()) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "3\n4\n");
}

#[test]
fn test_iter_chain_preserves_per_side_adaptors() {
    // Each side keeps its own adaptor chain — left's filter and
    // right's map both fire on their own elements only.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let w = [10, 20];
    let xs: Vec[i64] = v.iter().filter(|x| x > 2).chain(w.iter().map(|y| y + 100)).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "3\n4\n110\n120\n");
}

#[test]
fn test_iter_chain_downstream_step_applies_to_both_sides() {
    // Downstream map on the result of chain applies to ALL items
    // regardless of which side they came from.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    let w = [10, 20];
    for x in v.iter().chain(w.iter()).map(|x| x * 100) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "100\n200\n1000\n2000\n");
}

#[test]
fn test_iter_zip_pairs_elements_in_lockstep() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let w = ["a", "b", "c"];
    for (n, s) in v.iter().zip(w.iter()) {
        println(f"{n}:{s}");
    }
}
"#,
    );
    assert_eq!(output, "1:a\n2:b\n3:c\n");
}

#[test]
fn test_iter_zip_stops_at_shorter_left_side() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    let w = ["a", "b", "c", "d"];
    for (n, s) in v.iter().zip(w.iter()) {
        println(f"{n}:{s}");
    }
}
"#,
    );
    assert_eq!(output, "1:a\n2:b\n");
}

#[test]
fn test_iter_zip_stops_at_shorter_right_side() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let w = ["a", "b"];
    for (n, s) in v.iter().zip(w.iter()) {
        println(f"{n}:{s}");
    }
}
"#,
    );
    assert_eq!(output, "1:a\n2:b\n");
}

#[test]
fn test_iter_zip_either_empty_yields_nothing() {
    let output = run_no_errors(
        r#"
fn main() {
    let v: Vec[i64] = Vec[];
    let w = ["a", "b"];
    let mut it = v.iter().zip(w.iter());
    match it.next() {
        Some(_) => println("had value"),
        None => println("empty"),
    }
}
"#,
    );
    assert_eq!(output, "empty\n");
}

#[test]
fn test_iter_zip_preserves_per_side_adaptors() {
    // Left's filter and right's map both fire while zipping. Filtering
    // and mapping happen INSIDE each side's iteration; zip pulls from
    // the post-adaptor stream.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    let w = [10, 20, 30, 40];
    for (a, b) in v.iter().filter(|x| x > 1).zip(w.iter().map(|y| y * 2)) {
        println(f"{a}+{b}");
    }
}
"#,
    );
    // Left filtered: 2, 3, 4, 5. Right mapped: 20, 40, 60, 80.
    // Zipped: (2,20) (3,40) (4,60) (5,80).
    assert_eq!(output, "2+20\n3+40\n4+60\n5+80\n");
}

#[test]
fn test_iter_zip_with_enumerate_on_one_side() {
    // Composes with enumerate on the right side — index and value.
    let output = run_no_errors(
        r#"
fn main() {
    let v = ["a", "b", "c"];
    let w = [10, 20, 30];
    for (s, (i, n)) in v.iter().zip(w.iter().enumerate()) {
        println(f"{s}:{i}={n}");
    }
}
"#,
    );
    assert_eq!(output, "a:0=10\nb:1=20\nc:2=30\n");
}

#[test]
fn test_iter_chain_state_persists_across_next_calls() {
    // After exhausting left via two next() calls, the third pull
    // should switch to right transparently.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    let w = [9];
    let mut it = v.iter().chain(w.iter());
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    assert_eq!(output, "1\n2\n9\ndone\n");
}

#[test]
fn test_iter_take_while_yields_until_first_failure() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 10, 4, 5];
    for x in v.iter().take_while(|x| x < 5) {
        println(x);
    }
}
"#,
    );
    // Stops at the first element where predicate fails (10), yielding
    // only the 1, 2, 3 prefix — even though 4 and 5 follow, take_while
    // does not resume after a failure.
    assert_eq!(output, "1\n2\n3\n");
}

#[test]
fn test_iter_take_while_first_element_fails_yields_nothing() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30];
    let mut it = v.iter().take_while(|x| x < 5);
    match it.next() {
        Some(_) => println("had value"),
        None => println("empty"),
    }
}
"#,
    );
    assert_eq!(output, "empty\n");
}

#[test]
fn test_iter_take_while_all_pass_yields_all_elements() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    for x in v.iter().take_while(|x| x < 100) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n2\n3\n");
}

#[test]
fn test_iter_take_while_short_circuits_predicate() {
    // After the first false, predicate must NOT fire on subsequent
    // elements. Use println side effects to verify: the prefix prints
    // "p:N" for each predicate call, and the body prints "y:N" for
    // each yielded element.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 9, 3, 4];
    for x in v.iter().take_while(|x| { println(f"p:{x}"); x < 5 }) {
        println(f"y:{x}");
    }
}
"#,
    );
    // Predicate fires on 1, 2 (yield), 9 (stop). Never on 3 or 4.
    // For-loop drains the iterator first, then iterates the body — so
    // the order is predicate-prefix then yielded-prefix. The
    // short-circuit guarantee is still proven by the absence of "p:3"
    // and "p:4".
    assert_eq!(output, "p:1\np:2\np:9\ny:1\ny:2\n");
}

#[test]
fn test_iter_skip_while_drops_leading_prefix() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 10, 1, 2];
    for x in v.iter().skip_while(|x| x < 5) {
        println(x);
    }
}
"#,
    );
    // Skips 1, 2, 3 (predicate true), then yields 10 and everything
    // that follows — INCLUDING 1, 2 — because skip_while does not
    // re-test once the predicate has failed.
    assert_eq!(output, "10\n1\n2\n");
}

#[test]
fn test_iter_skip_while_all_pass_yields_nothing() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let mut it = v.iter().skip_while(|x| x < 100);
    match it.next() {
        Some(_) => println("had value"),
        None => println("empty"),
    }
}
"#,
    );
    assert_eq!(output, "empty\n");
}

#[test]
fn test_iter_skip_while_first_element_fails_yields_all() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30];
    for x in v.iter().skip_while(|x| x < 5) {
        println(x);
    }
}
"#,
    );
    // Predicate is false on the very first element, so skip_while
    // yields the whole iterator unchanged.
    assert_eq!(output, "10\n20\n30\n");
}

#[test]
fn test_iter_skip_while_does_not_re_test_after_first_failure() {
    // Same observation harness as the take_while short-circuit test:
    // predicate side-effects show the call sequence, body shows yields.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 9, 3, 4];
    for x in v.iter().skip_while(|x| { println(f"p:{x}"); x < 5 }) {
        println(f"y:{x}");
    }
}
"#,
    );
    // Predicate fires on 1, 2, 9 (trip). Never on 3 or 4. After the
    // trip, 9 is yielded and 3 / 4 pass through unconditionally.
    assert_eq!(output, "p:1\np:2\np:9\ny:9\ny:3\ny:4\n");
}

#[test]
fn test_iter_take_while_state_persists_across_next_calls() {
    // Once take_while has tripped, subsequent next() calls must
    // continue to return None — even though the source has more items.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 9, 3];
    let mut it = v.iter().take_while(|x| x < 5);
    println(it.next().unwrap());
    println(it.next().unwrap());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
    match it.next() {
        Some(_) => println("more"),
        None => println("still-done"),
    }
}
"#,
    );
    assert_eq!(output, "1\n2\ndone\nstill-done\n");
}

#[test]
fn test_iter_skip_while_state_persists_across_next_calls() {
    // Once skip_while has tripped, every subsequent next() must
    // return the next raw item without re-testing the predicate.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 9, 3, 4];
    let mut it = v.iter().skip_while(|x| x < 5);
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    assert_eq!(output, "9\n3\n4\ndone\n");
}

#[test]
fn test_iter_take_while_composes_with_filter() {
    // Filter feeds take_while — predicate sees only elements that
    // passed the filter; take_while stops on the first kept-but-failing.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5, 6, 7, 8];
    let xs: Vec[i64] = v.iter().filter(|x| x % 2 == 0).take_while(|x| x < 7).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // Filter yields 2, 4, 6, 8. take_while(<7) stops at 8 → [2, 4, 6].
    assert_eq!(output, "2\n4\n6\n");
}

#[test]
fn test_iter_skip_while_then_take_while_window() {
    // skip_while drops leading prefix, take_while bounds the tail.
    // Composition produces a "while-window" view.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 5, 6, 7, 9, 1];
    let xs: Vec[i64] = v.iter().skip_while(|x| x < 5).take_while(|x| x < 9).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // skip_while drops 1, 2 → trips on 5. take_while keeps 5, 6, 7;
    // stops at 9. Trailing 1 is unreachable because take_while is
    // sticky-stop after the first failure.
    assert_eq!(output, "5\n6\n7\n");
}

#[test]
fn test_iter_flat_map_yields_concatenated_inner_iters() {
    // For each outer item, closure yields a 2-element inner. Result
    // concatenates them in outer-then-inner order.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    for x in v.iter().flat_map(|n| { let inner = [n * 10, n * 100]; inner.iter() }) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "10\n100\n20\n200\n30\n300\n");
}

#[test]
fn test_iter_flat_map_empty_outer_yields_nothing() {
    let output = run_no_errors(
        r#"
fn main() {
    let v: Vec[i64] = Vec[];
    let mut it = v.iter().flat_map(|n| { let inner = [n, n]; inner.iter() });
    match it.next() {
        Some(_) => println("had value"),
        None => println("empty"),
    }
}
"#,
    );
    assert_eq!(output, "empty\n");
}

#[test]
fn test_iter_flat_map_inner_empty_skipped() {
    // Some outer items produce empty inner iterators — those are
    // skipped without yielding anything.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    for x in v.iter().flat_map(|n| {
        if n % 2 == 0 {
            let inner = [n * 10];
            inner.iter()
        } else {
            let inner: Vec[i64] = Vec[];
            inner.iter()
        }
    }) {
        println(x);
    }
}
"#,
    );
    // Outer 1 → empty; 2 → [20]; 3 → empty; 4 → [40].
    assert_eq!(output, "20\n40\n");
}

#[test]
fn test_iter_flat_map_state_persists_across_next_calls() {
    // next() pulls one item at a time. When the in-flight inner is
    // exhausted, the next pull must transparently switch to the
    // next outer's inner.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    let mut it = v.iter().flat_map(|n| { let inner = [n * 10, n * 100]; inner.iter() });
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    assert_eq!(output, "10\n100\n20\n200\ndone\n");
}

#[test]
fn test_iter_flat_map_inner_can_have_adaptors() {
    // Closure returns an inner iterator with its own adaptor chain
    // (filter here). Those filter-rejected items don't surface as
    // flat_map yields.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let xs: Vec[i64] = v.iter().flat_map(|n| {
        let inner = [n - 1, n, n + 1];
        inner.iter().filter(|x| x > 1)
    }).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // Outer 1 → [0,1,2] filtered to [2]
    // Outer 2 → [1,2,3] filtered to [2,3]
    // Outer 3 → [2,3,4] filtered to [2,3,4]
    assert_eq!(output, "2\n2\n3\n2\n3\n4\n");
}

#[test]
fn test_iter_flat_map_composes_with_filter_after() {
    // Downstream filter applies to the flattened stream.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let xs: Vec[i64] = v.iter()
        .flat_map(|n| { let inner = [n, n * 10]; inner.iter() })
        .filter(|x| x > 5)
        .collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // Flattened: 1, 10, 2, 20, 3, 30. Filter >5: 10, 20, 30.
    assert_eq!(output, "10\n20\n30\n");
}

#[test]
fn test_iter_flat_map_with_take_short_circuits_outer() {
    // Downstream take(2) means we should only need to drain enough
    // outer items to produce 2 yields. Side-effect prefixes prove
    // that outer 3 is never visited.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    for x in v.iter()
        .flat_map(|n| { println(f"outer:{n}"); let inner = [n * 10, n * 100]; inner.iter() })
        .take(2)
    {
        println(f"y:{x}");
    }
}
"#,
    );
    // For-loop drains: outer:1 → inner [10, 100], take pulls both →
    // remaining=0. Source still pulled but step rejects. Outer 2 is
    // pulled (because take exhaustion happens AFTER outer 1's inner
    // is fully drained — the take step counts post-flat_map). The
    // test of importance: outer 3 must NEVER be pulled.
    // Expected: outer:1, y:10, y:100 (or similar drain order),
    // then nothing more — definitely no "outer:3".
    let lines: Vec<&str> = output.lines().collect();
    assert!(
        lines.contains(&"outer:1"),
        "outer:1 must fire, got: {:?}",
        lines
    );
    assert!(
        !lines.contains(&"outer:3"),
        "outer:3 must NOT fire (take(2) short-circuits), got: {:?}",
        lines
    );
    assert!(
        lines.iter().filter(|l| l.starts_with("y:")).count() == 2,
        "exactly 2 yields expected, got: {:?}",
        lines
    );
}

#[test]
fn test_iter_flat_map_then_map_threads_types() {
    // flat_map produces a stream of one type, then map transforms.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    let xs: Vec[i64] = v.iter()
        .flat_map(|n| { let inner = [n, n + 100]; inner.iter() })
        .map(|x| x * 2)
        .collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // Flattened: 1, 101, 2, 102. Doubled: 2, 202, 4, 204.
    assert_eq!(output, "2\n202\n4\n204\n");
}

#[test]
fn test_iter_flat_map_after_filter() {
    // Filter the OUTER stream first — the closure only runs on
    // kept-outer items.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let xs: Vec[i64] = v.iter()
        .filter(|n| n % 2 == 0)
        .flat_map(|n| { let inner = [n, n * 10]; inner.iter() })
        .collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // Filtered outer: 2, 4. flat_map: [2, 20], [4, 40] → 2, 20, 4, 40.
    assert_eq!(output, "2\n20\n4\n40\n");
}

#[test]
fn test_iter_flat_map_with_count_terminal() {
    // Terminal count() drains and counts the flattened stream.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let n = v.iter().flat_map(|n| { let inner = [n, n, n]; inner.iter() }).count();
    println(n);
}
"#,
    );
    // 3 outer × 3 inner each = 9.
    assert_eq!(output, "9\n");
}

#[test]
fn test_iter_step_by_yields_every_nth_element() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5, 6, 7];
    for x in v.iter().step_by(2) {
        println(x);
    }
}
"#,
    );
    // Yields indices 0, 2, 4, 6 → 1, 3, 5, 7.
    assert_eq!(output, "1\n3\n5\n7\n");
}

#[test]
fn test_iter_step_by_one_is_observable_noop() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    for x in v.iter().step_by(1) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n2\n3\n");
}

#[test]
fn test_iter_step_by_zero_clamps_to_one() {
    // n=0 would underflow on the post-yield reset; runtime clamps
    // to 1, behaving like step_by(1).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    for x in v.iter().step_by(0) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n2\n3\n");
}

#[test]
fn test_iter_step_by_larger_than_length_yields_only_first() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    for x in v.iter().step_by(100) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n");
}

#[test]
fn test_iter_step_by_on_empty_yields_nothing() {
    let output = run_no_errors(
        r#"
fn main() {
    let v: Vec[i64] = Vec[];
    let mut it = v.iter().step_by(2);
    match it.next() {
        Some(_) => println("had value"),
        None => println("empty"),
    }
}
"#,
    );
    assert_eq!(output, "empty\n");
}

#[test]
fn test_iter_step_by_state_persists_across_next_calls() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30, 40, 50];
    let mut it = v.iter().step_by(2);
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    // Yields 10, 30, 50.
    assert_eq!(output, "10\n30\n50\ndone\n");
}

#[test]
fn test_iter_step_by_composes_with_filter() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5, 6, 7, 8];
    let xs: Vec[i64] = v.iter().filter(|x| x > 2).step_by(2).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // Filter yields 3, 4, 5, 6, 7, 8. step_by(2) → 3, 5, 7.
    assert_eq!(output, "3\n5\n7\n");
}

#[test]
fn test_iter_cycle_with_take_yields_repeated_elements() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    for x in v.iter().cycle().take(5) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n2\n1\n2\n1\n");
}

#[test]
fn test_iter_cycle_preserves_pre_adaptors() {
    // Adaptors applied BEFORE cycle live in the template's own
    // step chain — they re-run on each restart. Here the filter
    // re-rejects 1 each cycle.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    for x in v.iter().filter(|x| x > 1).cycle().take(5) {
        println(x);
    }
}
"#,
    );
    // Each cycle yields 2, 3. take(5) → 2, 3, 2, 3, 2.
    assert_eq!(output, "2\n3\n2\n3\n2\n");
}

#[test]
fn test_iter_cycle_on_empty_yields_nothing() {
    // Sticky-stop on empty template — must NOT infinite-loop.
    let output = run_no_errors(
        r#"
fn main() {
    let v: Vec[i64] = Vec[];
    let mut it = v.iter().cycle().take(10);
    match it.next() {
        Some(_) => println("had value"),
        None => println("empty"),
    }
}
"#,
    );
    assert_eq!(output, "empty\n");
}

#[test]
fn test_iter_cycle_composes_with_post_map() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    for x in v.iter().cycle().take(4).map(|x| x * 10) {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "10\n20\n10\n20\n");
}

#[test]
fn test_iter_cycle_state_persists_across_next_calls() {
    // next() pulls one item at a time, crossing cycle boundaries
    // transparently.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [7, 8];
    let mut it = v.iter().cycle();
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
}
"#,
    );
    assert_eq!(output, "7\n8\n7\n8\n7\n");
}

#[test]
fn test_iter_cycle_resets_stateful_adaptors_each_cycle() {
    // enumerate inside the template restarts at index 0 each cycle
    // because cycle clones the template (with its initial counters).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20];
    for (i, x) in v.iter().enumerate().cycle().take(4) {
        println(f"{i}:{x}");
    }
}
"#,
    );
    // First cycle: (0,10), (1,20). Second cycle re-runs enumerate
    // from 0 → (0,10), (1,20). take(4) total.
    assert_eq!(output, "0:10\n1:20\n0:10\n1:20\n");
}

#[test]
fn test_iter_step_by_then_cycle() {
    // step_by trims the template; cycle replays the trimmed
    // sequence forever.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    for x in v.iter().step_by(2).cycle().take(7) {
        println(x);
    }
}
"#,
    );
    // step_by(2) → 1, 3, 5. cycle.take(7) → 1, 3, 5, 1, 3, 5, 1.
    assert_eq!(output, "1\n3\n5\n1\n3\n5\n1\n");
}

#[test]
fn test_iter_inspect_passes_through_unchanged() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30];
    let xs: Vec[i64] = v.iter().inspect(|x| println(f"saw:{x}")).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // For-loop drains: inspect fires on each item during drain
    // (saw:10, saw:20, saw:30), then the body iterates the
    // collected Vec.
    assert_eq!(output, "saw:10\nsaw:20\nsaw:30\n10\n20\n30\n");
}

#[test]
fn test_iter_inspect_after_filter_only_fires_on_kept() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let xs: Vec[i64] = v.iter()
        .filter(|x| x > 2)
        .inspect(|x| println(f"saw:{x}"))
        .collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // Filter keeps 3, 4. inspect fires on 3, 4 only.
    assert_eq!(output, "saw:3\nsaw:4\n3\n4\n");
}

#[test]
fn test_iter_inspect_composes_with_downstream_steps() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let xs: Vec[i64] = v.iter()
        .inspect(|x| println(f"raw:{x}"))
        .map(|x| x * 10)
        .inspect(|x| println(f"mapped:{x}"))
        .collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // First inspect sees raw items; map transforms; second inspect
    // sees mapped items. All firing during the drain phase.
    assert_eq!(
        output,
        "raw:1\nmapped:10\nraw:2\nmapped:20\nraw:3\nmapped:30\n10\n20\n30\n"
    );
}

#[test]
fn test_iter_scan_yields_running_sum() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    for x in v.iter().scan(0, |state, item| {
        let new = state + item;
        Some((new, new))
    }) {
        println(x);
    }
}
"#,
    );
    // 0+1=1, 1+2=3, 3+3=6, 6+4=10.
    assert_eq!(output, "1\n3\n6\n10\n");
}

#[test]
fn test_iter_scan_short_circuits_on_none() {
    // scan returning None stops iteration; subsequent items are
    // not visited.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 100, 4, 5];
    for x in v.iter().scan(0, |state, item| {
        if item > 50 {
            None
        } else {
            let new = state + item;
            Some((new, new))
        }
    }) {
        println(x);
    }
}
"#,
    );
    // Stops at 100; running sums of 1, 2, 3 are 1, 3, 6.
    assert_eq!(output, "1\n3\n6\n");
}

#[test]
fn test_iter_scan_short_circuit_does_not_re_fire() {
    // Side-effect prefix proves scan does NOT call closure on items
    // after the first None.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 100, 3, 4];
    for x in v.iter().scan(0, |state, item| {
        println(f"c:{item}");
        if item > 50 {
            None
        } else {
            let new = state + item;
            Some((new, new))
        }
    }) {
        println(f"y:{x}");
    }
}
"#,
    );
    // Closure fires on 1, 2, 100. None on 100 → stop. 3 and 4 are
    // never visited. Yields: 1 (=0+1), 3 (=1+2).
    assert_eq!(output, "c:1\nc:2\nc:100\ny:1\ny:3\n");
}

#[test]
fn test_iter_scan_state_persists_across_next_calls() {
    // next() pulls one item at a time; scan's state survives
    // between pulls.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30];
    let mut it = v.iter().scan(0, |state, item| {
        let new = state + item;
        Some((new, new))
    });
    println(it.next().unwrap());
    println(it.next().unwrap());
    println(it.next().unwrap());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    assert_eq!(output, "10\n30\n60\ndone\n");
}

#[test]
fn test_iter_scan_with_string_state() {
    // State can be any type — String here.
    let output = run_no_errors(
        r#"
fn main() {
    let v = ["a", "b", "c"];
    for x in v.iter().scan("", |state, item| {
        let new = state + item;
        Some((new, new))
    }) {
        println(x);
    }
}
"#,
    );
    // Concatenating: "" + "a" = "a", "a" + "b" = "ab", "ab" + "c" = "abc".
    assert_eq!(output, "a\nab\nabc\n");
}

#[test]
fn test_iter_scan_after_filter() {
    // Filter first, then scan — the scan closure only sees kept
    // items.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5, 6];
    let xs: Vec[i64] = v.iter()
        .filter(|x| x % 2 == 0)
        .scan(0, |state, item| {
            let new = state + item;
            Some((new, new))
        })
        .collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    // Filter yields 2, 4, 6. Running sum: 2, 6, 12.
    assert_eq!(output, "2\n6\n12\n");
}

#[test]
fn test_iter_scan_state_independent_of_yielded_value() {
    // State and yielded value can differ — useful for "yield index
    // of running max" patterns.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [3, 1, 4, 1, 5, 9, 2, 6];
    for x in v.iter().scan(0, |state, item| {
        let new_state = if item > state { item } else { state };
        Some((new_state, new_state))
    }) {
        println(x);
    }
}
"#,
    );
    // Running max: 3, 3, 4, 4, 5, 9, 9, 9.
    assert_eq!(output, "3\n3\n4\n4\n5\n9\n9\n9\n");
}

#[test]
fn test_iter_peek_returns_next_without_consuming() {
    // peek() returns the upcoming element; the next next() call
    // returns the SAME element (the buffer is what gets consumed).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30];
    let mut p = v.iter().peekable();
    println(p.peek().unwrap());
    println(p.next().unwrap());
    println(p.next().unwrap());
}
"#,
    );
    assert_eq!(output, "10\n10\n20\n");
}

#[test]
fn test_iter_peek_idempotent_until_next() {
    // Multiple peek()s in a row see the same element; only next()
    // drains the buffer.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let mut p = v.iter().peekable();
    println(p.peek().unwrap());
    println(p.peek().unwrap());
    println(p.peek().unwrap());
    println(p.next().unwrap());
    println(p.peek().unwrap());
}
"#,
    );
    assert_eq!(output, "1\n1\n1\n1\n2\n");
}

#[test]
fn test_iter_peek_at_end_returns_none() {
    // After draining, peek returns None; subsequent peeks stay None.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1];
    let mut p = v.iter().peekable();
    println(p.next().unwrap());
    match p.peek() {
        Some(_) => println("more"),
        None => println("done"),
    }
    match p.peek() {
        Some(_) => println("more"),
        None => println("done"),
    }
    match p.next() {
        Some(_) => println("more"),
        None => println("done-next"),
    }
}
"#,
    );
    assert_eq!(output, "1\ndone\ndone\ndone-next\n");
}

#[test]
fn test_iter_peek_on_drained_iterator() {
    // After draining a single-element iterator with .take(0), peek
    // sees no element on a freshly constructed Peekable.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let mut p = v.iter().take(0).peekable();
    match p.peek() {
        Some(_) => println("yes"),
        None => println("none"),
    }
}
"#,
    );
    assert_eq!(output, "none\n");
}

#[test]
fn test_iter_peek_does_not_re_pull_after_buffered() {
    // peek() pulls from inner exactly once per buffered slot.
    // Side-effect prefix proves the closure does NOT fire on
    // repeated peek() calls — only the first peek() pulls.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let mut p = v.iter().inspect(|x| println(f"pull:{x}")).peekable();
    println(f"peek1:{p.peek().unwrap()}");
    println(f"peek2:{p.peek().unwrap()}");
    println(f"next:{p.next().unwrap()}");
    println(f"peek3:{p.peek().unwrap()}");
}
"#,
    );
    // Drain order: first peek() triggers inner pull (pull:1) and
    // buffers; second peek() returns from buffer (no pull); next()
    // drains buffer (no pull); third peek() pulls again (pull:2).
    assert_eq!(
        output,
        "pull:1\npeek1:1\npeek2:1\nnext:1\npull:2\npeek3:2\n"
    );
}

#[test]
fn test_iter_peek_sees_post_inner_step_value() {
    // When map is applied BEFORE peekable(), the buffered (and peeked)
    // value is the mapped value — peek and next agree.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let mut p = v.iter().map(|x| x * 10).peekable();
    println(p.peek().unwrap());
    println(p.next().unwrap());
    println(p.peek().unwrap());
}
"#,
    );
    assert_eq!(output, "10\n10\n20\n");
}

#[test]
fn test_iter_peekable_drains_in_for_loop() {
    // A Peekable iterator is still iterable; for-loop drains it
    // including any element already buffered by a prior peek().
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let mut p = v.iter().peekable();
    println(p.peek().unwrap());
    for x in p {
        println(x);
    }
}
"#,
    );
    // Buffered 1 from peek; for-loop drains 1, 2, 3, 4.
    assert_eq!(output, "1\n1\n2\n3\n4\n");
}

#[test]
fn test_iter_peekable_count_drains_buffer() {
    // Terminal `count()` on a Peekable counts the buffered element
    // plus the rest of the inner iterator.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30, 40, 50];
    let mut p = v.iter().peekable();
    println(p.peek().unwrap());
    println(p.count());
}
"#,
    );
    assert_eq!(output, "10\n5\n");
}

#[test]
fn test_iter_peekable_collect_includes_buffered() {
    // Round-trip: Peekable.collect() yields the same Vec as the
    // underlying iterator, even after a peek() has buffered an
    // element.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let mut p = v.iter().peekable();
    let _ = p.peek();
    let xs: Vec[i64] = p.collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n2\n3\n");
}

#[test]
fn test_iter_peekable_then_filter_drops_peek_capability_at_runtime() {
    // After .filter() the type is Iterator[T] (not Peekable), but
    // the underlying source chain still routes correctly — the
    // resulting iterator drains as if peekable() were a no-op
    // wrapper. This guards the runtime path that wraps the inner
    // for downstream adaptors.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    let xs: Vec[i64] = v.iter().peekable().filter(|x| x % 2 == 1).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n3\n5\n");
}

#[test]
fn test_iter_chunk_by_groups_consecutive_equal_keys() {
    // Consecutive items with the same parity group together.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 3, 5, 2, 4, 7, 9];
    for g in v.iter().chunk_by(|x| x % 2) {
        let mut s = "[";
        let mut first = true;
        for x in g {
            if not first { s = s + ","; }
            s = s + f"{x}";
            first = false;
        }
        s = s + "]";
        println(s);
    }
}
"#,
    );
    // Groups: [1,3,5] (odd), [2,4] (even), [7,9] (odd).
    assert_eq!(output, "[1,3,5]\n[2,4]\n[7,9]\n");
}

#[test]
fn test_iter_chunk_by_singleton_groups_when_all_keys_differ() {
    // Each item is its own group when key_fn returns a unique key.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let mut count = 0;
    for g in v.iter().chunk_by(|x| x) {
        for x in g {
            println(x);
        }
        count = count + 1;
    }
    println(f"groups:{count}");
}
"#,
    );
    assert_eq!(output, "1\n2\n3\n4\ngroups:4\n");
}

#[test]
fn test_iter_chunk_by_one_group_when_all_keys_equal() {
    // Constant key — single group containing every element.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30, 40];
    let mut count = 0;
    for g in v.iter().chunk_by(|x| 0) {
        for x in g {
            println(x);
        }
        count = count + 1;
    }
    println(f"groups:{count}");
}
"#,
    );
    assert_eq!(output, "10\n20\n30\n40\ngroups:1\n");
}

#[test]
fn test_iter_chunk_by_collects_into_vec_of_vec() {
    // Terminal collect() yields Vec[Vec[T]] — exercises the heap
    // allocation per group end-to-end.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 1, 2, 2, 2, 3];
    let groups: Vec[Vec[i64]] = v.iter().chunk_by(|x| x).collect();
    println(groups.len());
    for g in groups {
        println(g.len());
    }
}
"#,
    );
    // 3 groups: lengths 2, 3, 1.
    assert_eq!(output, "3\n2\n3\n1\n");
}

#[test]
fn test_iter_chunk_by_state_persists_across_next_calls() {
    // Calling next() one group at a time threads pending-item state
    // correctly across pulls.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 1, 2, 3, 3];
    let mut it = v.iter().chunk_by(|x| x);
    println(it.next().unwrap().len());
    println(it.next().unwrap().len());
    println(it.next().unwrap().len());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    assert_eq!(output, "2\n1\n2\ndone\n");
}

#[test]
fn test_iter_chunk_by_after_map_uses_mapped_item() {
    // Map runs before chunk_by — the groups carry mapped values.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let groups: Vec[Vec[i64]] = v.iter()
        .map(|x| x * 10)
        .chunk_by(|x| x > 25)
        .collect();
    for g in groups {
        let first = g[0];
        println(first);
    }
}
"#,
    );
    // Mapped: [10, 20, 30, 40]. Keys: false, false, true, true.
    // Groups: [10, 20], [30, 40]. First-elements: 10, 30.
    assert_eq!(output, "10\n30\n");
}

#[test]
fn test_iter_chunk_by_after_filter_only_groups_kept_items() {
    // Filter first, then chunk_by — the groups only include items
    // that passed the filter.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5, 6, 7, 8];
    let groups: Vec[Vec[i64]] = v.iter()
        .filter(|x| x != 4)
        .chunk_by(|x| x < 5)
        .collect();
    println(groups.len());
    for g in groups {
        println(g.len());
    }
}
"#,
    );
    // After filter: [1, 2, 3, 5, 6, 7, 8]. Keys: T, T, T, F, F, F, F.
    // Groups: [1, 2, 3] (len 3), [5, 6, 7, 8] (len 4).
    assert_eq!(output, "2\n3\n4\n");
}

#[test]
fn test_iter_chunk_by_key_fn_fires_once_per_item() {
    // Side-effect prefix proves key_fn is called exactly once per
    // inner item — even though the same item's key is consulted
    // twice (when ending a group and when seeding the next).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 1, 2, 2];
    let groups: Vec[Vec[i64]] = v.iter()
        .chunk_by(|x| {
            println(f"k:{x}");
            x
        })
        .collect();
    println(groups.len());
}
"#,
    );
    // key_fn fires once per element (not twice per boundary item).
    assert_eq!(output, "k:1\nk:1\nk:2\nk:2\n2\n");
}

#[test]
fn test_iter_chunk_by_with_take_short_circuits() {
    // Downstream take(n) limits how many groups we drain, so
    // chunk_by's inner pulls stop early.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 1, 2, 2, 3, 3, 4, 4];
    for g in v.iter().chunk_by(|x| x).take(2) {
        for x in g {
            println(x);
        }
    }
}
"#,
    );
    // First two groups only: [1, 1], [2, 2].
    assert_eq!(output, "1\n1\n2\n2\n");
}

#[test]
fn test_iter_chunk_by_on_empty_yields_no_groups() {
    // Empty source → no groups; for-loop body never runs.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1];
    let groups: Vec[Vec[i64]] = v.iter().take(0).chunk_by(|x| x).collect();
    println(f"groups:{groups.len()}");
}
"#,
    );
    assert_eq!(output, "groups:0\n");
}

#[test]
fn test_iter_chunks_groups_into_n_sized_pieces() {
    // Non-overlapping groups of n consecutive items; trailing
    // remainder is shorter when source isn't a multiple of n.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5, 6, 7];
    let groups: Vec[Vec[i64]] = v.iter().chunks(3).collect();
    println(groups.len());
    for g in groups {
        println(g.len());
    }
}
"#,
    );
    // 3 chunks: [1,2,3], [4,5,6], [7]. Lengths: 3, 3, 1.
    assert_eq!(output, "3\n3\n3\n1\n");
}

#[test]
fn test_iter_chunks_exact_multiple_yields_no_partial() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5, 6];
    let groups: Vec[Vec[i64]] = v.iter().chunks(2).collect();
    println(groups.len());
    for g in groups {
        println(g.len());
    }
}
"#,
    );
    // 3 chunks of size 2 each.
    assert_eq!(output, "3\n2\n2\n2\n");
}

#[test]
fn test_iter_chunks_n_larger_than_source_yields_one_partial() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let groups: Vec[Vec[i64]] = v.iter().chunks(10).collect();
    println(groups.len());
    println(groups[0].len());
}
"#,
    );
    assert_eq!(output, "1\n3\n");
}

#[test]
fn test_iter_chunks_zero_clamps_to_one() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let groups: Vec[Vec[i64]] = v.iter().chunks(0).collect();
    println(groups.len());
    for g in groups {
        println(g.len());
    }
}
"#,
    );
    // n=0 clamps to n=1: 3 singleton chunks.
    assert_eq!(output, "3\n1\n1\n1\n");
}

#[test]
fn test_iter_chunks_state_persists_across_next() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [10, 20, 30, 40, 50];
    let mut it = v.iter().chunks(2);
    println(it.next().unwrap().len());
    println(it.next().unwrap().len());
    println(it.next().unwrap().len());
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    assert_eq!(output, "2\n2\n1\ndone\n");
}

#[test]
fn test_iter_chunks_after_filter_only_groups_kept() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5, 6, 7, 8];
    let groups: Vec[Vec[i64]] = v.iter()
        .filter(|x| x % 2 == 0)
        .chunks(2)
        .collect();
    println(groups.len());
    for g in groups {
        println(g.len());
    }
}
"#,
    );
    // After filter: [2, 4, 6, 8]. Chunks(2): [2,4], [6,8].
    assert_eq!(output, "2\n2\n2\n");
}

#[test]
fn test_iter_windows_slides_by_one() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5];
    let wins: Vec[Vec[i64]] = v.iter().windows(3).collect();
    println(wins.len());
    for w in wins {
        let mut s = "[";
        let mut first = true;
        for x in w {
            if not first { s = s + ","; }
            s = s + f"{x}";
            first = false;
        }
        s = s + "]";
        println(s);
    }
}
"#,
    );
    // 3 windows: [1,2,3], [2,3,4], [3,4,5].
    assert_eq!(output, "3\n[1,2,3]\n[2,3,4]\n[3,4,5]\n");
}

#[test]
fn test_iter_windows_smaller_than_n_yields_nothing() {
    // No partial windows — when source is shorter than n, windows
    // emits zero items (matches Rust's [T].windows semantics).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2];
    let wins: Vec[Vec[i64]] = v.iter().windows(3).collect();
    println(wins.len());
}
"#,
    );
    assert_eq!(output, "0\n");
}

#[test]
fn test_iter_windows_exactly_n_yields_one() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let wins: Vec[Vec[i64]] = v.iter().windows(3).collect();
    println(wins.len());
    println(wins[0].len());
}
"#,
    );
    assert_eq!(output, "1\n3\n");
}

#[test]
fn test_iter_windows_state_persists_across_next() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let mut it = v.iter().windows(2);
    println(it.next().unwrap()[0]);
    println(it.next().unwrap()[0]);
    println(it.next().unwrap()[0]);
    match it.next() {
        Some(_) => println("more"),
        None => println("done"),
    }
}
"#,
    );
    // 3 windows of size 2: [1,2], [2,3], [3,4]. First element each.
    assert_eq!(output, "1\n2\n3\ndone\n");
}

#[test]
fn test_iter_windows_after_map_yields_mapped_values() {
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4];
    let wins: Vec[Vec[i64]] = v.iter().map(|x| x * 10).windows(2).collect();
    for w in wins {
        let a = w[0];
        let b = w[1];
        println(f"{a},{b}");
    }
}
"#,
    );
    // Mapped: [10, 20, 30, 40]. Windows(2): [10,20], [20,30], [30,40].
    assert_eq!(output, "10,20\n20,30\n30,40\n");
}

#[test]
fn test_iter_windows_zero_clamps_to_one() {
    // n=0 clamps to n=1 — degenerates to "each item as its own
    // singleton window" (still allocates per window).
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3];
    let wins: Vec[Vec[i64]] = v.iter().windows(0).collect();
    println(wins.len());
    for w in wins {
        println(w[0]);
    }
}
"#,
    );
    assert_eq!(output, "3\n1\n2\n3\n");
}

#[test]
fn test_iter_chunks_with_take_short_circuits() {
    // Downstream take(n) limits how many chunks we drain.
    let output = run_no_errors(
        r#"
fn main() {
    let v = [1, 2, 3, 4, 5, 6, 7, 8];
    let groups: Vec[Vec[i64]] = v.iter().chunks(2).take(2).collect();
    println(groups.len());
}
"#,
    );
    assert_eq!(output, "2\n");
}

// ── Range pattern matching (interpreter) ────────────────────────────
//
// All five forms — `..hi`, `lo..hi`, `lo..=hi`, `..=hi`, `lo..` — must
// match the value space correctly. Previously the interpreter had a
// "simplified" RangePattern arm that always returned `true`; this test
// pins the per-form semantics.

#[test]
fn test_range_pattern_matches_correctly() {
    let output = run_no_errors(
        r#"
fn classify(n: i32) -> i32 {
    match n {
        ..0 => -1,
        0..=9 => 1,
        10..100 => 2,
        100.. => 3,
        _ => 0,
    }
}
fn main() {
    println(classify(-5));
    println(classify(0));
    println(classify(5));
    println(classify(9));
    println(classify(10));
    println(classify(50));
    println(classify(99));
    println(classify(100));
    println(classify(200));
}
"#,
    );
    assert_eq!(output, "-1\n1\n1\n1\n2\n2\n2\n3\n3\n");
}

#[test]
fn test_range_pattern_char_bounded_inclusive() {
    let output = run_no_errors(
        r#"
fn classify(c: char) -> i32 {
    match c {
        'a'..='z' => 1,
        'A'..='Z' => 2,
        _ => 0,
    }
}
fn main() {
    println(classify('m'));
    println(classify('M'));
    println(classify('1'));
}
"#,
    );
    assert_eq!(output, "1\n2\n0\n");
}

#[test]
fn test_range_pattern_char_bounded_exclusive() {
    let output = run_no_errors(
        r#"
fn main() {
    let c: char = 'g';
    let r = match c {
        'a'..'h' => 1,
        _ => 0,
    };
    println(r);
}
"#,
    );
    // 'g' is in [a, h) — should match.
    assert_eq!(output, "1\n");
}

// ── dbg() task-id tagging and structured output (item 129) ────

#[test]
fn test_dbg_terminal_no_par_omits_task_prefix() {
    // Spec: `dbg(compute(x))` reports `compute(x)` as the expr — the
    // argument's source text, not the whole dbg call.
    let src = r#"fn main() {
    let _ = dbg(7);
}
"#;
    let (_stdout, dbg) = run_program_with_dbg(src, DbgOutputMode::Terminal);
    assert_eq!(dbg.len(), 1, "expected one dbg line, got {:?}", dbg);
    let line = &dbg[0];
    assert_eq!(line, "[test.kara:2] 7 = 7\n");
    assert!(
        !line.contains("task:"),
        "outside par {{}} should not include task tag"
    );
}

#[test]
fn test_dbg_terminal_in_par_tags_each_branch() {
    let src = r#"fn main() {
    par {
        let _ = dbg(1);
        let _ = dbg(2);
    }
}
"#;
    let (_stdout, dbg) = run_program_with_dbg(src, DbgOutputMode::Terminal);
    assert_eq!(dbg.len(), 2, "expected two dbg lines, got {:?}", dbg);
    // Branch 0 → task:1, branch 1 → task:2 (assigned in source order
    // before spawn, so deterministic regardless of OS scheduling).
    assert_eq!(dbg[0], "[task:1 test.kara:3] 1 = 1\n");
    assert_eq!(dbg[1], "[task:2 test.kara:4] 2 = 2\n");
}

#[test]
fn test_dbg_json_no_par_emits_null_task_id() {
    let src = r#"fn main() {
    let _ = dbg(42);
}
"#;
    let (_stdout, dbg) = run_program_with_dbg(src, DbgOutputMode::Json);
    assert_eq!(dbg.len(), 1);
    let line = &dbg[0];
    assert!(line.starts_with("{\"kind\":\"dbg\","));
    assert!(line.contains("\"task_id\":null"), "got {:?}", line);
    assert!(line.contains("\"file\":\"test.kara\""));
    assert!(line.contains("\"line\":2"));
    assert!(line.contains("\"expr\":\"42\""), "got {:?}", line);
    // Bare integer literal infers as i64 in the typechecker; the spec
    // example uses i32 because there `compute(x)` returns explicit i32.
    // The contract is "Display of the inferred type"; i64 is correct here.
    assert!(line.contains("\"type\":\"i64\""), "got {:?}", line);
    assert!(line.contains("\"value\":\"42\""));
    assert!(line.ends_with("}\n"));
}

#[test]
fn test_dbg_json_in_par_emits_numeric_task_id() {
    let src = r#"fn main() {
    par {
        let _ = dbg(10);
        let _ = dbg(20);
    }
}
"#;
    let (_stdout, dbg) = run_program_with_dbg(src, DbgOutputMode::Json);
    assert_eq!(dbg.len(), 2, "got {:?}", dbg);
    assert!(dbg[0].contains("\"task_id\":1"), "got {:?}", dbg[0]);
    assert!(dbg[0].contains("\"value\":\"10\""));
    assert!(dbg[1].contains("\"task_id\":2"), "got {:?}", dbg[1]);
    assert!(dbg[1].contains("\"value\":\"20\""));
}

#[test]
fn test_dbg_json_value_uses_debug_fmt_quoting() {
    // The `value` field must use `Debug` formatting: strings get
    // quoted as `"hello"` not `hello`. The whole field is then JSON-
    // escaped, so the inner quotes show up as `\"`.
    let src = r#"fn main() {
    let _ = dbg("hi");
}
"#;
    let (_stdout, dbg) = run_program_with_dbg(src, DbgOutputMode::Json);
    assert_eq!(dbg.len(), 1);
    // Inner Debug form: `"hi"`. JSON-escaped: `"\"hi\""`.
    assert!(
        dbg[0].contains("\"value\":\"\\\"hi\\\"\""),
        "expected JSON-escaped quoted string, got {:?}",
        dbg[0]
    );
}

#[test]
fn test_dbg_returns_its_argument() {
    // dbg() is an identity function — the value flows through.
    let src = r#"fn main() {
    let x = dbg(3) + dbg(4);
    println(x);
}
"#;
    let (stdout, dbg) = run_program_with_dbg(src, DbgOutputMode::Terminal);
    assert_eq!(stdout.join(""), "7\n");
    assert_eq!(dbg.len(), 2);
}

// ── Range / RangeInclusive as Iterator ─────────────────────────
//
// Range and RangeInclusive evaluate to `Value::Iterator` so the adaptor
// surface (`step_by`, `map`, `filter`, `take`, `collect`, ...) dispatches
// directly without a redundant `.iter()` layer. The for-loop iterable
// path drains `Value::Iterator` via `iterator_step`, so for-loop
// semantics are preserved.

#[test]
fn test_range_iter_step_by_collect() {
    // `(0..10).step_by(2).collect()` — half-open Range as Iterator.
    let output = run_no_errors(
        r#"
fn main() {
    let xs: Vec[i64] = (0..10).step_by(2).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "0\n2\n4\n6\n8\n");
}

#[test]
fn test_range_inclusive_step_by_collect() {
    // `(1..=10).step_by(2).collect()` — RangeInclusive as Iterator pins
    // the inclusive-end semantics (10 is reachable but the stride
    // skips it).
    let output = run_no_errors(
        r#"
fn main() {
    let xs: Vec[i64] = (1..=10).step_by(2).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "1\n3\n5\n7\n9\n");
}

#[test]
fn test_range_redundant_iter() {
    // `(0..5).iter().collect()` — the redundant iter() call on a Range
    // is a no-op pass-through. Pins the iter/into_iter early-return
    // guard (sub-step (d)) — without it, this hits an `unreachable!`
    // because Range now produces `Value::Iterator` rather than
    // `Value::Array`.
    let output = run_no_errors(
        r#"
fn main() {
    let xs: Vec[i64] = (0..5).iter().collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "0\n1\n2\n3\n4\n");
}

#[test]
fn test_range_chained_adaptors() {
    // Multi-step adaptor composition through the new entry surface:
    // map doubles each element, filter keeps those > 5.
    let output = run_no_errors(
        r#"
fn main() {
    let xs: Vec[i64] = (0..10).map(|x| x * 2).filter(|x| x > 5).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "6\n8\n10\n12\n14\n16\n18\n");
}

#[test]
fn test_range_for_loop_unchanged() {
    // Regression — for-loop semantics must survive the Range → Iterator
    // eval change. Sums 0+1+2+3+4 = 10.
    let output = run_no_errors(
        r#"
fn main() {
    let mut s = 0;
    for x in 0..5 {
        s = s + x;
    }
    println(s);
}
"#,
    );
    assert_eq!(output, "10\n");
}

#[test]
fn test_range_inclusive_take() {
    // `(0..=100).take(3).collect()` pins the eager-snapshot truncation
    // under `take` — the source pre-materialises 101 items but the
    // step-layer adaptor pulls only the first three.
    let output = run_no_errors(
        r#"
fn main() {
    let xs: Vec[i64] = (0..=100).take(3).collect();
    for x in xs {
        println(x);
    }
}
"#,
    );
    assert_eq!(output, "0\n1\n2\n");
}
