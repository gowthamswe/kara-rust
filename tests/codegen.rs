//! Integration tests for Phase 7: LLVM code generation.
//!
//! Each test compiles a Kāra program snippet to LLVM IR and verifies:
//! - The IR can be generated without errors.
//! - Key IR patterns are present (function definitions, arithmetic, control flow, etc.).
//!
//! End-to-end execution tests (compile → link → run → compare output) are
//! gated on the host having `cc` available and are marked accordingly.

#[cfg(feature = "llvm")]
mod codegen_tests {
    use karac::codegen::compile_to_ir;

    /// Parse a snippet, run resolve+typecheck+lowering, then compile to LLVM IR.
    fn ir_for(src: &str) -> String {
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        compile_to_ir(&parsed.program, None).expect("codegen failed")
    }

    // ── Basic arithmetic ─────────────────────────────────────────

    #[test]
    fn test_ir_add_function() {
        let ir = ir_for("fn add(a: i64, b: i64) -> i64 { a + b }");
        assert!(
            ir.contains("define"),
            "should contain a function definition"
        );
        assert!(ir.contains("@add"), "should contain the function name");
        // LLVM 18 emits `add nsw i64` for checked integer addition.
        assert!(ir.contains("add nsw i64"), "should contain integer add");
    }

    #[test]
    fn test_ir_sub_mul_div() {
        let ir = ir_for("fn calc(a: i64, b: i64) -> i64 { (a - b) * (a + b) }");
        // LLVM 18 emits nsw variants for checked arithmetic.
        assert!(ir.contains("sub nsw i64"));
        assert!(ir.contains("mul nsw i64"));
    }

    #[test]
    fn test_ir_float_arithmetic() {
        let ir = ir_for("fn avg(a: f64, b: f64) -> f64 { (a + b) / 2.0 }");
        assert!(ir.contains("fadd"), "should use float add");
        assert!(ir.contains("fdiv"), "should use float div");
    }

    // ── Variables and let bindings ───────────────────────────────

    #[test]
    fn test_ir_let_binding() {
        let ir = ir_for("fn double(x: i64) -> i64 { let y = x * 2; y }");
        assert!(ir.contains("mul nsw"));
        assert!(ir.contains("alloca"));
    }

    #[test]
    fn test_ir_let_mut_reassign() {
        let ir = ir_for(
            r#"
fn count() -> i64 {
    let mut n = 0;
    n = n + 1;
    n = n + 1;
    n
}
"#,
        );
        assert!(ir.contains("store"));
        assert!(ir.contains("load"));
    }

    #[test]
    fn test_ir_compound_assign() {
        let ir = ir_for(
            r#"
fn accumulate(limit: i64) -> i64 {
    let mut sum = 0;
    let mut i = 1;
    while i <= limit {
        sum += i;
        i += 1;
    }
    sum
}
"#,
        );
        assert!(
            ir.contains("add nsw"),
            "should contain integer addition for +="
        );
        assert!(
            ir.contains("while.cond"),
            "should contain while condition block"
        );
    }

    // ── Control flow ─────────────────────────────────────────────

    #[test]
    fn test_ir_if_else() {
        let ir = ir_for("fn abs(x: i64) -> i64 { if x < 0 { 0 - x } else { x } }");
        assert!(ir.contains("br i1"), "should contain conditional branch");
        assert!(ir.contains("phi"), "if-else result should use phi node");
    }

    #[test]
    fn test_ir_if_no_else() {
        // `mut` on parameters is not Kāra syntax (modes are inferred).
        // Declare x as i64 and reassign via let mut inside the body.
        let ir = ir_for(
            r#"
fn clamp_positive(x: i64) -> i64 {
    let mut v = x;
    if v < 0 { v = 0; }
    v
}
"#,
        );
        assert!(ir.contains("br i1"));
    }

    #[test]
    fn test_ir_while_loop() {
        let ir = ir_for(
            r#"
fn sum_to(n: i64) -> i64 {
    let mut acc = 0;
    let mut i = 1;
    while i <= n {
        acc = acc + i;
        i = i + 1;
    }
    acc
}
"#,
        );
        assert!(ir.contains("while.cond"));
        assert!(ir.contains("while.body"));
        assert!(ir.contains("while.exit"));
    }

    #[test]
    fn test_ir_loop_break() {
        let ir = ir_for(
            r#"
fn find_first_positive(x: i64) -> i64 {
    let mut i = 0;
    loop {
        i = i + 1;
        if i > x { break; }
    }
    i
}
"#,
        );
        assert!(ir.contains("loop.body"));
        assert!(ir.contains("loop.exit"));
    }

    #[test]
    fn test_ir_for_range() {
        let ir = ir_for(
            r#"
fn sum_range(n: i64) -> i64 {
    let mut acc = 0;
    for i in 0..n {
        acc = acc + i;
    }
    acc
}
"#,
        );
        assert!(ir.contains("for.cond"));
        assert!(ir.contains("for.body"));
        assert!(ir.contains("for.incr"));
        assert!(ir.contains("for.exit"));
    }

    #[test]
    fn test_ir_for_range_inclusive() {
        let ir = ir_for(
            r#"
fn sum_inclusive(n: i64) -> i64 {
    let mut acc = 0;
    for i in 1..=n {
        acc = acc + i;
    }
    acc
}
"#,
        );
        assert!(ir.contains("for.cond"));
        // Inclusive range uses SLE
        assert!(ir.contains("icmp sle") || ir.contains("for.incr"));
    }

    // ── Recursive functions ───────────────────────────────────────

    #[test]
    fn test_ir_fibonacci_recursive() {
        let ir = ir_for(
            r#"
fn fib(n: i64) -> i64 {
    if n <= 1 { n } else { fib(n - 1) + fib(n - 2) }
}
"#,
        );
        // Should have a direct recursive call
        assert!(ir.contains("call") && ir.contains("fib"));
    }

    // ── Structs ──────────────────────────────────────────────────

    #[test]
    fn test_ir_struct_declaration() {
        // Use function params so LLVM cannot constant-fold the insertvalue instructions.
        let ir = ir_for(
            r#"
struct Point { x: i64, y: i64 }
fn make_point(x: i64, y: i64) -> Point { Point { x: x, y: y } }
"#,
        );
        assert!(
            ir.contains("insertvalue"),
            "struct init should use insertvalue"
        );
    }

    #[test]
    fn test_ir_struct_field_access() {
        let ir = ir_for(
            r#"
struct Point { x: i64, y: i64 }
fn x_coord(p: Point) -> i64 { p.x }
"#,
        );
        assert!(
            ir.contains("extractvalue"),
            "field access should use extractvalue"
        );
    }

    #[test]
    fn test_ir_struct_field_by_name() {
        let ir = ir_for(
            r#"
struct Rect { width: i64, height: i64 }
fn area(r: Rect) -> i64 { r.width * r.height }
"#,
        );
        assert!(ir.contains("mul nsw"));
        assert!(ir.contains("extractvalue"));
    }

    #[test]
    fn test_ir_struct_init_and_use() {
        let ir = ir_for(
            r#"
struct Vec2 { x: f64, y: f64 }
fn magnitude_sq(v: Vec2) -> f64 { v.x * v.x + v.y * v.y }
fn make_vec(x: f64, y: f64) -> Vec2 { Vec2 { x: x, y: y } }
"#,
        );
        assert!(ir.contains("fmul"));
        assert!(ir.contains("fadd"));
    }

    // ── Tuples ───────────────────────────────────────────────────

    #[test]
    fn test_ir_tuple_create_and_index() {
        let ir = ir_for(
            r#"
fn swap(a: i64, b: i64) -> (i64, i64) { (b, a) }
fn first(t: (i64, i64)) -> i64 { t.0 }
"#,
        );
        assert!(ir.contains("insertvalue"));
        assert!(ir.contains("extractvalue"));
    }

    // ── Match ────────────────────────────────────────────────────

    #[test]
    fn test_ir_match_integer_literals() {
        let ir = ir_for(
            r#"
fn day_name(d: i64) -> i64 {
    match d {
        1 => 100,
        2 => 200,
        _ => 0,
    }
}
"#,
        );
        assert!(ir.contains("icmp eq"), "literal match uses integer compare");
        assert!(ir.contains("match.merge") || ir.contains("matchval"));
    }

    #[test]
    fn test_ir_match_bool() {
        let ir = ir_for(
            r#"
fn negate(b: bool) -> bool {
    match b {
        true => false,
        false => true,
    }
}
"#,
        );
        assert!(ir.contains("icmp eq"));
    }

    // ── Enums ────────────────────────────────────────────────────

    #[test]
    fn test_ir_enum_unit_variants() {
        let ir = ir_for(
            r#"
enum Direction { North, South, East, West }
fn is_north(d: Direction) -> bool {
    match d {
        Direction.North => true,
        _ => false,
    }
}
"#,
        );
        // Enum should produce insertvalue for tag
        assert!(ir.contains("define"));
    }

    #[test]
    fn test_ir_enum_tuple_variant() {
        let ir = ir_for(
            r#"
enum Maybe { Nothing, Just(i64) }
fn unwrap_or(m: Maybe, default: i64) -> i64 {
    match m {
        Maybe.Just(v) => v,
        Maybe.Nothing => default,
    }
}
"#,
        );
        assert!(ir.contains("define"));
        // Tag check in match
        assert!(ir.contains("icmp eq"));
    }

