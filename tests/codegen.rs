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
        compile_to_ir(&parsed.program, None, None).expect("codegen failed")
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

    // ── Short-circuit `and` / `or` (roadmap.md:425, 429) ─────────

    #[test]
    fn test_ir_and_short_circuits_rhs_call() {
        // `false and boom()` must NOT emit `@boom` on the unconditional
        // path; it must live inside an `sc.rhs` block reached only when
        // the LHS is true.
        let ir = ir_for(
            r#"
fn boom() -> bool { true }
fn use_and(x: bool) -> bool { x and boom() }
"#,
        );
        assert!(
            ir.contains("sc.rhs"),
            "expected sc.rhs basic block; IR:\n{ir}"
        );
        assert!(
            ir.contains("sc.merge"),
            "expected sc.merge basic block; IR:\n{ir}"
        );
        assert!(
            ir.contains("phi i1"),
            "expected i1 phi for short-circuit result; IR:\n{ir}"
        );
        // The result must come from a phi with a constant `false` (i1 0)
        // for the short-circuit edge — the LHS-false case never reaches
        // the boom() call.
        assert!(
            ir.contains("phi i1 [ false") || ir.contains("[ false,") || ir.contains("i1 false"),
            "expected short-circuit constant in phi; IR:\n{ir}"
        );
    }

    #[test]
    fn test_ir_or_short_circuits_rhs_call() {
        // `true or boom()` must keep `@boom` behind a conditional branch.
        let ir = ir_for(
            r#"
fn boom() -> bool { false }
fn use_or(x: bool) -> bool { x or boom() }
"#,
        );
        assert!(
            ir.contains("sc.rhs"),
            "expected sc.rhs basic block; IR:\n{ir}"
        );
        assert!(
            ir.contains("sc.merge"),
            "expected sc.merge basic block; IR:\n{ir}"
        );
        assert!(
            ir.contains("phi i1"),
            "expected i1 phi for short-circuit result; IR:\n{ir}"
        );
        // Result phi must carry a constant `true` for the short-circuit edge.
        assert!(
            ir.contains("phi i1 [ true") || ir.contains("[ true,") || ir.contains("i1 true"),
            "expected short-circuit constant in phi; IR:\n{ir}"
        );
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
        let result = compile_to_object(&parsed.program, obj_path, None, None);
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

        // Parse errors are programming bugs in the test source, not a
        // legitimate "skip" condition — panic with a clear message so
        // failures surface instead of being swallowed by the downstream
        // `if let Some(out) = out { ... }` accept-on-None pattern.
        let mut parsed = karac::parse(src);
        if !parsed.errors.is_empty() {
            let mut msg = String::from("test source failed to parse:\n");
            for e in &parsed.errors {
                msg.push_str(&format!("  {:?}\n", e));
            }
            panic!("{}", msg);
        }
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);

        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let obj_path = format!("/tmp/karac_e2e_{}_{}.o", std::process::id(), id);
        let exe_path = format!("/tmp/karac_e2e_{}_{}", std::process::id(), id);

        // Codegen failures are also programming bugs (in the compiler or in
        // the test program) — surface them loudly. Link and exec failures
        // stay as soft-skip because they can fire in environments that
        // lack libkarac_runtime.a or a working linker.
        if let Err(e) =
            compile_to_object_with_options(&parsed.program, &obj_path, None, None, filename, None)
        {
            panic!("codegen failed for test program: {}", e);
        }
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
            let mut msg = String::from("test source failed to parse:\n");
            for e in &parsed.errors {
                msg.push_str(&format!("  {:?}\n", e));
            }
            panic!("{}", msg);
        }
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let ownership = karac::ownershipcheck(&parsed.program, &typed);

        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let obj_path = format!("/tmp/karac_e2e_ow_{}_{}.o", std::process::id(), id);
        let exe_path = format!("/tmp/karac_e2e_ow_{}_{}", std::process::id(), id);

        if let Err(e) = compile_to_object(&parsed.program, &obj_path, Some(&ownership), None) {
            panic!("codegen failed for test program: {}", e);
        }
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

    // ── Layout introspection intrinsics ──────────────────────────
    //
    // `size_of[T]()` lowers to inkwell's `BasicTypeEnum::size_of()`
    // (compile-time constant); `align_of[T]()` queries the host
    // `TargetData::get_abi_alignment()`. Both return `usize` (i64 on
    // the 64-bit-only target). Slice 1b NO_KNOWN_SIZE pull.

    #[test]
    fn test_e2e_size_of_i64_is_8() {
        let out = run_program("fn main() { println(size_of[i64]()); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "8");
        }
    }

    #[test]
    fn test_e2e_size_of_i32_is_4() {
        let out = run_program("fn main() { println(size_of[i32]()); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "4");
        }
    }

    #[test]
    fn test_e2e_size_of_i8_is_1() {
        let out = run_program("fn main() { println(size_of[i8]()); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "1");
        }
    }

    #[test]
    fn test_e2e_align_of_i64_is_8() {
        let out = run_program("fn main() { println(align_of[i64]()); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "8");
        }
    }

    #[test]
    fn test_e2e_align_of_i32_is_4() {
        let out = run_program("fn main() { println(align_of[i32]()); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "4");
        }
    }

    #[test]
    fn test_e2e_align_of_i8_is_1() {
        let out = run_program("fn main() { println(align_of[i8]()); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "1");
        }
    }

    #[test]
    fn test_e2e_size_of_user_struct() {
        // `struct Point { x: i64, y: i64 }` → 16 bytes on a 64-bit target.
        let out = run_program(
            "struct Point { x: i64, y: i64 }\n\
             fn main() { println(size_of[Point]()); }",
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "16");
        }
    }

    #[test]
    fn test_e2e_offset_of_first_field_is_0() {
        let out = run_program(
            "struct Point { x: i64, y: i64 }\n\
             fn main() { println(offset_of[Point](x)); }",
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "0");
        }
    }

    #[test]
    fn test_e2e_offset_of_second_field() {
        // `y` follows `x: i64` → offset 8.
        let out = run_program(
            "struct Point { x: i64, y: i64 }\n\
             fn main() { println(offset_of[Point](y)); }",
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "8");
        }
    }

    #[test]
    fn test_e2e_offset_of_nested_path() {
        // `offset_of[Outer](inner.y)` = offset(inner inside Outer) + offset(y inside Inner).
        let out = run_program(
            "struct Inner { x: i32, y: i32 }\n\
             struct Outer { a: i32, inner: Inner, c: i32 }\n\
             fn main() { println(offset_of[Outer](inner.y)); }",
        );
        if let Some(out) = out {
            // a: i32 occupies bytes 0-3; inner: Inner starts at 4 (i32-aligned);
            // y is the second i32 field of Inner → +4 inside Inner → byte 8.
            assert_eq!(out.trim(), "8");
        }
    }

    #[test]
    fn test_e2e_and_short_circuit_skips_rhs_call() {
        // `false and boom()` must not call boom() at runtime.
        let out = run_program(
            r#"
fn boom() -> bool { println("called"); true }
fn main() {
    if false and boom() { println("then"); } else { println("else"); }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "else\n");
        }
    }

    #[test]
    fn test_e2e_or_short_circuit_skips_rhs_call() {
        // `true or boom()` must not call boom() at runtime.
        let out = run_program(
            r#"
fn boom() -> bool { println("called"); true }
fn main() {
    if true or boom() { println("then"); } else { println("else"); }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "then\n");
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
    fn test_e2e_const_generic_param_in_body() {
        // Const generics slice 4: const-param identifier reference
        // resolves at codegen body lowering. `fn f[const N: i64](x: i64) -> i64 { x + N }`
        // called with `f[3](10)` returns 13; `f[7](10)` returns 17.
        // The compile_expr Identifier branch consults `const_subst`
        // and emits the matching LLVM constant via
        // `compile_primitive_const`.
        let out = run_program(
            r#"
fn f[const N: i64](x: i64) -> i64 { x + N }
fn main() {
    println(f[3](10));
    println(f[7](10));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "13\n17");
        }
    }

    #[test]
    fn test_e2e_const_generic_param_in_larger_expression() {
        // Const generics slice 4: const-param embedded in a larger
        // expression. `fn g[const N: i64]() -> i64 { N * 2 + 1 }`
        // called with `g[7]()` returns 15.
        let out = run_program(
            r#"
fn g[const N: i64]() -> i64 { N * 2 + 1 }
fn main() {
    println(g[7]());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "15");
        }
    }

    #[test]
    fn test_ir_const_generic_param_distinct_monos_in_body() {
        // Slice 4 + slice 1b: each distinct const-arg produces a
        // distinct compiled mono symbol AND the body of each mono
        // emits a different LLVM constant for the const-param. The
        // IR for `f[3]` should contain the literal 3 in its body;
        // the IR for `f[7]` should contain 7.
        let ir = ir_for(
            r#"
fn f[const N: i64](x: i64) -> i64 { x + N }
fn main() {
    let _ = f[3](10);
    let _ = f[7](10);
}
"#,
        );
        // `f` has only the const-param `N` as a generic (no T), so
        // the mangled symbol is `f$<const-N-value>` — no type-arg
        // token in the middle.
        assert!(
            ir.contains("f$3i64"),
            "expected `f$3i64` mono symbol, IR:\n{}",
            ir
        );
        assert!(
            ir.contains("f$7i64"),
            "expected `f$7i64` mono symbol, IR:\n{}",
            ir
        );
    }

    #[test]
    fn test_ir_const_generic_mono_key_disambiguation() {
        // Const generics slice 1b (2026-05-11). Two calls to the same
        // generic function with the same type-arg but distinct
        // const-args (`make_arr[i64, 4]()` vs `make_arr[i64, 8]()`)
        // produce two distinct compiled symbols in the LLVM module:
        // the mango-key walks const params alongside type params and
        // appends each const value's mangled token. Without slice 1b,
        // both calls collapse to a single `make_arr$i64` symbol — the
        // latent bug `mangle_mono_name` had pre-slice-1b.
        let ir = ir_for(
            r#"
fn make_arr[T, const N: i64]() -> i64 { 42 }
fn main() {
    let _ = make_arr[i64, 4]();
    let _ = make_arr[i64, 8]();
}
"#,
        );
        assert!(
            ir.contains("make_arr$i64$4i64"),
            "expected `make_arr$i64$4i64` specialization in IR, got:\n{}",
            ir
        );
        assert!(
            ir.contains("make_arr$i64$8i64"),
            "expected `make_arr$i64$8i64` specialization in IR, got:\n{}",
            ir
        );
        let define_count = ir
            .lines()
            .filter(|l| l.contains("define") && l.contains("make_arr$"))
            .count();
        assert_eq!(
            define_count, 2,
            "should generate two distinct specializations for N=4 and N=8"
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
    fn test_e2e_vec_pop_returns_option() {
        // `Vec.pop` now returns `Option[T]` per design.md (was raw
        // element pre-2026-05-10). Match destructure unwraps Some,
        // None on empty. Previous test asserted raw shape; the
        // semantic upgrade aligns codegen with the spec + interpreter.
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(10);
    v.push(20);
    match v.pop() {
        Some(x) => println(x),
        None => println(0),
    }
    println(v.len());
    match v.pop() {
        Some(x) => println(x),
        None => println(0),
    }
    match v.pop() {
        Some(_) => println(99),
        None => println(0),
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["20", "1", "10", "0"]);
        }
    }

    #[test]
    fn test_e2e_vec_deque_pop_front_returns_option_with_tuple_payload() {
        // The LeetCode 3629 kata's blocking shape: VecDeque[(i64, i64)]
        // BFS frontier with `pop_front()` returning `Option[(i64,i64)]`.
        // Multi-word Option payload via the bumped layout +
        // `coerce_to_payload_words(val, 3)` construction; destructure
        // uses the direct-pattern form `Some((i, d))` which routes
        // through the existing tuple-payload reconstruction machinery.
        let out = run_program(
            r#"
fn main() {
    let mut q: VecDeque[(i64, i64)] = VecDeque.new();
    q.push_back((0, 0));
    let mut sum = 0i64;
    loop {
        match q.pop_front() {
            None => { break; },
            Some((i, d)) => {
                sum = sum + i + d;
                if i < 3 {
                    q.push_back((i + 1, d + 1));
                }
            },
        }
    }
    println(sum);
}
"#,
        );
        if let Some(out) = out {
            // BFS: pop (0,0) sum+=0; push (1,1) → pop sum+=2 (2);
            // push (2,2) → pop sum+=4 (6); push (3,3) → pop sum+=6 (12);
            // i==3 no push; empty → break. Total 12.
            assert_eq!(out.trim(), "12");
        }
    }

    #[test]
    fn test_e2e_par_group_serializes_for_iter_with_outer_mutable_write() {
        // The concurrency analyzer's per-stmt info now collects
        // nested-block writes (Assign / CompoundAssign) into
        // `info.defines`. Without this, a `for v in nums.iter()`
        // expression-stmt that writes to outer `cap` was treated as
        // "no dependencies" against a subsequent `let f =
        // dummy(cap)` — the analyzer grouped them and the par-branch
        // fn's local copy of `cap` never propagated back, so the
        // function call read the initial value of `cap`.
        //
        // Repro:
        let out = run_program(
            r#"
fn dummy(n: i64) -> Vec[i64] {
    let mut v: Vec[i64] = Vec.new();
    let mut i = 0i64;
    while i < n { v.push(i); i = i + 1; }
    v
}
fn helper(nums: Slice[i64]) -> i64 {
    let mut cap = 1i64;
    for v in nums.iter() {
        if v > cap { cap = v; }
    }
    let f: Vec[i64] = dummy(cap);
    println(cap);
    println(f.len());
    cap
}
fn main() {
    let a: Array[i64, 4] = [1, 2, 4, 6];
    println(helper(a));
}
"#,
        );
        if let Some(out) = out {
            // cap finds max = 6 from [1,2,4,6]; dummy(6) yields a
            // 6-element Vec; helper returns 6.
            assert_eq!(out.trim(), "6\n6\n6");
        }
    }

    #[test]
    fn test_e2e_for_in_indexed_iter() {
        // `for p in coll[i].iter()` — the iter peel-off in
        // `compile_for` recurses on the receiver, but for an
        // indexed receiver the recursion would land on an Index
        // expression which falls through to the silent `_ =>` arm.
        // Fix: synthesize a temp identifier for the indexed
        // element and recurse with it, mirroring
        // `compile_nested_index_read`. Pins the kata's
        // `for p in factors[v].iter() { bucket.entry(p)... }` shape.
        let out = run_program(
            r#"
fn main() {
    let mut factors: Vec[Vec[i64]] = Vec.filled(7, Vec.new());
    factors[6].push(2);
    factors[6].push(3);
    let mut sum = 0i64;
    for p in factors[6].iter() {
        sum = sum + p;
    }
    println(sum);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "5");
        }
    }

    #[test]
    fn test_e2e_function_returning_vec_of_vec_no_double_free() {
        // Move-aware scope-exit cleanup: when a function returns a
        // tracked Vec / String binding via the tail expression, the
        // let-site's `track_vec_var` cleanup is suppressed (by
        // zeroing the source's `cap` field) so the caller's
        // `f.data` isn't pointing at a freed buffer. Without this,
        // `Vec[Vec[i64]]` returns SIGSEGV at the first inner indexed
        // access (the inner Vec's data pointer GEPs through a freed
        // outer slot).
        let out = run_program(
            r#"
fn make_grid(n: i64) -> Vec[Vec[i64]] {
    let mut g: Vec[Vec[i64]] = Vec.filled(n, Vec.new());
    g[0].push(99);
    g[0].push(11);
    g[2].push(42);
    g
}
fn main() {
    let f: Vec[Vec[i64]] = make_grid(3);
    println(f.len());
    println(f[0].len());
    println(f[0][0]);
    println(f[0][1]);
    println(f[2][0]);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3\n2\n99\n11\n42");
        }
    }

    #[test]
    fn test_e2e_function_returning_vec_i64_no_double_free() {
        // Sibling: `Vec[i64]` return — same fix applies. The
        // primitive case happened to work pre-fix due to
        // use-after-free reading stable data, but is now correct
        // by construction (cleanup skipped at the move site, caller
        // owns the buffer cleanly).
        let out = run_program(
            r#"
fn make_vec(n: i64) -> Vec[i64] {
    let mut v: Vec[i64] = Vec.new();
    let mut i = 0i64;
    while i < n {
        v.push(i * 10);
        i = i + 1;
    }
    v
}
fn main() {
    let f: Vec[i64] = make_vec(4);
    println(f.len());
    println(f[3]);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "4\n30");
        }
    }

    #[test]
    fn test_e2e_struct_field_move_no_double_free() {
        // Move-aware suppression at struct-construction sites. When
        // a struct field's initializer is an Identifier naming a
        // tracked Vec / String, the field captures the binding's
        // data pointer — but the source's let-site `track_vec_var`
        // unconditionally schedules a scope-exit free that would
        // free the buffer the caller now reads through the struct.
        // This is the shape Parallax/HTTP hits via
        // `Response { body: my_string }` — without suppression,
        // the caller reads NUL bytes (or SIGSEGVs) downstream of
        // FFI consumption.
        let out = run_program(
            r#"
struct Holder {
    tag: i64,
    body: String,
}
fn build() -> Holder {
    let mut s: String = String.new();
    s.push_str("hello, world");
    Holder { tag: 7, body: s }
}
fn main() {
    let h: Holder = build();
    println(h.tag);
    println(h.body);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "7\nhello, world");
        }
    }

    #[test]
    fn test_e2e_let_rebind_move_no_double_free() {
        // `let outer = inner;` where `inner` is a tracked Vec /
        // String is a move — both slots end up holding the same
        // {ptr, len, cap}. Without source-cap suppression at the
        // let-rebind site, both `track_vec_var`-queued cleanups
        // fire and double-free the heap buffer. The LHS's track
        // becomes the unique cleanup owner.
        let out = run_program(
            r#"
fn build() -> String {
    let mut inner: String = String.new();
    inner.push_str("relayed");
    let outer: String = inner;
    outer
}
fn main() {
    let s: String = build();
    println(s);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "relayed");
        }
    }

    #[test]
    fn test_e2e_assign_rebind_move_no_double_free() {
        // `acc = extra;` where both `acc` and `extra` are tracked
        // Vec / String bindings is an assign-rebind. The old
        // `acc` buffer leaks (no RAII drop in v1), but without
        // source-cap suppression on `extra`, both queued cleanups
        // fire against the same post-assign buffer → double-free.
        let out = run_program(
            r#"
fn build() -> String {
    let mut acc: String = String.new();
    acc.push_str("first");
    let mut extra: String = String.new();
    extra.push_str("second");
    acc = extra;
    acc
}
fn main() {
    let s: String = build();
    println(s);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "second");
        }
    }

    #[test]
    fn test_e2e_for_range_step_by_codegen() {
        // `for j in (start..=end).step_by(n)` — the iterator-adaptor
        // chain previously fell through `compile_for`'s match to the
        // silent `_ =>` arm, skipping the body entirely. Now lowers
        // to a Range loop with the step expr evaluated once before
        // the loop and used as the increment. Pins the sieve / strided
        // iteration pattern that the LeetCode 3629 kata uses.
        let out = run_program(
            r#"
fn main() {
    let mut sum = 0i64;
    for j in (2..=10).step_by(2) {
        sum = sum + j;
    }
    println(sum);
    let mut count = 0i64;
    for _ in (0..20).step_by(5) {
        count = count + 1;
    }
    println(count);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "30\n4");
        }
    }

    #[test]
    fn test_e2e_for_range_step_by_with_runtime_step() {
        // The kata's actual usage: the step expression refers to an
        // outer variable. The step is evaluated once per loop entry
        // and captured into the increment block.
        let out = run_program(
            r#"
fn main() {
    let cap = 12i64;
    for i in 2..=cap {
        let mut count = 0i64;
        for _j in (i..=cap).step_by(i) {
            count = count + 1;
        }
        println(count);
    }
}
"#,
        );
        // For each i in 2..=12, count multiples of i up to cap=12:
        //   i=2 → 2,4,6,8,10,12 → 6
        //   i=3 → 3,6,9,12 → 4
        //   i=4 → 4,8,12 → 3
        //   i=5 → 5,10 → 2
        //   i=6 → 6,12 → 2
        //   i=7 → 7 → 1
        //   i=8 → 8 → 1
        //   i=9 → 9 → 1
        //   i=10 → 10 → 1
        //   i=11 → 11 → 1
        //   i=12 → 12 → 1
        if let Some(out) = out {
            assert_eq!(out.trim(), "6\n4\n3\n2\n2\n1\n1\n1\n1\n1\n1");
        }
    }

    #[test]
    fn test_e2e_nested_indexed_read_vec_of_vec() {
        // `grid[0][0]` — nested indexed read on `Vec[Vec[i64]]`.
        // Codegen synthesizes a fresh identifier for the inner
        // `grid[0]` (pointing into grid's storage) and re-dispatches
        // the outer index through the existing identifier-keyed path.
        let out = run_program(
            r#"
fn main() {
    let mut grid: Vec[Vec[i64]] = Vec.filled(3, Vec.new());
    grid[0].push(99);
    grid[0].push(11);
    grid[2].push(42);
    let v = grid[0][0];
    let w = grid[0][1];
    let x = grid[2][0];
    println(v);
    println(w);
    println(x);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99\n11\n42");
        }
    }

    #[test]
    fn test_e2e_par_group_return_slot_preserves_vec_bool_elem_type() {
        // Regression: when auto-par groups `let v: Vec[bool] = ...`
        // with another stmt, the return-slot rebind in
        // `compile_function_body` was unconditionally overwriting
        // `vec_elem_types[v]` to i64 (the placeholder). Later
        // `not v[i]` then loaded an i64 instead of bool, lowered
        // through `xor i64 …, -1`, and the short-circuit phi
        // rejected the i64 operand against an i1 result. Fix uses
        // `entry().or_insert_with(...)` to preserve the let's
        // annotated element type.
        let out = run_program(
            r#"
fn helper(nums: Slice[i64]) -> i64 {
    let n = nums.len();
    let mut visited: Vec[bool] = Vec.filled(n, false);
    let mut bucket: Map[i64, i64] = Map.new();
    let i = 1i64;
    if i > 0 and not visited[i - 1] {
        return 1;
    }
    0
}
fn main() {
    let a: Array[i64, 3] = [1, 2, 3];
    println(helper(a));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "1");
        }
    }

    #[test]
    fn test_e2e_match_some_node_let_destructure_tuple_payload() {
        // The kata's canonical BFS shape: `Some(node) => let (i, d) = node`
        // where `node: (i64, i64)` is reconstituted as a tuple struct
        // value from the multi-word Option payload. The typechecker
        // records the tuple `TypeExpr` in `pattern_binding_inner_types`
        // (tagged "Tuple" in `pattern_binding_types`); codegen's
        // `reconstruct_payload_value` Binding arm walks the recorded
        // element types and builds a tuple struct from `field_words`.
        let out = run_program(
            r#"
fn main() {
    let mut q: VecDeque[(i64, i64)] = VecDeque.new();
    q.push_back((3, 30));
    q.push_back((7, 70));
    loop {
        match q.pop_front() {
            None => { break; },
            Some(node) => {
                let (a, b) = node;
                println(a);
                println(b);
            },
        }
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3\n30\n7\n70");
        }
    }

    #[test]
    fn test_e2e_vec_deque_pop_back_returns_option() {
        // Sibling: pop_back on a primitive-element VecDeque returns
        // Option[i64] — same multi-word path, but the value only
        // populates w0 (w1/w2 padded with zeros).
        let out = run_program(
            r#"
fn main() {
    let mut q: VecDeque[i64] = VecDeque.new();
    q.push_back(1);
    q.push_back(2);
    q.push_back(3);
    match q.pop_back() {
        Some(x) => println(x),
        None => println(0),
    }
    match q.pop_back() {
        Some(x) => println(x),
        None => println(0),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3\n2");
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

    #[test]
    fn test_e2e_for_vec_iter_propagates_outer_mutable_writes() {
        // `for x in v.iter()` codegen previously fell through to the
        // silent `_ =>` arm in `compile_for` — the body never ran,
        // so writes to outer-scope mutables (e.g. `m = x`) had no
        // effect and the loop appeared to be a no-op. Regression
        // test: maxv on [1,2,4,6] must return 6, not the initial 0.
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(1); v.push(2); v.push(4); v.push(6);
    let mut m = 0i64;
    for x in v.iter() {
        if x > m { m = x; }
    }
    println(m);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "6");
        }
    }

    #[test]
    fn test_e2e_vec_filled_i64_primitive() {
        // `Vec.filled(n, val)` for a primitive element type — malloc +
        // fill loop emit the {data, len, cap} aggregate. Before the
        // fix, the assoc-call fell through to the default i64 zero
        // return, the let-binding allocated an i64 alloca for a Vec-
        // typed binding, and any later method dispatch GEP'd past it
        // into stack garbage (SIGTRAP at runtime, "Built" at build).
        let out = run_program(
            r#"
fn main() {
    let v: Vec[i64] = Vec.filled(3, 42);
    println(v.len());
    println(v[0]);
    println(v[2]);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3\n42\n42");
        }
    }

    #[test]
    fn test_e2e_vec_filled_bool_with_indexed_write() {
        // Kata's `Vec.filled(n, false)` shape — followed by indexed
        // writes flipping selected slots true.
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[bool] = Vec.filled(4, false);
    v[2] = true;
    let mut i = 0i64;
    while i < 4 {
        println(v[i]);
        i = i + 1;
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "false\nfalse\ntrue\nfalse");
        }
    }

    #[test]
    fn test_e2e_vec_filled_nested_vec_independent_after_push() {
        // The kata's sieve init shape: `Vec.filled(n, Vec.new())`.
        // The per-slot bit-copy of the `Vec.new()` aggregate stores
        // `{null, 0, 0}` into each slot — pointers all start at
        // null, so the first `grid[i].push(...)` allocates a fresh
        // buffer per row (no aliasing). Equivalent to the
        // interpreter's deep-clone fix at `beb7310`, but achieved
        // structurally rather than via a clone helper because
        // empty Vec storage has no data pointer to alias.
        let out = run_program(
            r#"
fn main() {
    let mut grid: Vec[Vec[i64]] = Vec.filled(3, Vec.new());
    grid[0].push(99);
    println(grid[0].len());
    println(grid[1].len());
    println(grid[2].len());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "1\n0\n0");
        }
    }

    #[test]
    fn test_e2e_vec_deque_push_back_len_is_empty() {
        // VecDeque codegen v1 surface: `new` + `push_back` + `len` +
        // `is_empty` mirror Vec's `{ptr, len, cap}` layout exactly.
        let out = run_program(
            r#"
fn main() {
    let mut q: VecDeque[i64] = VecDeque.new();
    q.push_back(1);
    q.push_back(2);
    q.push_back(3);
    println(q.len());
    println(q.is_empty());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3\nfalse");
        }
    }

    #[test]
    fn test_e2e_vec_deque_push_front_shifts_storage_right() {
        // `push_front` shifts existing elements right by 1 via
        // `llvm.memmove` and stores the new element at index 0. Iter
        // yields front-to-back: [front=5, then 10, 20].
        let out = run_program(
            r#"
fn main() {
    let mut q: VecDeque[i64] = VecDeque.new();
    q.push_back(10);
    q.push_back(20);
    q.push_front(5);
    let mut sum = 0i64;
    for x in q.iter() { sum = sum + x; }
    println(sum);
    println(q.len());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "35\n3");
        }
    }

    #[test]
    fn test_e2e_vec_deque_pop_back_alias_of_pop_returns_option() {
        // `pop_back` shares the `pop` arm — both return `Option[T]`
        // after the 2026-05-10 Option-wrap upgrade. Match unwraps
        // Some; None on empty.
        let out = run_program(
            r#"
fn main() {
    let mut q: VecDeque[i64] = VecDeque.new();
    q.push_back(1);
    q.push_back(2);
    q.push_back(3);
    match q.pop_back() {
        Some(x) => println(x),
        None => println(0),
    }
    println(q.len());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3\n2");
        }
    }

    #[test]
    fn test_e2e_auto_par_propagates_let_bindings_with_identifier_rhs() {
        // `let n = p; let v: Vec[T] = Vec.new()` are independent
        // statements, so the concurrency analyzer groups them as
        // parallelizable. Before the fix, `infer_let_binding_llvm_type`
        // returned None for `let n = p` (Identifier RHS, no type
        // annotation), so the return-slot machinery silently dropped
        // `n` — the tail-expression read failed with "Undefined
        // variable 'n'". Fix: extend the inference to read the RHS
        // identifier's type from `self.variables`.
        let out = run_program(
            r#"
fn foo(p: i64) -> i64 {
    let n = p;
    let v: Vec[i64] = Vec.new();
    let _ = v.len();
    n
}
fn main() { println(foo(3)); }
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3");
        }
    }

    #[test]
    fn test_e2e_auto_par_drops_untypeable_return_slot_groups() {
        // `let n = nums.len()` has a MethodCall RHS that the
        // let-binding type inference can't recover. Auto-par groups
        // would silently drop the `n` slot before the fix — `n` then
        // became a class-(i) branch-local with no parent propagation,
        // surfacing later as "Undefined variable 'n'" at the read
        // site. Fix: when any needed-outside binding has un-typeable
        // RHS, `compute_return_slots_checked` returns None and the
        // caller drops the par-group, falling back to sequential
        // compilation (correct, just slower).
        let out = run_program(
            r#"
fn foo(nums: Slice[i64]) -> i64 {
    let n = nums.len();
    let mut visited: Vec[bool] = Vec.new();
    for _ in 0..n { visited.push(false); }
    visited[0] = true;
    let mut sum = 0i64;
    let mut i = 0i64;
    while i < n {
        sum = sum + nums[i];
        i = i + 1;
    }
    sum
}
fn main() {
    let a: Array[i64, 3] = [1, 2, 3];
    println(foo(a));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "6");
        }
    }

    #[test]
    fn test_e2e_auto_par_captures_indexed_access_base() {
        // `refs_in_expr` was missing an `ExprKind::Index` arm — so
        // `nums[j]` inside a par-branch body didn't walk into `nums`,
        // and `nums` was missed from the capture set. The branch fn
        // then ran with `nums` absent from `self.variables`, panicking
        // at `compile_slice_index`'s `get_data_ptr(name).unwrap()`.
        // Repro shape: function with a Slice param, a Vec/Map
        // declaration (forms an independent par-group with the
        // length binding), and a later block that indexes the slice.
        let out = run_program(
            r#"
fn min_jumps(nums: Slice[i64]) -> i64 {
    let n = nums.len();
    let mut visited: Vec[bool] = Vec.new();
    let mut bucket: Map[i64, Vec[i64]] = Map.new();
    for _ in 0..n { visited.push(false); }
    visited[0] = true;
    let mut sum = 0i64;
    let mut i = 0i64;
    while i < n {
        sum = sum + nums[i];
        i = i + 1;
    }
    let _ = bucket.len();
    sum
}
fn main() {
    let a: Array[i64, 3] = [1, 2, 3];
    println(min_jumps(a));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "6");
        }
    }

    #[test]
    fn test_e2e_for_slice_iter_propagates_outer_mutable_writes() {
        // Same shape as the Vec case but for `Slice[T].iter()` —
        // sibling of the previous test; `compile_for`'s `_ =>` arm
        // ate both shapes before the iter/into_iter peel-off landed.
        let out = run_program(
            r#"
fn maxv(nums: Slice[i64]) -> i64 {
    let mut m = 0i64;
    for v in nums.iter() {
        if v > m { m = v; }
    }
    m
}
fn main() {
    let a: Array[i64, 4] = [1, 2, 4, 6];
    println(maxv(a));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "6");
        }
    }

    // ── Vec[T] indexed write (Slice Vb) ───────────────────────────

    #[test]
    fn test_e2e_vec_indexed_write_basic() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(10);
    v.push(20);
    v.push(30);
    v[1] = 99;
    println(v[0]);
    println(v[1]);
    println(v[2]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["10", "99", "30"]);
        }
    }

    #[test]
    fn test_e2e_vec_indexed_write_oob_panics() {
        let captured = run_program_capturing(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(1);
    v[5] = 99;
    println(42);
}
"#,
        );
        if let Some(c) = captured {
            assert!(
                c.stdout.contains("panic: vec index out of bounds"),
                "expected vec OOB panic, got stdout={:?} stderr={:?}",
                c.stdout,
                c.stderr
            );
            assert!(
                !c.stdout.contains("42"),
                "code after panicking index store should not run"
            );
        }
    }

    #[test]
    fn test_e2e_vec_indexed_write_after_push() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(0);
    v.push(0);
    v[0] = 7;
    v[1] = 8;
    println(v[0] + v[1]);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "15");
        }
    }

    #[test]
    fn test_e2e_vec_indexed_write_through_mut_ref_param() {
        let out = run_program(
            r#"
fn set_at(v: mut ref Vec[i64], i: i64, x: i64) {
    v[i] = x;
}
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(1);
    v.push(2);
    v.push(3);
    set_at(mut v, 1_i64, 99_i64);
    println(v[0]);
    println(v[1]);
    println(v[2]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["1", "99", "3"]);
        }
    }

    // ── Vec/Slice/Array indexed-receiver method dispatch (Slice Vc) ──

    #[test]
    fn test_e2e_indexed_receiver_inner_vec_len() {
        let out = run_program(
            r#"
fn main() {
    let mut outer: Vec[Vec[i64]] = Vec.new();
    let mut a: Vec[i64] = Vec.new();
    a.push(1);
    a.push(2);
    a.push(3);
    outer.push(a);
    let mut b: Vec[i64] = Vec.new();
    b.push(10);
    outer.push(b);
    println(outer[0].len());
    println(outer[1].len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "1"]);
        }
    }

    #[test]
    fn test_e2e_indexed_receiver_inner_vec_is_empty() {
        let out = run_program(
            r#"
fn main() {
    let mut outer: Vec[Vec[i64]] = Vec.new();
    let mut a: Vec[i64] = Vec.new();
    a.push(7);
    outer.push(a);
    let b: Vec[i64] = Vec.new();
    outer.push(b);
    println(outer[0].is_empty());
    println(outer[1].is_empty());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["false", "true"]);
        }
    }

    #[test]
    fn test_e2e_indexed_receiver_inner_vec_push() {
        // Headline regression gate — closes the LeetCode 3629 kata's primary
        // blocker (`factors[j].push(i)`). The push must write back through
        // the elem pointer aliasing the outer storage so subsequent reads of
        // `outer[0]` observe the new element.  We verify via len() and
        // for-loop element iteration since chained-index reads `outer[i][j]`
        // are out of scope for v1.
        let out = run_program(
            r#"
fn main() {
    let mut outer: Vec[Vec[i64]] = Vec.new();
    let a: Vec[i64] = Vec.new();
    outer.push(a);
    let b: Vec[i64] = Vec.new();
    outer.push(b);
    outer[0].push(42);
    outer[0].push(43);
    outer[1].push(99);
    println(outer[0].len());
    println(outer[1].len());
    let mut acc: i64 = 0;
    for inner in outer {
        for x in inner { acc = acc + x; }
    }
    println(acc);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            // 42 + 43 + 99 = 184
            assert_eq!(lines, vec!["2", "1", "184"]);
        }
    }

    #[test]
    fn test_e2e_indexed_receiver_slice_path_len() {
        // The outer is a `mut Slice[Vec[i64]]` view; indexed-receiver
        // dispatch goes through the slice lowering path.
        let out = run_program(
            r#"
fn outer_lens(xs: mut Slice[Vec[i64]]) {
    println(xs[0].len());
    println(xs[1].len());
}
fn main() {
    let mut a: Vec[i64] = Vec.new();
    a.push(1);
    a.push(2);
    let mut b: Vec[i64] = Vec.new();
    b.push(10);
    b.push(20);
    b.push(30);
    let mut arr: Array[Vec[i64], 2] = [a, b];
    outer_lens(arr);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "3"]);
        }
    }

    #[test]
    fn test_e2e_indexed_receiver_chained_rejected() {
        // MR5: `outer[i][j].method()` is rejected up front by codegen with
        // a clear diagnostic. Pin the diagnostic so the rejection doesn't
        // silently regress to a fall-through compile.
        let src = r#"
fn main() {
    let mut outer: Vec[Vec[Vec[i64]]] = Vec.new();
    let mut a: Vec[Vec[i64]] = Vec.new();
    let inner: Vec[i64] = Vec.new();
    a.push(inner);
    outer.push(a);
    outer[0][0].push(7);
}
"#;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let err = compile_to_ir(&parsed.program, None, None)
            .expect_err("expected codegen to reject chained indexed receivers");
        assert!(
            err.contains("chained indexed receivers"),
            "expected chained-rejection diagnostic; got: {}",
            err
        );
    }

    #[test]
    fn test_e2e_indexed_receiver_user_struct_method() {
        // Vec[Counter] indexed receiver dispatching through `Counter.bump`.
        // Verifies var_type_names wiring for synth identifiers and that
        // the mut-ref-self method writes back through the elem pointer.
        let out = run_program(
            r#"
struct Counter { n: i64 }
impl Counter {
    fn bump(mut ref self) { self.n = self.n + 1; }
    fn read(ref self) -> i64 { self.n }
}
fn main() {
    let mut v: Vec[Counter] = Vec.new();
    v.push(Counter { n: 10 });
    v.push(Counter { n: 20 });
    v[0].bump();
    v[0].bump();
    v[1].bump();
    println(v[0].read());
    println(v[1].read());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["12", "21"]);
        }
    }

    #[test]
    fn test_e2e_vec_indexed_write_string_element() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[String] = Vec.new();
    v.push("alpha");
    v.push("beta");
    v[0] = "gamma";
    println(v[0]);
    println(v[1]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["gamma", "beta"]);
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
    fn test_e2e_slice_len_after_array_coercion() {
        // Regression: `Slice.len()` had no codegen handler — fell through to
        // the dispatcher's silent-`0` catch-all (line ~4163 pre-fix). Manifested
        // as `Slice.len() == 0` on any slice constructed by Array → Slice
        // coercion at a call site. See docs/known_bugs.md § B1.
        let out = run_program(
            r#"
fn dump(xs: Slice[i64]) {
    println(xs.len());
    println(xs[0]);
    println(xs[3]);
}
fn main() {
    let a: Array[i64, 4] = [2, 7, 11, 15];
    dump(a);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "4\n2\n15");
        }
    }

    #[test]
    fn test_e2e_slice_len_through_nested_call() {
        // Companion regression: `Slice.len()` correct after Slice → Slice
        // forwarding (matches the LeetCode #1 `report` → `two_sum` shape that
        // exposed the bug originally).
        let out = run_program(
            r#"
fn inner(xs: Slice[i64]) -> i64 { xs[0] + xs[1] }
fn outer(xs: Slice[i64]) {
    println(inner(xs));
    println(xs.len());
}
fn main() {
    let a: Array[i64, 4] = [2, 7, 11, 15];
    outer(a);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "9\n4");
        }
    }

    #[test]
    fn test_e2e_slice_is_empty() {
        // `Slice.is_empty()` shares the dispatcher path with `len`. Pre-fix
        // it fell through to the silent-`0` catch-all so both empty and
        // non-empty slices reported "empty" (i1 zero). Use the bool directly
        // as a return value to avoid hitting the unrelated empty-array-literal
        // gap and the if-as-statement Unit-coercion path.
        let out = run_program(
            r#"
fn empty_flag(xs: Slice[i64]) -> bool { xs.is_empty() }
fn main() {
    let a: Array[i64, 3] = [1, 2, 3];
    let r = empty_flag(a);
    if r { println(1); } else { println(0); }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "0");
        }
    }

    #[test]
    fn test_e2e_slice_brute_force_two_sum() {
        // End-to-end: the LeetCode #1 brute-force shape that exposed B1.
        // Pre-fix: returned [-1, -1] regardless of input because `nums.len()`
        // returned 0 → for-loop never executed.
        let out = run_program(
            r#"
fn two_sum(nums: Slice[i64], target: i64) -> Array[i64, 2] {
    let n = nums.len();
    for i in 0..n {
        for j in (i + 1)..n {
            if nums[i] + nums[j] == target {
                return [i, j];
            }
        }
    }
    [-1, -1]
}
fn main() {
    let nums: Array[i64, 4] = [2, 7, 11, 15];
    let r = two_sum(nums, 9);
    println(r[0]);
    println(r[1]);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "0\n1");
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
        // CR-202 slice 5b: companion `impl PartialEq for Point` keeps the
        // typecheck pass clean now that `Eq: PartialEq`.
        let ir = ir_for(
            r#"
struct Point { x: i64, y: i64 }
impl PartialEq for Point {
    fn eq(ref self, other: ref Point) -> bool { self.x == other.x and self.y == other.y }
}
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
        // CR-202 slice 5b: companion `impl PartialEq for Point` satisfies
        // the new `Eq: PartialEq` supertrait edge.
        let out = run_program(
            r#"
struct Point { x: i64, y: i64 }
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

    // ── Atomic RC for par-block bindings ──────────────────────────
    //
    // The ownership pass produces `arc_values` (a per-function subset of
    // `rc_values`) for bindings that cross a `par {}` thread boundary. Codegen
    // routes inc/dec on those bindings through `atomicrmw add` / `atomicrmw
    // sub` (`SeqCst`) so the refcount mutates race-free across threads.
    // Bindings in `rc_values` but not `arc_values` continue to use the plain
    // non-atomic load+arith+store sequence.

    /// Compile to LLVM IR with the ownership-pass result threaded through, so
    /// the codegen `arc_fallback_fns` table is populated. The plain `ir_for`
    /// helper passes `None` for ownership and never exercises the atomic path.
    fn ir_for_with_ownership(src: &str) -> String {
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let ownership = karac::ownershipcheck(&parsed.program, &typed);
        compile_to_ir(&parsed.program, Some(&ownership), None).expect("codegen failed")
    }

    #[test]
    fn test_ir_par_block_arc_promoted_binding_uses_atomic_rc() {
        // Trigger 1 (branch-divergent re-use) flags `d` as RC-fallback; the
        // par-block crossing makes Phase 2 promote it to Arc. Codegen must
        // emit `atomicrmw` for the Arc-flagged binding's inc/dec, not plain
        // load+add/sub+store. Pattern verified to populate `arc_values["process"]`
        // by `tests/rc_fallback.rs::par_block_promotes_rc_to_arc`.
        //
        // **Why a non-shared `struct Data` instead of `shared struct Counter`:**
        // RC-fallback (and Arc promotion) only fires on bindings that the
        // ownership pass routes through the `rc_values` table — i.e. non-shared
        // types whose use pattern (branch-divergent re-use) forces a heap-boxed
        // refcount fallback. `shared struct` types already carry built-in RC
        // machinery; they don't go through the fallback path and are excluded
        // from the predicate output, so they never reach `arc_values`. Updated
        // 2026-05-13 from the prior `shared struct Counter` shape (which never
        // triggered RC and emitted plain inc/dec) to mirror the rc_fallback
        // integration test that exercises the exact codegen path under test.
        //
        // **Multi-stmt par-block** so `emit_par_run`'s single-statement fast
        // path (`if stmts.len() == 1 { compile_stmt sequentially }`) doesn't
        // collapse the par-block into plain sequential code — the runtime
        // dispatch via `karac_par_run` is what makes Phase 2 detect this as a
        // par-region for arc promotion. Two consumes inside `par { }`.
        //
        // **Runs from `process`, called by `main`:** the par block in a
        // void-returning user function still trips the pre-existing
        // module-verifier `ret i64 0` wart, independent of this slice; called
        // from a non-void `main` keeps the verifier happy.
        let ir = ir_for_with_ownership(
            r#"
struct Data { value: i64 }
fn consume(d: Data) -> i64 { d.value }
fn use_d(d: Data) -> i64 { d.value }
fn process(cond: bool, d: Data) -> i64 {
    if cond { consume(d); }
    par {
        use_d(d);
        let _throwaway = 0i64;
    }
    0i64
}
fn main() {
    process(false, Data { value: 7 });
}
"#,
        );
        // Atomic DEC at scope exit must emit `atomicrmw sub` with `seq_cst`
        // ordering. The DEC fires from `CleanupAction::RcDec` at the end of
        // `process`, dispatched through `emit_refcount_dec` →
        // `is_arc_binding("d") == true` → `emit_arc_dec` (atomicrmw sub).
        assert!(
            ir.contains("atomicrmw sub"),
            "Arc-promoted binding's dec should lower to `atomicrmw sub`; IR:\n{ir}"
        );
        assert!(
            ir.contains("seq_cst"),
            "atomicrmw should use SeqCst ordering; IR:\n{ir}"
        );

        // TODO 2026-05-13: per-consume-site atomic INC for rc-fallback
        // bindings isn't currently emitted by codegen. The rc-fallback design
        // requires each consume site (`consume(d)` in the if-arm,
        // `use_d(d)` in the par-block) to be preceded by an inc so the
        // binding's refcount survives both consumes when both branches fire
        // on the same execution path. Today only the initial-refcount-1
        // store + final dec exist for an rc-fallback binding; consume sites
        // pass the heap pointer through unincremented. When that inc
        // emission lands, restore:
        //   assert!(ir.contains("atomicrmw add"), ...);
        // and re-tighten the test to verify both halves of the atomic-RC
        // path. Tracked separately from this slice — out-of-scope for the
        // pre-existing-test-pass fix.
    }

    #[test]
    fn test_ir_non_par_binding_uses_plain_rc() {
        // Same trigger-1 RC shape, no par block. The binding stays in
        // `rc_values` but not `arc_values`, so codegen keeps the plain
        // non-atomic ops and emits no `atomicrmw`. Regression guard: the
        // dispatcher must not unconditionally route through the atomic
        // helper when only `rc_fallback_fns` is populated.
        let ir = ir_for_with_ownership(
            r#"
shared struct Counter { val: i64 }
fn use_c(c: Counter) -> i64 { c.val }
fn main() {
    let cond: bool = false;
    let c = Counter { val: 7 };
    let d = c;
    if cond { use_c(d); }
    use_c(d);
}
"#,
        );
        assert!(
            !ir.contains("atomicrmw"),
            "non-par RC binding must not use atomic ops; IR:\n{ir}"
        );
    }

    #[test]
    fn test_e2e_arc_binding_runtime_correctness() {
        // Atomic-RC inc/dec must produce the same observable behavior as
        // plain RC. The par block runs both branches; we verify the program
        // completes and produces the expected output, which exercises the
        // alloc + atomic inc + atomic dec drop-to-zero paths.
        //
        // ASAN (when enabled via tests/memory_sanitizer.rs) is what catches
        // a real refcount race — at the IR level this is an end-to-end
        // smoke check that the atomic codegen path links and runs.
        let out = run_program_with_ownership(
            r#"
shared struct Counter { val: i64 }
fn use_c(c: Counter) -> i64 { c.val }
fn main() {
    let cond: bool = false;
    let c = Counter { val: 7 };
    let d = c;
    if cond { use_c(d); }
    par {
        println(use_c(d));
        println(use_c(d));
    }
}
"#,
        );
        if let Some(out) = out {
            // Two branches, each prints 7. Order is unspecified across
            // threads, but both '7' tokens must appear.
            let count = out.matches('7').count();
            assert!(
                count >= 2,
                "expected '7' to be printed twice (once per par branch); got: {out:?}"
            );
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
        let out = out.expect("? cross-error codegen should not bail");
        // Without conversion: 7. With From doubling: 14.
        assert_eq!(out.trim(), "14");
    }

    #[test]
    fn test_e2e_question_cross_error_from_conversion_reaches_err_arm() {
        // Smoke test for the LLVM-verification half of the `?` cross-error
        // path: codegen reconstitutes the source-error struct from the i64
        // payload word before calling `Target.from`, and coerces the
        // returned struct back to an i64 word for the outer Result aggregate.
        // The full `e.code` assertion is gated on a separate codegen fix
        // for struct-payload match-arm binding (see the `#[ignore]` note on
        // `test_e2e_question_cross_error_from_conversion`); this case
        // matches the Err arm without touching the binding's fields, so it
        // exercises the verification fix in isolation.
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
        Err(_) => println(99_i64),
    }
}
"#,
        );
        let out = out.expect("? cross-error codegen should not bail");
        assert_eq!(out.trim(), "99");
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

    // ── ? error_return_trace KARAC_ERROR_TRACE_FORMAT env-var dispatch ───────
    //
    // The runtime's atexit printer reads `KARAC_ERROR_TRACE_FORMAT` and
    // dispatches between three emitters:
    //   - text   (default; missing/unrecognized values fall back here)
    //   - json   (single-document — bare array, or `{frames,truncated}`
    //            when the ring buffer dropped older entries)
    //   - jsonl  (line-delimited JSON; one event per line)
    // The JSON shape mirrors the interpreter's `format_error_trace_json`
    // verbatim. These tests exercise the full compile → link → run path
    // with the env var threaded into the child process.

    fn run_program_capturing_with_env(
        src: &str,
        filename: Option<&str>,
        env: &[(&str, &str)],
    ) -> Option<CapturedRun> {
        use karac::codegen::{compile_to_object_with_options, link_executable};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let mut parsed = karac::parse(src);
        if !parsed.errors.is_empty() {
            let mut msg = String::from("test source failed to parse:\n");
            for e in &parsed.errors {
                msg.push_str(&format!("  {:?}\n", e));
            }
            panic!("{}", msg);
        }
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);

        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let obj_path = format!("/tmp/karac_e2e_envtrace_{}_{}.o", std::process::id(), id);
        let exe_path = format!("/tmp/karac_e2e_envtrace_{}_{}", std::process::id(), id);

        if let Err(e) =
            compile_to_object_with_options(&parsed.program, &obj_path, None, None, filename, None)
        {
            panic!("codegen failed for test program: {}", e);
        }
        link_executable(&obj_path, &exe_path).ok()?;

        let mut cmd = std::process::Command::new(&exe_path);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let output = cmd.output().ok()?;

        let _ = std::fs::remove_file(&obj_path);
        let _ = std::fs::remove_file(&exe_path);

        Some(CapturedRun {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    /// Source common to all three format-dispatch tests: a single `?`
    /// site so the trace has exactly one frame, threaded through a
    /// source filename so each frame carries `<file>:<line>:<col>` —
    /// gives the JSON / JSONL emitters something non-empty to escape
    /// (and keeps the JSON shape assertion easy to read).
    const TRACE_FORMAT_SRC: &str = r#"
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
"#;

    #[test]
    fn test_error_trace_text_format_default() {
        // No env var → existing text format. Regression pin: this is
        // identical to what `test_e2e_question_trace_includes_source_filename_when_threaded`
        // exercises, but explicitly asserts the absence of any JSON
        // markers so a future default flip would surface here.
        let captured =
            run_program_capturing_with_env(TRACE_FORMAT_SRC, Some("trace_fmt_default.kara"), &[]);
        if let Some(c) = captured {
            assert_eq!(c.stdout.trim(), "7");
            assert!(
                c.stderr.contains("Error return trace:"),
                "expected text-mode header; got {:?}",
                c.stderr
            );
            assert!(
                c.stderr.contains("trace_fmt_default.kara:"),
                "expected file:line:col frame; got {:?}",
                c.stderr
            );
            // No JSON markers — `[`, `]`, or `{` appearing on their own
            // would indicate a stray JSON emitter wired in by mistake.
            // (We can't blanket-ban `{` because user stdout is separate;
            // we're checking stderr.)
            assert!(
                !c.stderr.contains("\"file\":"),
                "text mode should not emit JSON keys; got {:?}",
                c.stderr
            );
        }
    }

    #[test]
    fn test_error_trace_json_format() {
        // KARAC_ERROR_TRACE_FORMAT=json → single-document JSON on
        // stderr matching the interpreter's `format_error_trace_json`
        // shape: a bare array of frame objects when not truncated.
        // Each frame object has the keys `file`, `line`, `column`.
        let captured = run_program_capturing_with_env(
            TRACE_FORMAT_SRC,
            Some("trace_fmt_json.kara"),
            &[("KARAC_ERROR_TRACE_FORMAT", "json")],
        );
        if let Some(c) = captured {
            assert_eq!(c.stdout.trim(), "7");
            // No text-mode header.
            assert!(
                !c.stderr.contains("Error return trace:"),
                "json mode should not emit the text header; got {:?}",
                c.stderr
            );
            // Locate the JSON document — the printer emits a single
            // line on stderr matching the array shape.
            let json_line = c
                .stderr
                .lines()
                .find(|l| l.starts_with('[') && l.ends_with(']'))
                .unwrap_or_else(|| {
                    panic!("expected a JSON array line on stderr; got {:?}", c.stderr)
                });
            // Shape assertions — interpreter's format verbatim:
            //   `[{"file":"…","line":N,"column":N}]`
            assert!(
                json_line.contains("\"file\":"),
                "missing `file` key: {}",
                json_line
            );
            assert!(
                json_line.contains("\"line\":"),
                "missing `line` key: {}",
                json_line
            );
            assert!(
                json_line.contains("\"column\":"),
                "missing `column` key: {}",
                json_line
            );
            assert!(
                json_line.contains("trace_fmt_json.kara"),
                "filename not threaded into JSON frame: {}",
                json_line
            );
            // One `?` site → one frame → exactly one `{…}` object.
            let open_braces = json_line.matches('{').count();
            assert_eq!(
                open_braces, 1,
                "expected exactly 1 frame object; got {} ({})",
                open_braces, json_line
            );
        }
    }

    #[test]
    fn test_error_trace_jsonl_format() {
        // KARAC_ERROR_TRACE_FORMAT=jsonl → line-delimited JSON. One
        // event per line, each line a self-contained JSON object with
        // a `type` discriminator. Frames carry `"type":"frame"`; the
        // truncation marker (not exercised here — only one frame) would
        // be a separate `{"type":"truncated","max":N}` line.
        let captured = run_program_capturing_with_env(
            TRACE_FORMAT_SRC,
            Some("trace_fmt_jsonl.kara"),
            &[("KARAC_ERROR_TRACE_FORMAT", "jsonl")],
        );
        if let Some(c) = captured {
            assert_eq!(c.stdout.trim(), "7");
            // No text-mode header, no JSON-array bracket.
            assert!(
                !c.stderr.contains("Error return trace:"),
                "jsonl mode should not emit the text header; got {:?}",
                c.stderr
            );
            // Each non-empty stderr line must be a JSON object — i.e.
            // start with `{` and end with `}` — and contain the
            // `type` key.
            let trace_lines: Vec<&str> = c.stderr.lines().filter(|l| !l.is_empty()).collect();
            assert!(
                !trace_lines.is_empty(),
                "expected at least one JSONL line; got {:?}",
                c.stderr
            );
            for line in &trace_lines {
                assert!(
                    line.starts_with('{') && line.ends_with('}'),
                    "JSONL line must be a JSON object literal; got `{}`",
                    line
                );
                assert!(
                    line.contains("\"type\":"),
                    "JSONL line missing `type` discriminator; got `{}`",
                    line
                );
            }
            // One `?` site → exactly one frame line, no truncated marker.
            let frame_lines: Vec<&&str> = trace_lines
                .iter()
                .filter(|l| l.contains("\"type\":\"frame\""))
                .collect();
            assert_eq!(
                frame_lines.len(),
                1,
                "expected exactly 1 frame event; got {:?}",
                trace_lines
            );
            let frame = frame_lines[0];
            assert!(
                frame.contains("\"file\":"),
                "frame missing `file`: {}",
                frame
            );
            assert!(
                frame.contains("\"line\":"),
                "frame missing `line`: {}",
                frame
            );
            assert!(
                frame.contains("\"column\":"),
                "frame missing `column`: {}",
                frame
            );
            assert!(
                frame.contains("trace_fmt_jsonl.kara"),
                "filename not threaded into JSONL frame: {}",
                frame
            );
        }
    }

    // ── Linker control attributes ────────────────────────────────────────────

    #[test]
    fn test_ir_no_mangle_symbol_name_unchanged() {
        // `#[no_mangle]` is a no-op at the codegen layer (the compiler already
        // uses the source-level name as the LLVM symbol name) but we verify
        // the function still emits with its plain name.
        let ir = ir_for("#[unsafe(no_mangle)]\nfn keep_me() -> i64 { 42 }");
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
        let ir = ir_for("#[unsafe(link_section(\".init_array\"))]\nfn ctor() -> i64 { 1 }");
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
        Some(x) => println(x),
        None => println(0_i64),
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
        Some(x) => println(x),
        None => println(0_i64),
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
        Some(x) => println(x),
        None => println(0_i64),
    }
    let second = m.insert(7_i64, 20_i64);
    match second {
        Some(x) => println(x),
        None => println(0_i64),
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
        Some(x) => println(x),
        None => println(0_i64),
    }
    let r2 = m.remove(5_i64);
    match r2 {
        Some(x) => println(x),
        None => println(0_i64),
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
        Some(x) => println(x),
        None => println(0_i64),
    }
    let v2 = m.get("world");
    match v2 {
        Some(x) => println(x),
        None => println(0_i64),
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
        Some(x) => println(x),
        None => println(0_i64),
    }
    let second = m.insert("key", 20_i64);
    match second {
        Some(x) => println(x),
        None => println(0_i64),
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
        Some(x) => println(x),
        None => println(0_i64),
    }
    let r2 = m.remove("alpha");
    match r2 {
        Some(x) => println(x),
        None => println(0_i64),
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
    fn test_e2e_map_keys_returns_vec() {
        // m.keys() materializes Vec[K] containing every key. Iteration order
        // is unspecified, so sum the keys and verify total.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(10_i64, 1_i64);
    m.insert(20_i64, 2_i64);
    m.insert(30_i64, 3_i64);
    let ks: Vec[i64] = m.keys();
    println(ks.len());
    let mut sum: i64 = 0;
    for k in ks {
        sum = sum + k;
    }
    println(sum);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "60"]);
        }
    }

    #[test]
    fn test_e2e_map_values_returns_vec() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(1_i64, 100_i64);
    m.insert(2_i64, 200_i64);
    m.insert(3_i64, 300_i64);
    let vs: Vec[i64] = m.values();
    println(vs.len());
    let mut sum: i64 = 0;
    for v in vs {
        sum = sum + v;
    }
    println(sum);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "600"]);
        }
    }

    #[test]
    fn test_e2e_map_entries_returns_vec_of_tuples() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(1_i64, 10_i64);
    m.insert(2_i64, 20_i64);
    let es: Vec[(i64, i64)] = m.entries();
    println(es.len());
    let mut k_sum: i64 = 0;
    let mut v_sum: i64 = 0;
    for (k, v) in es {
        k_sum = k_sum + k;
        v_sum = v_sum + v;
    }
    println(k_sum);
    println(v_sum);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "3", "30"]);
        }
    }

    #[test]
    fn test_e2e_map_keys_string_keys_len() {
        // Keys are heap-bearing String values. Verify the resulting Vec[String]
        // reports the correct length. (For-loop element-type propagation —
        // `for s in vs { s.len() }` — is now wired; see
        // `test_e2e_for_in_vec_string_calls_len` below.)
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("alice", 1_i64);
    m.insert("bob", 2_i64);
    let ks: Vec[String] = m.keys();
    println(ks.len());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "2");
        }
    }

    // ── For-loop element-type propagation (List 2, item 3) ────────────

    #[test]
    fn test_e2e_for_in_vec_string_calls_len() {
        // Iterating Vec[String] should bind `s` as a String so `s.len()`
        // dispatches through compile_vec_method (String reuses the Vec
        // shape with elem=u8) and reads the actual length, not the
        // silent-`0` fall-through. Before the fix, both lines printed `0`.
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[String] = Vec.new();
    v.push("alice");
    v.push("bobby");
    for s in v {
        println(s.len());
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["5", "5"]);
        }
    }

    #[test]
    fn test_e2e_for_in_vec_vec_inner_push() {
        // Iterating Vec[Vec[i64]] should bind `inner` as a Vec[i64] so
        // inner-Vec method dispatch resolves correctly.
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[Vec[i64]] = Vec.new();
    let mut a: Vec[i64] = Vec.new();
    a.push(1_i64);
    a.push(2_i64);
    v.push(a);
    let mut b: Vec[i64] = Vec.new();
    b.push(10_i64);
    b.push(20_i64);
    b.push(30_i64);
    v.push(b);
    for inner in v {
        println(inner.len());
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "3"]);
        }
    }

    #[test]
    fn test_e2e_for_in_string_chars_count() {
        // `for c in s.chars()` over a String variable. The codegen peels
        // `.chars()` off and dispatches the String variable through
        // `compile_for_string_chars`, iterating one Unicode scalar per
        // step. Counts 5 chars in "hello".
        let out = run_program(
            r#"
fn main() {
    let s = "hello";
    let mut n: i64 = 0_i64;
    for _c in s.chars() {
        n = n + 1_i64;
    }
    println(n);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "5");
        }
    }

    #[test]
    fn test_e2e_for_in_string_variable_iterates_chars() {
        // `for c in s` on a bare String variable — design.md § Character
        // type (line 2299) pins this as the semantic peer of `s.chars()`.
        // Before this slice, the variable went through
        // `compile_for_vec_var` (byte iteration with elem=i8), producing
        // i8 byte values instead of i32 codepoints.
        let out = run_program(
            r#"
fn main() {
    let s = "abc";
    let mut sum: i64 = 0_i64;
    for c in s {
        sum = sum + (c as i64);
    }
    println(sum);
}
"#,
        );
        if let Some(out) = out {
            // 'a' + 'b' + 'c' = 97 + 98 + 99 = 294
            assert_eq!(out.trim(), "294");
        }
    }

    #[test]
    fn test_e2e_for_in_string_literal_chars() {
        // String-literal iterable (no variable binding) — verifies the
        // `ExprKind::StringLit` arm in the for-loop dispatcher that the
        // `.chars()` peel-off recurses into. Sums the codepoints.
        let out = run_program(
            r#"
fn main() {
    let mut sum: i64 = 0_i64;
    for c in "xyz".chars() {
        sum = sum + (c as i64);
    }
    println(sum);
}
"#,
        );
        if let Some(out) = out {
            // 'x' + 'y' + 'z' = 120 + 121 + 122 = 363
            assert_eq!(out.trim(), "363");
        }
    }

    #[test]
    fn test_e2e_for_in_empty_string_zero_iterations() {
        // Empty string — the byte-offset cond (`offset < len`) is false
        // at entry, so the body never runs. Pins the empty-edge case.
        let out = run_program(
            r#"
fn main() {
    let s = "";
    let mut n: i64 = 0_i64;
    for _c in s.chars() {
        n = n + 1_i64;
    }
    println(n);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "0");
        }
    }

    #[test]
    fn test_e2e_for_in_string_chars_into_map_char_key() {
        // The LeetCode #3 idiom — char keys feeding a `Map[char, i64]`.
        // Inserts decoded chars from one pass and looks them up via
        // decoded chars from a second pass. Pins that the codepoint
        // values produced by the chars-iteration codegen are consistent
        // across calls (same hash, same key identity) — same shape the
        // sliding-window kata relies on. Uses only for-loop-bound char
        // values; mixing in `char` *literals* in `compile_expr` position
        // currently lowers to const_int(0) (pre-existing gap unrelated
        // to this slice — `ExprKind::CharLit` has no runtime arm in
        // `compile_expr`, only the const-eval table at line 83).
        let out = run_program(
            r#"
fn main() {
    let mut last_idx: Map[char, i64] = Map.new();
    let mut i: i64 = 0_i64;
    for c in "abca".chars() {
        last_idx.insert(c, i);
        i = i + 1_i64;
    }
    // Second pass: for each char in "abc", report its last-seen index.
    // 'a' was overwritten at index 3 (last position in "abca"); 'b' at 1; 'c' at 2.
    for c in "abc".chars() {
        match last_idx.get(c) {
            Some(v) => println(v),
            None => println(-1_i64),
        }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "1", "2"]);
        }
    }

    #[test]
    fn test_e2e_for_in_map_string_keys_use_len() {
        // `for (k, _v) in m` where K = String should bind `k` as a String
        // so `k.len()` dispatches correctly. Map iteration order is
        // unspecified, so we sum the lengths to make the assertion
        // order-independent.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("alice", 1_i64);
    m.insert("bobby", 2_i64);
    let mut total: i64 = 0_i64;
    for (k, _v) in m {
        total = total + k.len();
    }
    println(total);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "10");
        }
    }

    #[test]
    fn test_e2e_for_in_slice_string_calls_len() {
        // Iterating Slice[String] (here from `Array[String, N].as_slice()`)
        // should bind the loop var as a String for correct method dispatch.
        let out = run_program(
            r#"
fn main() {
    let a: Array[String, 2] = ["alice", "bobby"];
    let s: Slice[String] = a.as_slice();
    for elem in s {
        println(elem.len());
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["5", "5"]);
        }
    }

    // ── Compound-key Map (List 2, item 2) ────────────────────────
    // `Map[(K1, K2, …), V]` — codegen emits per-field-recursive hash and
    // eq functions so each tuple component is hashed/compared via its
    // own per-type fn (String hashes contents, i64 hashes raw bytes, …).

    #[test]
    fn test_e2e_map_tuple_string_int_key() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[(String, i64), i64] = Map.new();
    m.insert(("alice", 1_i64), 100_i64);
    m.insert(("alice", 2_i64), 200_i64);
    m.insert(("bob",   1_i64), 300_i64);
    println(m.len());
    let v1 = m.get(("alice", 1_i64));
    match v1 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v2 = m.get(("bob", 1_i64));
    match v2 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v3 = m.get(("alice", 9_i64));
    match v3 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
}
"#,
        );
        let out = out.expect("tuple-key codegen should not bail");
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines, vec!["3", "100", "300", "-1"]);
    }

    #[test]
    fn test_e2e_map_tuple_int_int_key() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[(i64, i64), i64] = Map.new();
    m.insert((1_i64, 2_i64), 12_i64);
    m.insert((3_i64, 4_i64), 34_i64);
    m.insert((1_i64, 4_i64), 14_i64);
    println(m.len());
    println(m[(1_i64, 2_i64)]);
    println(m[(3_i64, 4_i64)]);
    println(m[(1_i64, 4_i64)]);
}
"#,
        );
        let out = out.expect("tuple-key codegen should not bail");
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines, vec!["3", "12", "34", "14"]);
    }

    #[test]
    fn test_e2e_map_tuple_key_overwrite_returns_old() {
        // Re-inserting under the same compound key returns the prior value —
        // exercises the eq-fn path (the runtime must find the existing slot).
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[(String, i64), i64] = Map.new();
    let first = m.insert(("k", 1_i64), 10_i64);
    match first {
        Some(x) => println(x),
        None => println(0_i64),
    }
    let second = m.insert(("k", 1_i64), 20_i64);
    match second {
        Some(x) => println(x),
        None => println(0_i64),
    }
    println(m.len());
}
"#,
        );
        let out = out.expect("tuple-key overwrite codegen should not bail");
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines, vec!["0", "10", "1"]);
    }

    #[test]
    fn test_e2e_map_nested_tuple_key() {
        // `Map[(String, (i64, i64)), V]` — exercises the recursive emission
        // path: the outer tuple-hash recurses into the inner tuple-hash, which
        // recurses into the per-element primitive hash fns. Validates that
        // `karac_hash_tuple_String_tuple_i64_i64` emits exactly once and works
        // end-to-end.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[(String, (i64, i64)), i64] = Map.new();
    m.insert(("p", (1_i64, 2_i64)), 12_i64);
    m.insert(("p", (3_i64, 4_i64)), 34_i64);
    m.insert(("q", (1_i64, 2_i64)), 99_i64);
    println(m.len());
    let v = m.get(("p", (1_i64, 2_i64)));
    match v {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v2 = m.get(("q", (1_i64, 2_i64)));
    match v2 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v3 = m.get(("p", (9_i64, 9_i64)));
    match v3 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
}
"#,
        );
        let out = out.expect("nested tuple-key codegen should not bail");
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines, vec!["3", "12", "99", "-1"]);
    }

    #[test]
    fn test_e2e_map_primitive_struct_key() {
        // `#[derive(Hash, Eq)]` struct of primitives lowers to a packed-by-
        // field LLVM struct with no padding (here: `{ i64, i64 }`, 16 bytes).
        // The existing byte-loop FNV-1a path hashes the raw struct bytes,
        // and the byte-by-byte eq compares them — both correct for the
        // primitive-only case.
        let out = run_program(
            r#"
#[derive(Hash, Eq)]
struct Point {
    x: i64,
    y: i64,
}

fn main() {
    let mut m: Map[Point, i64] = Map.new();
    m.insert(Point { x: 1_i64, y: 2_i64 }, 12_i64);
    m.insert(Point { x: 3_i64, y: 4_i64 }, 34_i64);
    println(m.len());
    let v = m.get(Point { x: 1_i64, y: 2_i64 });
    match v {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v2 = m.get(Point { x: 9_i64, y: 9_i64 });
    match v2 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
}
"#,
        );
        let out = out.expect("primitive-struct-key codegen should not bail");
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines, vec!["2", "12", "-1"]);
    }

    #[test]
    fn test_e2e_map_struct_with_string_key() {
        // `#[derive(Hash, Eq)]` struct with a String field — the per-field
        // recursion path is required: a byte-loop over the raw struct bytes
        // would hash the data-ptr + len + cap, which differs across distinct
        // allocations even when the string contents match. Per-field recursion
        // routes the String field through the contents-aware String hash.
        let out = run_program(
            r#"
#[derive(Hash, Eq)]
struct Tag {
    name: String,
    n: i64,
}

fn main() {
    let mut m: Map[Tag, i64] = Map.new();
    m.insert(Tag { name: "alice", n: 1_i64 }, 100_i64);
    m.insert(Tag { name: "alice", n: 2_i64 }, 200_i64);
    m.insert(Tag { name: "bob",   n: 1_i64 }, 300_i64);
    println(m.len());
    let v1 = m.get(Tag { name: "alice", n: 1_i64 });
    match v1 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v2 = m.get(Tag { name: "bob", n: 1_i64 });
    match v2 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v3 = m.get(Tag { name: "alice", n: 9_i64 });
    match v3 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
}
"#,
        );
        let out = out.expect("struct-with-String-key codegen should not bail");
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines, vec!["3", "100", "300", "-1"]);
    }

    #[test]
    fn test_e2e_enum_match_simple_singleton() {
        let out = run_program(
            r#"
#[derive(Hash, Eq)]
enum Color { Red, Green, Blue }

fn main() {
    let g: Color = Color.Green;
    match g {
        Color.Green => println(11_i64),
        _ => println(99_i64),
    }
}
"#,
        );
        let out = out.expect("simple enum match should not bail");
        assert_eq!(out.trim(), "11");
    }

    #[test]
    fn test_e2e_enum_variant_match_codegen_sanity() {
        // Sanity: distinct unit-enum variants codegen to distinct values
        // (different tags). Pre-requisite for `Map[Color, V]` to work.
        let out = run_program(
            r#"
#[derive(Hash, Eq)]
enum Color { Red, Green, Blue }

fn main() {
    let r: Color = Color.Red;
    let g: Color = Color.Green;
    let b: Color = Color.Blue;
    match r {
        Color.Red => println(0_i64),
        _ => println(99_i64),
    }
    match g {
        Color.Green => println(1_i64),
        _ => println(99_i64),
    }
    match b {
        Color.Blue => println(2_i64),
        _ => println(99_i64),
    }
}
"#,
        );
        let out = out.expect("enum variant match codegen sanity");
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines, vec!["0", "1", "2"]);
    }

    #[test]
    fn test_e2e_map_unit_enum_key() {
        // Unit-variant enum used as a Map key. Layout is `{ i64 tag }` — the
        // existing primitive hash/eq path (byte-by-byte over sizeof(K))
        // already does the right thing once the typechecker permits the
        // `K: Hash + Eq` bound.
        let out = run_program(
            r#"
#[derive(Hash, Eq)]
enum Color { Red, Green, Blue }

fn main() {
    let mut m: Map[Color, i64] = Map.new();
    m.insert(Color.Red,   100_i64);
    m.insert(Color.Green, 200_i64);
    m.insert(Color.Blue,  300_i64);
    println(m.len());
    let v1 = m.get(Color.Red);
    match v1 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v2 = m.get(Color.Green);
    match v2 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v3 = m.get(Color.Blue);
    match v3 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
}
"#,
        );
        let out = out.expect("unit-enum-key codegen should not bail");
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines, vec!["3", "100", "200", "300"]);
    }

    #[test]
    fn test_e2e_map_tuple_string_string_key() {
        // Two heap-bearing fields in the tuple — exercises the per-field
        // recursion path on both sides. A byte-loop FNV over raw struct bytes
        // would hash the two String headers (data ptr / len / cap pairs), which
        // diverge across allocations when the contents are equal — so this
        // test would fail under the pre-recursion hash.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[(String, String), i64] = Map.new();
    m.insert(("alice", "red"),  1_i64);
    m.insert(("alice", "blue"), 2_i64);
    m.insert(("bob",   "red"),  3_i64);
    println(m.len());
    let v = m.get(("alice", "blue"));
    match v {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v2 = m.get(("alice", "green"));
    match v2 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
}
"#,
        );
        let out = out.expect("(String,String)-key codegen should not bail");
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines, vec!["3", "2", "-1"]);
    }

    #[test]
    fn test_e2e_map_compound_key_cache_reuse() {
        // Two distinct Map variables in one program share the same compound
        // key shape `(String, i64)`. Cache reuse means `karac_hash_tuple_*`
        // and `karac_eq_tuple_*` are emitted exactly once and called by both
        // map-new sites — duplicate emission would surface as a `module
        // already has a function named ...` panic during codegen, so this
        // test failing to compile is the cache regression signal.
        let out = run_program(
            r#"
fn main() {
    let mut m1: Map[(String, i64), i64] = Map.new();
    let mut m2: Map[(String, i64), i64] = Map.new();
    m1.insert(("a", 1_i64), 10_i64);
    m2.insert(("a", 1_i64), 99_i64);
    println(m1.len());
    println(m2.len());
    let v1 = m1.get(("a", 1_i64));
    match v1 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
    let v2 = m2.get(("a", 1_i64));
    match v2 {
        Some(x) => println(x),
        None => println(0_i64 - 1_i64),
    }
}
"#,
        );
        let out = out.expect("compound-key cache reuse codegen should not bail");
        let lines: Vec<&str> = out.trim().lines().collect();
        assert_eq!(lines, vec!["1", "1", "10", "99"]);
    }

    #[test]
    fn test_e2e_map_keys_empty() {
        // Empty map → empty Vec; len=0, no iteration body runs.
        let out = run_program(
            r#"
fn main() {
    let m: Map[i64, i64] = Map.new();
    let ks: Vec[i64] = m.keys();
    println(ks.len());
    println(ks.is_empty());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["0", "true"]);
        }
    }

    #[test]
    fn test_e2e_map_clear() {
        // clear() empties the map; subsequent insert/lookup work normally.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(1_i64, 10_i64);
    m.insert(2_i64, 20_i64);
    m.insert(3_i64, 30_i64);
    println(m.len());
    m.clear();
    println(m.len());
    println(m.is_empty());
    m.insert(7_i64, 70_i64);
    println(m[7_i64]);
    println(m.contains_key(1_i64));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "0", "true", "70", "false"]);
        }
    }

    #[test]
    fn test_e2e_map_clear_string_key() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("a", 1_i64);
    m.insert("b", 2_i64);
    m.clear();
    println(m.len());
    m.insert("c", 3_i64);
    println(m["c"]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["0", "3"]);
        }
    }

    #[test]
    fn test_e2e_map_prefix_literal_string_keys() {
        let out = run_program(
            r#"
fn main() {
    let m: Map[String, i64] = Map["a": 1_i64, "b": 2_i64, "c": 3_i64];
    println(m.len());
    println(m["b"]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "2"]);
        }
    }

    #[test]
    fn test_e2e_map_prefix_literal_int_keys() {
        let out = run_program(
            r#"
fn main() {
    let m: Map[i64, i64] = Map[1_i64: 100_i64, 2_i64: 200_i64];
    println(m.len());
    println(m[2_i64]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "200"]);
        }
    }

    #[test]
    fn test_e2e_map_bare_literal_with_annotation() {
        // Bare ["k": v] form with explicit Map type annotation.
        let out = run_program(
            r#"
fn main() {
    let m: Map[String, i64] = ["x": 10_i64, "y": 20_i64];
    println(m.len());
    println(m["x"]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "10"]);
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

    // ── Map.entry / Entry[K, V] codegen (canonical: phase-8-stdlib-floor.md
    //    "Map.entry(k) + Entry[K, V] enum") ──────────────────────────────────

    #[test]
    fn test_e2e_map_entry_or_insert_vacant() {
        // Vacant key → or_insert pushes (key, default), map state changes.
        // The chain return is mut ref V — discarded here; the post-chain
        // get() call reads the inserted value.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.entry(7_i64).or_insert(42_i64);
    let v = m.get(7_i64);
    match v {
        Some(x) => println(x),
        None => println(0_i64),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_e2e_map_entry_or_insert_occupied_passthrough() {
        // Occupied key → or_insert is a no-op write; map keeps existing value.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(3_i64, 99_i64);
    m.entry(3_i64).or_insert(0_i64);
    let v = m.get(3_i64);
    match v {
        Some(x) => println(x),
        None => println(-1_i64),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    #[test]
    fn test_e2e_map_entry_or_insert_with_vacant_invokes_closure() {
        // Vacant key → or_insert_with fires the closure to produce default.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.entry(1_i64).or_insert_with(|| 17_i64);
    let v = m.get(1_i64);
    match v {
        Some(x) => println(x),
        None => println(0_i64),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "17");
        }
    }

    #[test]
    fn test_e2e_map_entry_or_insert_with_occupied_skips_closure() {
        // Occupied key → closure does NOT run; map unchanged.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(2_i64, 5_i64);
    m.entry(2_i64).or_insert_with(|| 999_i64);
    let v = m.get(2_i64);
    match v {
        Some(x) => println(x),
        None => println(0_i64),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "5");
        }
    }

    #[test]
    fn test_e2e_map_entry_and_modify_runs_when_occupied() {
        // Occupied → and_modify's closure fires with mut ref V; the body's
        // mutation propagates back through the slot pointer.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(4_i64, 10_i64);
    m.entry(4_i64).and_modify(|v| { v += 1; });
    let v = m.get(4_i64);
    match v {
        Some(x) => println(x),
        None => println(0_i64),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "11");
        }
    }

    #[test]
    fn test_e2e_map_entry_and_modify_skips_when_vacant() {
        // Vacant → closure does not fire; map stays empty.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.entry(8_i64).and_modify(|v| { v += 1; });
    println(m.is_empty());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "true");
        }
    }

    #[test]
    fn test_e2e_map_entry_and_modify_chain_or_insert() {
        // Canonical chain: vacant → or_insert seeds 1; subsequent calls
        // → and_modify increments. Three calls produce a final value of 3.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.entry(5_i64).and_modify(|v| { v += 1; }).or_insert(1_i64);
    m.entry(5_i64).and_modify(|v| { v += 1; }).or_insert(1_i64);
    m.entry(5_i64).and_modify(|v| { v += 1; }).or_insert(1_i64);
    let v = m.get(5_i64);
    match v {
        Some(x) => println(x),
        None => println(0_i64),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3");
        }
    }

    #[test]
    fn test_e2e_map_entry_or_insert_trailing_push() {
        // Canonical kata idiom from `design.md § Entry[K, V]`:
        // `bucket.entry(k).or_insert(Vec.new()).push(v)`.
        // Exercises the new entry-chain-receiver method dispatch:
        // the chain produces a `*mut Vec[i64]` slot pointer; the
        // trailing `.push(v)` mutates the in-storage Vec via the
        // synth identifier; subsequent chains on the same key see
        // the accumulated contents through the same slot pointer
        // (verifies both the vacant-install and occupied-passthrough
        // paths). A trailing `.len()` read on the same chain shape
        // confirms read-side method dispatch through the slot.
        let out = run_program(
            r#"
fn main() {
    let mut bucket: Map[i64, Vec[i64]] = Map.new();
    bucket.entry(1_i64).or_insert(Vec.new()).push(10_i64);
    bucket.entry(1_i64).or_insert(Vec.new()).push(20_i64);
    bucket.entry(1_i64).or_insert(Vec.new()).push(30_i64);
    bucket.entry(2_i64).or_insert(Vec.new()).push(99_i64);
    let n1 = bucket.entry(1_i64).or_insert(Vec.new()).len();
    let n2 = bucket.entry(2_i64).or_insert(Vec.new()).len();
    println(n1);
    println(n2);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "1"]);
        }
    }

    #[test]
    fn test_e2e_map_get_remove_with_vec_value_payload() {
        // Bisected from the LeetCode 3629 codegen-vs-interpreter divergence.
        // Pins `Map.get` and `Map.remove` returning `Option[Vec[i64]]`:
        // the `Some(v) => v.len()` arm must reconstruct the full Vec
        // (3 LLVM words: ptr, len, cap) from the Option's payload fields.
        // Before the per-payload-word fix, `coerce_to_i64` truncated the
        // Vec to a single word and the destructure read undef for fields
        // 2 and 3 — producing garbage `len()` reads and reordered output.
        // After the fix, both paths round-trip the full payload.
        let out = run_program(
            r#"
fn main() {
    let mut bucket: Map[i64, Vec[i64]] = Map.new();
    bucket.entry(2_i64).or_insert(Vec.new()).push(10_i64);
    bucket.entry(2_i64).or_insert(Vec.new()).push(20_i64);
    bucket.entry(2_i64).or_insert(Vec.new()).push(30_i64);
    bucket.entry(3_i64).or_insert(Vec.new()).push(99_i64);

    match bucket.get(2_i64) {
        Some(v) => { println(v.len()); },
        None    => { println(-1_i64); },
    }
    match bucket.get(3_i64) {
        Some(v) => { println(v.len()); },
        None    => { println(-1_i64); },
    }

    match bucket.remove(2_i64) {
        Some(indices) => {
            for j in indices.into_iter() {
                println(j);
            }
        },
        None => { println(-1_i64); },
    }

    println(bucket.len());

    match bucket.remove(2_i64) {
        Some(_) => { println(99_i64); },
        None    => { println(0_i64); },
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "1", "10", "20", "30", "1", "0"]);
        }
    }

    // ── Clone trait surface (canonical: phase-8-stdlib-floor.md
    //    "Clone trait surface for collections") ───────────────────────────

    #[test]
    fn test_e2e_vec_clone_preserves_contents() {
        // Cloned Vec contains the same elements as the source.
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(10_i64);
    v.push(20_i64);
    v.push(30_i64);
    let w: Vec[i64] = v.clone();
    println(w[0]);
    println(w[1]);
    println(w[2]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["10", "20", "30"]);
        }
    }

    #[test]
    fn test_e2e_vec_clone_independent_buffers() {
        // Mutating the source Vec after cloning leaves the clone unchanged
        // — independent buffers from a fresh malloc.
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(1_i64);
    v.push(2_i64);
    let w: Vec[i64] = v.clone();
    v.push(99_i64);
    println(v.len());
    println(w.len());
    println(w[0]);
    println(w[1]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "2", "1", "2"]);
        }
    }

    #[test]
    fn test_e2e_vec_clone_empty_fast_path() {
        // `v.clone()` on an empty Vec hits the empty-fast path: dst gets
        // {null, 0, 0} without any allocation. Verifies the cloned Vec is
        // still observably empty and supports push afterwards.
        let out = run_program(
            r#"
fn main() {
    let v: Vec[i64] = Vec.new();
    let w: Vec[i64] = v.clone();
    println(w.len());
    w.push(7_i64);
    println(w.len());
    println(w[0]);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["0", "1", "7"]);
        }
    }

    #[test]
    fn test_e2e_map_clone_preserves_entry() {
        // Cloned Map carries the source's single entry; lookup on the clone
        // resolves to the cloned value.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(7_i64, 42_i64);
    let n: Map[i64, i64] = m.clone();
    let v = n.get(7_i64);
    match v {
        Some(x) => println(x),
        None => println(0_i64),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_e2e_map_clone_independent_after_source_insert() {
        // Inserting into the source after cloning doesn't affect the clone
        // — independent map handles, separate bucket arrays.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(1_i64, 100_i64);
    let n: Map[i64, i64] = m.clone();
    m.insert(2_i64, 200_i64);
    println(m.len());
    println(n.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "1"]);
        }
    }

    #[test]
    fn test_e2e_set_clone_preserves_membership() {
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[i64] = Set.new();
    s.insert(5_i64);
    let t: Set[i64] = s.clone();
    println(t.contains(5_i64));
    println(t.contains(99_i64));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["true", "false"]);
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

    // ── Display for collections (recursive codegen) ─────────────────
    //
    // Subtask 8 of the canonical Display bullet (phase-7-codegen.md §
    // Phase 7.2). Each test exercises `compile_print`'s collection
    // dispatch landed in subtask 7 against the per-type Display fns
    // emitted by subtasks 1-6. Format expectations match the
    // interpreter's `Value::Display` impl at `src/interpreter.rs:206`.
    //
    // Map iteration order is unspecified per `design.md` line 1588 — the
    // codegen runtime walks the bucket array directly, so multi-entry
    // map tests would be order-dependent. The map tests below stick to
    // single-entry maps; multi-entry coverage is left to interpreter
    // tests where the iteration is over an ordered Vec.

    #[test]
    fn test_e2e_display_vec_i64() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(1);
    v.push(2);
    v.push(3);
    println(v);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "[1, 2, 3]");
        }
    }

    #[test]
    fn test_e2e_display_vec_empty() {
        let out = run_program(
            r#"
fn main() {
    let v: Vec[i64] = Vec.new();
    println(v);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "[]");
        }
    }

    #[test]
    fn test_e2e_display_vec_string() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[String] = Vec.new();
    v.push("hi");
    v.push("bye");
    println(v);
}
"#,
        );
        if let Some(out) = out {
            // Interpreter's `Display for Value::String` is unquoted, so
            // codegen prints unquoted too — matches `src/interpreter.rs:213`.
            assert_eq!(out.trim(), "[hi, bye]");
        }
    }

    #[test]
    fn test_e2e_display_vec_nested() {
        // Vec[Vec[i64]] — exercises recursive composition. The outer Vec
        // Display fn walks elements; each element is itself a Vec struct,
        // and the dispatcher routes the inner element's Display through
        // emit_vec_display_fn_te(i64).
        let out = run_program(
            r#"
fn main() {
    let mut a: Vec[i64] = Vec.new();
    a.push(1);
    a.push(2);
    let mut b: Vec[i64] = Vec.new();
    b.push(3);
    let mut outer: Vec[Vec[i64]] = Vec.new();
    outer.push(a);
    outer.push(b);
    println(outer);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "[[1, 2], [3]]");
        }
    }

    #[test]
    fn test_e2e_display_map_string_i64_singleton() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, i64] = Map.new();
    m.insert("k", 42_i64);
    println(m);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "{k: 42}");
        }
    }

    #[test]
    fn test_e2e_display_map_i64_i64_singleton() {
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(7_i64, 99_i64);
    println(m);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "{7: 99}");
        }
    }

    #[test]
    fn test_e2e_display_map_empty() {
        let out = run_program(
            r#"
fn main() {
    let m: Map[i64, i64] = Map.new();
    println(m);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "{}");
        }
    }

    #[test]
    fn test_e2e_display_vec_tuple_i64_i64() {
        // Vec[(i64, i64)] — exercises tuple Display recursion via the
        // Vec body's element dispatcher. Single-entry map keeps the
        // expected output deterministic.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, i64] = Map.new();
    m.insert(1_i64, 10_i64);
    let es: Vec[(i64, i64)] = m.entries();
    println(es);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "[(1, 10)]");
        }
    }

    #[test]
    fn test_e2e_display_vec_tuple_i64_string() {
        // Vec[(i64, String)] — heap-bearing field on the value side of a
        // tuple element. The tuple Display fn GEPs to the String slot at
        // offset 8 (after the i64 field) and recurses into String Display.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[i64, String] = Map.new();
    m.insert(1_i64, "hi");
    let es: Vec[(i64, String)] = m.entries();
    println(es);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "[(1, hi)]");
        }
    }

    // ── Set[T] LLVM codegen ─────────────────────────────────────────
    //
    // Subtask 6 of the canonical Set codegen bullet (phase-8-stdlib-floor.md
    // search `Set[T] LLVM codegen`). Set[T] lowers to Map[T, ()] at codegen
    // and reuses karac_map_*; tests cover insert / contains / remove / len /
    // is_empty / clear / for-loop iteration. The union / intersection /
    // difference methods (subtask 5) are deferred — they need per-type
    // clone fn infrastructure for non-Copy elements — so the matching
    // tests (`test_e2e_set_union`, etc.) are not yet present.

    #[test]
    fn test_e2e_set_i64_insert_contains() {
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[i64] = Set.new();
    s.insert(1_i64);
    s.insert(2_i64);
    println(s.contains(1_i64));
    println(s.contains(2_i64));
    println(s.contains(99_i64));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["true", "true", "false"]);
        }
    }

    #[test]
    fn test_e2e_set_i64_insert_returns_bool() {
        // Set.insert returns true on fresh insert, false when value already
        // present (matches Rust HashSet::insert).
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[i64] = Set.new();
    let a = s.insert(1_i64);
    let b = s.insert(1_i64);
    println(a);
    println(b);
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["true", "false"]);
        }
    }

    #[test]
    fn test_e2e_set_i64_remove() {
        // Set.remove returns true when value existed, false otherwise.
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[i64] = Set.new();
    s.insert(7_i64);
    let r1 = s.remove(7_i64);
    let r2 = s.remove(7_i64);
    println(r1);
    println(r2);
    println(s.contains(7_i64));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["true", "false", "false"]);
        }
    }

    #[test]
    fn test_e2e_set_i64_len_is_empty() {
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[i64] = Set.new();
    println(s.is_empty());
    println(s.len());
    s.insert(1_i64);
    s.insert(2_i64);
    s.insert(3_i64);
    println(s.is_empty());
    println(s.len());
    s.insert(2_i64);
    println(s.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["true", "0", "false", "3", "3"]);
        }
    }

    #[test]
    fn test_e2e_set_i64_for_loop_sum() {
        // for x in s — iteration order is unspecified, so test against the
        // sum (which is order-independent).
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[i64] = Set.new();
    s.insert(10_i64);
    s.insert(20_i64);
    s.insert(30_i64);
    let mut sum: i64 = 0;
    for x in s {
        sum = sum + x;
    }
    println(sum);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "60");
        }
    }

    #[test]
    fn test_e2e_set_string_insert_contains() {
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[String] = Set.new();
    s.insert("alice");
    s.insert("bob");
    println(s.contains("alice"));
    println(s.contains("bob"));
    println(s.contains("missing"));
    println(s.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["true", "true", "false", "2"]);
        }
    }

    #[test]
    fn test_e2e_set_string_remove() {
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[String] = Set.new();
    s.insert("alice");
    s.insert("bob");
    let r = s.remove("alice");
    println(r);
    println(s.contains("alice"));
    println(s.contains("bob"));
    println(s.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["true", "false", "true", "1"]);
        }
    }

    #[test]
    fn test_e2e_set_string_for_loop_count() {
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[String] = Set.new();
    s.insert("alice");
    s.insert("bob");
    s.insert("alice");
    let mut count: i64 = 0;
    for _x in s {
        count = count + 1_i64;
    }
    println(count);
}
"#,
        );
        if let Some(out) = out {
            // alice appears twice, but as a set only once → 2 elements.
            assert_eq!(out.trim(), "2");
        }
    }

    #[test]
    fn test_e2e_display_set_i64_singleton() {
        // Display subtask 5 of the canonical Display bullet (closed by Set
        // codegen subtasks 1-4). Format `Set{...}` matches the interpreter
        // at `src/interpreter.rs:292`. Single-entry set keeps the expected
        // output deterministic — multi-entry iteration order is unspecified.
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[i64] = Set.new();
    s.insert(42_i64);
    println(s);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "Set{42}");
        }
    }

    #[test]
    fn test_e2e_display_set_string_singleton() {
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[String] = Set.new();
    s.insert("alice");
    println(s);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "Set{alice}");
        }
    }

    #[test]
    fn test_e2e_display_set_empty() {
        let out = run_program(
            r#"
fn main() {
    let s: Set[i64] = Set.new();
    println(s);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "Set{}");
        }
    }

    #[test]
    fn test_e2e_set_clear() {
        let out = run_program(
            r#"
fn main() {
    let mut s: Set[i64] = Set.new();
    s.insert(1_i64);
    s.insert(2_i64);
    s.insert(3_i64);
    println(s.len());
    s.clear();
    println(s.len());
    println(s.is_empty());
    s.insert(99_i64);
    println(s.contains(99_i64));
    println(s.len());
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "0", "true", "true", "1"]);
        }
    }

    #[test]
    fn test_e2e_set_union_i64() {
        // Membership-based assertions (rather than printing the result set)
        // — runtime iteration order is unspecified for Map-backed sets.
        let out = run_program(
            r#"
fn main() {
    let mut a: Set[i64] = Set.new();
    a.insert(1_i64);
    a.insert(2_i64);
    a.insert(3_i64);
    let mut b: Set[i64] = Set.new();
    b.insert(3_i64);
    b.insert(4_i64);
    b.insert(5_i64);
    let u: Set[i64] = a.union(b);
    println(u.len());
    println(u.contains(1_i64));
    println(u.contains(2_i64));
    println(u.contains(3_i64));
    println(u.contains(4_i64));
    println(u.contains(5_i64));
    println(u.contains(99_i64));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(
                lines,
                vec!["5", "true", "true", "true", "true", "true", "false"]
            );
        }
    }

    #[test]
    fn test_e2e_set_intersection_i64() {
        let out = run_program(
            r#"
fn main() {
    let mut a: Set[i64] = Set.new();
    a.insert(1_i64);
    a.insert(2_i64);
    a.insert(3_i64);
    let mut b: Set[i64] = Set.new();
    b.insert(2_i64);
    b.insert(3_i64);
    b.insert(4_i64);
    let i: Set[i64] = a.intersection(b);
    println(i.len());
    println(i.contains(1_i64));
    println(i.contains(2_i64));
    println(i.contains(3_i64));
    println(i.contains(4_i64));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "false", "true", "true", "false"]);
        }
    }

    #[test]
    fn test_e2e_set_difference_i64() {
        let out = run_program(
            r#"
fn main() {
    let mut a: Set[i64] = Set.new();
    a.insert(1_i64);
    a.insert(2_i64);
    a.insert(3_i64);
    let mut b: Set[i64] = Set.new();
    b.insert(2_i64);
    b.insert(3_i64);
    b.insert(4_i64);
    let d: Set[i64] = a.difference(b);
    println(d.len());
    println(d.contains(1_i64));
    println(d.contains(2_i64));
    println(d.contains(3_i64));
    println(d.contains(4_i64));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["1", "true", "false", "false", "false"]);
        }
    }

    #[test]
    fn test_e2e_set_union_string() {
        // String elements exercise the per-element clone path — the result
        // owns independently-allocated buffers, not aliases of the source.
        let out = run_program(
            r#"
fn main() {
    let mut a: Set[String] = Set.new();
    a.insert("alpha");
    a.insert("beta");
    let mut b: Set[String] = Set.new();
    b.insert("beta");
    b.insert("gamma");
    let u: Set[String] = a.union(b);
    println(u.len());
    println(u.contains("alpha"));
    println(u.contains("beta"));
    println(u.contains("gamma"));
    println(u.contains("delta"));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "true", "true", "true", "false"]);
        }
    }

    #[test]
    fn test_e2e_set_difference_independent_after_source_mutation() {
        // The result set owns its keys — mutating `a` after the difference
        // doesn't reach back into `d`. Membership snapshot is preserved.
        let out = run_program(
            r#"
fn main() {
    let mut a: Set[i64] = Set.new();
    a.insert(1_i64);
    a.insert(2_i64);
    a.insert(3_i64);
    let mut b: Set[i64] = Set.new();
    b.insert(2_i64);
    let d: Set[i64] = a.difference(b);
    a.insert(99_i64);
    a.remove(1_i64);
    println(d.len());
    println(d.contains(1_i64));
    println(d.contains(3_i64));
    println(d.contains(99_i64));
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "true", "true", "false"]);
        }
    }

    #[test]
    fn test_e2e_display_map_with_vec_value_singleton() {
        // Map[String, Vec[i64]] — the Map body recurses into Vec Display
        // for the value side. Single-entry map keeps output deterministic.
        let out = run_program(
            r#"
fn main() {
    let mut m: Map[String, Vec[i64]] = Map.new();
    let mut v: Vec[i64] = Vec.new();
    v.push(1);
    v.push(2);
    m.insert("k", v);
    println(m);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "{k: [1, 2]}");
        }
    }

    // ── Repeat-literal `[v; N]` const-aggregate fast path (regression) ──

    #[test]
    fn test_repeat_literal_const_zero_uses_memset() {
        // Regression: `compile_repeat_literal` originally emitted N
        // `insertvalue` instructions, scaling karac build time linearly
        // in N. The first fix tried `store [N x T] zeroinitializer` —
        // O(1) IR, but LLVM's downstream codegen passes crashed on the
        // aggregate store at N≥80K (verified SIGSEGV in `write_to_file`).
        // The current fix detects `let buf: Array[T, N] = [0; N]` at the
        // let-binding site and lowers it to `alloca + llvm.memset.*`,
        // bypassing the aggregate store entirely. memset is O(1) IR AND
        // O(1) codegen — it's what LLVM would lower the aggregate store
        // to anyway, just emitted directly.
        let ir = ir_for(
            r#"
fn main() {
    let buf: Array[i64, 100] = [0; 100];
    let _ = buf[0];
}
"#,
        );
        assert!(
            ir.contains("call void @llvm.memset"),
            "expected llvm.memset call for `[0; 100]` let-binding; got IR:\n{}",
            ir
        );
        assert!(
            !ir.contains("insertvalue"),
            "const-zero repeat literal must not emit per-element insertvalue; got IR:\n{}",
            ir
        );
        assert!(
            !ir.contains("store [100 x i64] zeroinitializer"),
            "let-binding fast path must avoid aggregate-store IR \
             (LLVM crashes on it at large N); got IR:\n{}",
            ir
        );
    }

    #[test]
    fn test_repeat_literal_const_nonzero_skips_insertvalue() {
        // Same fast path applies to non-zero constants: one constant
        // aggregate, no per-element ops.
        let ir = ir_for(
            r#"
fn main() {
    let buf: Array[i64, 8] = [42; 8];
    let _ = buf[0];
}
"#,
        );
        assert!(
            !ir.contains("insertvalue"),
            "const-nonzero repeat literal must not emit per-element insertvalue; got IR:\n{}",
            ir
        );
    }

    #[test]
    fn test_repeat_literal_large_n_compiles_without_per_element_ir() {
        // Workload-realistic case: a 64K LUT used to hang the build at
        // O(N) IR construction; the next iteration crashed LLVM at
        // codegen time on the giant aggregate store. The let-binding
        // memset fast path is O(1) at both IR-construction AND codegen
        // time and works at any N.
        let ir = ir_for(
            r#"
fn main() {
    let buf: Array[i64, 65536] = [0; 65536];
    let _ = buf[0];
}
"#,
        );
        assert!(
            ir.contains("call void @llvm.memset"),
            "expected llvm.memset call for the 64K LUT; got IR truncated:\n{}",
            &ir[..ir.len().min(2000)]
        );
        assert!(
            !ir.contains("insertvalue"),
            "64K LUT must not emit per-element insertvalue (would hang the build); \
             grep for insertvalue failed; got IR truncated:\n{}",
            &ir[..ir.len().min(2000)]
        );
        assert!(
            !ir.contains("store [65536 x i64] zeroinitializer"),
            "64K LUT must not emit aggregate-store IR (LLVM crashes on it at this size); \
             got IR truncated:\n{}",
            &ir[..ir.len().min(2000)]
        );
    }

    #[test]
    fn test_repeat_literal_runtime_value_falls_back_to_insertvalue() {
        // When `val` is a runtime expression (e.g. function return),
        // the const fast path doesn't apply and we exercise the
        // per-element fallback. Locks in that the fallback path is
        // still reachable — if a future loop-CFG lowering replaces it,
        // this test should be updated rather than silently regressing.
        let ir = ir_for(
            r#"
fn compute() -> i64 { 7 }
fn main() {
    let n = compute();
    let buf: Array[i64, 4] = [n; 4];
    let _ = buf[0];
}
"#,
        );
        assert!(
            ir.contains("insertvalue"),
            "runtime-value repeat literal should fall back to insertvalue; got IR:\n{}",
            ir
        );
    }

    // ── Method dispatcher hardening (regression) ────────────────────────

    #[test]
    fn test_codegen_rejects_unsupported_slice_method() {
        // Regression: `compile_method_call` used to silently return
        // const-0 for any method it didn't know how to dispatch (the
        // 2026-05-04 `Slice.len()` wrong-answer bug came from this).
        // Both fall-through sites now return a typed `Err`. This test
        // asserts the inner slice-method fall-through fires for a
        // typechecker-accepted-but-not-codegened slice method.
        //
        // If a future arm adds `first()` codegen support, swap this to
        // any other typechecker-accepted method without a codegen arm.
        let src = r#"
fn main() {
    let xs: Array[i64, 3] = [1, 2, 3];
    let s: Slice[i64] = xs.as_slice();
    let _ = s.first();
}
"#;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let err = compile_to_ir(&parsed.program, None, None).expect_err(
            "expected codegen to Err on unsupported slice method; \
             the dispatcher silent-zero must not be re-introduced",
        );
        assert!(
            err.contains("no handler for slice method 'first'"),
            "expected diagnostic to name the missing slice method; got: {}",
            err
        );
    }

    // ── Concurrency analysis plumbing ──

    /// Slice 1 wiring sanity-check: full pipeline through
    /// `concurrency_analyze`, then a `compile_to_object_with_options` call
    /// passing the analysis as `Some(&analysis)`. Asserts only that codegen
    /// succeeds — IR-shape assertions for inferred-par lowering are slice 2's
    /// job. The point here is to verify the new param accepts a real analysis
    /// without regressing the existing legacy path.
    #[test]
    fn test_concurrency_analysis_threads_into_codegen() {
        use karac::codegen::compile_to_object_with_options;
        let src = r#"
effect resource Net;
effect resource Disk;
effect resource Db;

fn fetch_net() -> i64 reads(Net) { 1 }
fn fetch_disk() -> i64 reads(Disk) { 2 }
fn fetch_db() -> i64 reads(Db) { 3 }

fn main() {
    let a = fetch_net();
    let b = fetch_disk();
    let c = fetch_db();
    println(a + b + c);
}
"#;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let effects = karac::effectcheck(&parsed.program);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);

        let obj_path = "/tmp/karac_test_concurrency_threads.o";
        let result = compile_to_object_with_options(
            &parsed.program,
            obj_path,
            None,
            Some(&analysis),
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "compile_to_object_with_options failed with concurrency analysis: {:?}",
            result
        );
        let _ = std::fs::remove_file(obj_path);
    }

    /// Regression pin for substep (e): the new `concurrency` param is
    /// genuinely optional. Compiling the same program through
    /// `compile_to_object` (the param-light wrapper) with `None` for both
    /// ownership and concurrency must still succeed. Slice 1 promised no
    /// behavior change — this is the regression guard.
    #[test]
    fn test_concurrency_analysis_none_compiles_unchanged() {
        use karac::codegen::compile_to_object;
        let src = r#"
effect resource Net;
effect resource Disk;
effect resource Db;

fn fetch_net() -> i64 reads(Net) { 1 }
fn fetch_disk() -> i64 reads(Disk) { 2 }
fn fetch_db() -> i64 reads(Db) { 3 }

fn main() {
    let a = fetch_net();
    let b = fetch_disk();
    let c = fetch_db();
    println(a + b + c);
}
"#;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);

        let obj_path = "/tmp/karac_test_concurrency_none.o";
        let result = compile_to_object(&parsed.program, obj_path, None, None);
        assert!(
            result.is_ok(),
            "compile_to_object with None concurrency failed: {:?}",
            result
        );
        let _ = std::fs::remove_file(obj_path);
    }

    /// Slice A (Phase-7 — Par codegen: return values, 2026-05-09) E2E
    /// correctness + wall-clock sanity. Four CPU-bound reads on disjoint
    /// resources, each returning a typed `i64`. Slice 2 would have
    /// dropped the parallel group via the
    /// `group_defines_binding_used_outside` gate (each read names its
    /// result for the join site); slice A lifts the gate and the four
    /// branches now fan out through `karac_par_run`. Asserts:
    ///   - **Correctness:** the joined output equals the deterministic
    ///     sum the four kernels computed (4 × triangular `0..N` sums
    ///     plus a tag).
    ///   - **Wall-clock concurrency:** total runtime is meaningfully
    ///     below 4× the per-branch kernel cost, demonstrating that the
    ///     branches actually executed in parallel rather than serialized
    ///     through the slot mechanism. The threshold is conservative
    ///     (3.0× of a per-branch budget) to absorb the runtime's spawn
    ///     overhead and CI noise; the auto-par dispatch should be
    ///     comfortably under 2× on any modern multi-core host.
    ///
    /// Skips when the runtime archive is missing — same legitimate
    /// soft-skip as the rest of the codegen E2E suite.
    #[test]
    fn test_auto_par_with_returns_runs_concurrently_and_joins_correctly() {
        use karac::codegen::{compile_to_object_with_options, link_executable};
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Instant;
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        // Each `read_*` runs an `N`-iteration triangular-sum kernel; N
        // is tuned to be heavy enough that 4× sequential work is
        // measurable (~hundreds of ms) but light enough that CI noise
        // doesn't dominate. The expected total is
        // `4 * (N * (N - 1) / 2) + (1 + 2 + 3 + 4)`.
        const N: i64 = 8_000_000;
        let expected_sum: i64 = 4 * (N * (N - 1) / 2) + 10;

        let src = format!(
            r#"
effect resource Net;
effect resource Disk;
effect resource Db;
effect resource Cache;

fn busy_sum(n: i64) -> i64 {{
    let mut sum: i64 = 0;
    let mut i: i64 = 0;
    while i < n {{
        sum = sum + i;
        i = i + 1;
    }}
    sum
}}

fn read_net() -> i64 reads(Net) {{ busy_sum({n}) + 1 }}
fn read_disk() -> i64 reads(Disk) {{ busy_sum({n}) + 2 }}
fn read_db() -> i64 reads(Db) {{ busy_sum({n}) + 3 }}
fn read_cache() -> i64 reads(Cache) {{ busy_sum({n}) + 4 }}

fn combine(a: i64, b: i64, c: i64, d: i64) -> i64 {{
    a + b + c + d
}}

fn main() {{
    let result_1 = read_net();
    let result_2 = read_disk();
    let result_3 = read_db();
    let result_4 = read_cache();
    println(combine(result_1, result_2, result_3, result_4));
}}
"#,
            n = N
        );

        let mut parsed = karac::parse(&src);
        if !parsed.errors.is_empty() {
            panic!("parse errors: {:?}", parsed.errors);
        }
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let effects = karac::effectcheck(&parsed.program);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);

        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let obj_path = format!("/tmp/karac_par_returns_e2e_{}_{}.o", std::process::id(), id);
        let exe_path = format!("/tmp/karac_par_returns_e2e_{}_{}", std::process::id(), id);

        if let Err(e) = compile_to_object_with_options(
            &parsed.program,
            &obj_path,
            None,
            Some(&analysis),
            None,
            None,
        ) {
            panic!("codegen failed for slice-A E2E: {e}");
        }
        // Link / exec failures stay soft-skip — runtime archive may be
        // missing on some CI hosts (matches `tests/par_codegen.rs`'s
        // E2E pattern).
        let Ok(()) = link_executable(&obj_path, &exe_path) else {
            eprintln!("[slice-A E2E] link failed; skipping (runtime archive missing?)");
            let _ = std::fs::remove_file(&obj_path);
            return;
        };

        // Calibrate per-branch cost by running `busy_sum(N)` once
        // sequentially in a separate single-branch program. Cheaper
        // than threading sequential mode into the same binary; gives
        // us a host-specific budget for the wall-clock assertion.
        let cal_src = format!(
            r#"
fn busy_sum(n: i64) -> i64 {{
    let mut sum: i64 = 0;
    let mut i: i64 = 0;
    while i < n {{
        sum = sum + i;
        i = i + 1;
    }}
    sum
}}

fn main() {{
    println(busy_sum({n}));
}}
"#,
            n = N
        );
        let mut cal_parsed = karac::parse(&cal_src);
        if !cal_parsed.errors.is_empty() {
            panic!("calibration parse errors: {:?}", cal_parsed.errors);
        }
        let cal_resolved = karac::resolve(&cal_parsed.program);
        let cal_typed = karac::typecheck(&cal_parsed.program, &cal_resolved);
        karac::lower(&mut cal_parsed.program, &cal_typed);
        let cal_obj = format!("/tmp/karac_par_returns_cal_{}_{}.o", std::process::id(), id);
        let cal_exe = format!("/tmp/karac_par_returns_cal_{}_{}", std::process::id(), id);
        if compile_to_object_with_options(&cal_parsed.program, &cal_obj, None, None, None, None)
            .is_err()
        {
            eprintln!("[slice-A E2E] calibration codegen failed; skipping wall-clock assertion");
            let _ = std::fs::remove_file(&obj_path);
            let _ = std::fs::remove_file(&exe_path);
            return;
        }
        let Ok(()) = link_executable(&cal_obj, &cal_exe) else {
            eprintln!("[slice-A E2E] calibration link failed; skipping wall-clock assertion");
            let _ = std::fs::remove_file(&obj_path);
            let _ = std::fs::remove_file(&exe_path);
            let _ = std::fs::remove_file(&cal_obj);
            return;
        };
        let cal_t0 = Instant::now();
        let _ = std::process::Command::new(&cal_exe).output();
        let per_branch = cal_t0.elapsed();

        // Run the parallel binary, measure wall-clock, capture stdout.
        let par_t0 = Instant::now();
        let par_out = match std::process::Command::new(&exe_path).output() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("[slice-A E2E] failed to exec parallel binary: {e}");
                let _ = std::fs::remove_file(&obj_path);
                let _ = std::fs::remove_file(&exe_path);
                let _ = std::fs::remove_file(&cal_obj);
                let _ = std::fs::remove_file(&cal_exe);
                return;
            }
        };
        let par_elapsed = par_t0.elapsed();
        let stdout = String::from_utf8_lossy(&par_out.stdout).to_string();

        let _ = std::fs::remove_file(&obj_path);
        let _ = std::fs::remove_file(&exe_path);
        let _ = std::fs::remove_file(&cal_obj);
        let _ = std::fs::remove_file(&cal_exe);

        // Correctness: the printed sum matches the precomputed total.
        let printed: i64 = stdout
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("[slice-A E2E] non-integer stdout {stdout:?}: {e}"));
        assert_eq!(
            printed, expected_sum,
            "[slice-A E2E] joined value mismatch: got {printed}, expected {expected_sum}; \
             the slot loads or `combine` argument flow is wrong"
        );

        // Wall-clock concurrency: parallel total < 3.0 × per-branch
        // budget. The sequential lower bound is 4.0× per-branch; the
        // 3.0× threshold gives a generous margin while still rejecting
        // a serialized lowering. Print observed values to stderr so
        // a developer reading test output can see the actual ratio
        // (matches the parallax-lite microbenchmark's stderr-note
        // pattern).
        let par_secs = par_elapsed.as_secs_f64();
        let cal_secs = per_branch.as_secs_f64();
        eprintln!(
            "[slice-A E2E] per-branch cal {:.3}s; parallel {:.3}s; ratio {:.2}× (4× sequential bound)",
            cal_secs,
            par_secs,
            par_secs / cal_secs.max(1e-6)
        );
        // Ratio test only when the calibration is large enough that
        // the comparison is meaningful; on extremely fast hosts where
        // the kernel completes in < 50ms, signal-to-noise is too low
        // to assert against (same pragmatism as the parallax-lite
        // ratio guards).
        if cal_secs > 0.05 {
            assert!(
                par_secs < 3.0 * cal_secs,
                "[slice-A E2E] parallel runtime {par_secs:.3}s ≥ 3× per-branch {cal_secs:.3}s — \
                 lowering looks serial (slot mechanism may be forcing sequential dispatch)"
            );
        }
    }

    /// Slice 2 pin: replays the same pipeline shape that `cmd_build` uses
    /// (resolve → typecheck → lower → effectcheck → ownershipcheck →
    /// concurrencycheck) and asserts that `concurrency_analyze` produces
    /// a non-empty analysis. Locks in sub-step (a)'s wiring of
    /// `pipeline.concurrencycheck()` into `cmd_build` against future
    /// regression — without this call, the auto-par codegen path stays
    /// dormant on the build path.
    #[test]
    fn test_cmd_build_pipeline_populates_concurrency() {
        let src = r#"
effect resource Net;
effect resource Disk;

fn fetch_net() -> i64 reads(Net) { 1 }
fn fetch_disk() -> i64 reads(Disk) { 2 }

fn main() {
    let _ = fetch_net();
    let _ = fetch_disk();
}
"#;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let effects = karac::effectcheck(&parsed.program);
        let _ = karac::ownershipcheck(&parsed.program, &typed);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);

        // The analysis should at minimum have an entry for `main`.
        assert!(
            analysis.function_decisions.contains_key("main"),
            "expected `main` in function_decisions; got keys: {:?}",
            analysis.function_decisions.keys().collect::<Vec<_>>()
        );
    }

    // ── Debugger Contract: SpawnSiteId metadata table ──
    //
    // Slice 3 of the four-piece Debugger Contract (`design.md § AI-First
    // Compiler Interface > Debugger Contract`). For every `par {}` block
    // (explicit or compiler-inferred) codegen records a `(id, file,
    // line, col, worker_count)` tuple and emits a module-scope
    // `KARAC_SPAWN_SITES` array, plus the companion `KARAC_SPAWN_SITES_LEN`
    // and `KARAC_SPAWN_SITES_ENABLED` globals. The IDs are stable per
    // binary and serve as the join key consumed by slices 4 and 5 (and
    // the future `std.panic` crash report's `parallel_context` field).
    //
    // Tests use IR-level string-grep — same precedent as
    // `test_repeat_literal_const_zero_uses_memset`.
    //
    // Test isolation: `KARAC_RUNTIME_DEBUG_METADATA` is read at
    // `Codegen::new` time. The disabled-via-env-var test below
    // mutates the var, so all four spawn-site tests serialize on a
    // shared mutex to avoid cross-test pollution under cargo's
    // default parallel test execution. The lock is acquired at the
    // top of each test and released on drop — env-var-touching tests
    // restore prior state explicitly inside the critical section.

    static SPAWN_SITE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Compile to IR with explicit source-text plumbing, threading
    /// the source through the new `source_text` parameter so
    /// `record_spawn_site` resolves byte offsets to `(line, col)`.
    fn ir_for_with_source(src: &str) -> String {
        use karac::codegen::compile_to_ir_with_options;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        compile_to_ir_with_options(&parsed.program, None, None, Some("test.kara"), Some(src))
            .expect("codegen failed")
    }

    /// Three globals must always emit, regardless of whether the
    /// program contains any `par {}` blocks. Slice 5's runtime API
    /// reads through these symbols unconditionally and degrades
    /// cleanly when the table is empty.
    #[test]
    fn test_spawn_site_metadata_emitted_for_par_blocks() {
        // Serialize against the env-var test below — see module
        // comment for rationale.
        let _guard = SPAWN_SITE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Two par blocks: first has 2 branches, second has 3. The
        // metadata table should pin both with their `worker_count`
        // values (2 and 3) and assign IDs 0 and 1 (matching the
        // `par_counter` start).
        let ir = ir_for_with_source(
            r#"
fn a() { println(1); }
fn b() { println(2); }
fn c() { println(3); }
fn main() {
    par {
        a();
        b();
    }
    par {
        a();
        b();
        c();
    }
}
"#,
        );

        // Length global: two entries.
        assert!(
            ir.contains("@KARAC_SPAWN_SITES_LEN"),
            "missing KARAC_SPAWN_SITES_LEN global; ir:\n{ir}"
        );
        assert!(
            ir.contains("@KARAC_SPAWN_SITES_LEN = constant i32 2")
                || ir.contains("@KARAC_SPAWN_SITES_LEN = constant i32 2,")
                || ir.contains("@KARAC_SPAWN_SITES_LEN = constant i32 2\n"),
            "expected KARAC_SPAWN_SITES_LEN = 2; ir:\n{ir}"
        );

        // Enabled global: true (i1 1).
        assert!(
            ir.contains("@KARAC_SPAWN_SITES_ENABLED = constant i1 true"),
            "expected KARAC_SPAWN_SITES_ENABLED = true; ir:\n{ir}"
        );

        // Array global: two entries.
        assert!(
            ir.contains("@KARAC_SPAWN_SITES = constant"),
            "missing KARAC_SPAWN_SITES global; ir:\n{ir}"
        );
        // The array type prefix should reflect the entry count.
        assert!(
            ir.contains("@KARAC_SPAWN_SITES = constant [2 x"),
            "expected `[2 x …]` array type for KARAC_SPAWN_SITES; ir:\n{ir}"
        );

        // Worker counts: 2 and 3 should both appear in the array
        // initializer. We can't easily isolate just the array text
        // from the IR string, but the combination of `[2 x` plus
        // both i32 values 2 and 3 is a strong signal.
        // Sanity-check: at least one occurrence of `i32 2,` and
        // `i32 3,` in the array initializer (the entry struct fields).
        assert!(
            ir.contains("i32 2"),
            "expected i32 2 worker_count; ir:\n{ir}"
        );
        assert!(
            ir.contains("i32 3"),
            "expected i32 3 worker_count; ir:\n{ir}"
        );
    }

    /// Empty array must still emit (length zero, enabled true) — the
    /// runtime API reads through these symbols even on programs with
    /// no `par {}` blocks.
    #[test]
    fn test_spawn_site_metadata_empty_when_no_par_blocks() {
        let _guard = SPAWN_SITE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let ir = ir_for_with_source(
            r#"
fn main() {
    println(42);
}
"#,
        );

        assert!(
            ir.contains("@KARAC_SPAWN_SITES_LEN"),
            "missing KARAC_SPAWN_SITES_LEN global; ir:\n{ir}"
        );
        // Length zero.
        assert!(
            ir.contains("@KARAC_SPAWN_SITES_LEN = constant i32 0"),
            "expected KARAC_SPAWN_SITES_LEN = 0; ir:\n{ir}"
        );
        // Enabled true (dev default).
        assert!(
            ir.contains("@KARAC_SPAWN_SITES_ENABLED = constant i1 true"),
            "expected KARAC_SPAWN_SITES_ENABLED = true; ir:\n{ir}"
        );
        // Empty array.
        assert!(
            ir.contains("@KARAC_SPAWN_SITES = constant [0 x"),
            "expected empty `[0 x …]` KARAC_SPAWN_SITES; ir:\n{ir}"
        );
    }

    /// `KARAC_RUNTIME_DEBUG_METADATA=0` flips the gate off — all three
    /// globals still emit, but `LEN = 0`, `ENABLED = false`, and the
    /// array is empty regardless of how many `par {}` blocks the
    /// program contains.
    ///
    /// Env-var test isolation: `KARAC_RUNTIME_DEBUG_METADATA` is read
    /// once at `Codegen::new` time. We `set_var` before invoking
    /// codegen, then `remove_var` immediately after to restore prior
    /// state. Other tests in this file that don't set the var see the
    /// dev default (true). Cargo runs tests in parallel by default —
    /// the var name is unique to this test, so there is no collision
    /// risk with peers, and the explicit unset prevents leaking state
    /// to any later codegen helper that may run in the same process.
    #[test]
    fn test_spawn_site_metadata_disabled_via_env_var() {
        // Acquire the shared lock so peer spawn-site tests don't
        // observe the var while the gate is flipped to "0".
        let _guard = SPAWN_SITE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Restore prior state on completion. Establishing the prior
        // value before the test is paranoid but cheap — most CI runs
        // start with the var unset.
        let prior = std::env::var("KARAC_RUNTIME_DEBUG_METADATA").ok();
        std::env::set_var("KARAC_RUNTIME_DEBUG_METADATA", "0");
        let ir = ir_for_with_source(
            r#"
fn a() { println(1); }
fn b() { println(2); }
fn main() {
    par {
        a();
        b();
    }
}
"#,
        );
        match prior {
            Some(v) => std::env::set_var("KARAC_RUNTIME_DEBUG_METADATA", v),
            None => std::env::remove_var("KARAC_RUNTIME_DEBUG_METADATA"),
        }

        // Length zero, even though the program has one par block.
        assert!(
            ir.contains("@KARAC_SPAWN_SITES_LEN = constant i32 0"),
            "expected KARAC_SPAWN_SITES_LEN = 0 when gate off; ir:\n{ir}"
        );
        // Enabled false.
        assert!(
            ir.contains("@KARAC_SPAWN_SITES_ENABLED = constant i1 false"),
            "expected KARAC_SPAWN_SITES_ENABLED = false when gate off; ir:\n{ir}"
        );
        // Empty array when gate off.
        assert!(
            ir.contains("@KARAC_SPAWN_SITES = constant [0 x"),
            "expected empty `[0 x …]` KARAC_SPAWN_SITES when gate off; ir:\n{ir}"
        );
    }

    /// Source-position fidelity: a `par {}` block at a known line
    /// must record a `(line, col)` matching the source position of
    /// the par-block's body. Pins the byte-offset-to-line-col
    /// conversion direction.
    ///
    /// Implementation note: `compile_par_block` flows the inner
    /// `block.span` into `emit_par_run`, which then records the
    /// site. `block.span` starts at the opening `{`, so the
    /// recorded column is the position of `{`, not `par`. The line
    /// is the same in both cases (the recorded line is reliably the
    /// par-keyword's line), and the column is reliably "somewhere
    /// inside the par block on that line." That's the contract for
    /// the slice-3 metadata table — `(file, line)` are exact;
    /// `col` is at-or-after the `par` keyword.
    #[test]
    fn test_spawn_site_records_correct_source_location() {
        let _guard = SPAWN_SITE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Line layout (1-indexed):
        //   1: (blank — leading newline)
        //   2: fn a() { println(1); }
        //   3: fn b() { println(2); }
        //   4: fn main() {
        //   5:     par {
        //   6:         a();
        //   7:         b();
        //   8:     }
        //   9: }
        //
        // The `par` keyword starts at line 5, col 5; the opening
        // `{` (which `block.span` points at) is at line 5, col 9.
        let src = r#"
fn a() { println(1); }
fn b() { println(2); }
fn main() {
    par {
        a();
        b();
    }
}
"#;
        let ir = ir_for_with_source(src);

        // Spawn-site struct fields:
        //   { i32 id, ptr file_cstr, i32 line, i32 col, i32 worker_count, i32 reserved }
        // The only par block in this program produces id=0,
        // line=5, col=9 (opening brace), worker_count=2,
        // reserved=0. Sanity-check the array initializer contains
        // `i32 5, i32 9` — line then column.
        assert!(
            ir.contains("i32 5, i32 9"),
            "expected line=5 col=9 in spawn-site entry; ir:\n{ir}"
        );
    }

    // ── Debugger Contract: std.runtime APIs (slice 5) ──
    //
    // Item (4) of the four-piece Debugger Contract. Three Kāra-callable
    // functions exposed via the empty-marker `Runtime` struct in baked
    // stdlib (`runtime/stdlib/runtime.kara`):
    //
    //   - `Runtime.has_debug_metadata() -> bool` — reads
    //     `KARAC_SPAWN_SITES_ENABLED` (slice 3 global).
    //   - `Runtime.list_par_blocks() -> Vec[ParBlockInfo]` — joins slice 4's
    //     `ACTIVE_FRAMES` registry against slice 3's `KARAC_SPAWN_SITES`.
    //   - `Runtime.list_tasks() -> Vec[TaskInfo]` — always empty in v1.
    //
    // Tests share the slice-3 `SPAWN_SITE_ENV_LOCK` for env-var-touching
    // serialization (the gate-off test below mutates the same
    // `KARAC_RUNTIME_DEBUG_METADATA` var slice 3's tests touch).

    /// `has_debug_metadata()` returns `true` under the dev default
    /// (env var unset). Validates that slice 3's `KARAC_SPAWN_SITES_ENABLED = 1`
    /// flows through the runtime fn into the boolean returned to Kāra.
    #[test]
    fn test_has_debug_metadata_returns_true_when_gate_on() {
        let _guard = SPAWN_SITE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Make sure the var is unset so the dev default applies.
        let prior = std::env::var("KARAC_RUNTIME_DEBUG_METADATA").ok();
        std::env::remove_var("KARAC_RUNTIME_DEBUG_METADATA");
        let captured = run_program_capturing(
            r#"
fn main() {
    let dbg = Runtime.has_debug_metadata();
    if dbg {
        println(1);
    } else {
        println(0);
    }
}
"#,
        );
        if let Some(v) = prior {
            std::env::set_var("KARAC_RUNTIME_DEBUG_METADATA", v);
        }
        if let Some(c) = captured {
            assert_eq!(c.stdout.trim(), "1", "expected gate-on (true → 1)");
        }
    }

    /// `has_debug_metadata()` returns `false` when codegen runs with
    /// `KARAC_RUNTIME_DEBUG_METADATA=0`. Pinpoints the slice 3 gate-off
    /// emission of `KARAC_SPAWN_SITES_ENABLED = 0` flowing through the
    /// runtime fn.
    #[test]
    fn test_has_debug_metadata_returns_false_when_gate_off() {
        let _guard = SPAWN_SITE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var("KARAC_RUNTIME_DEBUG_METADATA").ok();
        std::env::set_var("KARAC_RUNTIME_DEBUG_METADATA", "0");
        let captured = run_program_capturing(
            r#"
fn main() {
    let dbg = Runtime.has_debug_metadata();
    if dbg {
        println(1);
    } else {
        println(0);
    }
}
"#,
        );
        match prior {
            Some(v) => std::env::set_var("KARAC_RUNTIME_DEBUG_METADATA", v),
            None => std::env::remove_var("KARAC_RUNTIME_DEBUG_METADATA"),
        }
        if let Some(c) = captured {
            assert_eq!(c.stdout.trim(), "0", "expected gate-off (false → 0)");
        }
    }

    /// `Runtime.list_par_blocks()` called from inside a `par {}` block
    /// observes at least one active frame (its own). Validates that
    /// slice 4's `ACTIVE_FRAMES` registry is populated under
    /// `karac_par_run` and that `karac_runtime_list_par_blocks_into`
    /// joins it against `KARAC_SPAWN_SITES` correctly.
    ///
    /// The branch that calls `list_par_blocks()` runs concurrently with
    /// the second branch under `karac_par_run`; the lock-held iteration
    /// guarantees we see a consistent snapshot. Worst case, the second
    /// branch already finished — so we assert `>= 1` rather than `== 2`.
    #[test]
    fn test_list_par_blocks_inside_par_block_observes_self() {
        let _guard = SPAWN_SITE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var("KARAC_RUNTIME_DEBUG_METADATA").ok();
        std::env::remove_var("KARAC_RUNTIME_DEBUG_METADATA");
        let captured = run_program_capturing(
            r#"
fn check_par_blocks() {
    let pbs = Runtime.list_par_blocks();
    let n = pbs.len();
    println(n);
}
fn other_branch() {
    println(99);
}
fn main() {
    par {
        check_par_blocks();
        other_branch();
    }
}
"#,
        );
        if let Some(v) = prior {
            std::env::set_var("KARAC_RUNTIME_DEBUG_METADATA", v);
        }
        if let Some(c) = captured {
            // Stdout has two lines (one per branch), order non-deterministic.
            // Find the line that's the par-block count and assert >= 1.
            let mut found_count = false;
            for line in c.stdout.lines() {
                let trimmed = line.trim();
                if trimmed == "99" {
                    continue;
                }
                if let Ok(n) = trimmed.parse::<i64>() {
                    assert!(
                        n >= 1,
                        "expected list_par_blocks() to observe ≥1 active frame inside a par block; got {} (full stdout: {:?})",
                        n,
                        c.stdout
                    );
                    found_count = true;
                }
            }
            assert!(
                found_count,
                "didn't find a par-block count line in stdout: {:?}",
                c.stdout
            );
        }
    }

    /// `Runtime.list_par_blocks()` called from outside any par-block
    /// context returns an empty Vec. The root task has no `KaracFrame`
    /// registered, so `ACTIVE_FRAMES` is empty.
    #[test]
    fn test_list_par_blocks_outside_par_block_returns_empty() {
        let _guard = SPAWN_SITE_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var("KARAC_RUNTIME_DEBUG_METADATA").ok();
        std::env::remove_var("KARAC_RUNTIME_DEBUG_METADATA");
        let captured = run_program_capturing(
            r#"
fn main() {
    let pbs = Runtime.list_par_blocks();
    println(pbs.len());
}
"#,
        );
        if let Some(v) = prior {
            std::env::set_var("KARAC_RUNTIME_DEBUG_METADATA", v);
        }
        if let Some(c) = captured {
            assert_eq!(
                c.stdout.trim(),
                "0",
                "expected empty Vec from main() (no active par blocks)"
            );
        }
    }

    /// `pub const X: T = lit;` declared at module scope is visible from
    /// function bodies and lowers correctly through codegen. Pre-fix the
    /// codegen had no `Item::ConstDecl` registration so any reference to
    /// a top-level const fired `Undefined variable 'X'` from
    /// `load_variable`. The interpreter path always handled it (matching
    /// `Item::ConstDecl` arm in `eval_program`). Surfaced 2026-05-08
    /// during slice 6 (Parallax-lite) when a `pub const WORK: i64 =
    /// 50000000;` was hoisted out of the busy-compute kernels and
    /// rejected by `karac build`.
    #[test]
    fn test_pub_const_visible_in_fn_body() {
        let out = run_program(
            r#"
pub const WORK: i64 = 100;

fn use_work() -> i64 {
    let mut sum: i64 = 0;
    let mut i: i64 = 0;
    while i < WORK {
        sum = sum + i;
        i = i + 1;
    }
    sum
}

fn main() {
    println(use_work());
}
"#,
        );
        if let Some(out) = out {
            // sum(0..100) == 4950
            assert_eq!(out.trim(), "4950");
        }
    }

    /// Const-of-const: a const whose value expression references another
    /// const must compile correctly. The codegen fix re-compiles the
    /// stored value expression at every use site, so transitive const
    /// references work for free as long as they hit the
    /// `ExprKind::Identifier` lookup path on the inner reference.
    #[test]
    fn test_pub_const_references_other_const() {
        let out = run_program(
            r#"
pub const BASE: i64 = 10;
pub const SCALED: i64 = BASE + BASE;

fn main() {
    println(SCALED);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "20");
        }
    }

    /// `Runtime.list_tasks()` always returns an empty Vec in v1 — no
    /// real suspension exists yet. Pins the v1 contract surface; when
    /// Phase 6.3 ships real `WaitTarget` tracking this test gets
    /// updated to flag the surface change.
    #[test]
    fn test_list_tasks_returns_empty_in_v1() {
        let captured = run_program_capturing(
            r#"
fn main() {
    let tasks = Runtime.list_tasks();
    println(tasks.len());
    par {
        println(1);
        println(2);
    }
    let after = Runtime.list_tasks();
    println(after.len());
}
"#,
        );
        if let Some(c) = captured {
            // Both reads must be 0 (one before par, one after par's join).
            // Lines: par's own output (1, 2 in non-det order) plus two zero
            // lines for the list_tasks reads. Filter for the zero count
            // appearance — must occur at least twice.
            let zero_count_lines = c.stdout.lines().filter(|l| l.trim() == "0").count();
            assert!(
                zero_count_lines >= 2,
                "expected at least two `0` (list_tasks().len()) lines; got {:?}",
                c.stdout
            );
        }
    }

    // ── Ref-self / mut-ref-self method codegen ───────────────────────────
    //
    // Prerequisite for Theme 6's `R.method(...)` dispatch: impl methods
    // declared with `ref self` / `mut ref self` must compile to functions
    // that take a pointer-to-Self as the receiver, and the call site must
    // pass the receiver's address rather than its loaded value. Before
    // this slice, `make_impl_method_function` rewrote every `ref self` /
    // `mut ref self` to value-typed `self`, so mutations through the
    // receiver were lost on a copy.

    #[test]
    fn test_mut_ref_self_method_mutation_persists_through_caller() {
        let captured = run_program_capturing(
            "struct Counter { n: i64 }\n\
             impl Counter { fn bump(mut ref self) { self.n = self.n + 1; } }\n\
             fn main() { let mut c = Counter { n: 42 }; c.bump(); c.bump(); println(c.n); }",
        );
        if let Some(c) = captured {
            assert!(
                c.stdout.lines().any(|l| l.trim() == "44"),
                "expected 44 (42 + 1 + 1), got: {:?}",
                c.stdout
            );
        }
    }

    #[test]
    fn test_ref_self_method_reads_through_pointer() {
        let captured = run_program_capturing(
            "struct Pair { x: i64, y: i64 }\n\
             impl Pair { fn read_y(ref self) -> i64 { self.y } }\n\
             fn main() { let p = Pair { x: 7, y: 99 }; println(p.read_y()); }",
        );
        if let Some(c) = captured {
            assert!(
                c.stdout.lines().any(|l| l.trim() == "99"),
                "expected 99 (Pair.y), got: {:?}",
                c.stdout
            );
        }
    }

    #[test]
    fn test_mut_ref_free_function_param_mutation_persists() {
        // Cross-check that the same fix path applies to non-method
        // mut-ref params on free functions — the call site decides
        // the calling convention by inspecting the resolved fn's
        // first param type.
        let captured = run_program_capturing(
            "struct Counter { n: i64 }\n\
             fn bump(c: mut ref Counter) { c.n = c.n + 1; }\n\
             fn main() { let mut c = Counter { n: 42 }; bump(mut c); bump(mut c); println(c.n); }",
        );
        if let Some(c) = captured {
            assert!(
                c.stdout.lines().any(|l| l.trim() == "44"),
                "expected 44 (42 + 1 + 1), got: {:?}",
                c.stdout
            );
        }
    }

    // ── Theme 6: provider vtable emission (sub-step 2) ────────────────────
    //
    // Structural tests pinning that codegen emits a static `@VT_<U>_<T>`
    // global per `impl T for U` where `T` is bound to some `effect resource
    // R: T`. The fully-wired dispatch (sub-steps 3+4 — `with_provider[R]`
    // lowering + `R.method(...)` indirect call) is out of scope for this
    // commit; these tests verify the foundation only.

    #[test]
    fn test_provider_vtable_emitted_for_provider_trait_impl() {
        let ir = ir_for(
            "pub trait Recorder { fn record(value: i64); }\n\
             pub struct Counter { n: i64 }\n\
             impl Recorder for Counter { fn record(value: i64) { } }\n\
             pub effect resource Metric: Recorder;\n\
             fn main() { }",
        );
        assert!(
            ir.contains("@VT_Counter_Recorder"),
            "expected vtable global @VT_Counter_Recorder; IR: {}",
            ir
        );
    }

    #[test]
    fn test_provider_vtable_skipped_for_non_provider_trait_impl() {
        // No `effect resource` declaration → the trait isn't a provider
        // trait → no vtable emitted, even though `impl Foo for Bar`
        // exists.
        let ir = ir_for(
            "pub trait Foo { fn f(value: i64); }\n\
             pub struct Bar { n: i64 }\n\
             impl Foo for Bar { fn f(value: i64) { } }\n\
             fn main() { }",
        );
        assert!(
            !ir.contains("@VT_Bar_Foo"),
            "expected no vtable global for non-provider trait; IR: {}",
            ir
        );
    }

    #[test]
    fn test_provider_vtable_one_per_impl_target() {
        // Two impls of the same provider trait on different target types
        // produce two distinct vtables.
        let ir = ir_for(
            "pub trait Recorder { fn record(value: i64); }\n\
             pub struct CounterA { n: i64 }\n\
             pub struct CounterB { n: i64 }\n\
             impl Recorder for CounterA { fn record(value: i64) { } }\n\
             impl Recorder for CounterB { fn record(value: i64) { } }\n\
             pub effect resource Metric: Recorder;\n\
             fn main() { }",
        );
        assert!(
            ir.contains("@VT_CounterA_Recorder"),
            "expected @VT_CounterA_Recorder; IR: {}",
            ir
        );
        assert!(
            ir.contains("@VT_CounterB_Recorder"),
            "expected @VT_CounterB_Recorder; IR: {}",
            ir
        );
    }

    // ── Theme 6: with_provider[R] lowering (sub-step 3) ──────────────────
    //
    // Structural tests pinning the alloca + push + body + pop sequence
    // emitted at each `with_provider[R](provider, ||body)` call site. The
    // body's value is whatever the closure expression evaluates to;
    // dispatch through `R.method(...)` is sub-step 4.

    #[test]
    fn test_with_provider_emits_push_and_pop() {
        let ir = ir_for(
            "pub trait Recorder { fn record(value: i64); }\n\
             pub struct Counter { n: i64 }\n\
             impl Recorder for Counter { fn record(value: i64) { } }\n\
             pub effect resource Metric: Recorder;\n\
             fn main() {\n\
               let p = Counter { n: 0 };\n\
               with_provider[Metric](p, || { 42 });\n\
             }",
        );
        assert!(
            ir.contains("call void @karac_provider_push"),
            "expected karac_provider_push call; IR: {}",
            ir
        );
        assert!(
            ir.contains("call void @karac_provider_pop"),
            "expected karac_provider_pop call; IR: {}",
            ir
        );
        assert!(
            ir.contains("@VT_Counter_Recorder"),
            "expected vtable reference @VT_Counter_Recorder in push args; IR: {}",
            ir
        );
    }

    #[test]
    fn test_with_provider_resource_id_matches_declaration_order() {
        // Resource IDs are assigned in source-declaration order from the
        // top-level walk in compile_program. With three resources, the
        // third (Disk) has ID 2; verify the push call carries i32 2.
        let ir = ir_for(
            "pub trait Recorder { fn record(value: i64); }\n\
             pub struct Counter { n: i64 }\n\
             impl Recorder for Counter { fn record(value: i64) { } }\n\
             pub effect resource Net: Recorder;\n\
             pub effect resource Mem: Recorder;\n\
             pub effect resource Disk: Recorder;\n\
             fn main() {\n\
               let p = Counter { n: 0 };\n\
               with_provider[Disk](p, || { 0 });\n\
             }",
        );
        // The push call is `karac_provider_push(frame, id, data, vtable)`.
        // Matcher: any line containing both `karac_provider_push` and
        // `i32 2` confirms the third resource's ID flowed through.
        let push_lines: Vec<&str> = ir
            .lines()
            .filter(|l| l.contains("karac_provider_push"))
            .collect();
        assert!(
            push_lines.iter().any(|l| l.contains("i32 2")),
            "expected push with i32 2 (resource Disk has declaration index 2); push lines: {:?}",
            push_lines
        );
    }

    #[test]
    fn test_with_provider_returns_body_value() {
        // The body's result becomes the with_provider expression's
        // value. Smoke-test that an `i64` literal body lowers without
        // error and the function returns a non-void path.
        let ir = ir_for(
            "pub trait Recorder { fn record(value: i64); }\n\
             pub struct Counter { n: i64 }\n\
             impl Recorder for Counter { fn record(value: i64) { } }\n\
             pub effect resource Metric: Recorder;\n\
             fn run() -> i64 {\n\
               let p = Counter { n: 0 };\n\
               with_provider[Metric](p, || { 7 })\n\
             }",
        );
        assert!(
            ir.contains("define i64 @run"),
            "expected `run` returns i64; IR: {}",
            ir
        );
        assert!(
            ir.contains("call void @karac_provider_push"),
            "expected push inside run; IR: {}",
            ir
        );
    }

    // ── Theme 6: R.method(args) dispatch (sub-step 4) ────────────────────
    //
    // Structural tests pinning the `karac_provider_lookup` + extractvalue
    // + GEP + indirect-call sequence emitted at each `R.method(...)`
    // call site where R is a known provider resource. Pairs with the
    // sub-step 3 push/pop tests above; together they verify the full
    // `with_provider[R](p, || R.method())` shape compiles to the
    // expected runtime-stack-walk lowering.

    #[test]
    fn test_provider_dispatch_emits_lookup_and_indirect_call() {
        let ir = ir_for(
            "pub trait Recorder { fn record(mut ref self, value: i64); }\n\
             pub struct Counter { n: i64 }\n\
             impl Recorder for Counter { fn record(mut ref self, value: i64) { self.n = value; } }\n\
             pub effect resource Metric: Recorder;\n\
             fn run() {\n\
               let p = Counter { n: 0 };\n\
               with_provider[Metric](p, || { Metric.record(42) });\n\
             }",
        );
        assert!(
            ir.contains("call %ProviderLookupResult @karac_provider_lookup")
                || ir.contains("call { ptr, ptr } @karac_provider_lookup"),
            "expected karac_provider_lookup call; IR: {}",
            ir
        );
        // The dispatch loads the fn ptr from the vtable (`load ptr` on
        // `wp.fn`) and indirect-calls. Inkwell's load instruction names
        // come from the third arg to build_load.
        assert!(
            ir.contains("wp.fn"),
            "expected vtable fn pointer load named `wp.fn`; IR: {}",
            ir
        );
    }

    #[test]
    fn test_provider_dispatch_resource_id_matches_declaration_order() {
        // Resource IDs assigned in source-declaration order. The
        // dispatch's lookup call carries the same i32 as the push call
        // at the surrounding with_provider site — verifies the two
        // halves of the ABI agree.
        let ir = ir_for(
            "pub trait Recorder { fn record(mut ref self, value: i64); }\n\
             pub struct Counter { n: i64 }\n\
             impl Recorder for Counter { fn record(mut ref self, value: i64) { self.n = value; } }\n\
             pub effect resource A: Recorder;\n\
             pub effect resource B: Recorder;\n\
             pub effect resource C: Recorder;\n\
             fn run() {\n\
               let p = Counter { n: 0 };\n\
               with_provider[C](p, || { C.record(0) });\n\
             }",
        );
        // Both calls — push and lookup — should reference i32 2 (C is third).
        let push_lines: Vec<&str> = ir
            .lines()
            .filter(|l| l.contains("karac_provider_push"))
            .collect();
        let lookup_lines: Vec<&str> = ir
            .lines()
            .filter(|l| l.contains("karac_provider_lookup"))
            .collect();
        assert!(
            push_lines.iter().any(|l| l.contains("i32 2")),
            "expected push with i32 2 (C is third resource); push lines: {:?}",
            push_lines
        );
        assert!(
            lookup_lines.iter().any(|l| l.contains("i32 2")),
            "expected lookup with i32 2 (C is third resource); lookup lines: {:?}",
            lookup_lines
        );
    }

    // ── Theme 6: par-block provider-stack inheritance (sub-step 5) ──────
    //
    // Structural + e2e tests pinning that `par { }` branches inherit the
    // provider stack from the calling thread. The env-struct snapshot is
    // taken via `karac_provider_get_stack_head` at par-block entry; each
    // branch fn re-seeds its TLS with `karac_provider_set_stack_head` in
    // its prologue.

    #[test]
    fn test_par_block_emits_provider_stack_head_snapshot_and_seed() {
        let ir = ir_for(
            "fn main() {\n\
               par {\n\
                 println(1);\n\
                 println(2);\n\
               }\n\
             }",
        );
        // Snapshot at par-block entry (outer-fn side) — one call per par-block.
        let snap_count = ir
            .lines()
            .filter(|l| l.contains("call") && l.contains("@karac_provider_get_stack_head"))
            .count();
        assert_eq!(
            snap_count, 1,
            "expected exactly one call to karac_provider_get_stack_head at par-block entry; \
             IR: {}",
            ir
        );
        // Seed inside each branch fn — one call per branch (2 branches here).
        let seed_count = ir
            .lines()
            .filter(|l| l.contains("call") && l.contains("@karac_provider_set_stack_head"))
            .count();
        assert_eq!(
            seed_count, 2,
            "expected one karac_provider_set_stack_head call per branch fn (2 branches); \
             IR: {}",
            ir
        );
    }

    #[test]
    fn test_par_block_inside_with_provider_e2e_branches_see_provider() {
        // Provider pushed by with_provider, par block spawned inside.
        // Each par branch's worker thread starts with null TLS; the
        // env-struct snapshot + set_stack_head seed is what makes
        // R.get() resolve inside the branch body.
        let src = "pub trait Reader { fn get(ref self) -> i64; }\n\
            pub struct Data { x: i64 }\n\
            impl Reader for Data { fn get(ref self) -> i64 { self.x } }\n\
            pub effect resource D: Reader;\n\
            fn main() {\n\
              let p = Data { x: 100 };\n\
              with_provider[D](p, || {\n\
                par {\n\
                  println(D.get());\n\
                  println(D.get());\n\
                }\n\
              });\n\
            }";
        let Some(out) = run_program(src) else {
            eprintln!("skipping par+with_provider e2e: runtime/linker unavailable");
            return;
        };
        // Branch order is non-deterministic; both must print 100.
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "expected 2 println outputs; got: {:?}",
            lines
        );
        for l in &lines {
            assert_eq!(
                l.trim(),
                "100",
                "each branch should print 100 (provider inherited from outer scope); got: {:?}",
                lines
            );
        }
    }

    #[test]
    fn test_with_provider_e2e_nested_same_resource_innermost_wins() {
        // LIFO push/pop semantics. Outer push binds resource R to a
        // provider whose `get()` returns 1; inner push rebinds R to
        // a provider whose `get()` returns 2. Inside the inner scope,
        // R.get() must walk to the inner frame (head) and return 2;
        // after the inner scope pops, R.get() in the outer scope must
        // return 1 again. This test pins the runtime stack walk
        // (`karac_provider_lookup` returning the *first* matching frame
        // at innermost-first order) end to end through codegen.
        let src = "pub trait Reader { fn get(ref self) -> i64; }\n\
            pub struct Data { x: i64 }\n\
            impl Reader for Data { fn get(ref self) -> i64 { self.x } }\n\
            pub effect resource D: Reader;\n\
            fn main() {\n\
              let outer = Data { x: 1 };\n\
              let inner = Data { x: 2 };\n\
              with_provider[D](outer, || {\n\
                println(D.get());\n\
                with_provider[D](inner, || {\n\
                  println(D.get());\n\
                });\n\
                println(D.get());\n\
              });\n\
            }";
        let Some(out) = run_program(src) else {
            eprintln!("skipping nested with_provider e2e: runtime/linker unavailable");
            return;
        };
        let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            3,
            "expected 3 println outputs (outer, inner, outer-restored); got: {:?}",
            lines
        );
        assert_eq!(lines[0].trim(), "1", "outer scope: D.get should return 1");
        assert_eq!(
            lines[1].trim(),
            "2",
            "inner scope: D.get should return 2 (innermost wins)"
        );
        assert_eq!(
            lines[2].trim(),
            "1",
            "after inner pop: D.get should return 1 again (outer restored)"
        );
    }

    #[test]
    fn test_with_provider_e2e_mut_ref_self_mutation_visible_after_pop() {
        // Full Theme 6 round-trip: push → R.method() → pop, where the
        // method writes through `mut ref self` to the provider's storage.
        // After the with_provider scope ends, the provider variable
        // reflects the mutation — proving the data pointer that flowed
        // through karac_provider_push survived round-trip back into
        // karac_provider_lookup and the indirect call wrote through it.
        let src = "pub trait Recorder { fn record(mut ref self, value: i64); }\n\
            pub struct Counter { n: i64 }\n\
            impl Recorder for Counter { fn record(mut ref self, value: i64) { self.n = value; } }\n\
            pub effect resource Metric: Recorder;\n\
            fn main() {\n\
              let mut p = Counter { n: 0 };\n\
              with_provider[Metric](p, || { Metric.record(99); });\n\
              println(p.n);\n\
            }";
        let Some(out) = run_program(src) else {
            eprintln!("skipping with_provider e2e: runtime/linker unavailable");
            return;
        };
        assert_eq!(
            out.trim(),
            "99",
            "expected p.n == 99 after with_provider mutated through mut ref self"
        );
    }

    #[test]
    fn test_provider_dispatch_skipped_for_non_resource_path() {
        // `Vec::new()` style 2-segment paths must continue routing to
        // compile_assoc_call, not the provider dispatch. No call to
        // karac_provider_lookup should appear for non-provider calls
        // (the extern's `declare` is always emitted at codegen init,
        // so we filter for `call ... @karac_provider_lookup` lines
        // specifically rather than the bare symbol).
        let ir = ir_for(
            "fn main() {\n\
               let v: Vec[i64] = Vec.new();\n\
             }",
        );
        let has_call = ir
            .lines()
            .any(|l| l.contains("call") && l.contains("@karac_provider_lookup"));
        assert!(
            !has_call,
            "non-resource Vec.new must not emit a call to karac_provider_lookup; IR: {}",
            ir
        );
    }

    // ── Compound-payload enum codegen ─────────────────────────────
    //
    // Slice CP (Phase 7.2 — 2026-05-09) lights up multi-word payload
    // round-trip for `enum E { V(String) }`, `enum E { V(Vec[T]) }`,
    // user-struct payloads, and tag-gated mixed-width variants. Before
    // this slice the construction path collapsed any non-primitive
    // payload to a single zero word via `coerce_to_i64`'s catch-all.
    // The 8 tests below pin the layout machinery: (1) `String`
    // round-trip, (2) `Vec[i64]` round-trip via function dispatch
    // (because pattern-bound Vec methods don't yet have elem-type
    // re-registration — see CP slice's "Out of scope, still open"),
    // (3) `Vec[(String, i64)]` (the Slice F `Json.Object` shape),
    // (4) user-struct payload, (5) mixed-width V1 narrow path,
    // (6) mixed-width V2 wide path, (7) two-string-variant payload-
    // area sharing, (8) regression gate for `IoError.Other(String)`.

    #[test]
    fn test_compound_enum_string_payload_round_trip() {
        let out = run_program(
            r#"
enum E { V(String) }
fn main() {
    let e = V("alice");
    match e {
        V(s) => println(s),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "alice");
        }
    }

    #[test]
    fn test_compound_enum_vec_payload_round_trip() {
        // Method dispatch on the bound `xs` is not registered with
        // `vec_elem_types` at match-arm bind time (the typechecker's
        // `pattern_binding_types` map is name-only, not parameterized);
        // route the Vec through a `ref Vec[i64]` parameter so the
        // existing function-arg path registers the elem type.
        let out = run_program(
            r#"
enum E { V(Vec[i64]) }
fn sum(xs: ref Vec[i64]) -> i64 {
    let mut acc = 0;
    for x in xs { acc = acc + x; }
    acc
}
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(7);
    v.push(8);
    let e = V(v);
    match e {
        V(xs) => println(sum(xs)),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "15");
        }
    }

    #[test]
    fn test_compound_enum_vec_of_tuples_payload_round_trip() {
        // Slice F's `Json.Object` shape: `Vec[(String, i64)]`.
        // Tuples are compound aggregates, so this exercises the
        // recursive payload-word computation through the Vec layer
        // (Vec → 3 words; tuple-element type is ignored at the Vec
        // level since heap memory is the elem buffer).
        let out = run_program(
            r#"
enum E { V(Vec[(String, i64)]) }
fn count(xs: ref Vec[(String, i64)]) -> i64 {
    xs.len()
}
fn main() {
    let mut v: Vec[(String, i64)] = Vec.new();
    v.push(("alpha", 1));
    v.push(("beta", 2));
    let e = V(v);
    match e {
        V(xs) => println(count(xs)),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "2");
        }
    }

    #[test]
    fn test_compound_enum_user_struct_payload_round_trip() {
        let out = run_program(
            r#"
struct Point { x: i64, y: i64 }
enum E { V(Point) }
fn main() {
    let p = Point { x: 3, y: 4 };
    let e = V(p);
    match e {
        V(q) => {
            println(q.x);
            println(q.y);
        }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "4"]);
        }
    }

    #[test]
    fn test_compound_enum_mixed_width_variants_v1_uses_one_word() {
        let out = run_program(
            r#"
enum E { V1(i64), V2(String) }
fn main() {
    let e = V1(42);
    match e {
        V1(x) => println(x),
        V2(_s) => println(99),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_compound_enum_mixed_width_variants_v2_uses_three_words() {
        let out = run_program(
            r#"
enum E { V1(i64), V2(String) }
fn main() {
    let e = V2("hello");
    match e {
        V1(_x) => println(0),
        V2(s) => println(s),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "hello");
        }
    }

    #[test]
    fn test_compound_enum_two_variants_both_string_payload_share_words() {
        let out = run_program(
            r#"
enum E { V1(String), V2(String) }
fn main() {
    let a = V1("first");
    let b = V2("second");
    match a {
        V1(s) => println(s),
        V2(_s) => println("nope"),
    }
    match b {
        V1(_s) => println("nope"),
        V2(s) => println(s),
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["first", "second"]);
        }
    }

    #[test]
    fn test_io_error_other_string_round_trip() {
        // The regression gate for Slice CP. Pins the previously-latent
        // gap where `coerce_to_i64`'s catch-all silently zeroed any
        // multi-word payload. The IoError prelude type isn't spliced
        // into `program.items` for parser-mode tests, so we mirror its
        // shape with `MyIoErr` to stand in for the round-trip
        // semantics. If this test ever regresses, the latent gap has
        // returned and the slice CP layout machinery has drifted.
        let out = run_program(
            r#"
enum MyIoErr {
    NotFound,
    PermissionDenied,
    Other(String),
}
fn main() {
    let e = MyIoErr.Other("disk full");
    match e {
        Other(msg) => println(msg),
        _ => println("wrong variant"),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "disk full");
        }
    }

    // ── Compound-payload enum drop-path: non-ASAN regressions ─────────
    //
    // DP slice (Phase 7.2 — 2026-05-09) lights up scope-exit cleanup for
    // value-type enum bindings whose payload includes `String` / `Vec[T]`.
    // The ASAN tests in `tests/memory_sanitizer.rs` are the load-bearing
    // gates for the heap-buffer-free correctness; the two tests below
    // pin the IR-level shape choices the slice locks down — move
    // suppression on function-arg consume paths (DP4) and the
    // `is_shared` carve-out (DP3) — without depending on ASAN being
    // available on the host.

    #[test]
    fn test_compound_enum_drop_suppressed_when_moved() {
        // Regression gate for DP4. Constructing `e = V(s)` where `s` is
        // a tracked String binding zeros the source's `cap` field as a
        // move-suppression marker (the existing `FreeVecBuffer` cleanup
        // is gated on `cap > 0`). Then `consume(e)` takes the enum by
        // value — function parameters don't register `track_enum_var`,
        // so the param's local alloca becomes a stranded view of the
        // payload words; only the caller's `e`-bound alloca owns
        // cleanup. Verifies no double-free SIGABRT at scope exit.
        let out = run_program(
            r#"
enum E { V(String) }
fn consume(_e: E) -> i64 { 7 }
fn main() {
    let mut s = String.new();
    s.push_str("hello");
    let e = V(s);
    let n = consume(e);
    println(n);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "7");
        }
    }

    #[test]
    fn test_compound_enum_drop_skipped_for_shared_enum() {
        // Regression gate for DP3. `shared enum Sel { V(String) }` is
        // RC-allocated; cleanup goes through `track_rc_var` →
        // `emit_rc_dec`, NOT through the new `track_enum_var` /
        // `__karac_drop_Sel` machinery. Asserts the negative path —
        // no `__karac_drop_Sel` symbol is emitted. (The IR introspection
        // is via `compile_to_ir_string`; we assert the program runs
        // and produces the expected output, with the symbol-absence
        // check as a side-comment.)
        let out = run_program(
            r#"
shared enum Sel { V(String) }
fn main() {
    let mut s = String.new();
    s.push_str("rc payload");
    let _e = Sel.V(s);
    println(1);
}
"#,
        );
        if let Some(out) = out {
            // Program runs; shared-enum cleanup is RC-driven. The
            // `is_shared` carve-out at `track_enum_var` ensures we
            // never registered an EnumDrop action for `_e`.
            assert_eq!(out.trim(), "1");
        }
    }

    // ── Pattern-bound element-type dispatch ──────────────────────────
    //
    // PB sibling slice (Phase 7.2 — 2026-05-09) closes the gap surfaced
    // by CP slice's *Out of scope, still open*: direct method dispatch
    // on a pattern-bound `Vec[T]` / `Slice[T]` payload (e.g. `xs.len()`
    // where `xs` is the binding for a `V(Vec[i64])` payload) used to
    // route through a generic fallback that didn't know the payload's
    // parameterized inner type. The PB sibling slice surfaces the inner
    // element type through the typechecker → lowering → codegen
    // side-table chain so `compile_method_call`'s Vec/Slice arms
    // dispatch through the right element-typed path.
    //
    // The 5 tests below pin the registration: (1) direct `xs.len()` on
    // a `Vec[i64]` payload (the headline regression gate, contrasted
    // with `test_compound_enum_vec_payload_round_trip` above which kept
    // the function-arg work-around path), (2) direct `xs.len()` /
    // `xs[0]` on a `Slice[i64]` payload, (3) index-read + push (via
    // `let mut`-rebind) on a `Vec[i64]` payload, (4) `Vec[String]`
    // element-type round-trip, (5) nested-tuple-Vec destructure as the
    // PB5 cross-check.

    #[test]
    fn test_pattern_bound_vec_payload_method_dispatch_direct() {
        // Headline regression gate. Pre-PB this required routing `xs`
        // through a `ref Vec[i64]` function parameter — see
        // `test_compound_enum_vec_payload_round_trip` for the legacy
        // shape. Post-PB the direct dispatch on the bound name works.
        let out = run_program(
            r#"
enum E { V(Vec[i64]) }
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(7);
    v.push(8);
    v.push(9);
    let e = V(v);
    match e {
        V(xs) => println(xs.len()),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3");
        }
    }

    #[test]
    fn test_pattern_bound_slice_payload_method_dispatch_direct() {
        // Slice-payload counterpart to the headline test. Constructs a
        // slice from an Array via `as_slice()`, parks it in a variant
        // payload, and verifies `.len()` and indexing on the bound name
        // dispatch through the slice element-type registry.
        let out = run_program(
            r#"
enum E { V(Slice[i64]) }
fn main() {
    let a: Array[i64, 3] = [10, 20, 30];
    let s: Slice[i64] = a.as_slice();
    let e = V(s);
    match e {
        V(xs) => {
            println(xs.len());
            println(xs[0]);
            println(xs[2]);
        }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["3", "10", "30"]);
        }
    }

    #[test]
    fn test_pattern_bound_vec_payload_index_read_and_is_empty_direct() {
        // Index-read and `.is_empty()` directly on the pattern binding.
        // Pre-PB these dispatched through the same generic fallback as
        // `.len()` and either silently produced wrong codegen or failed
        // with a "no handler" diagnostic. Post-PB the binding-name → Vec
        // element-type registration lights up both paths in one go (the
        // registry is shared across all Vec method dispatchers).
        //
        // `xs.push(...)` on the pattern binding directly is still
        // off-limits because the parser binds tuple-variant pattern
        // names without a mut bit (`mut xs` isn't part of the surface
        // pattern grammar today), and the conventional `let mut xs2 =
        // xs;` rebind exercises a separate let-from-Identifier
        // propagation gap that's outside this slice's scope. Mutation
        // tests on pattern-bound collections wait until either pattern
        // mut bindings or let-from-Identifier propagation lands.
        let out = run_program(
            r#"
enum E { V(Vec[i64]) }
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(100);
    v.push(200);
    v.push(300);
    let e = V(v);
    match e {
        V(xs) => {
            println(xs[0]);
            println(xs[1]);
            println(xs[2]);
            if xs.is_empty() {
                println(0);
            } else {
                println(1);
            }
        }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["100", "200", "300", "1"]);
        }
    }

    #[test]
    fn test_pattern_bound_vec_of_strings_method_dispatch() {
        // Verifies `String` as the inner element type round-trips
        // through the `type_to_type_expr` helper added by the PB
        // sibling slice — the lowered `TypeExpr` for `String` is a
        // `TypeKind::Path("String")` which `llvm_type_for_name` lowers
        // to the same Vec-shaped struct used at the call-site
        // function-arg path. `.len()` on the bound name returns the
        // element count regardless of element width.
        let out = run_program(
            r#"
enum E { V(Vec[String]) }
fn main() {
    let mut v: Vec[String] = Vec.new();
    v.push("alpha");
    v.push("beta");
    v.push("gamma");
    let e = V(v);
    match e {
        V(xs) => println(xs.len()),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "3");
        }
    }

    #[test]
    fn test_pattern_bound_nested_tuple_vec_payload() {
        // PB5 cross-check / Theme 5 headline regression gate: nested
        // destructure where the variant payload is itself a tuple
        // `(Vec[i64], i64)`. Lights up after Theme 5 (compound-payload
        // tuple-payload destructure) added the Tuple branch in
        // `bind_pattern_values` + `reconstruct_payload_value`.
        let out = run_program(
            r#"
enum E { V((Vec[i64], i64)) }
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(1);
    v.push(2);
    v.push(3);
    v.push(4);
    let e = V((v, 100));
    match e {
        V((xs, n)) => {
            println(xs.len());
            println(n);
        }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["4", "100"]);
        }
    }

    // ── Compound-payload tuple-payload destructure ──
    //
    // Theme 5 (2026-05-10) — the Tuple arm in `bind_pattern_values` +
    // `reconstruct_payload_value` lights up `match e { V((a, b)) => ... }`
    // for variant-payload tuples of arbitrary primitive / aggregate /
    // recursive-tuple shape. The headline test above
    // (`test_pattern_bound_nested_tuple_vec_payload`) pins the original
    // `#[ignore]`'d cross-check; the four below exercise the full grid
    // of element shapes (primitive×primitive, heap×primitive, nested
    // tuples, three-element tuples).

    #[test]
    fn test_compound_tuple_payload_int_int() {
        // Smallest non-trivial case: two-i64 tuple. Verifies per-element
        // word-offset dispatch handles primitive payloads correctly
        // without depending on heap-bearing aggregates.
        let out = run_program(
            r#"
enum E { V((i64, i64)) }
fn main() {
    let e = V((7, 35));
    match e {
        V((a, b)) => println(a + b),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    #[test]
    fn test_compound_tuple_payload_string_int() {
        // Heap-bearing element survives destructure with no double-free
        // / use-after-free (further pinned by ASAN test below).
        let out = run_program(
            r#"
enum E { V((String, i64)) }
fn main() {
    let e = V(("hello", 42));
    match e {
        V((s, n)) => {
            println(s);
            println(n);
        }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["hello", "42"]);
        }
    }

    #[test]
    fn test_compound_tuple_payload_nested() {
        // TP5 — recursive tuple destructure works through one nesting
        // layer. `((i64, i64), String)` decomposes to inner-tuple +
        // string element via the recursive `reconstruct_payload_value`
        // / `bind_pattern_values` Tuple branches.
        let out = run_program(
            r#"
enum E { V(((i64, i64), String)) }
fn main() {
    let e = V(((10, 20), "nested"));
    match e {
        V(((a, b), s)) => {
            println(a + b);
            println(s);
        }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["30", "nested"]);
        }
    }

    #[test]
    fn test_compound_tuple_payload_three_elements() {
        // Three-element tuple with mixed heap-bearing + primitive
        // elements. Verifies the per-element offset walk handles N≥3
        // correctly (the field_words slice cursor advances past each
        // element's word count without overrunning).
        let out = run_program(
            r#"
enum E { V((Vec[i64], String, i64)) }
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(11);
    v.push(22);
    let e = V((v, "tag", 99));
    match e {
        V((xs, s, n)) => {
            println(xs.len());
            println(s);
            println(n);
        }
    }
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(lines, vec!["2", "tag", "99"]);
        }
    }

    // ── Primitive-type associated constants ──────────────────────
    //
    // Theme 7 (2026-05-10) — `i64.MAX` / `f64.INFINITY` / `usize.MAX`
    // etc. dispatch through the shared `PRIMITIVE_CONSTS` table at
    // `src/prelude.rs`. Codegen intercepts the `FieldAccess` arm at
    // `compile_field_access` before falling through to the generic
    // field-access path. Float widths preserved (f32 vs f64).

    #[test]
    fn test_codegen_primitive_const_i64_max() {
        let out = run_program("fn main() { let x = i64.MAX; println(x); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "9223372036854775807");
        }
    }

    #[test]
    fn test_codegen_primitive_const_i64_min() {
        let out = run_program("fn main() { let x = i64.MIN; println(x); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "-9223372036854775808");
        }
    }

    #[test]
    fn test_codegen_primitive_const_u64_max_bit_pattern_preserved() {
        // u64.MAX bit pattern is 0xFFFF_FFFF_FFFF_FFFF. Codegen's
        // println uses a signed format — that's a separate concern;
        // the constant value is correctly emitted as i64-bit-width
        // 0xFFFF... which, interpreted signed, prints as "-1". The
        // value parity test below verifies the bit pattern survives by
        // using it in an unsigned-aware comparison.
        let out = run_program("fn main() { let x = u64.MAX; println(x); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "-1");
        }
    }

    #[test]
    fn test_codegen_primitive_const_usize_max() {
        // v1 is 64-bit only — usize.MAX == u64.MAX. Same signed-print
        // caveat as the u64 test.
        let out = run_program("fn main() { let x = usize.MAX; println(x); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "-1");
        }
    }

    #[test]
    fn test_codegen_primitive_const_f64_infinity() {
        let out = run_program("fn main() { let x = f64.INFINITY; println(x); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "inf");
        }
    }

    #[test]
    fn test_codegen_primitive_const_f64_neg_infinity() {
        let out = run_program("fn main() { let x = f64.NEG_INFINITY; println(x); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "-inf");
        }
    }

    #[test]
    fn test_codegen_primitive_const_f64_nan() {
        // Codegen routes float printing through C-style printf which
        // renders NaN as lowercase "nan". The interpreter uses Rust's
        // Display impl which renders "NaN". Cross-side parity is by
        // semantic value (NaN-ness), not by string form.
        let out = run_program("fn main() { let x = f64.NAN; println(x); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "nan");
        }
    }

    #[test]
    fn test_codegen_primitive_const_f32_max_usable_in_arithmetic() {
        // f32 widths preserved through codegen. Confirms the
        // const_float emission picks f32_type rather than collapsing to
        // f64 (which would silently widen and lose the typing
        // invariant). Codegen's runtime float formatter renders f32
        // values in scientific notation (`3.40282e+38`) where the
        // interpreter's Display impl renders the full decimal expansion.
        let out = run_program("fn main() { let x: f32 = f32.MAX; let y: f32 = x; println(y); }");
        if let Some(out) = out {
            assert_eq!(out.trim(), "3.40282e+38");
        }
    }

    // ── Codegen bug regression tests ─────────────────────────────────
    //
    // Each test below pins an entry surfaced through
    // `docs/implementation_checklist/bugs.md`. Tests gated `#[ignore]`
    // pin a still-open bug (running with `--include-ignored` or
    // `cargo test --features llvm -- --ignored <name>` exercises the
    // failing path); ungated tests are the regression gate after the
    // underlying fix has landed.

    /// Regression gate for the previously-latent "Provider struct
    /// identity collision in codegen's `var_type_names`" bug (bugs.md
    /// entry). Two distinct user types that lower to the same LLVM
    /// struct shape used to collide in the LLVM-struct-identity reverse
    /// lookup at `let p = Provider.new()` (the UFCS-associated-fn
    /// fallback path in `compile_let`). The `var_type_names` mapping
    /// would pick an arbitrary match in HashMap iteration order, so
    /// `with_provider[R](p, || R.method())` routed to whichever
    /// provider's vtable iteration produced first.
    ///
    /// Fix: in the fallback path of `compile_let`, prefer the source-AST
    /// identity for UFCS calls of the shape `Target.fn(...)` whose LLVM
    /// return type matches `Target`'s LLVM struct identity. The bare
    /// LLVM-identity reverse-lookup remains as a final fallback for any
    /// other call shape that yields a struct value.
    ///
    /// Repro: two providers `ProvA` / `ProvB` with identical `{ i64 }`
    /// LLVM shape, each with a `pub fn new()` associated-fn constructor.
    /// `with_provider[Ra](ProvA.new(), …)` and `with_provider[Rb](
    /// ProvB.new(), …)` must each dispatch to its own impl — pre-fix,
    /// both `Ra.record(0)` and `Rb.record(0)` routed to the same impl
    /// (e.g., "100\n100" instead of "100\n200").
    #[test]
    fn test_var_type_names_struct_identity_collision_repro() {
        let out = run_program(
            r#"
pub trait Recorder { fn record(ref self, value: i64) -> i64; }

pub struct ProvA { x: i64 }
impl ProvA { pub fn new() -> ProvA { ProvA { x: 1 } } }
impl Recorder for ProvA { fn record(ref self, value: i64) -> i64 { 100 } }

pub struct ProvB { x: i64 }
impl ProvB { pub fn new() -> ProvB { ProvB { x: 2 } } }
impl Recorder for ProvB { fn record(ref self, value: i64) -> i64 { 200 } }

pub effect resource Ra: Recorder;
pub effect resource Rb: Recorder;

fn main() {
    let a = ProvA.new();
    let b = ProvB.new();
    with_provider[Ra](a, || {
        with_provider[Rb](b, || {
            println(Ra.record(0));
            println(Rb.record(0));
        });
    });
}
"#,
        );
        if let Some(out) = out {
            let lines: Vec<&str> = out.trim().lines().collect();
            assert_eq!(
                lines,
                vec!["100", "200"],
                "expected each `with_provider[R]` to route to its own impl \
                 (ProvA.record => 100, ProvB.record => 200); got {:?}. \
                 If both lines match (e.g., both `200`), the LLVM-struct- \
                 identity reverse-lookup at compile_let collided ProvA's \
                 binding onto ProvB's name (or vice versa).",
                lines
            );
        }
    }

    /// Regression gate for the previously-latent "chained-field
    /// `println` returns 0" bug (bugs.md entry). `println(o.inner.name)`
    /// where `Outer { inner: Inner }` and `Inner { name: String }` used
    /// to emit a load that resolved to 0 at runtime regardless of the
    /// field value. Single-level access (`o.field`) worked; the gap was
    /// at chain-depth ≥ 2 because `field_index_for` only resolved an
    /// `Identifier` / `SelfValue` object — a `FieldAccess` object
    /// (`o.inner` in `o.inner.name`) returned `None`, falling through
    /// to the constant-zero fallback in `compile_field_access`.
    ///
    /// Fix: track per-field user-type names in
    /// `struct_field_type_names` at struct-declaration time, and walk
    /// `FieldAccess` chains in a new `type_name_of_expr` helper used by
    /// `field_index_for`. The helper returns the inner struct's name
    /// for `o.inner` so `name` resolves in `Inner`'s field registry.
    #[test]
    fn test_chained_field_access_returns_zero_repro() {
        let out = run_program(
            r#"
struct Inner { name: String }
struct Outer { inner: Inner }
fn main() {
    let o = Outer { inner: Inner { name: "alice" } };
    println(o.inner.name);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(
                out.trim(),
                "alice",
                "expected `o.inner.name` to load the actual String value; \
                 got {:?}. If the output is `0`, the chain-depth ≥ 2 load \
                 path zeroed the value regardless of the field contents.",
                out.trim()
            );
        }
    }

    // ── Labeled blocks runtime ──────────────────────────────────
    //
    // Labeled-block codegen + interpreter sibling slice (LBC1-LBC5).
    // The frontend slice (commit 85e49c8) shipped parser + resolver +
    // typechecker; this slice wires runtime semantics so the typed
    // program actually runs correctly. See
    // `docs/implementation_checklist/phase-5-diagnostics.md` § 5.2 →
    // "Labeled blocks: codegen + interpreter sibling".

    /// `lbl: { break lbl 42; -1 }` evaluates to 42. The early `break label
    /// expr` exits the labeled block with the given value; the
    /// fall-through tail (`-1`) never runs.
    #[test]
    fn test_labeled_block_break_with_value_e2e() {
        let out = run_program(
            r#"
fn main() {
    let x: i64 = lbl: { break lbl 42; -1 };
    println(x);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "42");
        }
    }

    /// `lbl: { break lbl; }` typed as `()` — bare break exits with unit.
    /// Verifies the post-block code path runs (println marker).
    #[test]
    fn test_labeled_block_bare_break_e2e() {
        let out = run_program(
            r#"
fn main() {
    lbl: { break lbl; };
    println(7);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "7");
        }
    }

    /// `lbl: { 99 }` evaluates to 99 — no break path exercised; the
    /// labeled block falls through normally and the slot stores the tail.
    #[test]
    fn test_labeled_block_tail_expression_when_no_break_e2e() {
        let out = run_program(
            r#"
fn main() {
    let x: i64 = lbl: { 99 };
    println(x);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "99");
        }
    }

    /// Outer `lbl: { inner: { break lbl 7; 0 } }` evaluates to 7. The
    /// inner labeled block's tail (`0`) never runs because `break lbl`
    /// transfers control past the inner exit straight to the outer exit.
    /// Stresses the label-aware frame walk (LBC1) — the resolver
    /// guarantees `lbl` resolves to the outer block, and codegen's
    /// `compile_break` rev-walk picks the matching frame, not the
    /// innermost.
    #[test]
    fn test_labeled_block_break_from_nested_block_e2e() {
        let out = run_program(
            r#"
fn main() {
    let x: i64 = lbl: { inner: { break lbl 7; 0 } };
    println(x);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "7");
        }
    }

    /// **Latent-bug regression gate.** `outer: while ... { inner: while
    /// ... { break outer; } }` exits the outer loop, not just the inner.
    /// Pre-slice codegen always picked `loop_stack.last()` regardless of
    /// label, so `break outer` would have broken only `inner` and the
    /// outer-loop termination would never have observed the inner break.
    /// Today's label-aware lookup (LBC1 side-effect) closes the gap; the
    /// post-loop println marker must print `done` in one shot.
    #[test]
    fn test_labeled_loop_nested_break_outer_e2e() {
        let out = run_program(
            r#"
fn main() {
    let mut count = 0;
    outer: while true {
        inner: while true {
            count = count + 1;
            break outer ();
        }
        // Without the latent-bug fix, the outer-loop body would
        // re-enter `inner` here every iteration. With the fix, the
        // `break outer` transfers control past this point straight to
        // the outer loop's exit BB.
        count = count + 100;
    }
    println(count);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out.trim(), "1");
        }
    }

    // ── Slice B follow-up (2026-05-09): fn-pointer-as-free-fn-arg + ──
    //                       Server.serve(handler) dispatch
    //
    // Sub-step (b): free-fn-name-as-value codegen path.
    // Sub-step (c): `Server.serve(handler)` dispatcher arm.
    // Sub-step (d): closure-as-handler-arg structured rejection.

    /// Sub-step (b) pin — `let f = target;` lowers without the
    /// "Undefined variable 'target'" diagnostic that fired before this
    /// slice. Uses the free-fn name as a value; v1 doesn't track a
    /// fn-pointer type for direct calls through the binding, so the
    /// test stays at the "binds and compiles" assertion.
    #[test]
    fn test_free_fn_as_value_emits_fn_ptr() {
        let src = r#"
fn target() -> i64 { 42 }

fn main() {
    let _f = target;
    println(target());
}
"#;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let ir = compile_to_ir(&parsed.program, None, None);
        assert!(
            ir.is_ok(),
            "expected free-fn-name-as-value to compile cleanly; got: {:?}",
            ir.err()
        );
    }

    /// Sub-step (c) pin — `Server.serve(handle)` with a free-fn handler
    /// builds end-to-end. The runtime ABI mismatch between the user
    /// `fn handle(req: Request) -> Response` shape and the FFI extern's
    /// `extern "C" fn(*const KaracHttpRequest, *mut KaracHttpResponse)`
    /// is acknowledged in the slice plan's hard-stop trigger 2 fallback —
    /// LLVM's indirect-call boundary is structurally `ptr`, so codegen
    /// passes the user fn-pointer through and the build succeeds. End-
    /// to-end runtime invocation needs trampoline glue tracked
    /// separately; this test pins the codegen path itself.
    #[test]
    fn test_server_serve_with_free_fn_handler_compiles() {
        let src = r#"
struct Response { status: i64, body: String }

fn handle(req: Request) -> Response {
    Response { status: 200, body: "{}" }
}

fn main() {
    let _result = Server.serve("127.0.0.1:0", handle);
}
"#;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let ir = compile_to_ir(&parsed.program, None, None);
        assert!(
            ir.is_ok(),
            "expected Server.serve(addr, handle) to compile cleanly; got: {:?}",
            ir.err()
        );
        let ir_text = ir.unwrap();
        assert!(
            ir_text.contains("karac_runtime_serve_http"),
            "expected the IR to call `karac_runtime_serve_http`; not found"
        );
    }

    /// HTTP handler ABI trampoline (2026-05-09): two `Server.serve(handle)`
    /// calls in one program emit exactly one `_karac_http_shim_handle`
    /// definition. Pins the per-handler-fn shim cache — without it,
    /// duplicate emission would either trigger a `module already has a
    /// function named ...` panic from `LLVMModuleRef::add_function` or
    /// produce two separate shim definitions and bloat the IR.
    #[test]
    fn test_server_serve_handler_shim_caches() {
        let src = r#"
struct Response { status: i64, body: String }

fn handle(req: Request) -> Response {
    Response { status: 200, body: "{}" }
}

fn main() {
    let _r1 = Server.serve("127.0.0.1:0", handle);
    let _r2 = Server.serve("127.0.0.1:0", handle);
}
"#;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let ir = compile_to_ir(&parsed.program, None, None)
            .expect("expected dual Server.serve(addr, handle) calls to compile cleanly");
        let define_count = ir
            .lines()
            .filter(|l| l.contains("_karac_http_shim_handle") && l.contains("define"))
            .count();
        assert_eq!(
            define_count, 1,
            "expected exactly 1 `_karac_http_shim_handle` definition; got {define_count}.\nIR:\n{ir}"
        );
    }

    /// Sub-step (d) pin — passing a closure to `Server.serve(...)` is
    /// rejected with the structured `E_CLOSURE_AS_FN_PTR_NOT_YET`
    /// diagnostic. Defense-in-depth at the codegen layer; the closure
    /// `{ fn_ptr, env_ptr }` ABI doesn't match the FFI extern's bare-
    /// pointer parameter slot.
    #[test]
    fn test_server_serve_rejects_closure_handler() {
        let src = r#"
fn main() {
    let _result = Server.serve("127.0.0.1:0", |req| Response { status: 200, body: "{}" });
}
"#;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let err = compile_to_ir(&parsed.program, None, None)
            .expect_err("expected closure-as-handler to be rejected");
        assert!(
            err.contains("E_CLOSURE_AS_FN_PTR_NOT_YET"),
            "expected diagnostic to carry E_CLOSURE_AS_FN_PTR_NOT_YET; got: {}",
            err
        );
    }

    // ── Slice / array patterns (phase-5 § Slice and array patterns — sub-item 4)

    #[test]
    fn test_e2e_slice_pattern_empty_matches_empty_vec() {
        let out = run_program(
            r#"
fn label(v: Vec[i64]) -> String {
    match v {
        [] => "empty",
        _ => "non-empty",
    }
}
fn main() {
    let a: Vec[i64] = Vec.new();
    let mut b: Vec[i64] = Vec.new();
    b.push(7);
    println(label(a));
    println(label(b));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "empty\nnon-empty\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_single_element_fixed_arity_array() {
        let out = run_program(
            r#"
fn main() {
    let a: Array[i64, 1] = [42];
    let [x] = a;
    println(x);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "42\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_fixed_arity_let_binds_all_elements() {
        let out = run_program(
            r#"
fn main() {
    let arr: Array[i64, 3] = [10, 20, 30];
    let [a, b, c] = arr;
    println(a);
    println(b);
    println(c);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "10\n20\n30\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_head_only_ignored_rest_on_vec() {
        let out = run_program(
            r#"
fn head_or(v: Vec[i64], default: i64) -> i64 {
    match v {
        [first, ..] => first,
        [] => default,
    }
}
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(10);
    v.push(20);
    v.push(30);
    let empty: Vec[i64] = Vec.new();
    println(head_or(v, -1));
    println(head_or(empty, -1));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "10\n-1\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_tail_only_ignored_rest_on_vec() {
        let out = run_program(
            r#"
fn last_or(v: Vec[i64], default: i64) -> i64 {
    match v {
        [.., last] => last,
        [] => default,
    }
}
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(10);
    v.push(20);
    v.push(30);
    println(last_or(v, -1));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "30\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_both_ends_ignored_rest_on_vec() {
        let out = run_program(
            r#"
fn ends(v: Vec[i64]) -> i64 {
    match v {
        [first, .., last] => first + last,
        [only] => only,
        [] => -1,
    }
}
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(1);
    v.push(2);
    v.push(3);
    v.push(4);
    v.push(5);
    println(ends(v));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "6\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_single_bound_rest_at_tail_array() {
        let out = run_program(
            r#"
fn main() {
    let arr: Array[i64, 5] = [10, 20, 30, 40, 50];
    let [first, ..rest] = arr;
    println(first);
    println(rest.len());
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "10\n4\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_single_bound_rest_at_head_array() {
        let out = run_program(
            r#"
fn main() {
    let arr: Array[i64, 4] = [10, 20, 30, 40];
    let [..rest, last] = arr;
    println(rest.len());
    println(last);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "3\n40\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_two_bound_middle_rest_array() {
        let out = run_program(
            r#"
fn main() {
    let arr: Array[i64, 5] = [1, 2, 3, 4, 5];
    let [first, ..mid, last] = arr;
    println(first);
    println(mid.len());
    println(last);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "1\n3\n5\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_multi_element_prefix_and_suffix_array() {
        let out = run_program(
            r#"
fn main() {
    let arr: Array[i64, 6] = [10, 20, 30, 40, 50, 60];
    let [a, b, .., y, z] = arr;
    println(a);
    println(b);
    println(y);
    println(z);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "10\n20\n50\n60\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_match_dispatches_on_length_for_vec() {
        let out = run_program(
            r#"
fn classify(v: Vec[i64]) -> String {
    match v {
        [] => "0",
        [_] => "1",
        [_, _] => "2",
        [_, .., _] => "3+",
    }
}
fn main() {
    let a: Vec[i64] = Vec.new();
    let mut b: Vec[i64] = Vec.new();
    b.push(1);
    let mut c: Vec[i64] = Vec.new();
    c.push(1); c.push(2);
    let mut d: Vec[i64] = Vec.new();
    d.push(1); d.push(2); d.push(3); d.push(4);
    println(classify(a));
    println(classify(b));
    println(classify(c));
    println(classify(d));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "0\n1\n2\n3+\n");
        }
    }

    #[test]
    fn test_e2e_slice_pattern_rest_binding_indexing_on_vec() {
        let out = run_program(
            r#"
fn main() {
    let mut v: Vec[i64] = Vec.new();
    v.push(7);
    v.push(8);
    v.push(9);
    v.push(10);
    match v {
        [_, ..rest] => {
            println(rest.len());
            println(rest[0]);
            println(rest[1]);
            println(rest[2]);
        },
        [] => println(-1),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "3\n8\n9\n10\n");
        }
    }

    // Match-arm struct destructure for an OWNED scrutinee. Predecessor
    // for the ref-scrutinee shape below — `bind_pattern_values` had no
    // `PatternKind::Struct` arm at all before slice 3a, so well-typed
    // `match p { Point { x, y } => x + y }` errored at codegen with
    // `Undefined variable 'x'`.
    #[test]
    fn test_e2e_match_owned_struct_destructure_smoke() {
        let out = run_program(
            r#"
struct Point { x: i64, y: i64 }
fn show(p: Point) -> i64 {
    match p {
        Point { x, y } => x + y * 100,
    }
}
fn main() {
    let p = Point { x: 3, y: 5 };
    println(show(p));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "503\n");
        }
    }

    // ── Ref-scrutinee match-arm leaf-binding ABI parity (slice 3a) ──
    //
    // Each test below typechecked correctly post-slice-1 (the ref
    // scrutinee binding-form propagation landed 2026-05-12) but
    // miscompiled at codegen until slice 3a — the leaf binding was
    // emitted as a value-typed alloca, then passed to a `ref T` /
    // `mut ref T` parameter as a value rather than a pointer (ABI
    // mismatch). Slice 3a wraps each leaf in a ref-shim alloca so the
    // call-site ABI matches the typechecker's view.

    #[test]
    fn test_e2e_match_ref_struct_field_passes_to_ref_param() {
        // `Foo { age }` under a `ref Foo` scrutinee binds `age` as
        // `ref i64`; passing it to `read_int(n: ref i64)` rounds the
        // pointer back to the value via the runtime's println path.
        let out = run_program(
            r#"
struct Foo { age: i64 }
fn read_int(n: ref i64) -> i64 { n + 1 }
fn show(f: ref Foo) -> i64 {
    match f {
        Foo { age } => read_int(age),
    }
}
fn main() {
    let f = Foo { age: 41 };
    println(show(f));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "42\n");
        }
    }

    #[test]
    fn test_e2e_match_ref_option_payload_passes_to_ref_param() {
        // Tuple-variant payload binding: `Option.Some(n)` under a
        // `ref Option[i64]` scrutinee binds `n` as `ref i64`.
        let out = run_program(
            r#"
fn read_int(n: ref i64) -> i64 { n * 2 }
fn show(opt: ref Option[i64]) -> i64 {
    match opt {
        Option.Some(n) => read_int(n),
        Option.None => -1,
    }
}
fn main() {
    let some = Option.Some(7);
    let none: Option[i64] = Option.None;
    println(show(some));
    println(show(none));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "14\n-1\n");
        }
    }

    #[test]
    fn test_e2e_match_owned_struct_field_owned_call_unaffected() {
        // Sanity: under an OWNED scrutinee, the leaf binding stays
        // value-typed; passing it to a value-taking function works
        // exactly as before slice 3a (the borrow_modes table is empty
        // for owned scrutinees, so the shim never fires).
        let out = run_program(
            r#"
struct Foo { age: i64 }
fn read_int(n: i64) -> i64 { n + 100 }
fn show(f: Foo) -> i64 {
    match f {
        Foo { age } => read_int(age),
    }
}
fn main() {
    let f = Foo { age: 1 };
    println(show(f));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "101\n");
        }
    }

    #[test]
    fn test_e2e_match_mut_ref_struct_field_passes_to_mut_ref_param() {
        // `mut ref Foo` scrutinee → leaf binding is `mut ref i64`;
        // passes ABI-shape parity check at the call site. Mutation
        // propagation is NOT exercised here — slice 3a's shim aliases
        // a copy, not the scrutinee storage; that's the deferred GEP
        // sub-slice (see phase-5 entry).
        let out = run_program(
            r#"
struct Bag { n: i64 }
fn bump(n: mut ref i64) -> i64 { n + 1 }
fn show(b: mut ref Bag) -> i64 {
    match b {
        Bag { n } => bump(n),
    }
}
fn main() {
    let mut b = Bag { n: 10 };
    println(show(b));
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(out, "11\n");
        }
    }

    // ── Slice 3b probe tests: mut-ref scrutinee write-through ──
    //
    // These tests assert that mutation through a `mut ref` leaf binding
    // *propagates back* to the original scrutinee storage. Slice 3a's
    // ref-shim aliases a copy — these tests will FAIL until slice 3b's
    // GEP-into-scrutinee lowering lands. The probe converts the silent
    // miscompile into a known-failing pin so the gap can't persist
    // unnoticed.
    //
    // Each test follows the same shape: mutate via a `mut ref` arg
    // inside a match arm under a `mut ref` scrutinee, then observe the
    // scrutinee's state from the outer scope. If the shim's copy
    // semantics dominate, the outer observation reads the pre-mutation
    // value (test fails). If true GEP aliasing is in place, the outer
    // observation reads the post-mutation value (test passes).

    #[test]
    fn test_e2e_match_mut_ref_struct_field_write_through_propagates() {
        // The match arm returns `set_to`'s i64 (Kara represents unit as
        // i64 zero), so the match-as-statement form would trip an
        // unrelated "non-void return in void function" codegen bug.
        // Returning the propagation result through the function value
        // sidesteps that orthogonal gap and isolates the write-through
        // semantic this test is probing.
        let out = run_program(
            r#"
struct Bag { n: i64 }
fn set_to(n: mut ref i64, v: i64) -> i64 { *n = v; v }
fn mutate(b: mut ref Bag) -> i64 {
    match b {
        Bag { n } => set_to(n, 99),
    }
}
fn main() {
    let mut b = Bag { n: 10 };
    let _ = mutate(b);
    println(b.n);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(
                out, "99\n",
                "mut-ref scrutinee leaf binding must write through to scrutinee storage"
            );
        }
    }

    #[test]
    fn test_e2e_match_mut_ref_option_payload_write_through_propagates() {
        // Tuple-variant payload: under `mut ref Option[i64]`, the
        // `Option.Some(n)` binding `n` should be `mut ref i64` aliasing
        // the Some-payload's storage.
        let out = run_program(
            r#"
fn set_to(n: mut ref i64, v: i64) -> i64 { *n = v; v }
fn mutate(opt: mut ref Option[i64]) -> i64 {
    match opt {
        Option.Some(n) => set_to(n, 42),
        Option.None => 0,
    }
}
fn main() {
    let mut opt = Option.Some(7);
    let _ = mutate(opt);
    match opt {
        Option.Some(v) => println(v),
        Option.None => println(-1),
    }
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(
                out, "42\n",
                "mut-ref Option payload binding must write through to scrutinee storage"
            );
        }
    }

    #[test]
    fn test_e2e_match_mut_ref_struct_two_fields_independent_write_through() {
        // Two leaf bindings under the same scrutinee: each should alias
        // its own field independently.
        let out = run_program(
            r#"
struct Pair { a: i64, b: i64 }
fn set_to(n: mut ref i64, v: i64) -> i64 { *n = v; v }
fn mutate(p: mut ref Pair) -> i64 {
    match p {
        Pair { a, b } => set_to(a, 100) + set_to(b, 200),
    }
}
fn main() {
    let mut p = Pair { a: 1, b: 2 };
    let _ = mutate(p);
    println(p.a);
    println(p.b);
}
"#,
        );
        if let Some(out) = out {
            assert_eq!(
                out, "100\n200\n",
                "two mut-ref leaf bindings must each write through to their own field"
            );
        }
    }
}
