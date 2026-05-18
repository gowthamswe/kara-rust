use karac::ast::Program;
use karac::logical_lint::{check_ambiguous_not_comparison, LintDiagnostic, LintLevel};

fn parse_program(source: &str) -> Program {
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

fn lint(source: &str) -> Vec<LintDiagnostic> {
    let prog = parse_program(source);
    check_ambiguous_not_comparison(&prog, &karac::lints::CliLintOverrides::default())
}

#[test]
fn test_not_before_eq_warns() {
    let diags = lint("fn main() { let r = not x == y; }");
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].level, LintLevel::Warning);
    assert_eq!(diags[0].lint_name, "ambiguous_not_comparison");
    assert!(diags[0].message.contains("not"));
}

#[test]
fn test_not_before_neq_warns() {
    let diags = lint("fn main() { let r = not x != y; }");
    assert_eq!(diags.len(), 1);
}

#[test]
fn test_not_before_lt_warns() {
    let diags = lint("fn main() { let r = not x < y; }");
    assert_eq!(diags.len(), 1);
}

#[test]
fn test_not_before_lte_warns() {
    let diags = lint("fn main() { let r = not x <= y; }");
    assert_eq!(diags.len(), 1);
}

#[test]
fn test_not_before_gt_warns() {
    let diags = lint("fn main() { let r = not x > y; }");
    assert_eq!(diags.len(), 1);
}

#[test]
fn test_not_before_gte_warns() {
    let diags = lint("fn main() { let r = not x >= y; }");
    assert_eq!(diags.len(), 1);
}

#[test]
fn test_not_with_parens_around_comparison_silent() {
    // `not (x == y)` parses as `Unary(Not, Binary(Eq, x, y))` — no comparison
    // adjacent to a not at the AST level, so no warning.
    let diags = lint("fn main() { let r = not (x == y); }");
    assert!(diags.is_empty(), "diags: {:?}", diags);
}

#[test]
fn test_double_not_silent() {
    let diags = lint("fn main() { let r = not not x; }");
    assert!(diags.is_empty(), "diags: {:?}", diags);
}

#[test]
fn test_not_alone_silent() {
    let diags = lint("fn main() { let r = not x; }");
    assert!(diags.is_empty(), "diags: {:?}", diags);
}

#[test]
fn test_comparison_alone_silent() {
    let diags = lint("fn main() { let r = x == y; }");
    assert!(diags.is_empty(), "diags: {:?}", diags);
}

#[test]
fn test_not_in_and_or_silent() {
    let diags = lint("fn main() { let r = not x and not y; }");
    assert!(diags.is_empty(), "diags: {:?}", diags);
}

#[test]
fn test_not_on_right_of_comparison_warns() {
    // `x == not y` parses as `Binary(Eq, x, Unary(Not, y))`. Lint also
    // fires on the right-side variant.
    let diags = lint("fn main() { let r = x == not y; }");
    assert_eq!(diags.len(), 1);
}

#[test]
fn test_lint_inside_if_condition() {
    let diags = lint("fn main() { if not a == b { do_thing(); } }");
    assert_eq!(diags.len(), 1);
}

#[test]
fn test_lint_walks_into_method_calls() {
    let diags = lint("fn main() { let xs = vec.filter(|item| not item.value == threshold); }");
    assert_eq!(diags.len(), 1);
}

// ── Slice 4b cross-cutting — CLI fall-through ──────────────────

#[test]
fn test_cli_allow_suppresses_emission() {
    let prog = parse_program("fn main() { let r = not x == y; }");
    let cli = karac::lints::CliLintOverrides::with_level(
        "ambiguous_not_comparison",
        karac::lints::LintLevel::Allow,
    );
    let diags = check_ambiguous_not_comparison(&prog, &cli);
    assert!(
        diags.is_empty(),
        "`-A ambiguous_not_comparison` should suppress all emissions; got: {diags:?}",
    );
}

#[test]
fn test_cli_deny_promotes_to_error() {
    let prog = parse_program("fn main() { let r = not x == y; }");
    let cli = karac::lints::CliLintOverrides::with_level(
        "ambiguous_not_comparison",
        karac::lints::LintLevel::Deny,
    );
    let diags = check_ambiguous_not_comparison(&prog, &cli);
    assert_eq!(diags.len(), 1);
    assert_eq!(
        diags[0].level,
        LintLevel::Error,
        "`-D ambiguous_not_comparison` should promote to Error; got: {:?}",
        diags[0].level,
    );
}

#[test]
fn test_cli_deny_warnings_promotes_default_warn() {
    let prog = parse_program("fn main() { let r = not x == y; }");
    let cli = karac::lints::CliLintOverrides::with_deny_warnings();
    let diags = check_ambiguous_not_comparison(&prog, &cli);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].level, LintLevel::Error);
}