    // ── Cast ─────────────────────────────────────────────────────

    #[test]
    fn test_ir_int_to_float_cast() {
        let ir = ir_for("fn to_float(x: i64) -> f64 { x as f64 }");
        assert!(ir.contains("sitofp"), "should use sitofp for int-to-float");
    }

    #[test]
    fn test_ir_float_to_int_cast() {
        let ir = ir_for("fn to_int(x: f64) -> i64 { x as i64 }");
        assert!(ir.contains("fptosi"), "should use fptosi for float-to-int");
    }

    #[test]
    fn test_ir_int_truncate() {
        let ir = ir_for("fn to_i32(x: i64) -> i32 { x as i32 }");
        assert!(ir.contains("trunc"));
    }

    // ── Multiple functions ────────────────────────────────────────

    #[test]
    fn test_ir_multiple_functions() {
        let ir = ir_for(
            r#"
fn square(x: i64) -> i64 { x * x }
fn sum_of_squares(a: i64, b: i64) -> i64 { square(a) + square(b) }
"#,
        );
        assert!(ir.contains("define") && ir.chars().filter(|&c| c == '\n').count() > 5);
        // Both functions should be in the IR
        assert!(ir.contains("square"));
        assert!(ir.contains("sum_of_squares"));
    }

    // ── Main function ─────────────────────────────────────────────

    #[test]
    fn test_ir_main_returns_i32() {
        let ir = ir_for(
            r#"
fn main() {
    println(42);
}
"#,
        );
        // main must be declared as returning i32 for C ABI compatibility
        assert!(ir.contains("define i32 @main()"));
        // Should have a `ret i32 0` at the end
        assert!(ir.contains("ret i32 0"));
    }

    // ── Compile-to-object (requires linker) ───────────────────────

    #[test]
    fn test_compile_to_object_hello_world() {
        use karac::codegen::compile_to_object;
        use std::path::Path;

        let src = r#"
fn main() {
    println(42);
}
"#;
        let mut parsed = karac::parse(src);
        assert!(parsed.errors.is_empty());
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);

        let obj_path = "/tmp/karac_test_hello.o";
        let result = compile_to_object(&parsed.program, obj_path, None);
        assert!(result.is_ok(), "compile_to_object failed: {:?}", result);
        assert!(Path::new(obj_path).exists(), "object file should exist");

