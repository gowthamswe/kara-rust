pub mod ast;
pub mod cfg;
pub mod cli;
#[cfg(feature = "llvm")]
pub mod codegen;
pub mod concurrency;
pub mod cost_summary;
pub mod doc;
pub mod dominator;
pub mod edit_distance;
pub mod effectchecker;
pub mod exhaustive;
pub mod ffi_lint;
pub mod formatter;
pub mod interpreter;
pub mod lexer;
pub mod logical_lint;
pub mod lowering;
pub mod manifest;
pub mod module;
pub mod ownership;
pub mod parser;
pub mod prelude;
pub mod provider_escape;
pub mod rc_predicate;
pub mod repl;
pub mod resolver;
pub mod scaffold;
pub mod token;
pub mod typechecker;
pub mod unsafe_lint;
pub mod use_classifier;
pub mod walker;

use crate::ast::Program;
use crate::concurrency::{ConcurrencyAnalysis, ConcurrencyChecker};
use crate::effectchecker::{EffectCheckResult, EffectChecker, PublicEffectsPolicy};
use crate::lexer::Lexer;
use crate::manifest::CompileProfile;
use crate::ownership::{OwnershipCheckResult, OwnershipChecker};
use crate::parser::{ParseResult, Parser};
use crate::resolver::{ResolveResult, Resolver};
use crate::token::SpannedToken;
use crate::typechecker::{TypeCheckResult, TypeChecker};

