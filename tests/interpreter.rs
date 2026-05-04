// tests/interpreter.rs

use karac::{run_program, run_program_full, run_program_with_trace};

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
    // any structural fallback.
    assert_eq!(
        run("struct Point { x: i64, y: i64 }
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
    assert_eq!(
        run("struct Point { x: i64, y: i64 }
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
             println(a.load(Ordering.SeqCst));\n\
         }");
    assert_eq!(output, "42\n");
}

#[test]
fn test_atomic_store_and_load() {
    let output = run("fn main() {\n\
             let mut a = Atomic.new(0);\n\
             a.store(99, Ordering.Relaxed);\n\
             println(a.load(Ordering.Relaxed));\n\
         }");
    assert_eq!(output, "99\n");
}

#[test]
fn test_atomic_bool() {
    let output = run("fn main() {\n\
             let flag = Atomic.new(false);\n\
             println(flag.load(Ordering.SeqCst));\n\
         }");
    assert_eq!(output, "false\n");
}

// ── Ordering Enum ─────────────────────────────────────────────

#[test]
fn test_ordering_variants() {
    let output = run("fn main() {\n\
             let r = Ordering.Relaxed;\n\
             let a = Ordering.Acquire;\n\
             let rel = Ordering.Release;\n\
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