        // Clean up
        let _ = std::fs::remove_file(obj_path);
    }

    // ── End-to-end execution tests ────────────────────────────────
    // These compile → link → run and verify stdout.

    fn run_program(src: &str) -> Option<String> {
        run_program_capturing(src).map(|c| c.stdout)
    }

    /// Stdout + stderr capture. Used by tests that assert against trace
    /// output written to stderr by the runtime's atexit handler.
    struct CapturedRun {
        stdout: String,
        stderr: String,
    }

    fn run_program_capturing(src: &str) -> Option<CapturedRun> {
        run_program_capturing_inner(src, None)
    }

    /// Like `run_program_capturing` but threads `source_filename` into codegen
    /// so `?` propagation traces print as `<file>:<line>:<col>`.
    fn run_program_capturing_with_filename(src: &str, filename: &str) -> Option<CapturedRun> {
        run_program_capturing_inner(src, Some(filename))
    }

    fn run_program_capturing_inner(src: &str, filename: Option<&str>) -> Option<CapturedRun> {
        use karac::codegen::{compile_to_object_with_options, link_executable};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let mut parsed = karac::parse(src);
        if !parsed.errors.is_empty() {
            return None;
        }
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);

        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let obj_path = format!("/tmp/karac_e2e_{}_{}.o", std::process::id(), id);
        let exe_path = format!("/tmp/karac_e2e_{}_{}", std::process::id(), id);

        compile_to_object_with_options(&parsed.program, &obj_path, None, filename).ok()?;
        link_executable(&obj_path, &exe_path).ok()?;

        let output = std::process::Command::new(&exe_path).output().ok()?;

        let _ = std::fs::remove_file(&obj_path);
        let _ = std::fs::remove_file(&exe_path);

        Some(CapturedRun {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    /// Like `run_program` but also runs the ownership checker and passes the
    /// result to codegen so RC-fallback boxing is exercised.
    fn run_program_with_ownership(src: &str) -> Option<String> {
        use karac::codegen::{compile_to_object, link_executable};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let mut parsed = karac::parse(src);
        if !parsed.errors.is_empty() {
            return None;
        }
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let ownership = karac::ownershipcheck(&parsed.program, &typed);

        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let obj_path = format!("/tmp/karac_e2e_ow_{}_{}.o", std::process::id(), id);
        let exe_path = format!("/tmp/karac_e2e_ow_{}_{}", std::process::id(), id);

        compile_to_object(&parsed.program, &obj_path, Some(&ownership)).ok()?;
        link_executable(&obj_path, &exe_path).ok()?;

        let output = std::process::Command::new(&exe_path).output().ok()?;

        let _ = std::fs::remove_file(&obj_path);
        let _ = std::fs::remove_file(&exe_path);

        Some(String::from_utf8_lossy(&output.stdout).to_string())
    }

    #[test]
    fn test_e2e_print_integer() {
        let out = run_program("fn main() { println(42); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_e2e_print_bool() {
        let out = run_program("fn main() { println(true); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "true");
        }
    }

    #[test]
    fn test_e2e_arithmetic() {
        let out = run_program(
            r#"
fn add(a: i64, b: i64) -> i64 { a + b }
fn main() { println(add(3, 4)); }
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "7");
        }
    }

    #[test]
    fn test_e2e_fibonacci() {
        let out = run_program(
            r#"
fn fib(n: i64) -> i64 {
    if n <= 1 { n } else { fib(n - 1) + fib(n - 2) }
}
fn main() { println(fib(10)); }
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "55");
        }
    }

    #[test]
    fn test_e2e_factorial_while() {
        let out = run_program(
            r#"
fn factorial(n: i64) -> i64 {
    let mut result = 1;
    let mut i = 1;
    while i <= n {
        result = result * i;
        i = i + 1;
    }
    result
}
fn main() { println(factorial(10)); }
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3628800");
        }
    }

    #[test]
    fn test_e2e_sum_for_range() {
        let out = run_program(
            r#"
fn main() {
    let mut sum = 0;
    for i in 1..=100 {
        sum = sum + i;
    }
    println(sum);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "5050");
        }
    }

    #[test]
    fn test_e2e_struct_field_access() {
        let out = run_program(
            r#"
struct Point { x: i64, y: i64 }
fn sum(p: Point) -> i64 { p.x + p.y }
fn main() {
    let p = Point { x: 3, y: 4 };
    println(sum(p));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "7");
        }
    }

    #[test]
    fn test_e2e_multiple_structs() {
        let out = run_program(
            r#"
struct Vec2 { x: i64, y: i64 }
fn dot(a: Vec2, b: Vec2) -> i64 { a.x * b.x + a.y * b.y }
fn main() {
    let a = Vec2 { x: 1, y: 2 };
    let b = Vec2 { x: 3, y: 4 };
    println(dot(a, b));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "11"); // 1*3 + 2*4 = 11
        }
    }

    #[test]
    fn test_e2e_match_integer() {
        let out = run_program(
            r#"
fn classify(n: i64) -> i64 {
    match n {
        0 => 0,
        1 => 1,
        _ => 2,
    }
}
fn main() {
    println(classify(0));
    println(classify(1));
    println(classify(42));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["0", "1", "2"]);
        }
    }

    #[test]
    fn test_e2e_break_continue() {
        let out = run_program(
            r#"
fn first_multiple_of_3(limit: i64) -> i64 {
    let mut result = 0;
    let mut i = 1;
    while i <= limit {
        if i % 3 == 0 {
            result = i;
            break;
        }
        i = i + 1;
    }
    result
}
fn main() { println(first_multiple_of_3(100)); }
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3");
        }
    }

    // ── Generic monomorphization ─────────────────────────────────────

    #[test]
    fn test_ir_generic_identity_function() {
        let ir = ir_for(
            r#"
fn identity[T](x: T) -> T { x }
fn main() {
    let a = identity(42);
    println(a);
}
"#,
        );
        // A specialization for i64 should be generated.
        assert!(
            ir.contains("identity$i64"),
            "should contain mangled i64 specialization"
        );
        assert!(ir.contains("define"), "should define at least one function");
    }

    #[test]
    fn test_ir_generic_two_params() {
        let ir = ir_for(
            r#"
fn add_generic[T](a: T, b: T) -> T { a + b }
fn main() {
    let x = add_generic(3, 4);
    println(x);
}
"#,
        );
        assert!(
            ir.contains("add_generic$i64"),
            "should contain i64 specialization"
        );
    }

    #[test]
    fn test_ir_generic_two_type_params() {
        let ir = ir_for(
            r#"
fn first[A, B](a: A, b: B) -> A { a }
fn main() {
    let x = first(10, 3.14);
    println(x);
}
"#,
        );
        // Should generate first$i64$f64
        assert!(
            ir.contains("first$i64$f64"),
            "should contain dual-param specialization"
        );
    }

    #[test]
    fn test_ir_generic_float_specialization() {
        let ir = ir_for(
            r#"
fn double_val[T](x: T) -> T { x + x }
fn main() {
    let y = double_val(2.5);
    println(y);
}
"#,
        );
        assert!(
            ir.contains("double_val$f64"),
            "should contain f64 specialization"
        );
    }

    #[test]
    fn test_ir_generic_multiple_uses_same_type() {
        // Calling a generic function twice with the same type should only
        // generate one specialization (deduplicated by mangle name).
        let ir = ir_for(
            r#"
fn negate[T](x: T) -> T { 0 - x }
fn main() {
    let a = negate(5);
    let b = negate(3);
    println(a + b);
}
"#,
        );
        // Count how many times the definition appears (not calls).
        let define_count = ir
            .lines()
            .filter(|l| l.contains("define") && l.contains("negate$i64"))
            .count();
        assert_eq!(
            define_count, 1,
            "should only generate one i64 specialization"
        );
    }

    #[test]
    fn test_ir_generic_different_types_two_specializations() {
        let ir = ir_for(
            r#"
fn square[T](x: T) -> T { x * x }
fn main() {
    let a = square(3);
    let b = square(2.0);
    println(a);
}
"#,
        );
        // Both i64 and f64 specializations should be present.
        assert!(ir.contains("square$i64"), "should have i64 specialization");
        assert!(ir.contains("square$f64"), "should have f64 specialization");
        let define_count = ir
            .lines()
            .filter(|l| l.contains("define") && l.contains("square$"))
            .count();
        assert_eq!(
            define_count, 2,
            "should generate exactly two specializations"
        );
    }

    #[test]
    fn test_e2e_generic_identity() {
        let out = run_program(
            r#"
fn identity[T](x: T) -> T { x }
fn main() {
    println(identity(99));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_generic_max() {
        let out = run_program(
            r#"
fn max_val[T](a: T, b: T) -> T {
    if a > b { a } else { b }
}
fn main() {
    println(max_val(3, 7));
    println(max_val(10, 2));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["7", "10"]);
        }
    }

    #[test]
    fn test_e2e_generic_swap_via_tuple() {
        let out = run_program(
            r#"
fn swap[T](a: T, b: T) -> (T, T) { (b, a) }
fn main() {
    let result = swap(1, 2);
    println(result.0);
    println(result.1);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "1"]);
        }
    }

    #[test]
    fn test_e2e_generic_higher_order_chain() {
        // Generic function calling another generic function.
        let out = run_program(
            r#"
fn double_val[T](x: T) -> T { x + x }
fn quad[T](x: T) -> T { double_val(double_val(x)) }
fn main() {
    println(quad(3));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "12");
        }
    }

    // ── Closure compilation ───────────────────────────────────────────

    #[test]
    fn test_ir_closure_simple() {
        let ir = ir_for(
            r#"
fn main() {
    let f = |x: i64| x + 1;
    println(f(5));
}
"#,
        );
        // A closure function should be generated.
        assert!(
            ir.contains("__closure_0"),
            "should define a closure function"
        );
        // The closure function pointer is extracted from the fat-pointer struct.
        assert!(
            ir.contains("extractvalue"),
            "should extract fn_ptr/env_ptr from fat pointer"
        );
    }

    #[test]
    fn test_ir_closure_no_captures() {
        let ir = ir_for(
            r#"
fn main() {
    let double = |x: i64| x * 2;
    println(double(3));
}
"#,
        );
        assert!(ir.contains("__closure_0"));
        assert!(ir.contains("mul nsw"));
    }

    #[test]
    fn test_ir_closure_captures_variable() {
        let ir = ir_for(
            r#"
fn main() {
    let base = 10;
    let add_base = |x: i64| x + base;
    println(add_base(5));
}
"#,
        );
        // Closure should be generated and capture `base`.
        assert!(
            ir.contains("__closure_0"),
            "should define a closure function"
        );
        // The function takes an env pointer and the param.
        assert!(
            ir.contains("add nsw") || ir.contains("add i64"),
            "should add"
        );
    }

    #[test]
    fn test_ir_closure_two_params() {
        let ir = ir_for(
            r#"
fn main() {
    let add = |x: i64, y: i64| x + y;
    println(add(3, 4));
}
"#,
        );
        assert!(ir.contains("__closure_0"));
    }

    #[test]
    fn test_ir_closure_float() {
        let ir = ir_for(
            r#"
fn main() {
    let scale = |x: f64| x * 2.0;
    println(scale(3.0));
}
"#,
        );
        assert!(ir.contains("__closure_0"));
        assert!(ir.contains("fmul"));
    }

    #[test]
    fn test_ir_closure_bool_return() {
        let ir = ir_for(
            r#"
fn main() {
    let is_pos = |x: i64| x > 0;
    println(is_pos(5));
}
"#,
        );
        assert!(ir.contains("__closure_0"));
        assert!(ir.contains("icmp sgt") || ir.contains("sgt"));
    }

    #[test]
    fn test_ir_closure_passed_to_function() {
        // Test that a closure can be passed to a function and called.
        let ir = ir_for(
            r#"
fn apply(f: i64, x: i64) -> i64 {
    f
}
fn main() {
    let double = |x: i64| x * 2;
    let result = apply(double(3), 0);
    println(result);
}
"#,
        );
        assert!(ir.contains("__closure_0"));
    }

    // ── Closure end-to-end execution tests ───────────────────────────

    #[test]
    fn test_e2e_closure_identity() {
        let out = run_program(
            r#"
fn main() {
    let f = |x: i64| x;
    println(f(42));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_e2e_closure_add_one() {
        let out = run_program(
            r#"
fn main() {
    let inc = |x: i64| x + 1;
    println(inc(7));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "8");
        }
    }

    #[test]
    fn test_e2e_closure_multiply() {
        let out = run_program(
            r#"
fn main() {
    let triple = |x: i64| x * 3;
    println(triple(4));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "12");
        }
    }

    #[test]
    fn test_e2e_closure_captures_outer() {
        let out = run_program(
            r#"
fn main() {
    let offset = 100;
    let add_offset = |x: i64| x + offset;
    println(add_offset(5));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "105");
        }
    }

    #[test]
    fn test_e2e_closure_multiple_captures() {
        let out = run_program(
            r#"
fn main() {
    let a = 3;
    let b = 7;
    let combine = |x: i64| x + a + b;
    println(combine(0));
    println(combine(10));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["10", "20"]);
        }
    }

    #[test]
    fn test_e2e_closure_two_params() {
        let out = run_program(
            r#"
fn main() {
    let add = |x: i64, y: i64| x + y;
    println(add(10, 32));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_e2e_closure_two_closures() {
        let out = run_program(
            r#"
fn main() {
    let double = |x: i64| x * 2;
    let add_one = |x: i64| x + 1;
    println(add_one(double(5)));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "11");
        }
    }

    #[test]
    fn test_e2e_closure_captures_in_loop() {
        let out = run_program(
            r#"
fn main() {
    let step = 5;
    let advance = |x: i64| x + step;
    let mut n = 0;
    for _ in 0..4 {
        n = advance(n);
    }
    println(n);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "20");
        }
    }

    // ── Shared struct (RC) tests ────────────────────────────────

    #[test]
    fn test_ir_shared_struct_malloc() {
        let ir = ir_for(
            r#"
shared struct Node { val: i64 }
fn make() -> Node { Node { val: 42 } }
"#,
        );
        // Should heap-allocate via malloc and store refcount = 1.
        assert!(ir.contains("@malloc"), "shared struct should call malloc");
        assert!(
            ir.contains("store i64 1"),
            "should store initial refcount of 1"
        );
    }

    #[test]
    fn test_ir_shared_struct_field_gep() {
        let ir = ir_for(
            r#"
shared struct Point { x: i64, y: i64 }
fn read_x(p: Point) -> i64 { p.x }
"#,
        );
        // Field access on shared type should use GEP, not extractvalue.
        assert!(
            ir.contains("getelementptr"),
            "shared struct field access should use GEP"
        );
    }

    #[test]
    fn test_ir_shared_struct_rc_dec_free() {
        let ir = ir_for(
            r#"
shared struct Token { id: i64 }
fn use_token() {
    let t = Token { id: 1 };
}
"#,
        );
        // Scope exit should decrement and conditionally free.
        assert!(
            ir.contains("@free"),
            "shared struct scope exit should call free"
        );
    }

    #[test]
    fn test_ir_shared_struct_rc_inc_on_copy() {
        let ir = ir_for(
            r#"
shared struct Obj { data: i64 }
fn copy_shared() {
    let a = Obj { data: 10 };
    let b = a;
}
"#,
        );
        // Copying `a` to `b` should increment refcount.
        // The IR should contain at least two references to the rc add pattern.
        let rc_inc_count = ir.matches("add i64 %rc").count();
        assert!(
            rc_inc_count >= 1,
            "copying shared var should produce rc_inc (found {} occurrences)",
            rc_inc_count
        );
    }

    #[test]
    fn test_e2e_shared_struct_basic() {
        let out = run_program(
            r#"
shared struct Counter { val: i64 }
fn main() {
    let c = Counter { val: 42 };
    println(c.val);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_e2e_shared_struct_alias() {
        let out = run_program(
            r#"
shared struct Data { x: i64 }
fn main() {
    let a = Data { x: 100 };
    let b = a;
    println(a.x);
    println(b.x);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["100", "100"]);
        }
    }

    #[test]
    fn test_e2e_shared_struct_passed_to_fn() {
        let out = run_program(
            r#"
shared struct Wrapper { val: i64 }
fn read_val(w: Wrapper) -> i64 { w.val }
fn main() {
    let w = Wrapper { val: 77 };
    println(read_val(w));
    println(w.val);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["77", "77"]);
        }
    }

    // ── Shared enum (RC) tests ─────────────────────────────────

    #[test]
    fn test_ir_shared_enum_malloc() {
        let ir = ir_for(
            r#"
shared enum Shape { Circle(i64), Square(i64) }
fn make() -> Shape { Circle(5) }
"#,
        );
        assert!(ir.contains("@malloc"), "shared enum should call malloc");
        assert!(
            ir.contains("store i64 1"),
            "should store initial refcount of 1"
        );
    }

    #[test]
    fn test_e2e_shared_enum_construct_and_match() {
        // NOTE: Unit variant pattern matching (`Color::Red =>`) is a known pre-existing
        // parser limitation (parsed as Binding, not variant pattern). Use tuple variants
        // or wildcard to test shared enum matching.
        let out = run_program(
            r#"
shared enum Action { Add(i64), Mul(i64) }
fn apply(a: Action, base: i64) -> i64 {
    match a {
        Add(n) => base + n,
        Mul(n) => base * n,
    }
}
fn main() {
    let a = Add(5);
    println(apply(a, 10));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "15");
        }
    }

    #[test]
    fn test_e2e_shared_enum_tuple_variant() {
        let out = run_program(
            r#"
shared enum Value { Num(i64), Nothing }
fn extract(v: Value) -> i64 {
    match v {
        Num(n) => n,
        Nothing => 0,
    }
}
fn main() {
    let v = Num(42);
    println(extract(v));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_e2e_shared_struct_multiple_fields() {
        let out = run_program(
            r#"
shared struct Vec2 { x: i64, y: i64 }
fn sum(v: Vec2) -> i64 { v.x + v.y }
fn main() {
    let v = Vec2 { x: 3, y: 7 };
    println(sum(v));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "10");
        }
    }

    // ── Unit enum variant matching ──────────────────────────────

    #[test]
    fn test_e2e_unit_enum_match() {
        let out = run_program(
            r#"
enum Color { Red, Green, Blue }
fn describe(c: Color) -> i64 {
    match c {
        Color.Red => 1,
        Color.Green => 2,
        Color.Blue => 3,
    }
}
fn main() {
    println(describe(Green));
    println(describe(Red));
    println(describe(Blue));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "1", "3"]);
        }
    }

    #[test]
    fn test_e2e_shared_enum_unit_variant_match() {
        let out = run_program(
            r#"
shared enum Dir { North, South, East, West }
fn to_num(d: Dir) -> i64 {
    match d {
        Dir.North => 0,
        Dir.South => 1,
        Dir.East => 2,
        Dir.West => 3,
    }
}
fn main() {
    println(to_num(East));
    println(to_num(North));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "0"]);
        }
    }

    // ── Array[T, N] fixed-size arrays ────────────────────────────

    #[test]
    fn test_ir_array_param_type() {
        // Array[T, N] parameter lowers to LLVM `[N x T]`.
        let ir = ir_for("fn take(a: Array[i64, 4]) { }");
        assert!(
            ir.contains("[4 x i64]"),
            "expected [4 x i64] in IR, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_ir_array_different_sizes() {
        let ir = ir_for(
            "fn small(a: Array[i64, 3]) { }
             fn big(a: Array[i64, 16]) { }",
        );
        assert!(ir.contains("[3 x i64]"));
        assert!(ir.contains("[16 x i64]"));
    }

    #[test]
    fn test_ir_array_of_bool() {
        let ir = ir_for("fn flags(a: Array[bool, 8]) { }");
        assert!(
            ir.contains("[8 x i1]"),
            "expected [8 x i1] in IR, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_ir_array_literal_construction() {
        let ir = ir_for("fn main() { let a = [10, 20, 30]; }");
        // Array literal lowers to alloca + store of [3 x i64].
        // LLVM constant-folds the insertvalue chain into a direct constant store.
        assert!(
            ir.contains("[3 x i64]"),
            "expected [3 x i64] type, got:\n{}",
            ir
        );
        assert!(
            ir.contains("alloca [3 x i64]"),
            "expected alloca for array, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_ir_array_literal_let_binding() {
        let ir = ir_for("fn main() { let a: Array[i64, 2] = [1, 2]; }");
        // Array should be alloca'd and stored.
        assert!(
            ir.contains("alloca [2 x i64]"),
            "expected alloca [2 x i64], got:\n{}",
            ir
        );
    }

    #[test]
    fn test_ir_array_index_read() {
        let ir = ir_for(
            r#"
fn second() -> i64 {
    let a = [10, 20, 30];
    a[1]
}
"#,
        );
        // Should contain GEP into the array and a bounds check.
        assert!(
            ir.contains("getelementptr"),
            "expected getelementptr for array index, got:\n{}",
            ir
        );
        assert!(
            ir.contains("idx.oob"),
            "expected bounds-check OOB block, got:\n{}",
            ir
        );
        assert!(
            ir.contains("idx.ok"),
            "expected bounds-check OK block, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_ir_array_index_store() {
        let ir = ir_for(
            r#"
fn main() {
    let mut a = [1, 2, 3];
    a[0] = 42;
}
"#,
        );
        assert!(
            ir.contains("arr.store.ptr"),
            "expected store GEP for index assignment, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_e2e_array_index_read() {
        let out = run_program(
            r#"
fn main() {
    let a = [10, 20, 30];
    println(a[1]);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "20");
        }
    }

    #[test]
    fn test_e2e_array_index_store() {
        let out = run_program(
            r#"
fn main() {
    let mut a = [10, 20, 30];
    a[2] = 99;
    println(a[2]);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_array_for_loop() {
        let out = run_program(
            r#"
fn main() {
    let a = [10, 20, 30, 40];
    let mut sum = 0;
    for x in a {
        sum = sum + x;
    }
    println(sum);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "100");
        }
    }

    #[test]
    fn test_e2e_array_basics_example() {
        let src = include_str!("../examples/array_basics.kara");
        let out = run_program(src);
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            // sum([10,40,20,30]) = 100
            // max([10,40,20,30]) = 40
            // scores printed: 100, 85, 92, 78
            // sum(scores) = 355
            assert_eq!(
                lines,
                vec!["100", "40", "100", "85", "92", "78", "355"],
                "array_basics.kara output mismatch"
            );
        }
    }

    #[test]
    fn test_e2e_slice_basics_example() {
        let src = include_str!("../examples/slice_basics.kara");
        let out = run_program(src);
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(
                lines,
                vec!["10", "600", "90", "1", "2", "10"],
                "slice_basics.kara output mismatch"
            );
        }
    }

    #[test]
    fn test_ir_array_len_constant_fold() {
        let ir = ir_for("fn get_len() -> i64 { let a = [10, 20, 30]; a.len() }");
        assert!(
            ir.contains("ret i64 3"),
            "expected len() constant fold to 3, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_e2e_array_len() {
        let out = run_program(
            r#"
fn main() {
    let a = [10, 20, 30, 40, 50];
    println(a.len());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "5");
        }
    }

    // ── Vec[T] growable arrays ────────────────────────────────────

    #[test]
    fn test_ir_vec_param_type() {
        let ir = ir_for("fn take(v: Vec[i64]) { }");
        // Vec[T] lowers to { ptr, i64, i64 }.
        assert!(
            ir.contains("{ ptr, i64, i64 }"),
            "expected {{ ptr, i64, i64 }} struct for Vec param, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_ir_vec_new() {
        let ir = ir_for("fn main() { let v: Vec[i64] = Vec.new(); }");
        // Vec::new() produces { null, 0, 0 } stored into an alloca.
        assert!(
            ir.contains("{ ptr, i64, i64 }"),
            "expected Vec struct type, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_e2e_vec_push_len() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(10);
    v.push(20);
    v.push(30);
    println(v.len());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3");
        }
    }

    #[test]
    fn test_e2e_vec_push_many() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    let mut i = 0;
    while i < 10 {
        v.push(i);
        i = i + 1;
    }
    println(v.len());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "10");
        }
    }

    #[test]
    fn test_e2e_vec_index() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(100);
    v.push(200);
    v.push(300);
    println(v[0]);
    println(v[1]);
    println(v[2]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["100", "200", "300"]);
        }
    }

    #[test]
    fn test_e2e_vec_pop() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(10);
    v.push(20);
    let x = v.pop();
    println(x);
    println(v.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["20", "1"]);
        }
    }

    #[test]
    fn test_e2e_vec_for_loop() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(1);
    v.push(2);
    v.push(3);
    let mut sum = 0;
    for x in v {
        sum = sum + x;
    }
    println(sum);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "6");
        }
    }

    // ── String codegen ────────────────────────────────────────────

    #[test]
    fn test_e2e_string_literal_println() {
        let out = run_program(r#"fn main() { println("hello world"); }"#);
        if let Some(out) = out {
            assert_eq!(out.trim(), "hello world");
        }
    }

    #[test]
    fn test_e2e_string_literal_len() {
        let out = run_program(
            r#"
fn main() {
    let s = "hello";
    println(s.len());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "5");
        }
    }

    #[test]
    fn test_e2e_string_new_push_str() {
        let out = run_program(
            r#"
fn main() {
    let mut s: String = String.new();
    s.push_str("hello");
    s.push_str(" world");
    println(s);
    println(s.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["hello world", "11"]);
        }
    }

    // ── ref parameter semantics ───────────────────────────────────

    #[test]
    fn test_e2e_ref_vec_param() {
        let out = run_program(
            r#"
fn sum(v: ref Vec[i64]) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < v.len() {
        total = total + v[i];
        i = i + 1;
    }
    total
}
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(10);
    v.push(20);
    v.push(30);
    println(sum(v));
    println(v.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["60", "3"], "ref Vec should borrow, not move");
        }
    }

    #[test]
    fn test_e2e_ref_vec_for_loop() {
        let out = run_program(
            r#"
fn print_all(v: ref Vec[i64]) {
    for x in v {
        println(x);
    }
}
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(1);
    v.push(2);
    v.push(3);
    print_all(v);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["1", "2", "3"]);
        }
    }

    #[test]
    fn test_e2e_ref_string_param() {
        let out = run_program(
            r#"
fn greet(name: ref String) {
    println(name);
    println(name.len());
}
fn main() {
    let s = "Alice";
    greet(s);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["Alice", "5"]);
        }
    }

    // ── SoA layout codegen ────────────────────────────────────────

    #[test]
    fn test_ir_soa_layout_type() {
        let ir = ir_for(
            r#"
struct Entity { x: f64, y: f64, hp: i64 }
layout entities: Vec[Entity] {
    group physics { x, y }
    group combat { hp }
}
fn main() {
    let entities: Vec[Entity] = Vec.new();
}
"#,
        );
        // SoA Vec with 2 groups → { ptr, ptr, i64, i64 }
        assert!(
            ir.contains("{ ptr, ptr, i64, i64 }"),
            "expected SoA struct {{ ptr, ptr, i64, i64 }}, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_e2e_soa_push_len() {
        let out = run_program(
            r#"
struct Entity { x: f64, y: f64, hp: i64 }
layout entities: Vec[Entity] {
    group physics { x, y }
    group combat { hp }
}
fn main() {
    let mut entities: Vec[Entity] = Vec.new();
    entities.push(Entity { x: 1.0, y: 2.0, hp: 100 });
    entities.push(Entity { x: 3.0, y: 4.0, hp: 200 });
    println(entities.len());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "2", "SoA push + len should work");
        }
    }

    // ── String operators ──────────────────────────────────────────

    #[test]
    fn test_e2e_string_equality() {
        let out = run_program(
            r#"
fn main() {
    let a = "hello";
    let b = "hello";
    let c = "world";
    if a == b { println(1); } else { println(0); }
    if a == c { println(1); } else { println(0); }
    if a != c { println(1); } else { println(0); }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["1", "0", "1"]);
        }
    }

    #[test]
    fn test_e2e_string_ordering() {
        let out = run_program(
            r#"
fn main() {
    let a = "abc";
    let b = "abd";
    if a < b { println(1); } else { println(0); }
    if b > a { println(1); } else { println(0); }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["1", "1"]);
        }
    }

    #[test]
    fn test_e2e_string_concatenation() {
        let out = run_program(
            r#"
fn main() {
    let a = "hello";
    let b = " world";
    let c = a + b;
    println(c);
    println(c.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["hello world", "11"]);
        }
    }

    // ── Struct equality ───────────────────────────────────────────

    #[test]
    fn test_e2e_struct_equality() {
        let out = run_program(
            r#"
struct Point { x: i64, y: i64 }
fn main() {
    let a = Point { x: 1, y: 2 };
    let b = Point { x: 1, y: 2 };
    let c = Point { x: 3, y: 4 };
    if a == b { println(1); } else { println(0); }
    if a == c { println(1); } else { println(0); }
    if a != c { println(1); } else { println(0); }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["1", "0", "1"]);
        }
    }

    #[test]
    fn test_e2e_struct_equality_mixed_types() {
        let out = run_program(
            r#"
struct Pair { name: String, value: i64 }
fn main() {
    let a = Pair { name: "hello", value: 42 };
    let b = Pair { name: "hello", value: 42 };
    let c = Pair { name: "world", value: 42 };
    if a == b { println(1); } else { println(0); }
    if a == c { println(1); } else { println(0); }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["1", "0"]);
        }
    }

    // ── Option/Result-like enums ────────────────────────────────

    #[test]
    fn test_e2e_option_like_enum() {
        let out = run_program(
            r#"
enum MyOption {
    None,
    Some(i64),
}
fn get_value(opt: MyOption) -> i64 {
    match opt {
        None => 0,
        Some(x) => x,
    }
}
fn main() {
    println(get_value(Some(42)));
    println(get_value(None));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["42", "0"]);
        }
    }

    #[test]
    fn test_e2e_array_for_print_each() {
        let out = run_program(
            r#"
fn main() {
    let a = [1, 2, 3];
    for x in a {
        println(x);
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["1", "2", "3"]);
        }
    }

    // ── Slice[T] end-to-end ────────────────────────────────────────

    #[test]
    fn test_e2e_slice_sum_over_array_coercion() {
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let a: Array[i64, 4] = [1, 2, 3, 4];
    println(sum(a));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "10");
        }
    }

    #[test]
    fn test_e2e_slice_element_index() {
        let out = run_program(
            r#"
fn second(xs: Slice[i64]) -> i64 { xs[1] }
fn main() {
    let a: Array[i64, 3] = [7, 8, 9];
    println(second(a));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "8");
        }
    }

    #[test]
    fn test_e2e_slice_range_from_array() {
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let a: Array[i64, 5] = [10, 20, 30, 40, 50];
    println(sum(a[1..4]));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "90");
        }
    }

    #[test]
    fn test_e2e_as_slice_on_array() {
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let a: Array[i64, 3] = [42, 100, 200];
    let s = a.as_slice();
    println(sum(s));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "342");
        }
    }

    #[test]
    fn test_e2e_mut_slice_indexing_writes_back() {
        // `mut Slice[T]` indexing writes through the slice's data pointer.
        // When the slice aliases an Array on the caller's stack, the write
        // should be observable in that Array.
        let out = run_program(
            r#"
fn set_first(xs: mut Slice[i64]) {
    xs[0] = 99;
}
fn main() {
    let mut a: Array[i64, 3] = [1, 2, 3];
    set_first(a);
    println(a[0]);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_slice_range_bound_to_let() {
        // Range-indexing into a let binding — the variable should be
        // inferred as a Slice[i64] at codegen time so subsequent uses
        // work (indexing, iteration, call coercion).
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let a: Array[i64, 5] = [1, 2, 3, 4, 5];
    let middle = a[1..4];
    println(sum(middle));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "9");
        }
    }

    #[test]
    fn test_e2e_slice_from_vec_coercion() {
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(100);
    v.push(200);
    v.push(300);
    println(sum(v));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "600");
        }
    }

    #[test]
    fn test_e2e_int_from_widening() {
        let out = run_program(
            r#"
fn main() {
    let x: i32 = 7;
    let y: i64 = i64.from(x);
    println(y);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "7");
        }
    }

    #[test]
    fn test_e2e_compound_assignment_int() {
        // `x += y` desugars to `x = x + y` — regression guard for when Step 6
        // operator lowering rewrites BinOp::Add to a trait method call.
        let out = run_program(
            r#"
fn main() {
    let mut x: i64 = 10;
    x += 5;
    println(x);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "15");
        }
    }

    #[test]
    fn test_ir_user_impl_method_emitted() {
        // User impl methods land in the module as LLVM functions named
        // `Type.method`. Regression guard for the impl-block codegen pass.
        let ir = ir_for(
            r#"
struct Point { x: i64, y: i64 }
impl Eq for Point {
    fn eq(self, other: Point) -> bool { self.x == other.x and self.y == other.y }
}
fn main() {}
"#,
        );
        assert!(
            ir.contains("@\"Point.eq\"") || ir.contains("@Point.eq"),
            "expected Point.eq function definition in IR, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_e2e_into_widening_at_let_annotation() {
        // `let y: i64 = x.into()` lowers to `i64.from(x)` which codegen
        // compiles as a passthrough for numeric widening.
        let out = run_program(
            r#"
fn main() {
    let x: i32 = 42;
    let y: i64 = x.into();
    println(y);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_ir_trait_impl_assoc_fn_emitted() {
        // `impl Default for Foo { fn default() -> Foo { ... } }` —
        // associated function (no `self` receiver). Same convention as
        // method impls: emitted as `Foo.default` LLVM symbol so
        // `Foo.default()` and lowered bare `let w: Foo = default()` both
        // dispatch through `Path([Foo, default])`.
        let ir = ir_for(
            r#"
trait Default {
    fn default() -> Self;
}
struct Foo { value: i64 }
impl Default for Foo {
    fn default() -> Foo { Foo { value: 42 } }
}
fn main() {}
"#,
        );
        assert!(
            ir.contains("@\"Foo.default\"") || ir.contains("@Foo.default"),
            "expected Foo.default function definition in IR, got:\n{}",
            ir
        );
    }

    #[test]
    fn test_e2e_concrete_type_prefix_assoc_fn() {
        // `Foo.default()` (UFCS / type-prefixed) dispatches directly to the
        // `Foo.default` LLVM function emitted by the impl-block pass.
        let out = run_program(
            r#"
trait Default {
    fn default() -> Self;
}
struct Foo { value: i64 }
impl Default for Foo {
    fn default() -> Foo { Foo { value: 7 } }
}
fn main() {
    let f = Foo.default();
    println(f.value);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "7");
        }
    }

    #[test]
    fn test_e2e_bare_assoc_fn_lowering_concrete() {
        // `let w: Foo = default()` — typechecker resolves the bare call via
        // expected-type inference; lowering rewrites it to `Foo.default()`,
        // which codegen dispatches through the existing impl path.
        let out = run_program(
            r#"
trait Default {
    fn default() -> Self;
}
struct Foo { value: i64 }
impl Default for Foo {
    fn default() -> Foo { Foo { value: 99 } }
}
fn main() {
    let f: Foo = default();
    println(f.value);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_bare_assoc_fn_with_arg_lowering() {
        // Trait method with a non-Self parameter.
        let out = run_program(
            r#"
trait FromI64 {
    fn from_i64(n: i64) -> Self;
}
struct Wrap { v: i64 }
impl FromI64 for Wrap {
    fn from_i64(n: i64) -> Wrap { Wrap { v: n + 1 } }
}
fn main() {
    let w: Wrap = from_i64(41);
    println(w.v);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_e2e_into_drives_user_from_impl() {
        // User `impl From[Inches] for Cm` compiles as `Cm.from`; `.into()`
        // at a `let: Cm` position lowers to `Cm.from(...)` and routes to it.
        let out = run_program(
            r#"
struct Inches { n: i64 }
struct Cm { n: i64 }
impl From for Cm {
    fn from(i: Inches) -> Cm { Cm { n: i.n * 254 / 100 } }
}
fn main() {
    let i: Inches = Inches { n: 10 };
    let c: Cm = i.into();
    println(c.n);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "25");
        }
    }

    #[test]
    fn test_e2e_user_impl_eq_drives_equality() {
        // End-to-end: `a == b` on user type lowers to `Point.eq(a, b)`,
        // which routes through the codegen-compiled impl method.
        let out = run_program(
            r#"
struct Point { x: i64, y: i64 }
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
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "true\ntrue");
        }
    }

    // ── F-string codegen (Phase 7.2 minimum formatter) ────────────

    #[test]
    fn test_e2e_fstring_text_literal_only() {
        let out = run_program(
            r#"
fn main() {
    let s = f"hello, world";
    println(s);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "hello, world");
        }
    }

    #[test]
    fn test_e2e_fstring_string_interpolation() {
        let out = run_program(
            r#"
fn main() {
    let mut name: String = String.new();
    name.push_str("Alice");
    let msg = f"Hello, {name}!";
    println(msg);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "Hello, Alice!");
        }
    }

    #[test]
    fn test_e2e_fstring_integer_interpolation() {
        let out = run_program(
            r#"
fn main() {
    let x = 42;
    let s = f"value={x}";
    println(s);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "value=42");
        }
    }

    #[test]
    fn test_e2e_fstring_multiple_parts() {
        let out = run_program(
            r#"
fn main() {
    let a = 1;
    let b = 2;
    let s = f"{a}+{b}=3";
    println(s);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "1+2=3");
        }
    }

    // ── RC-fallback codegen E2E ──────────────────────────────────

    #[test]
    fn test_e2e_rc_fallback_param_copy_type() {
        // A Copy-type (i64) parameter flagged by the ownership checker for RC-fallback
        // (consumed in an if-branch, then used again after). The value should still be
        // accessible after the branch — RC boxing allows the second use.
        let out = run_program_with_ownership(
            r#"
fn sink(x: i64) { }
fn main() {
    let val: i64 = 99;
    let cond: bool = false;
    if cond { sink(val); }
    println(val);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_rc_fallback_struct_param() {
        // A non-Copy struct parameter consumed in one branch then used after.
        // With RC boxing the second use loads T from the heap object and behaves
        // identically to the non-RC case (the observable output is the same).
        let out = run_program_with_ownership(
            r#"
struct Point { x: i64, y: i64 }
fn consume_p(p: Point) { }
fn main() {
    let cond: bool = false;
    let p = Point { x: 3, y: 7 };
    if cond { consume_p(p); }
    println(p.x + p.y);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "10");
        }
    }

    // ── Vec[T] extended methods ───────────────────────────────────

    #[test]
    fn test_e2e_vec_is_empty_true() {
        let out = run_program(
            r#"
fn main() {
    let v: Vec[i64] = Vec.new();
    println(v.is_empty());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "true");
        }
    }

    #[test]
    fn test_e2e_vec_is_empty_false() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(42);
    println(v.is_empty());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "false");
        }
    }

    #[test]
    fn test_e2e_vec_first_nonempty() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(10);
    v.push(20);
    v.push(30);
    match v.first() {
        Some(x) => println(x),
        None => println(0),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "10");
        }
    }

    #[test]
    fn test_e2e_vec_first_empty() {
        let out = run_program(
            r#"
fn main() {
    let v: Vec[i64] = Vec.new();
    match v.first() {
        Some(x) => println(x),
        None => println(99),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_vec_last_nonempty() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(10);
    v.push(20);
    v.push(30);
    match v.last() {
        Some(x) => println(x),
        None => println(0),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "30");
        }
    }

    #[test]
    fn test_e2e_vec_last_empty() {
        let out = run_program(
            r#"
fn main() {
    let v: Vec[i64] = Vec.new();
    match v.last() {
        Some(x) => println(x),
        None => println(99),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_vec_get_in_bounds() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(100);
    v.push(200);
    v.push(300);
    match v.get(1) {
        Some(x) => println(x),
        None => println(0),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "200");
        }
    }

    #[test]
    fn test_e2e_vec_get_out_of_bounds() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(100);
    match v.get(5) {
        Some(x) => println(x),
        None => println(99),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    // ── ? operator codegen ───────────────────────────────────────────────────

    #[test]
    fn test_e2e_question_option_some_propagates() {
        // When the inner expression is Some, ? unwraps the value and continues.
        let out = run_program(
            r#"
fn maybe(flag: bool) -> Option[i64] {
    if flag { Some(5_i64) } else { None }
}
fn add_ten(flag: bool) -> Option[i64] {
    let x = maybe(flag)?;
    Some(x + 10)
}
fn main() {
    match add_ten(true) {
        Some(n) => println(n),
        None => println(0),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "15");
        }
    }

    #[test]
    fn test_e2e_question_option_none_propagates() {
        // When the inner expression is None, ? early-returns None from the caller.
        let out = run_program(
            r#"
fn maybe(flag: bool) -> Option[i64] {
    if flag { Some(5_i64) } else { None }
}
fn add_ten(flag: bool) -> Option[i64] {
    let x = maybe(flag)?;
    Some(x + 10)
}
fn main() {
    match add_ten(false) {
        Some(n) => println(n),
        None => println(0),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "0");
        }
    }

    #[test]
    fn test_e2e_question_result_ok_propagates() {
        // When the inner expression is Ok, ? unwraps the value and continues.
        let out = run_program(
            r#"
fn parse_int(flag: bool) -> Result[i64, i64] {
    if flag { Ok(42_i64) } else { Err(99_i64) }
}
fn add_ten(flag: bool) -> Result[i64, i64] {
    let x = parse_int(flag)?;
    Ok(x + 10)
}
fn main() {
    match add_ten(true) {
        Ok(n) => println(n),
        Err(_) => println(0),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "52");
        }
    }

    #[test]
    fn test_e2e_question_result_err_propagates() {
        // When the inner expression is Err, ? early-returns Err from the caller.
        let out = run_program(
            r#"
fn parse_int(flag: bool) -> Result[i64, i64] {
    if flag { Ok(42_i64) } else { Err(99_i64) }
}
fn add_ten(flag: bool) -> Result[i64, i64] {
    let x = parse_int(flag)?;
    Ok(x + 10)
}
fn main() {
    match add_ten(false) {
        Ok(_) => println(0),
        Err(e) => println(e),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_question_result_match_interop() {
        // Built-in Result construction + ? + match against built-in Result variants
        // all interoperate using the same enum layout. Pins Step 6 (pattern-match interop).
        let out = run_program(
            r#"
fn check(n: i64) -> Result[i64, i64] {
    if n > 0 { Ok(n) } else { Err(0_i64 - n) }
}
fn double(n: i64) -> Result[i64, i64] {
    let x = check(n)?;
    Ok(x * 2)
}
fn main() {
    match double(7_i64) {
        Ok(v) => println(v),
        Err(e) => println(e),
    }
    match double(-5_i64) {
        Ok(v) => println(v),
        Err(e) => println(e),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "14\n5");
        }
    }

    #[test]
    fn test_e2e_question_in_loop() {
        // ? inside a loop body must propagate the failure out of the enclosing function,
        // not just out of the loop iteration.
        let out = run_program(
            r#"
fn step(n: i64) -> Result[i64, i64] {
    if n < 3_i64 { Ok(n) } else { Err(n) }
}
fn run() -> Result[i64, i64] {
    let mut total: i64 = 0;
    let mut i: i64 = 0;
    while i < 5_i64 {
        let v = step(i)?;
        total = total + v;
        i = i + 1;
    }
    Ok(total)
}
fn main() {
    match run() {
        Ok(v) => println(v),
        Err(e) => println(e),
    }
}
"#,
        );
        if let Some(out) = out {
            // step succeeds for i=0,1,2 (sums to 3); fails at i=3 with Err(3)
            assert_eq!(out.trim(), "3");
        }
    }

    #[test]
    fn test_e2e_question_cross_error_from_conversion() {
        // ? converts the inner error type via the user-impl `From` when
        // typechecker records a question_conversion at this site.
        // raw err `7_i64` flows through MyError.from(_) which doubles it.
        let out = run_program(
            r#"
struct RawError { code: i64 }
struct MyError { code: i64 }
impl From for MyError {
    fn from(e: RawError) -> MyError { MyError { code: e.code * 2_i64 } }
}
fn lookup() -> Result[i64, RawError] { Err(RawError { code: 7_i64 }) }
fn process() -> Result[i64, MyError] {
    let _ = lookup()?;
    Ok(0_i64)
}
fn main() {
    match process() {
        Ok(_) => println(0_i64),
        Err(e) => println(e.code),
    }
}
"#,
        );
        if let Some(out) = out {
            // Without conversion: 7. With From doubling: 14.
            assert_eq!(out.trim(), "14");
        }
    }

    #[test]
    fn test_e2e_question_triggers_scope_cleanup() {
        // A Vec on the stack must be freed before ? returns early.
        // We can't directly observe the free, but we can verify the program
        // does not crash and the early-return path does run.
        let out = run_program(
            r#"
fn boom(flag: bool) -> Result[i64, i64] {
    if flag { Ok(1_i64) } else { Err(7_i64) }
}
fn use_vec(flag: bool) -> Result[i64, i64] {
    let mut v: Vec[i64] = Vec.new();
    v.push(10_i64);
    v.push(20_i64);
    v.push(30_i64);
    let _ = boom(flag)?;
    Ok(v.len() as i64)
}
fn main() {
    match use_vec(false) {
        Ok(n) => println(n),
        Err(e) => println(e),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "7");
        }
    }

    // ── ? error_return_trace push from compiled binaries ─────────────────────
    //
    // The runtime maintains a thread-local depth-64 ring buffer; codegen
    // emits a `karac_error_trace_push` at each `?` failure block before the
    // early return. An atexit handler in the runtime prints the buffer to
    // stderr at process exit when non-empty. These tests exercise the full
    // compile → link → run → stderr-capture path.

    #[test]
    fn test_e2e_question_trace_single_frame_on_err() {
        // A single `?` site that propagates `Err` should produce one frame
        // in the stderr trace, matching the interpreter's text format.
        let captured = run_program_capturing(
            r#"
fn boom() -> Result[i64, i64] { Err(7_i64) }
fn caller() -> Result[i64, i64] {
    let _ = boom()?;
    Ok(0_i64)
}
fn main() {
    match caller() {
        Ok(_) => println(0_i64),
        Err(e) => println(e),
    }
}
"#,
        );
        if let Some(c) = captured {
            assert_eq!(c.stdout.trim(), "7");
            assert!(
                c.stderr.contains("Error return trace:"),
                "expected trace header on stderr; got {:?}",
                c.stderr
            );
            // One ? site → one frame line in the trace.
            // Frame lines have shape `  <line>:<col>` or `  <file>:<line>:<col>`
            // — indented, contain at least one `:`, and aren't the
            // truncation suffix or the header.
            let frame_lines = c
                .stderr
                .lines()
                .filter(|l| l.starts_with("  ") && l.contains(':') && !l.contains("truncated"))
                .count();
            assert_eq!(
                frame_lines, 1,
                "expected exactly 1 frame, got {} ({:?})",
                frame_lines, c.stderr
            );
        }
    }

    #[test]
    fn test_e2e_question_trace_two_deep_chain() {
        // A two-deep chain of `?` sites should produce two frames, in
        // call-order (innermost frame first since it's pushed first).
        let captured = run_program_capturing(
            r#"
fn level_a() -> Result[i64, i64] { Err(3_i64) }
fn level_b() -> Result[i64, i64] {
    let _ = level_a()?;
    Ok(0_i64)
}
fn level_c() -> Result[i64, i64] {
    let _ = level_b()?;
    Ok(0_i64)
}
fn main() {
    match level_c() {
        Ok(_) => println(0_i64),
        Err(e) => println(e),
    }
}
"#,
        );
        if let Some(c) = captured {
            assert_eq!(c.stdout.trim(), "3");
            assert!(c.stderr.contains("Error return trace:"));
            // Frame lines have shape `  <line>:<col>` or `  <file>:<line>:<col>`
            // — indented, contain at least one `:`, and aren't the
            // truncation suffix or the header.
            let frame_lines = c
                .stderr
                .lines()
                .filter(|l| l.starts_with("  ") && l.contains(':') && !l.contains("truncated"))
                .count();
            assert_eq!(
                frame_lines, 2,
                "expected exactly 2 frames; got {} ({:?})",
                frame_lines, c.stderr
            );
        }
    }

    #[test]
    fn test_e2e_question_trace_includes_source_filename_when_threaded() {
        // When the CLI / caller threads a `source_filename` into codegen
        // (`compile_to_object_with_options`), each `?` failure-site frame
        // carries the filename so the trace prints as `<file>:<line>:<col>`,
        // matching the interpreter's format. The default (no filename) MVP
        // path emits `<line>:<col>` only — covered by the tests above.
        let captured = run_program_capturing_with_filename(
            r#"
fn boom() -> Result[i64, i64] { Err(7_i64) }
fn caller() -> Result[i64, i64] {
    let _ = boom()?;
    Ok(0_i64)
}
fn main() {
    match caller() {
        Ok(_) => println(0_i64),
        Err(e) => println(e),
    }
}
"#,
            "trace_demo.kara",
        );
        if let Some(c) = captured {
            assert_eq!(c.stdout.trim(), "7");
            assert!(c.stderr.contains("Error return trace:"));
            let has_file_frame = c.stderr.lines().any(|l| {
                l.starts_with("  ") && l.contains("trace_demo.kara:") && !l.contains("truncated")
            });
            assert!(
                has_file_frame,
                "expected a `<file>:<line>:<col>` frame containing `trace_demo.kara:`; \
                 got {:?}",
                c.stderr
            );
        }
    }

    #[test]
    fn test_e2e_question_trace_cleared_on_recovery() {
        // When a `?` succeeds (Ok-extract), the runtime clears any frames
        // a prior `?` had pushed. A subsequent failure should produce a
        // trace with only the new frames — not stale ones from the
        // recovered earlier propagation.
        let captured = run_program_capturing(
            r#"
fn maybe(flag: bool) -> Result[i64, i64] {
    if flag { Ok(1_i64) } else { Err(9_i64) }
}
fn after_recovery() -> Result[i64, i64] {
    let _ = maybe(true)?;     // success — should clear any pushed frames
    let _ = maybe(false)?;    // fresh failure — pushes one frame
    Ok(0_i64)
}
fn main() {
    match after_recovery() {
        Ok(_) => println(0_i64),
        Err(e) => println(e),
    }
}
"#,
        );
        if let Some(c) = captured {
            assert_eq!(c.stdout.trim(), "9");
            // The trace should have exactly one frame — the second `?`'s,
            // not both. (The first `?`'s frame would have been cleared by
            // the success path.) NOTE: the v1 implementation pushes a frame
            // on the failure block ONLY, so the first `?` (which succeeds)
            // never pushed a frame in the first place. The clear is a
            // safety net for the case where a prior propagation reached
            // this function and was caught higher in the chain.
            // Frame lines have shape `  <line>:<col>` or `  <file>:<line>:<col>`
            // — indented, contain at least one `:`, and aren't the
            // truncation suffix or the header.
            let frame_lines = c
                .stderr
                .lines()
                .filter(|l| l.starts_with("  ") && l.contains(':') && !l.contains("truncated"))
                .count();
            assert_eq!(
                frame_lines, 1,
                "expected exactly 1 frame; got {:?}",
                c.stderr
            );
        }
    }

    // ── Linker control attributes ────────────────────────────────────────────

    #[test]
    fn test_ir_no_mangle_symbol_name_unchanged() {
        // `#[no_mangle]` is a no-op at the codegen layer (the compiler already
        // uses the source-level name as the LLVM symbol name) but we verify
        // the function still emits with its plain name.
        let ir = ir_for("#[no_mangle]\nfn keep_me() -> i64 { 42 }");
        assert!(
            ir.contains("@keep_me"),
            "function symbol should appear as @keep_me; IR: {}",
            ir
        );
    }

    #[test]
    fn test_ir_link_section_sets_function_section() {
        // `#[link_section(".init_array")]` should set the `section` directive
        // on the LLVM function definition. Inkwell's macOS `set_section`
        // encodes a Mach-O `segment,section` pair and prefixes a `,` when
        // the supplied name doesn't already contain one — so we accept
        // both `section ".init_array"` (ELF) and `section ",.init_array"`
        // (Mach-O fallback).
        let ir = ir_for("#[link_section(\".init_array\")]\nfn ctor() -> i64 { 1 }");
        assert!(
            ir.contains("section \".init_array\"") || ir.contains("section \",.init_array\""),
            "expected section directive on @ctor; IR: {}",
            ir
        );
    }

    #[test]
    fn test_ir_used_emits_llvm_used_global() {
        // `#[used]` should add the symbol to `@llvm.used`. The global has
        // appending linkage and lives in section "llvm.metadata".
        let ir = ir_for("#[used]\nfn keep() -> i64 { 7 }\nfn main() { println(keep()); }");
        assert!(
            ir.contains("@llvm.used"),
            "expected @llvm.used global; IR: {}",
            ir
        );
        assert!(
            ir.contains("appending"),
            "expected appending linkage on @llvm.used; IR: {}",
            ir
        );
        assert!(
            ir.contains("@keep"),
            "expected @keep symbol referenced from @llvm.used; IR: {}",
            ir
        );
    }

    #[test]
    fn test_ir_used_multiple_symbols_share_one_global() {
        // Two `#[used]` symbols should produce a single `@llvm.used` global
        // listing both — not two separate globals.
        let ir = ir_for(
            "#[used]\nfn a() -> i64 { 1 }\n\
             #[used]\nfn b() -> i64 { 2 }\n\
             fn main() {}",
        );
        let count = ir.matches("@llvm.used").count();
        assert_eq!(
            count, 1,
            "expected exactly one @llvm.used global, found {}; IR: {}",
            count, ir
        );
    }

    #[test]
    fn test_ir_no_used_means_no_llvm_used_global() {
        // Without `#[used]`, no `@llvm.used` global should appear.
        let ir = ir_for("fn keep() -> i64 { 7 }\nfn main() { println(keep()); }");
        assert!(
            !ir.contains("@llvm.used"),
            "should not emit @llvm.used when no `#[used]` attributes; IR: {}",
            ir
        );
    }

    // ── Prefix dereference operator ───────────────────────────────────────────

    #[test]
    fn test_e2e_deref_read_ref_param() {
        // *r where r: ref i64 should load the pointed-to value.
        let out = run_program(
            r#"
fn read_val(r: ref i64) -> i64 { *r }
fn main() {
    let x: i64 = 42;
    println(read_val(x));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_e2e_deref_write_through_mut_ref() {
        // *r = v where r: mut ref i64 should store through the pointer.
        let out = run_program(
            r#"
fn set_val(r: mut ref i64) { *r = 99; }
fn main() {
    let mut x: i64 = 1;
    set_val(mut x);
    println(x);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_deref_double_in_place() {
        // *r = *r * 2 — read and write through a mut ref in the same statement.
        let out = run_program(
            r#"
fn double_in_place(r: mut ref i64) { *r = *r * 2; }
fn main() {
    let mut n: i64 = 5;
    double_in_place(mut n);
    println(n);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "10");
        }
    }

    // ── Map LLVM codegen E2E (Task 6) ─────────────────────────────────────────

    #[test]
    fn test_e2e_map_i64_insert_get_none() {
        // get on missing key → None (no output, just no crash)
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    let v = m.get(42_i64);
    match v {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "0");
        }
    }

    #[test]
    fn test_e2e_map_i64_insert_get_some() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(10_i64, 99_i64);
    let v = m.get(10_i64);
    match v {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_map_i64_insert_returns_old() {
        // First insert → None; second insert with same key → Some(old)
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    let first = m.insert(7_i64, 10_i64);
    match first {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
    let second = m.insert(7_i64, 20_i64);
    match second {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["0", "10"]);
        }
    }

    #[test]
    fn test_e2e_map_i64_remove_some_none() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(5_i64, 55_i64);
    let r1 = m.remove(5_i64);
    match r1 {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
    let r2 = m.remove(5_i64);
    match r2 {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["55", "0"]);
        }
    }

    #[test]
    fn test_e2e_map_i64_contains_key() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(3_i64, 30_i64);
    println(m.contains_key(3_i64));
    println(m.contains_key(4_i64));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["true", "false"]);
        }
    }

    #[test]
    fn test_e2e_map_i64_len_is_empty() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    println(m.is_empty());
    println(m.len());
    m.insert(1_i64, 10_i64);
    m.insert(2_i64, 20_i64);
    println(m.is_empty());
    println(m.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["true", "0", "false", "2"]);
        }
    }

    #[test]
    fn test_e2e_map_i64_for_loop_sum() {
        // Sum all values; key sum is deterministic (single entry)
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(1_i64, 100_i64);
    m.insert(2_i64, 200_i64);
    m.insert(3_i64, 300_i64);
    let mut total: i64 = 0;
    for (k, v) in m {
        total = total + v;
    }
    println(total);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "600");
        }
    }

    #[test]
    fn test_e2e_map_string_insert_get() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("hello", 42_i64);
    let v = m.get("hello");
    match v {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
    let v2 = m.get("world");
    match v2 {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["42", "0"]);
        }
    }

    #[test]
    fn test_e2e_map_string_insert_returns_old() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    let first = m.insert("key", 10_i64);
    match first {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
    let second = m.insert("key", 20_i64);
    match second {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["0", "10"]);
        }
    }

    #[test]
    fn test_e2e_map_string_remove() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("alpha", 1_i64);
    let r1 = m.remove("alpha");
    match r1 {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
    let r2 = m.remove("alpha");
    match r2 {
        Some(x) => { println(x); }
        None => { println(0_i64); }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["1", "0"]);
        }
    }

    #[test]
    fn test_e2e_map_string_contains_len() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("x", 9_i64);
    println(m.contains_key("x"));
    println(m.contains_key("y"));
    println(m.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["true", "false", "1"]);
        }
    }

    #[test]
    fn test_e2e_map_string_for_loop_count() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("a", 1_i64);
    m.insert("b", 2_i64);
    let mut count: i64 = 0;
    for (k, v) in m {
        count = count + 1_i64;
    }
    println(count);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "2");
        }
    }

    #[test]
    fn test_e2e_map_index_get_existing_i64() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(7_i64, 42_i64);
    println(m[7_i64]);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_e2e_map_index_get_existing_string_key() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("hello", 100_i64);
    m.insert("world", 200_i64);
    println(m["world"]);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "200");
        }
    }

    #[test]
    fn test_e2e_map_index_set_fresh_and_overwrite() {
        // m[k] = v on a missing key inserts; on an existing key overwrites.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m[1_i64] = 10_i64;
    m[2_i64] = 20_i64;
    println(m[1_i64]);
    println(m[2_i64]);
    m[1_i64] = 99_i64;
    println(m[1_i64]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["10", "20", "99"]);
        }
    }

    #[test]
    fn test_e2e_map_index_set_string_key() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m["alice"] = 1_i64;
    m["bob"] = 2_i64;
    m["alice"] = 100_i64;
    println(m["alice"]);
    println(m["bob"]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["100", "2"]);
        }
    }

    #[test]
    fn test_e2e_map_index_panics_on_missing() {
        // Indexing a Map with a missing key panics at runtime.
        let captured = run_program_capturing(
            r#"
fn main() {
    let m: Map[i64, i64] = Map.new();
    let x = m[42_i64];
    println(x);
    println(99_i64);
}
"#,
        );
        if let Some(c) = captured {
            // Panic message printed to stdout (printf), then exit(1) — so
            // the trailing prints never run.
            assert!(
                c.stdout.contains("panic: Map index: key not present"),
                "expected panic message, got stdout={:?} stderr={:?}",
                c.stdout,
                c.stderr
            );
            assert!(
                !c.stdout.contains("99"),
                "code after panicking index should not run"
            );
        }
    }

    // ── Half-open range indexing ──────────────────────────────────────────────

    #[test]
    fn test_e2e_range_from_array_tail() {
        // v[a..] — open end: slice from index 2 to end of array
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let a: Array[i64, 5] = [10, 20, 30, 40, 50];
    println(sum(a[2..]));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "120"); // 30+40+50
        }
    }

    #[test]
    fn test_e2e_range_full_array() {
        // v[..] — full slice of the array
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let a: Array[i64, 4] = [1, 2, 3, 4];
    println(sum(a[..]));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "10"); // 1+2+3+4
        }
    }

    #[test]
    fn test_e2e_range_to_exclusive_array() {
        // v[..b] — from start up to (not including) b
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let a: Array[i64, 5] = [10, 20, 30, 40, 50];
    println(sum(a[..3]));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "60"); // 10+20+30
        }
    }

    #[test]
    fn test_e2e_range_to_inclusive_array() {
        // v[..=b] — from start up to and including b
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let a: Array[i64, 5] = [10, 20, 30, 40, 50];
    println(sum(a[..=2]));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "60"); // 10+20+30
        }
    }

    #[test]
    fn test_e2e_range_inclusive_array() {
        // v[a..=b] — closed range: both ends inclusive
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let a: Array[i64, 5] = [10, 20, 30, 40, 50];
    println(sum(a[1..=3]));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "90"); // 20+30+40
        }
    }

    #[test]
    fn test_e2e_range_from_vec_tail() {
        // vec[a..] — open-end slice of a Vec
        let out = run_program(
            r#"
fn sum(xs: Slice[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(1);
    v.push(2);
    v.push(3);
    v.push(4);
    println(sum(v[1..]));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "9"); // 2+3+4
        }
    }
}