/// Convert a byte offset into the source string into a (line, column)
/// pair suitable for diagnostic display. 1-indexed for both axes,
/// matching the rest of `karac`'s diagnostic output.
///
/// Originally lived in `cli.rs`; promoted here so codegen's debugger-
/// contract metadata emission (Phase 8 § Auto-Concurrency Codegen —
/// Debugger Contract slice 3) can record `(line, col)` for each `par {}`
/// site without depending on cli-private state.
pub fn byte_offset_to_line_col(source: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in source.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Tokenize source code into a vector of spanned tokens.
pub fn tokenize(source: &str) -> Vec<SpannedToken> {
    let mut lexer = Lexer::new(source);
    let mut tokens = Vec::new();

    loop {
        let spanned = lexer.next_token();
        let is_eof = spanned.token == crate::token::Token::EOF;
        tokens.push(spanned);
        if is_eof {
            break;
        }
    }
    tokens
}

/// Parse source code into an AST with error reporting.
pub fn parse(source: &str) -> ParseResult {
    let tokens = tokenize(source);
    let parser = Parser::new(tokens);
    parser.parse()
}

/// Resolve names in a parsed program.
pub fn resolve(program: &Program) -> ResolveResult {
    let resolver = Resolver::new(program);
    resolver.resolve()
}

/// Type-check a parsed and resolved program.
pub fn typecheck(program: &Program, resolve_result: &ResolveResult) -> TypeCheckResult {
    let checker = TypeChecker::new(program, resolve_result);
    checker.check()
}

/// Rewrite operator expressions into trait-method calls in place.
/// Runs after typecheck (uses inferred operand types) and before
/// effectcheck / ownership / interpret / codegen.
pub fn lower(program: &mut Program, tc: &TypeCheckResult) {
    crate::lowering::lower_program(program, tc);
}

/// Check effects in a parsed program (default policy: `Declared`).
pub fn effectcheck(program: &Program) -> EffectCheckResult {
    let checker = EffectChecker::new(program);
    checker.check()
}

/// Check effects in a parsed program with an explicit public-effects policy.
/// See `effectchecker::PublicEffectsPolicy`.
pub fn effectcheck_with_policy(
    program: &Program,
    policy: PublicEffectsPolicy,
) -> EffectCheckResult {
    let checker = EffectChecker::new_with_policy(program, policy);
    checker.check()
}

/// Check effects in a parsed program with an explicit policy and compile profile.
/// The profile gates which effects are legal at `extern` declaration sites.
pub fn effectcheck_with_profile(
    program: &Program,
    policy: PublicEffectsPolicy,
    profile: CompileProfile,
) -> EffectCheckResult {
    let checker = EffectChecker::new_with_policy_and_profile(program, policy, profile);
    checker.check()
}

/// Check effects with the typechecker's method-callee resolution table threaded
/// in. Required for method-call-site analyses (`with E` unification, Fn-slot
/// subtyping, function-reference arg propagation through polymorphic methods)
/// to resolve to the precise `Type.method` instead of an over-approximation
/// over every method with a matching name. Pass the result of `typecheck`
/// (specifically `tc.method_callee_types`) before `lower`.
pub fn effectcheck_with_method_types(
    program: &Program,
    policy: PublicEffectsPolicy,
    profile: CompileProfile,
    method_callee_types: std::collections::HashMap<crate::resolver::SpanKey, String>,
) -> EffectCheckResult {
    let checker = EffectChecker::new_with_policy_and_profile(program, policy, profile)
        .with_method_callee_types(method_callee_types);
    checker.check()
}

/// Check effects with both the method-callee resolution table and the per-call
/// type-parameter substitutions from the typechecker (Round 10.3 step 7).
/// Required for E0404 diagnostics on compound polymorphic calls to render the
/// callee's monomorphized signature.
pub fn effectcheck_with_typecheck_data(
    program: &Program,
    policy: PublicEffectsPolicy,
    profile: CompileProfile,
    method_callee_types: std::collections::HashMap<crate::resolver::SpanKey, String>,
    call_type_subs: std::collections::HashMap<
        crate::resolver::SpanKey,
        std::collections::HashMap<String, String>,
    >,
) -> EffectCheckResult {
    let checker = EffectChecker::new_with_policy_and_profile(program, policy, profile)
        .with_method_callee_types(method_callee_types)
        .with_call_type_subs(call_type_subs);
    checker.check()
}

/// Analyze concurrency opportunities in a parsed program.
pub fn concurrency_analyze(program: &Program, effects: &EffectCheckResult) -> ConcurrencyAnalysis {
    let checker = ConcurrencyChecker::new(program, effects);
    checker.analyze()
}

/// Check ownership and move semantics.
pub fn ownershipcheck(
    program: &Program,
    typecheck_result: &TypeCheckResult,
) -> OwnershipCheckResult {
    let checker = OwnershipChecker::new(program, typecheck_result);
    checker.check()
}

/// Run the formal RC predicate pipeline (use classifier → CFG →
/// dominator tree → predicate) over every function in `program`.
/// Returns `function_key → binding → witness`, mirroring the shape
/// of `OwnershipCheckResult::rc_values`. Round 12.10 parity scaffold.
///
/// This driver does NOT replace `ownershipcheck`'s live diagnostics —
/// it only catches the trigger-1 (branch-divergent re-use after
/// consume) flavor today. Closure-capture (trigger 2) and container-store
/// (trigger 3) classification stay in `OwnershipChecker` until later
/// rounds wire them into the use classifier.
pub fn predicate_rc_candidates(
    program: &Program,
    typecheck_result: &TypeCheckResult,
) -> std::collections::HashMap<String, std::collections::HashMap<String, rc_predicate::RcWitness>> {
    rc_predicate::predicate_rc_candidates_for_program(program, typecheck_result)
}

/// Round 12.15: companion driver for direct use-after-move. Returns
/// the witnesses where a Consume site of a binding strictly precedes
/// another use (same-block source order or cross-block dominance) —
/// the error case the formal RC predicate explicitly excludes. Same
/// shape as `predicate_rc_candidates`; consumed by the parity matrix
/// in `tests/rc_predicate_parity.rs` and the eventual in-place
/// integration into `OwnershipChecker::check_function_body`.
pub fn predicate_uam_candidates(
    program: &Program,
    typecheck_result: &TypeCheckResult,
) -> std::collections::HashMap<String, std::collections::HashMap<String, rc_predicate::UamWitness>>
{
    rc_predicate::predicate_uam_candidates_for_program(program, typecheck_result)
}

/// Check for closures that capture a provider-rooted resource and escape
/// their `with_provider` / `providers { }` scope. See design.md §
/// Provider-Rooted Resources and `src/provider_escape.rs` for scope.
///
/// When `types` is `Some`, instance-method calls are resolved against
/// the typechecker's `expr_types` map; pass `None` when typecheck info
/// is unavailable (the checker still catches every other escape path).
pub fn provider_escape_check(
    program: &Program,
    types: Option<&TypeCheckResult>,
) -> Vec<provider_escape::EscapeError> {
    provider_escape::check_provider_escape(program, types)
}

/// Run a program through all phases and execute it via the interpreter.
/// Returns captured output lines (for testing).
pub fn run_program(source: &str) -> Vec<String> {
    let (output, _trace, _truncated) = run_program_with_trace(source);
    output
}

/// Run a program and return (output_lines, error_trace, trace_truncated).
pub fn run_program_with_trace(
    source: &str,
) -> (Vec<String>, Vec<interpreter::ErrorTraceFrame>, bool) {
    let (output, _errors, trace, truncated) = run_program_full(source);
    (output, trace, truncated)
}

/// Run a program and return (output_lines, drop_trace). The drop trace
/// records the order in which `CleanupAction::Drop` slots fire — both
/// NLL early-drops (mid-block, after a binding's last use) and scope-exit
/// drops drained from the unified cleanup stack. Used by sub-step 3
/// (live-range-end placement) tests since the interpreter has no
/// observable user-`impl Drop` dispatch yet.
pub fn run_program_with_drops(source: &str) -> (Vec<String>, Vec<String>) {
    let mut parsed = parse(source);
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
    lower(&mut parsed.program, &typed);
    let mut interp = interpreter::Interpreter::new(&parsed.program, &typed);
    interp.captured_output = Some(Vec::new());
    interp.run();
    let output = interp.captured_output.take().unwrap_or_default();
    let drops = std::mem::take(&mut interp.drop_trace);
    (output, drops)
}

/// Run a program with `dbg()` output capture enabled. Returns
/// `(stdout_lines, dbg_lines)`. The interpreter is configured with
/// `source_text` set so `dbg()` can slice expression text from the
/// source. `mode` selects terminal-format vs JSON-format dbg lines —
/// see [`interpreter::DbgOutputMode`]. Each dbg line includes the
/// trailing `\n`. Used by `tests/interpreter.rs` to assert exact dbg
/// formatting.
pub fn run_program_with_dbg(
    source: &str,
    mode: interpreter::DbgOutputMode,
) -> (Vec<String>, Vec<String>) {
    let mut parsed = parse(source);
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
    lower(&mut parsed.program, &typed);
    let mut interp = interpreter::Interpreter::new(&parsed.program, &typed);
    interp.captured_output = Some(Vec::new());
    interp.captured_dbg = Some(Vec::new());
    interp.set_source_text(source);
    interp.set_source_filename("test.kara");
    interp.set_dbg_output_mode(mode);
    interp.run();
    let stdout = interp.captured_output.take().unwrap_or_default();
    let dbg = interp.captured_dbg.take().unwrap_or_default();
    (stdout, dbg)
}

/// Run a program and return output, runtime errors, error trace, and truncation flag.
/// Used by tests that need to assert on user-triggered runtime errors (division by
/// zero, integer overflow, unwrap of None, index out of bounds, etc.) without
/// catching a Rust panic.
pub fn run_program_full(
    source: &str,
) -> (
    Vec<String>,
    Vec<interpreter::RuntimeError>,
    Vec<interpreter::ErrorTraceFrame>,
    bool,
) {
    let mut parsed = parse(source);
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
    // Type-check but don't abort on errors — the tree-walk interpreter
    // is dynamically typed and handles generics, partial types, etc.
    let typed = typecheck(&parsed.program, &resolved);
    // Operator lowering: rewrite Binary/Unary into trait-method calls.
    lower(&mut parsed.program, &typed);
    let mut interp = interpreter::Interpreter::new(&parsed.program, &typed);
    interp.captured_output = Some(Vec::new());
    interp.run();
    let trace = interp.error_trace().to_vec();
    let truncated = interp.error_trace_truncated();
    let errors = std::mem::take(&mut interp.runtime_errors);
    let output = interp.captured_output.take().unwrap_or_default();
    (output, errors, trace, truncated)
}
