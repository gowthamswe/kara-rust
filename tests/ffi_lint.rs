use karac::ffi_lint::check_ffi_float_eq;

fn parse_program(source: &str) -> karac::ast::Program {
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
    parsed.program
}

fn lint(source: &str) -> Vec<karac::ffi_lint::FfiFloatEqDiagnostic> {
    check_ffi_float_eq(&parse_program(source))
}

#[test]
fn test_ffi_float_eq_direct_warns() {
    let diags = lint(
        r#"unsafe extern "C" { fn sin(x: f64) -> f64; }
           fn f() { let _ok = sin(1.0) == 0.0; }"#,
    );
    assert_eq!(diags.len(), 1, "Expected 1 diagnostic, got: {:?}", diags);
    assert!(diags[0].extern_fn == "sin");
    assert!(diags[0].message.contains("=="));
}

#[test]
fn test_ffi_float_ne_direct_warns() {
    let diags = lint(
        r#"unsafe extern "C" { fn cos(x: f64) -> f64; }
           fn f() { let _ok = cos(0.0) != 1.0; }"#,
    );
    assert_eq!(diags.len(), 1, "Expected 1 diagnostic, got: {:?}", diags);
    assert!(diags[0].message.contains("!="));
}

#[test]
fn test_ffi_non_float_no_warn() {
    let diags = lint(
        r#"unsafe extern "C" { fn strlen(s: *const u8) -> i64; }
           fn f() { let _ok = strlen(0 as *const u8) == 0; }"#,
    );
    assert!(
        diags.is_empty(),
        "Non-float FFI should not warn, got: {:?}",
        diags
    );
}

#[test]
fn test_regular_float_comparison_no_warn() {
    // Regular (non-FFI) float comparisons are not flagged by this lint
    let diags = lint(r#"fn f(x: f64) { let _ok = x == 0.0; }"#);
    assert!(
        diags.is_empty(),
        "Non-FFI float comparison should not warn, got: {:?}",
        diags
    );
}

#[test]
fn test_ffi_float_eq_rhs_warns() {
    // FFI float on the right side of ==
    let diags = lint(
        r#"unsafe extern "C" { fn get_pi() -> f32; }
           fn f() { let _ok = 3.14 == get_pi(); }"#,
    );
    assert_eq!(
        diags.len(),
        1,
        "Expected 1 diagnostic (rhs), got: {:?}",
        diags
    );
    assert_eq!(diags[0].extern_fn, "get_pi");
}

#[test]
fn test_ffi_float_less_than_no_warn() {
    // Only == and != are flagged; < is fine
    let diags = lint(
        r#"unsafe extern "C" { fn norm(x: f64) -> f64; }
           fn f() { let _ok = norm(1.0) < 0.001; }"#,
    );
    assert!(
        diags.is_empty(),
        "< on FFI float should not warn, got: {:?}",
        diags
    );
}
