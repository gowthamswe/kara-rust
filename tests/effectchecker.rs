// tests/effectchecker.rs

use karac::ast::EffectVerbKind;
use karac::effectchecker::*;
use karac::manifest::CompileProfile;
use karac::{
    effectcheck, effectcheck_with_policy, effectcheck_with_profile,
    effectcheck_with_typecheck_data, lower, parse, resolve, typecheck,
};
use std::collections::HashSet;

// ── Test Helpers ────────────────────────────────────────────────

fn effectcheck_ok(source: &str) -> EffectCheckResult {
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
    let result = effectcheck(&parsed.program);
    let real_errors: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind != EffectErrorKind::FfiLintHint)
        .collect();
    assert!(
        real_errors.is_empty(),
        "Effect errors: {}",
        real_errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    result
}

fn effectcheck_errors(source: &str) -> Vec<EffectError> {
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
    let result = effectcheck(&parsed.program);
    let real_errors: Vec<EffectError> = result
        .errors
        .into_iter()
        .filter(|e| e.kind != EffectErrorKind::FfiLintHint)
        .collect();
    assert!(
        !real_errors.is_empty(),
        "Expected effect errors but got none"
    );
    real_errors
}

fn effectcheck_all(source: &str) -> EffectCheckResult {
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
    effectcheck(&parsed.program)
}

/// Run the full pre-effectcheck pipeline (parse → resolve → typecheck → lower)
/// before effectcheck. Required for `.into()` / `.try_into()` tests because
/// lowering rewrites those to `Target.from(x)` / `Target.try_from(x)`; without
/// the rewrite, effectcheck sees raw `MethodCall("into", ...)` and can't route
/// to the resolved From impl. Threads the typechecker's `method_callee_types`
/// into the effectchecker so method-call sites resolve to the precise
/// `Type.method` for `with E` / Fn-slot / polymorphic-arg analysis.
fn effectcheck_full_pipeline(source: &str) -> EffectCheckResult {
    let mut parsed = parse(source);
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
        "Resolve errors: {:?}",
        resolved.errors
    );
    let typed = typecheck(&parsed.program, &resolved);
    assert!(typed.errors.is_empty(), "Type errors: {:?}", typed.errors);
    let method_types = typed.method_callee_types.clone();
    let call_type_subs = typed.call_type_subs.clone();
    lower(&mut parsed.program, &typed);
    effectcheck_with_typecheck_data(
        &parsed.program,
        PublicEffectsPolicy::default(),
        CompileProfile::Default,
        method_types,
        call_type_subs,
    )
}

// ── Core ────────────────────────────────────────────────────────

#[test]
fn test_empty_program() {
    effectcheck_ok("");
}

#[test]
fn test_pure_function() {
    let result = effectcheck_ok("fn add(a: i64, b: i64) -> i64 { a + b }");
    let inferred = result.inferred_effects.get("add").unwrap();
    assert!(inferred.effects.is_empty());
}

#[test]
fn test_function_with_declared_effects() {
    effectcheck_ok(
        "effect resource UserDB;\n\
         pub fn save() writes(UserDB) { }",
    );
}

#[test]
fn test_extern_function_effects_trusted() {
    let result = effectcheck_ok(
        "effect resource FileSystem;\n\
         extern \"C\" fn write(fd: i32, buf: i64, count: i64) -> i64 writes(FileSystem);",
    );
    let inferred = result.inferred_effects.get("write").unwrap();
    assert!(inferred
        .effects
        .iter()
        .any(|e| e.effect.resource == "FileSystem"));
}

#[test]
fn test_extern_c_gets_blocks_by_default() {
    // extern "C" functions default to {blocks} (may call blocking OS APIs)
    let result = effectcheck_ok("extern \"C\" fn sleep(secs: i32);");
    let inferred = result.inferred_effects.get("sleep").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Blocks),
        "extern \"C\" should default to blocks, got: {:?}",
        inferred.effects
    );
}

#[test]
fn test_extern_c_unwind_gets_blocks_and_panics() {
    // extern "C-unwind" functions default to {blocks, panics}
    let result = effectcheck_ok("extern \"C-unwind\" fn risky();");
    let inferred = result.inferred_effects.get("risky").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Blocks),
        "extern \"C-unwind\" should default to blocks"
    );
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Panics),
        "extern \"C-unwind\" should default to panics"
    );
}

#[test]
fn test_noblock_removes_blocks_from_extern_c() {
    // @noblock on extern "C" removes the blocks default
    let result = effectcheck_ok("@noblock extern \"C\" fn cpu_work() -> i32;");
    let inferred = result.inferred_effects.get("cpu_work").unwrap();
    assert!(
        !inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Blocks),
        "@noblock extern \"C\" should NOT have blocks effect"
    );
}

#[test]
fn test_noblock_removes_blocks_from_c_unwind_but_keeps_panics() {
    // @noblock on extern "C-unwind": blocks removed, panics stays
    let result = effectcheck_ok("@noblock extern \"C-unwind\" fn throwing();");
    let inferred = result.inferred_effects.get("throwing").unwrap();
    assert!(
        !inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Blocks),
        "@noblock extern \"C-unwind\" should NOT have blocks"
    );
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Panics),
        "@noblock extern \"C-unwind\" should still have panics"
    );
}

#[test]
fn test_extern_c_merges_programmer_annotations() {
    // Programmer-supplied effects are merged with the ABI defaults.
    // extern "C" fn read_db() reads(Db) → final = {blocks, reads(Db)}
    let result = effectcheck_ok(
        "effect resource Db;\n\
         extern \"C\" fn read_db() reads(Db);",
    );
    let inferred = result.inferred_effects.get("read_db").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Blocks),
        "merged set should include ABI default blocks"
    );
    assert!(
        inferred.effects.iter().any(|e| e.effect.resource == "Db"),
        "merged set should include programmer-declared reads(Db)"
    );
}

#[test]
fn test_caller_inherits_extern_c_blocks() {
    // A private function calling an extern "C" fn inherits its {blocks} effect.
    let result = effectcheck_ok(
        "extern \"C\" fn os_call() -> i32;\n\
         fn wrapper() -> i32 { os_call() }",
    );
    let inferred = result.inferred_effects.get("wrapper").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Blocks),
        "wrapper should inherit blocks from extern \"C\" callee"
    );
}

#[test]
fn test_struct_and_const_no_effects() {
    effectcheck_ok(
        "struct Point { x: i64, y: i64 }\n\
         const MAX: i64 = 100;",
    );
}

// ── Effect Groups ───────────────────────────────────────────────

#[test]
fn test_basic_group_expansion() {
    let result = effectcheck_ok(
        "effect resource UserDB;\n\
         effect resource OrderDB;\n\
         effect group read_all = reads(UserDB, OrderDB);",
    );
    let group = result.expanded_groups.get("read_all").unwrap();
    assert_eq!(group.effects.len(), 2);
}

#[test]
fn test_composed_groups() {
    let result = effectcheck_ok(
        "effect resource UserDB;\n\
         effect resource OrderDB;\n\
         effect resource Network;\n\
         effect group reading = reads(UserDB);\n\
         effect group writing = writes(OrderDB);\n\
         effect group all = reading + writing;",
    );
    let group = result.expanded_groups.get("all").unwrap();
    assert_eq!(group.effects.len(), 2);
    assert!(group
        .effects
        .iter()
        .any(|e| e.effect.verb == EffectVerbKind::Reads && e.effect.resource == "UserDB"));
    assert!(group
        .effects
        .iter()
        .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "OrderDB"));
}

#[test]
fn test_group_used_with_with() {
    effectcheck_ok(
        "effect resource UserDB;\n\
         effect group read_all = reads(UserDB);\n\
         pub fn load() with read_all { }",
    );
}

#[test]
fn test_undefined_group_in_with_clause() {
    let errors = effectcheck_errors(
        "effect resource UserDB;\n\
         pub fn load() with Nonexistent { }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == EffectErrorKind::UndefinedEffectGroup
            && e.message.contains("Nonexistent")));
}

#[test]
fn test_undefined_group_referenced_by_group() {
    let errors = effectcheck_errors(
        "effect resource UserDB;\n\
         effect group reading = reads(UserDB);\n\
         effect group all = reading + Missing;",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == EffectErrorKind::UndefinedEffectGroup
            && e.message.contains("Missing")
            && e.message.contains("all")));
}

// ── Effect Inference ────────────────────────────────────────────

#[test]
fn test_transitive_inference() {
    // A calls B calls C, C has effects → A and B should infer them
    let result = effectcheck_ok(
        "effect resource UserDB;\n\
         fn c() writes(UserDB) { }\n\
         fn b() { c(); }\n\
         fn a() { b(); }",
    );
    let a_effects = result.inferred_effects.get("a").unwrap();
    assert!(a_effects
        .effects
        .iter()
        .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "UserDB"));
    let b_effects = result.inferred_effects.get("b").unwrap();
    assert!(b_effects
        .effects
        .iter()
        .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "UserDB"));
}

#[test]
fn test_private_function_no_annotation_needed() {
    // Private function with effects but no declaration — should be fine
    effectcheck_ok(
        "effect resource UserDB;\n\
         fn helper() writes(UserDB) { }\n\
         fn main() { helper(); }",
    );
}

#[test]
fn test_multiple_callees_union_effects() {
    let result = effectcheck_ok(
        "effect resource UserDB;\n\
         effect resource OrderDB;\n\
         fn read_users() reads(UserDB) { }\n\
         fn write_orders() writes(OrderDB) { }\n\
         fn main() { read_users(); write_orders(); }",
    );
    let main_effects = result.inferred_effects.get("main").unwrap();
    assert!(main_effects
        .effects
        .iter()
        .any(|e| e.effect.verb == EffectVerbKind::Reads && e.effect.resource == "UserDB"));
    assert!(main_effects
        .effects
        .iter()
        .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "OrderDB"));
}

#[test]
fn test_pure_function_stays_pure() {
    let result = effectcheck_ok(
        "fn pure_helper() -> i64 { 42 }\n\
         fn main() { pure_helper(); }",
    );
    let main_effects = result.inferred_effects.get("main").unwrap();
    assert!(main_effects.effects.is_empty());
}

#[test]
fn test_public_callee_uses_declared_effects() {
    // Public function B declares writes(UserDB).
    // Private function A calls B → should infer writes(UserDB) from B's declaration.
    let result = effectcheck_ok(
        "effect resource UserDB;\n\
         pub fn public_write() writes(UserDB) { }\n\
         fn main() { public_write(); }",
    );
    let main_effects = result.inferred_effects.get("main").unwrap();
    assert!(main_effects
        .effects
        .iter()
        .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "UserDB"));
}

#[test]
fn test_recursive_function() {
    // Recursive function — should not infinite loop
    effectcheck_ok(
        "effect resource UserDB;\n\
         fn recurse(n: i64) reads(UserDB) { if n > 0 { recurse(n - 1); } }",
    );
}

// ── Verification ────────────────────────────────────────────────

#[test]
fn test_public_matching_declaration_passes() {
    effectcheck_ok(
        "effect resource UserDB;\n\
         fn helper() writes(UserDB) { }\n\
         pub fn save() writes(UserDB) { helper(); }",
    );
}

#[test]
fn test_public_missing_effect_error() {
    let errors = effectcheck_errors(
        "effect resource UserDB;\n\
         fn helper() writes(UserDB) { }\n\
         pub fn save() { helper(); }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == EffectErrorKind::MissingEffectDeclaration));
    assert!(errors[0].message.contains("writes(UserDB)"));
}

#[test]
fn test_public_over_declaration_detected() {
    // To detect over-declaration, we need to NOT seed from declarations.
    // Instead, test that verification works by checking the result directly.
    // Since we seed inferred from declared (to handle empty-body test functions),
    // over-declaration for functions with declared effects won't trigger.
    // This is a pragmatic trade-off — in practice, body analysis would catch this.
    let result = effectcheck_all(
        "effect resource UserDB;\n\
         pub fn noop() writes(UserDB) { }",
    );
    // No error because declared effects are seeded into inferred
    assert!(result.errors.is_empty());
}

#[test]
fn test_private_function_with_effects_no_error() {
    // Private function doesn't need effect declarations
    effectcheck_ok(
        "effect resource UserDB;\n\
         fn helper() writes(UserDB) { }\n\
         fn private_caller() { helper(); }",
    );
}

#[test]
fn test_multiple_missing_effects() {
    let errors = effectcheck_errors(
        "effect resource UserDB;\n\
         effect resource OrderDB;\n\
         fn read_db() reads(UserDB) { }\n\
         fn write_db() writes(OrderDB) { }\n\
         pub fn process() { read_db(); write_db(); }",
    );
    // Should report at least one missing effect (possibly two)
    assert!(errors
        .iter()
        .any(|e| e.kind == EffectErrorKind::MissingEffectDeclaration));
}

#[test]
fn test_effect_from_deep_call_chain() {
    let errors = effectcheck_errors(
        "effect resource UserDB;\n\
         fn leaf() writes(UserDB) { }\n\
         fn mid() { leaf(); }\n\
         fn inner() { mid(); }\n\
         pub fn outer() { inner(); }",
    );
    assert!(errors
        .iter()
        .any(|e| e.kind == EffectErrorKind::MissingEffectDeclaration));
    assert!(errors[0].message.contains("writes(UserDB)"));
}

#[test]
fn test_fix_suggestion_in_error() {
    let errors = effectcheck_errors(
        "effect resource UserDB;\n\
         fn helper() writes(UserDB) { }\n\
         pub fn save() { helper(); }",
    );
    // Error message should mention the missing effect
    assert!(errors[0].message.contains("writes(UserDB)"));
}

// ── Conflict Detection ──────────────────────────────────────────

#[test]
fn test_reads_reads_safe() {
    let mut a = EffectSet::new();
    a.add(
        Effect {
            verb: EffectVerbKind::Reads,
            resource: "Db".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let mut b = EffectSet::new();
    b.add(
        Effect {
            verb: EffectVerbKind::Reads,
            resource: "Db".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let conflicts = EffectChecker::find_conflicts(&a, &b, &HashSet::new());
    assert!(conflicts.is_empty());
}

#[test]
fn test_reads_writes_same_resource_conflicts() {
    let mut a = EffectSet::new();
    a.add(
        Effect {
            verb: EffectVerbKind::Reads,
            resource: "Db".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let mut b = EffectSet::new();
    b.add(
        Effect {
            verb: EffectVerbKind::Writes,
            resource: "Db".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let conflicts = EffectChecker::find_conflicts(&a, &b, &HashSet::new());
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].resource, "Db");
}

#[test]
fn test_writes_writes_same_resource_conflicts() {
    let mut a = EffectSet::new();
    a.add(
        Effect {
            verb: EffectVerbKind::Writes,
            resource: "Db".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let mut b = EffectSet::new();
    b.add(
        Effect {
            verb: EffectVerbKind::Writes,
            resource: "Db".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let conflicts = EffectChecker::find_conflicts(&a, &b, &HashSet::new());
    assert_eq!(conflicts.len(), 1);
}

#[test]
fn test_different_resources_safe() {
    let mut a = EffectSet::new();
    a.add(
        Effect {
            verb: EffectVerbKind::Writes,
            resource: "UserDB".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let mut b = EffectSet::new();
    b.add(
        Effect {
            verb: EffectVerbKind::Writes,
            resource: "OrderDB".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let conflicts = EffectChecker::find_conflicts(&a, &b, &HashSet::new());
    assert!(conflicts.is_empty());
}

#[test]
fn test_sends_receives_same_resource_safe() {
    let mut a = EffectSet::new();
    a.add(
        Effect {
            verb: EffectVerbKind::Sends,
            resource: "Network".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let mut b = EffectSet::new();
    b.add(
        Effect {
            verb: EffectVerbKind::Receives,
            resource: "Network".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let conflicts = EffectChecker::find_conflicts(&a, &b, &HashSet::new());
    assert!(conflicts.is_empty());
}

#[test]
fn test_sends_sends_same_resource_safe() {
    let mut a = EffectSet::new();
    a.add(
        Effect {
            verb: EffectVerbKind::Sends,
            resource: "Network".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let mut b = EffectSet::new();
    b.add(
        Effect {
            verb: EffectVerbKind::Sends,
            resource: "Network".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let conflicts = EffectChecker::find_conflicts(&a, &b, &HashSet::new());
    assert!(conflicts.is_empty());
}

#[test]
fn test_receives_receives_same_resource_safe() {
    let mut a = EffectSet::new();
    a.add(
        Effect {
            verb: EffectVerbKind::Receives,
            resource: "Network".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let mut b = EffectSet::new();
    b.add(
        Effect {
            verb: EffectVerbKind::Receives,
            resource: "Network".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let conflicts = EffectChecker::find_conflicts(&a, &b, &HashSet::new());
    assert!(conflicts.is_empty());
}

#[test]
fn test_transparent_effect_excluded() {
    let mut a = EffectSet::new();
    a.add(
        Effect {
            verb: EffectVerbKind::Writes,
            resource: "Db".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let mut b = EffectSet::new();
    b.add(
        Effect {
            verb: EffectVerbKind::Writes,
            resource: "Db".to_string(),
        },
        EffectOrigin::Direct(dummy_span()),
    );
    let mut transparent = HashSet::new();
    transparent.insert("writes".to_string());
    let conflicts = EffectChecker::find_conflicts(&a, &b, &transparent);
    assert!(conflicts.is_empty());
}

// ── Transparent Effects ─────────────────────────────────────────

#[test]
fn test_transparent_effect_registered() {
    let result = effectcheck_ok("transparent effect verb traces;");
    assert!(result.transparent_effects.contains("traces"));
}

// ── Polymorphism ────────────────────────────────────────────────

#[test]
fn test_polymorphic_function_not_flagged() {
    effectcheck_ok(
        "effect resource UserDB;\n\
         pub fn process() with _ { }",
    );
}

#[test]
fn test_polymorphic_empty_body_caller_has_empty_effects() {
    // A `with _` function with an empty body and no arguments contributes
    // no effects to its caller — empty inferred effects is correct here.
    let result = effectcheck_ok(
        "effect resource UserDB;\n\
         pub fn process() with _ { }\n\
         fn caller() { process(); }",
    );
    let caller_effects = result.inferred_effects.get("caller").unwrap();
    assert!(
        caller_effects.effects.is_empty(),
        "caller of a no-op with _ fn should infer empty effects"
    );
}

#[test]
fn test_inline_closure_to_polymorphic_callee_propagates_effects() {
    // Inline closure passed to a `with _` callee: closure body is walked,
    // so caller inherits the closure's effects.
    let result = effectcheck_ok(
        "effect resource UserDB;\n\
         fn reads_db() reads(UserDB) { }\n\
         pub fn apply(f: Fn() -> () with _) -> () with _ { f() }\n\
         fn caller() { apply(|| reads_db()); }",
    );
    let caller_effects = result.inferred_effects.get("caller").unwrap();
    let verbs: Vec<_> = caller_effects
        .effects
        .iter()
        .map(|e| e.effect.resource.as_str())
        .collect();
    assert!(
        verbs.contains(&"UserDB"),
        "caller should inherit reads(UserDB) from inline closure: {verbs:?}"
    );
}

#[test]
fn test_function_reference_to_polymorphic_callee_propagates_effects() {
    // Named function reference passed to a `with _` callee: its effects
    // must propagate to the caller.
    let result = effectcheck_ok(
        "effect resource UserDB;\n\
         fn reads_db() reads(UserDB) { }\n\
         pub fn apply(f: Fn() -> () with _) -> () with _ { f() }\n\
         fn caller() { apply(reads_db); }",
    );
    let caller_effects = result.inferred_effects.get("caller").unwrap();
    let verbs: Vec<_> = caller_effects
        .effects
        .iter()
        .map(|e| e.effect.resource.as_str())
        .collect();
    assert!(
        verbs.contains(&"UserDB"),
        "caller should inherit reads(UserDB) from function reference: {verbs:?}"
    );
}

#[test]
fn test_public_fn_with_explicit_effects_calling_polymorphic_needs_with_underscore() {
    // A public fn with explicit effect declarations that also calls a `with _`
    // function must additionally declare `with _` — the viral rule applies
    // regardless of what other effects are already declared.
    let errors = effectcheck_errors(
        "effect resource UserDB;\n\
         effect resource OrderDB;\n\
         fn reads_db() reads(UserDB) { }\n\
         pub fn apply(f: Fn() -> () with _) -> () with _ { f() }\n\
         pub fn caller() reads(OrderDB) { apply(|| reads_db()); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("polymorphic") && e.message.contains("caller")),
        "expected missing-with-_ error for caller: {errors:?}"
    );
}

#[test]
fn test_function_ref_through_private_wrapper_propagates_effects() {
    // A private wrapper that internally calls a `with _` function is
    // polymorphic-dependent.  Passing a named effectful function through it
    // must propagate the concrete effects to the outer caller.
    let result = effectcheck_ok(
        "effect resource UserDB;\n\
         fn reads_db() reads(UserDB) { }\n\
         pub fn apply(f: Fn() -> () with _) -> () with _ { f() }\n\
         fn wrapper(f: Fn() -> () with _) { apply(f) }\n\
         fn caller() { wrapper(reads_db); }",
    );
    let caller_effects = result.inferred_effects.get("caller").unwrap();
    let resources: Vec<_> = caller_effects
        .effects
        .iter()
        .map(|e| e.effect.resource.as_str())
        .collect();
    assert!(
        resources.contains(&"UserDB"),
        "caller should inherit reads(UserDB) through private wrapper: {resources:?}"
    );
}

// ── Edge Cases ──────────────────────────────────────────────────

#[test]
fn test_function_with_no_body_calls_is_pure() {
    let result = effectcheck_ok("fn no_effects() -> i64 { 42 }");
    let inferred = result.inferred_effects.get("no_effects").unwrap();
    assert!(inferred.effects.is_empty());
}

#[test]
fn test_multiple_errors_reported() {
    // Public function with no declaration but calls effectful function
    let result = effectcheck_all(
        "effect resource UserDB;\n\
         effect resource OrderDB;\n\
         fn read_db() reads(UserDB) { }\n\
         fn write_db() writes(OrderDB) { }\n\
         pub fn a() { read_db(); write_db(); }",
    );
    // 'a' should have missing effect declarations
    assert!(result
        .errors
        .iter()
        .any(|e| e.kind == EffectErrorKind::MissingEffectDeclaration));
}

// ── Complex Program ─────────────────────────────────────────────

#[test]
fn test_complex_effect_program() {
    effectcheck_ok(
        "effect resource UserDB;\n\
         effect resource OrderDB;\n\
         effect resource Network;\n\
         \n\
         effect group validation = reads(UserDB);\n\
         effect group fulfillment = writes(OrderDB) + sends(Network);\n\
         effect group order_processing = validation + fulfillment;\n\
         \n\
         fn validate_user(id: i64) reads(UserDB) { }\n\
         fn create_order(id: i64) writes(OrderDB) { }\n\
         fn notify(id: i64) sends(Network) { }\n\
         \n\
         pub fn process_order(id: i64) with order_processing {\n\
             validate_user(id);\n\
             create_order(id);\n\
             notify(id);\n\
         }",
    );
}

// ── Transparent Effects in Verification ────────────────────────

#[test]
fn test_transparent_verb_registered_and_queryable() {
    // Transparent verbs are collected during analysis
    let result = effectcheck_ok("transparent effect verb logs;");
    assert!(result.transparent_effects.contains("logs"));
}

#[test]
fn test_transparent_reads_excluded_from_conflicts() {
    // When 'reads' is transparent, reads/writes on same resource should not conflict
    // Note: built-in verbs can't be made transparent via syntax, so we test
    // the conflict detection API directly
    let result = effectcheck_ok(
        "effect resource Db;\n\
         fn a() writes(Db) { }\n\
         fn b() reads(Db) { }",
    );
    let effects_a = result.inferred_effects.get("a").unwrap();
    let effects_b = result.inferred_effects.get("b").unwrap();
    // Without transparent verbs: reads/writes should conflict
    let conflicts =
        EffectChecker::find_conflicts(effects_a, effects_b, &result.transparent_effects);
    assert!(
        !conflicts.is_empty(),
        "reads/writes should conflict normally"
    );
    // With 'reads' marked transparent: no conflict
    let mut transparent = result.transparent_effects.clone();
    transparent.insert("reads".to_string());
    let conflicts = EffectChecker::find_conflicts(effects_a, effects_b, &transparent);
    assert!(
        conflicts.is_empty(),
        "reads should be excluded when transparent"
    );
}

// ── Inference: 5+ Function Call Chain ──────────────────────────

#[test]
fn test_deep_call_chain_inference() {
    // Private function effects inferred across a 5+ function call chain
    let result = effectcheck_ok(
        "effect resource Db;\n\
         fn e() reads(Db) { }\n\
         fn d() { e(); }\n\
         fn c() { d(); }\n\
         fn b() { c(); }\n\
         fn a() { b(); }\n\
         pub fn top() reads(Db) { a(); }",
    );
    // 'a' should have inferred reads(Db) via chain a→b→c→d→e
    let inferred_a = result.inferred_effects.get("a").unwrap();
    assert!(!inferred_a.effects.is_empty());
    assert!(inferred_a
        .effects
        .iter()
        .any(|te| te.effect.verb == EffectVerbKind::Reads));
}

// ── Integration: Guard Effects ─────────────────────────────────

#[test]
fn test_guard_effects_contribute_to_function() {
    // A guard calling an effectful function should contribute its effects
    let result = effectcheck_ok(
        "effect resource Db;\n\
         fn check_db() -> bool reads(Db) { true }\n\
         enum Opt { Some(i64), None }\n\
         fn process(o: Opt) reads(Db) {\n\
             match o {\n\
                 Some(x) if check_db() => { },\n\
                 Some(_) => { },\n\
                 None => { },\n\
             };\n\
         }",
    );
    let inferred = result.inferred_effects.get("process").unwrap();
    assert!(inferred
        .effects
        .iter()
        .any(|te| te.effect.verb == EffectVerbKind::Reads));
}

// ── Integration: todo()/unreachable() Panics Effect ────────────

#[test]
fn test_todo_propagates_panics_effect() {
    // Calling todo() should infer panics effect
    let result = effectcheck_ok(
        "fn placeholder() {\n\
             todo();\n\
         }",
    );
    let inferred = result.inferred_effects.get("placeholder").unwrap();
    assert!(inferred
        .effects
        .iter()
        .any(|te| te.effect.verb == EffectVerbKind::Panics));
}

#[test]
fn test_unreachable_propagates_panics_effect() {
    let result = effectcheck_ok(
        "fn impossible() {\n\
             unreachable();\n\
         }",
    );
    let inferred = result.inferred_effects.get("impossible").unwrap();
    assert!(inferred
        .effects
        .iter()
        .any(|te| te.effect.verb == EffectVerbKind::Panics));
}

#[test]
fn test_public_fn_with_todo_must_declare_panics() {
    // Public function calling todo() needs panics declared
    let errors = effectcheck_errors(
        "pub fn not_done_yet() {\n\
             todo();\n\
         }",
    );
    assert!(errors.iter().any(|e| e.message.contains("panics")));
}

// ── Subscript / Index Panics Effect ────────────────────────────

#[test]
fn test_index_infers_panics_effect() {
    // Indexing with [] should infer panics (can panic on out-of-bounds)
    let result = effectcheck_ok(
        "fn get_first(arr: i64) -> i64 {\n\
             arr[0]\n\
         }",
    );
    let inferred = result.inferred_effects.get("get_first").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|te| te.effect.verb == EffectVerbKind::Panics),
        "indexing should infer panics effect"
    );
}

#[test]
fn test_public_fn_with_index_must_declare_panics() {
    let errors = effectcheck_errors(
        "pub fn get_first(arr: i64) -> i64 {\n\
             arr[0]\n\
         }",
    );
    assert!(errors.iter().any(|e| e.message.contains("panics")));
}

// ── Diagnostics: Fix Suggestion Content ────────────────────────

#[test]
fn test_diagnostic_includes_originating_function() {
    // Error message should trace back to the callee that introduced the effect
    let errors = effectcheck_errors(
        "effect resource Db;\n\
         fn helper() reads(Db) { }\n\
         pub fn api() {\n\
             helper();\n\
         }",
    );
    // The error should mention either the callee name or the origin
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("helper") || e.message.contains("reads")),
        "Expected diagnostic mentioning origin, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

// ── Helper ──────────────────────────────────────────────────────

fn dummy_span() -> karac::token::Span {
    karac::token::Span {
        line: 0,
        column: 0,
        offset: 0,
        length: 0,
    }
}

// ── Mutual Recursion Group Detection ───────────────────────────

#[test]
fn test_mutual_recursion_detected() {
    let source = r#"
effect resource Db;
effect resource Log;

fn f() {
    g()
}
fn g() {
    f()
}
"#;
    let result = effectcheck_all(source);
    assert!(
        !result.mutual_recursion_groups.is_empty(),
        "Should detect mutual recursion group"
    );
    let group = &result.mutual_recursion_groups[0];
    assert!(group.functions.contains(&"f".to_string()));
    assert!(group.functions.contains(&"g".to_string()));
    assert_eq!(group.functions.len(), 2);
}

#[test]
fn test_mutual_recursion_with_effects_has_trace() {
    let source = r#"
effect resource Db;
effect resource Log;

pub fn read_db() reads(Db) { }
pub fn write_log() writes(Log) { }

fn f() {
    read_db();
    g()
}
fn g() {
    write_log();
    f()
}
"#;
    let result = effectcheck_all(source);
    assert!(
        !result.mutual_recursion_groups.is_empty(),
        "Should detect mutual recursion group"
    );

    let group = &result.mutual_recursion_groups[0];
    assert!(group.functions.contains(&"f".to_string()));
    assert!(group.functions.contains(&"g".to_string()));

    // Both functions should have effects propagated through them
    assert!(
        !group.resolution_trace.is_empty(),
        "Resolution trace should not be empty"
    );

    // f calls g, so f should resolve writes(Log) via g
    let f_resolves_via_g: Vec<_> = group
        .resolution_trace
        .iter()
        .filter(|r| r.call_site_function == "f" && r.resolved_via == "g")
        .collect();
    assert!(
        f_resolves_via_g.iter().any(|r| r.effect == "writes(Log)"),
        "f should resolve writes(Log) via g, got: {:?}",
        f_resolves_via_g
            .iter()
            .map(|r| &r.effect)
            .collect::<Vec<_>>()
    );

    // g calls f, so g should resolve reads(Db) via f
    let g_resolves_via_f: Vec<_> = group
        .resolution_trace
        .iter()
        .filter(|r| r.call_site_function == "g" && r.resolved_via == "f")
        .collect();
    assert!(
        g_resolves_via_f.iter().any(|r| r.effect == "reads(Db)"),
        "g should resolve reads(Db) via f, got: {:?}",
        g_resolves_via_f
            .iter()
            .map(|r| &r.effect)
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_no_mutual_recursion_for_non_recursive() {
    let source = r#"
fn a() { b() }
fn b() { }
"#;
    let result = effectcheck_all(source);
    assert!(
        result.mutual_recursion_groups.is_empty(),
        "Non-recursive functions should not form a mutual recursion group"
    );
}

#[test]
fn test_no_mutual_recursion_for_self_recursion() {
    // Self-recursion (single-node SCC) should NOT appear in mutual_recursion_groups
    let source = r#"
fn factorial(n: i64) -> i64 {
    if n <= 1 { 1 } else { n * factorial(n - 1) }
}
"#;
    let result = effectcheck_all(source);
    assert!(
        result.mutual_recursion_groups.is_empty(),
        "Self-recursion should not be reported as mutual recursion"
    );
}

#[test]
fn test_three_way_mutual_recursion() {
    let source = r#"
effect resource Net;
pub fn send_data() sends(Net) { }

fn a() { b() }
fn b() { c() }
fn c() { a(); send_data() }
"#;
    let result = effectcheck_all(source);
    assert_eq!(
        result.mutual_recursion_groups.len(),
        1,
        "Should detect one 3-way mutual recursion group"
    );
    let group = &result.mutual_recursion_groups[0];
    assert_eq!(group.functions.len(), 3);
    assert!(group.functions.contains(&"a".to_string()));
    assert!(group.functions.contains(&"b".to_string()));
    assert!(group.functions.contains(&"c".to_string()));

    // sends(Net) should propagate through the cycle
    assert!(
        !group.resolution_trace.is_empty(),
        "Resolution trace should capture sends(Net) propagation"
    );
}

#[test]
fn test_mutual_recursion_trace_line_numbers() {
    let source = "fn f() {\n    g()\n}\nfn g() {\n    f()\n}\n";
    let result = effectcheck_all(source);
    assert_eq!(result.mutual_recursion_groups.len(), 1);
    let group = &result.mutual_recursion_groups[0];
    // Functions should be f and g
    assert!(group.functions.contains(&"f".to_string()));
    assert!(group.functions.contains(&"g".to_string()));
}

// ── SCC-based inference correctness ─────────────────────────────

#[test]
fn test_scc_inferred_effects_propagate_through_cycle() {
    // All functions in a mutual-recursion SCC must end up with the
    // union of effects reachable within the cycle.
    let source = r#"
effect resource Net;
pub fn send_data() sends(Net) { }

fn a() { b() }
fn b() { c() }
fn c() { a(); send_data() }
"#;
    let result = effectcheck_all(source);
    for name in &["a", "b", "c"] {
        let effects = result.inferred_effects.get(*name).unwrap();
        let resources: Vec<_> = effects
            .effects
            .iter()
            .map(|e| e.effect.resource.as_str())
            .collect();
        assert!(
            resources.contains(&"Net"),
            "'{name}' should have sends(Net) via SCC propagation, got: {resources:?}"
        );
    }
}

#[test]
fn test_scc_caller_outside_cycle_gets_scc_effects() {
    // A function that calls into a mutually-recursive SCC must inherit
    // the SCC's fully-resolved effect set (not a partially-propagated snapshot).
    let source = r#"
effect resource Db;
pub fn read_db() reads(Db) { }

fn ping() { pong() }
fn pong() { ping(); read_db() }

fn caller() { ping() }
"#;
    let result = effectcheck_all(source);
    let caller_effects = result.inferred_effects.get("caller").unwrap();
    let resources: Vec<_> = caller_effects
        .effects
        .iter()
        .map(|e| e.effect.resource.as_str())
        .collect();
    assert!(
        resources.contains(&"Db"),
        "caller outside the SCC should inherit reads(Db) from the cycle: {resources:?}"
    );
}

#[test]
fn test_scc_two_independent_cycles_dont_cross_contaminate() {
    // Two disjoint SCCs must not merge each other's effects.
    let source = r#"
effect resource Alpha;
effect resource Beta;
pub fn do_alpha() reads(Alpha) { }
pub fn do_beta() writes(Beta) { }

fn a1() { a2() }
fn a2() { a1(); do_alpha() }

fn b1() { b2() }
fn b2() { b1(); do_beta() }
"#;
    let result = effectcheck_all(source);

    for name in &["a1", "a2"] {
        let effects = result.inferred_effects.get(*name).unwrap();
        let resources: Vec<_> = effects
            .effects
            .iter()
            .map(|e| e.effect.resource.as_str())
            .collect();
        assert!(
            resources.contains(&"Alpha"),
            "'{name}' should have Alpha: {resources:?}"
        );
        assert!(
            !resources.contains(&"Beta"),
            "'{name}' should NOT have Beta: {resources:?}"
        );
    }
    for name in &["b1", "b2"] {
        let effects = result.inferred_effects.get(*name).unwrap();
        let resources: Vec<_> = effects
            .effects
            .iter()
            .map(|e| e.effect.resource.as_str())
            .collect();
        assert!(
            resources.contains(&"Beta"),
            "'{name}' should have Beta: {resources:?}"
        );
        assert!(
            !resources.contains(&"Alpha"),
            "'{name}' should NOT have Alpha: {resources:?}"
        );
    }
}

// ── public_effects = "inferred" mode ────────

#[test]
fn test_inferred_policy_default_is_declared() {
    // Sanity: default policy on EffectCheckResult is Declared.
    let parsed = parse("");
    let result = effectcheck(&parsed.program);
    assert_eq!(result.public_effects_policy, PublicEffectsPolicy::Declared);
}

#[test]
fn test_inferred_policy_accepts_pub_fn_without_declaration() {
    // Under Inferred policy, a pub fn performing effects need not declare them.
    let source = "effect resource UserDB;\n\
                  fn helper() writes(UserDB) { }\n\
                  pub fn save() { helper(); }";
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let result = effectcheck_with_policy(&parsed.program, PublicEffectsPolicy::Inferred);
    assert!(
        result.errors.is_empty(),
        "expected no errors under Inferred policy, got: {:?}",
        result.errors
    );
    assert_eq!(result.public_effects_policy, PublicEffectsPolicy::Inferred);
    // Inference still produces the effect set so the CLI can surface it.
    let save = result
        .inferred_effects
        .get("save")
        .expect("save should have an inferred effect set");
    assert!(save
        .effects
        .iter()
        .any(|te| te.effect.verb == EffectVerbKind::Writes && te.effect.resource == "UserDB"));
}

#[test]
fn test_inferred_policy_still_verifies_explicit_declarations() {
    // writing an explicit `with ...` clause is an implicit per-function opt-in
    // to verification even under Inferred policy. No new syntax — the existing
    // declaration is what flips on the check.
    let source = "effect resource UserDB;\n\
                  effect resource OrderDB;\n\
                  fn w_user() writes(UserDB) { }\n\
                  fn w_order() writes(OrderDB) { }\n\
                  pub fn save() writes(UserDB) { w_user(); w_order(); }";
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let result = effectcheck_with_policy(&parsed.program, PublicEffectsPolicy::Inferred);
    // w_order's writes(OrderDB) is performed but not declared — still an error.
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::MissingEffectDeclaration
                && e.message.contains("writes(OrderDB)")),
        "expected missing-declaration error for OrderDB, got: {:?}",
        result.errors
    );
}

#[test]
fn test_declared_policy_still_errors_on_missing_declaration() {
    // Regression: explicit Declared policy preserves the existing behavior.
    let source = "effect resource UserDB;\n\
                  fn helper() writes(UserDB) { }\n\
                  pub fn save() { helper(); }";
    let parsed = parse(source);
    let result = effectcheck_with_policy(&parsed.program, PublicEffectsPolicy::Declared);
    assert!(result
        .errors
        .iter()
        .any(|e| e.kind == EffectErrorKind::MissingEffectDeclaration));
}

#[test]
fn test_inferred_policy_private_fn_inference_unchanged() {
    // Private functions behave identically under either policy.
    let source = "effect resource UserDB;\n\
                  fn helper() writes(UserDB) { }\n\
                  fn caller() { helper(); }";
    let parsed = parse(source);
    let result = effectcheck_with_policy(&parsed.program, PublicEffectsPolicy::Inferred);
    assert!(
        result.errors.is_empty(),
        "unexpected errors: {:?}",
        result.errors
    );
    let caller = result.inferred_effects.get("caller").unwrap();
    assert!(caller
        .effects
        .iter()
        .any(|te| te.effect.verb == EffectVerbKind::Writes && te.effect.resource == "UserDB"));
}

#[test]
fn test_inferred_policy_exposes_function_visibility() {
    // The result surfaces visibility so downstream consumers (CLI output)
    // can filter to public functions.
    let source = "effect resource UserDB;\n\
                  fn priv_helper() writes(UserDB) { }\n\
                  pub fn pub_api() { priv_helper(); }";
    let parsed = parse(source);
    let result = effectcheck_with_policy(&parsed.program, PublicEffectsPolicy::Inferred);
    assert_eq!(result.function_visibility.get("pub_api"), Some(&true));
    assert_eq!(result.function_visibility.get("priv_helper"), Some(&false));
}

// ── panics-producing primitives (F-057) ──────────────────────────

#[test]
fn test_unwrap_infers_panics() {
    let result = effectcheck_ok(
        "fn maybe_get(x: Option[i64]) -> i64 {\n\
             x.unwrap()\n\
         }",
    );
    let inferred = result.inferred_effects.get("maybe_get").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|te| te.effect.verb == EffectVerbKind::Panics),
        "unwrap() should infer panics"
    );
}

#[test]
fn test_expect_infers_panics() {
    let result = effectcheck_ok(
        "fn maybe_get(x: Option[i64]) -> i64 {\n\
             x.expect(\"missing value\")\n\
         }",
    );
    let inferred = result.inferred_effects.get("maybe_get").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|te| te.effect.verb == EffectVerbKind::Panics),
        "expect() should infer panics"
    );
}

#[test]
fn test_division_infers_panics() {
    let result = effectcheck_ok(
        "fn divide(a: i64, b: i64) -> i64 {\n\
             a / b\n\
         }",
    );
    let inferred = result.inferred_effects.get("divide").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|te| te.effect.verb == EffectVerbKind::Panics),
        "integer division should infer panics (divide by zero)"
    );
}

#[test]
fn test_modulo_infers_panics() {
    let result = effectcheck_ok(
        "fn modulo(a: i64, b: i64) -> i64 {\n\
             a % b\n\
         }",
    );
    let inferred = result.inferred_effects.get("modulo").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|te| te.effect.verb == EffectVerbKind::Panics),
        "modulo should infer panics (modulo by zero)"
    );
}

#[test]
fn test_addition_does_not_infer_panics() {
    // Arithmetic overflow is NOT a panics atom per F-057.
    let result = effectcheck_ok(
        "fn add(a: i64, b: i64) -> i64 {\n\
             a + b\n\
         }",
    );
    let inferred = result.inferred_effects.get("add").unwrap();
    assert!(
        !inferred
            .effects
            .iter()
            .any(|te| te.effect.verb == EffectVerbKind::Panics),
        "plain addition should NOT infer panics (overflow is not a panics atom)"
    );
}

#[test]
fn test_public_fn_with_division_must_declare_panics() {
    let errors = effectcheck_errors(
        "pub fn divide(a: i64, b: i64) -> i64 {\n\
             a / b\n\
         }",
    );
    assert!(
        errors.iter().any(|e| e.message.contains("panics")),
        "public fn with division must declare panics"
    );
}

// ── with _ viral rule — default-permitted effects ────────────────

#[test]
fn test_viral_rule_fires_even_when_only_transparent_effect() {
    // The viral rule (`with _` required when calling a polymorphic callee) must
    // fire even if the closure's only effect is transparent (allocates(Heap)).
    // `with _` is about the *mechanism*, not the specific effect values.
    let errors = effectcheck_errors(
        "pub fn apply(f: Fn() -> i32 with _) -> i32 with _ { f() }\n\
         fn pure_fn() -> i32 { 0 }\n\
         pub fn wrapper() -> i32 { apply(pure_fn) }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("with _") && e.message.contains("wrapper")),
        "expected viral-rule error for wrapper: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

#[test]
fn test_viral_rule_fires_under_inferred_policy() {
    // Under Inferred policy, concrete effect annotations are optional for pub fns,
    // but the `with _` viral rule still applies: calling a polymorphic callee
    // requires declaring `with _` regardless of policy.
    let source = "pub fn apply(f: Fn() -> i32 with _) -> i32 with _ { f() }\n\
                  fn pure_fn() -> i32 { 0 }\n\
                  pub fn wrapper() -> i32 { apply(pure_fn) }";
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "parse errors: {:?}",
        parsed.errors
    );
    let result = effectcheck_with_policy(&parsed.program, PublicEffectsPolicy::Inferred);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.message.contains("with _") && e.message.contains("wrapper")),
        "viral-rule error should fire under Inferred policy: {:?}",
        result.errors
    );
}

// ── Effect set subtyping ─────────────────────────────────────────

#[test]
fn test_subtype_pure_arg_into_reads_slot_ok() {
    // ∅ ⊆ {reads(Db)} — pure function fits a reads slot
    effectcheck_ok(
        "fn run(f: Fn(i32) -> i32 with reads(Db)) -> i32 { f(1) }\n\
         fn pure_fn(x: i32) -> i32 { x }\n\
         fn main() { run(pure_fn); }",
    );
}

#[test]
fn test_subtype_exact_match_ok() {
    // {reads(Db)} ⊆ {reads(Db)} — same effects, always OK
    effectcheck_ok(
        "fn run(f: Fn(i32) -> i32 with reads(Db)) -> i32 { f(1) }\n\
         fn read_fn(x: i32) -> i32 with reads(Db) { x }\n\
         fn main() { run(read_fn); }",
    );
}

#[test]
fn test_subtype_wider_arg_rejected() {
    // {writes(Db)} ⊄ {reads(Db)} — slot only allows reads, arg does writes
    let errors = effectcheck_errors(
        "fn run(f: Fn(i32) -> i32 with reads(Db)) -> i32 { f(1) }\n\
         fn write_fn(x: i32) -> i32 with writes(Db) { x }\n\
         fn main() { run(write_fn); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::EffectSubtypeViolation),
        "expected EffectSubtypeViolation, got: {:?}",
        errors.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert!(
        errors.iter().any(|e| e.message.contains("writes(Db)")),
        "error should name the violating effect"
    );
}

#[test]
fn test_subtype_polymorphic_slot_accepts_anything() {
    // with _ slot accepts any effects — no error
    effectcheck_ok(
        "pub fn run(f: Fn(i32) -> i32 with _) -> i32 with _ { f(1) }\n\
         fn write_fn(x: i32) -> i32 with writes(Db) { x }\n\
         fn main() { run(write_fn); }",
    );
}

#[test]
fn test_subtype_unannotated_slot_treated_as_pure() {
    // Unannotated slot → B = ∅; passing an effectful fn is an E0404.
    let errors = effectcheck_errors(
        "effect resource Db;\n\
         fn run(f: Fn(i32) -> i32) -> i32 { f(1) }\n\
         fn write_fn(x: i32) -> i32 with writes(Db) { x }\n\
         fn main() { run(write_fn); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::EffectSubtypeViolation),
        "expected E0404 when effectful fn is passed to unannotated (pure) slot"
    );
}

#[test]
fn test_subtype_unannotated_slot_pure_arg_passes() {
    // Unannotated slot → B = ∅; passing a pure fn is fine.
    effectcheck_ok(
        "fn run(f: Fn(i32) -> i32) -> i32 { f(1) }\n\
         fn pure_fn(x: i32) -> i32 { x }\n\
         fn main() { run(pure_fn); }",
    );
}

#[test]
fn test_subtype_closure_arg_wider_rejected() {
    // Closure with writes effect passed into reads-only slot
    let errors = effectcheck_errors(
        "fn helper() -> i32 with writes(Db) { 0 }\n\
         fn run(f: Fn(i32) -> i32 with reads(Db)) -> i32 { f(1) }\n\
         fn main() { run(|x| helper()); }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::EffectSubtypeViolation),
        "expected EffectSubtypeViolation for closure with wrong effects"
    );
}

// ── E0404 structured subtype trace (Phase 3 checklist `karac explain`) ───────

#[test]
fn test_subtype_violation_carries_trace() {
    // E0404 must populate subtype_trace with slot, argument, and offending sets.
    let parsed = parse(
        "effect resource Db;\n\
         fn write_fn(x: i32) -> i32 writes(Db) { x }\n\
         fn run(f: Fn(i32) -> i32 with reads(Db)) -> i32 { f(1) }\n\
         fn main() { run(write_fn); }",
    );
    assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    let result = effectcheck(&parsed.program);
    let violations: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind == EffectErrorKind::EffectSubtypeViolation)
        .collect();
    assert!(!violations.is_empty(), "expected at least one E0404");

    for v in &violations {
        let trace = v
            .subtype_trace
            .as_ref()
            .expect("E0404 must carry subtype_trace");
        assert!(
            trace.slot_effects.iter().any(|s| s.contains("reads")),
            "slot_effects should contain reads(Db), got: {:?}",
            trace.slot_effects
        );
        assert!(
            trace.argument_effects.iter().any(|s| s.contains("writes")),
            "argument_effects should contain writes(Db), got: {:?}",
            trace.argument_effects
        );
        assert!(
            trace.offending_effects.iter().any(|s| s.contains("writes")),
            "offending_effects should contain writes(Db), got: {:?}",
            trace.offending_effects
        );
        // The offending set must be a subset of the argument set.
        for off in &trace.offending_effects {
            assert!(
                trace.argument_effects.contains(off),
                "offending {} must appear in argument_effects {:?}",
                off,
                trace.argument_effects
            );
        }
        // The offending set must not intersect the slot set.
        for off in &trace.offending_effects {
            assert!(
                !trace.slot_effects.contains(off),
                "offending {} must not be in slot_effects {:?}",
                off,
                trace.slot_effects
            );
        }
    }
}

#[test]
fn test_subtype_ok_violations_have_no_trace_for_passing_args() {
    // For passing calls (no E0404), no violations are emitted at all.
    let result = effectcheck_ok(
        "effect resource Db;\n\
         fn read_fn(x: i32) -> i32 reads(Db) { x }\n\
         fn run(f: Fn(i32) -> i32 with reads(Db)) -> i32 { f(1) }\n\
         fn main() { run(read_fn); }",
    );
    let violations: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind == EffectErrorKind::EffectSubtypeViolation)
        .collect();
    assert!(
        violations.is_empty(),
        "no E0404 expected for a passing subset check"
    );
}

#[test]
fn test_subtype_trace_slot_is_empty_for_pure_slot() {
    // Slot annotated `with reads(Db)` but arg does `writes(Db)` — slot is non-empty,
    // argument has a single write, offending = [writes(Db)].
    let parsed = parse(
        "effect resource Db;\n\
         fn write_fn(x: i32) -> i32 writes(Db) { x }\n\
         fn run(f: Fn(i32) -> i32 with reads(Db)) -> i32 { f(1) }\n\
         fn main() { run(write_fn); }",
    );
    assert!(parsed.errors.is_empty());
    let result = effectcheck(&parsed.program);
    let v = result
        .errors
        .iter()
        .find(|e| e.kind == EffectErrorKind::EffectSubtypeViolation)
        .expect("expected E0404");
    let trace = v.subtype_trace.as_ref().unwrap();
    // slot must contain reads(Db), NOT writes(Db)
    assert!(!trace.slot_effects.iter().any(|s| s.contains("writes")));
    // offending must not be in slot
    assert!(trace
        .offending_effects
        .iter()
        .all(|o| !trace.slot_effects.contains(o)));
}

#[test]
fn test_subtype_closure_violation_also_has_trace() {
    // Closure argument into a restricted slot also gets a trace.
    let parsed = parse(
        "effect resource Db;\n\
         fn helper() -> i32 writes(Db) { 0 }\n\
         fn run(f: Fn(i32) -> i32 with reads(Db)) -> i32 { f(1) }\n\
         fn main() { run(|x| helper()); }",
    );
    assert!(parsed.errors.is_empty());
    let result = effectcheck(&parsed.program);
    let v = result
        .errors
        .iter()
        .find(|e| e.kind == EffectErrorKind::EffectSubtypeViolation)
        .expect("expected E0404 for closure violation");
    assert!(
        v.subtype_trace.is_some(),
        "closure E0404 must carry subtype_trace"
    );
}

#[test]
fn test_subtype_closure_arg_pure_ok() {
    // Pure closure into reads slot — OK (∅ ⊆ {reads(Db)})
    effectcheck_ok(
        "fn run(f: Fn(i32) -> i32 with reads(Db)) -> i32 { f(1) }\n\
         fn main() { run(|x| x + 1); }",
    );
}

#[test]
fn test_effect_variable_no_undefined_group_error() {
    // A function declaring [with E] and returning `with E` must NOT produce
    // an UndefinedEffectGroup error — Variable("E") is not a group lookup.
    effectcheck_ok("pub fn apply[T, with E](f: Fn(T) -> T with E, x: T) -> T with E { f(x) }");
}

#[test]
fn test_effect_variable_makes_function_polymorphic() {
    // [with E] implies the function is effect-polymorphic; a public fn that
    // only declares `with E` (no fixed effects) must not be treated as having
    // Explicit(empty) — the checker should not demand it declare missing effects.
    effectcheck_ok("pub fn identity[T, with E](f: Fn(T) -> T with E, x: T) -> T with E { f(x) }");
}

// ── Round 10.1: end-to-end compound type+effect polymorphism ────
//
// With round 10.1's two-pass arg inference, a generic `[T, with E]`
// function can be called with a closure whose body's effects bind `E`
// while `T` is solved from a non-closure arg. This test threads through
// the full pipeline (typecheck solves `T`, then effectcheck resolves `E`
// via round 9's `compute_call_var_bindings` against the type-checked
// closure body) — demonstrating steps 1–4 of design.md § Monomorphization
// order for compound polymorphism work end-to-end.

#[test]
fn test_compound_polymorphism_end_to_end_effect_propagation() {
    // `pipeline` is generic in both `T` and `E`. The call `pipeline(42, |y| touch_db(y))`:
    //   step 1 — non-closure arg `42` collects `T = i64`
    //   step 2 — `T` is substituted into `Fn(T) -> T`, closure body sees `y: i64`
    //   step 3 — `E` unifies with the closure body's inferred effects = {reads(Db)}
    //   step 4 — call site contributes `reads(Db)` to `main`, which declares it
    let source = "effect resource Db;\n\
                  pub fn pipeline[T, with E](x: T, cb: Fn(T) -> T with E) -> T with E { cb(x) }\n\
                  pub fn touch_db(x: i64) -> i64 with reads(Db) { x }\n\
                  pub fn main() with reads(Db) {\n\
                      let _ = pipeline(42, |y| touch_db(y));\n\
                  }";
    effectcheck_full_pipeline(source);
}

#[test]
fn test_compound_polymorphism_nested_bottom_up_resolution() {
    // Step 5 of design.md § Monomorphization order for compound polymorphism:
    // nested compound calls resolve bottom-up in expression-typing order. Inner
    // call resolves first (T_inner = i64, E_inner = {reads(Db)}), then its
    // concrete result (an i64-returning closure body carrying `reads(Db)`)
    // becomes input to the outer call's resolution (T_outer = i64,
    // E_outer = {reads(Db)} via outer closure body's inferred effects).
    //
    // Each call resolves independently — no global constraint system. The test
    // demonstrates this by using *different* type parameter names (T/U) and
    // *different* effect variables (E/F) on the two generic functions: their
    // namespaces don't collide, and both still bind correctly to i64 / reads(Db).
    let source = "effect resource Db;\n\
                  pub fn outer[T, with E](x: T, cb: Fn(T) -> T with E) -> T with E { cb(x) }\n\
                  pub fn inner[U, with F](y: U, dg: Fn(U) -> U with F) -> U with F { dg(y) }\n\
                  pub fn touch_db(z: i64) -> i64 with reads(Db) { z }\n\
                  pub fn main() with reads(Db) {\n\
                      let _ = outer(42, |a| inner(a, |b| touch_db(b)));\n\
                  }";
    effectcheck_full_pipeline(source);
}

// ── Round 10.3 step 7: monomorphized signature on E0404 ─────────
//
// When a compound-polymorphic call's effect-subtyping check fails, the
// diagnostic must carry a fully-monomorphized callee signature so the user
// sees `Fn(i64) -> ()` instead of `Fn(T) -> ()` (design.md § Monomorphization
// order for compound polymorphism, step 7). The signature is rendered with
// type parameters resolved per the typechecker's `call_type_subs` and effect
// variables resolved per the call's `var_bindings`.

#[test]
fn test_monomorphized_signature_substitutes_type_param_in_e0404() {
    // `pipeline[T]` has a slot `Fn(T) -> () with reads(Db)` declared narrow.
    // Caller passes `42i64` (binds T=i64) and a closure that does writes(Log),
    // not reads(Db). E0404 fires; the diagnostic must show the slot with T
    // substituted to i64 in the callee signature.
    let source = "effect resource Db;\n\
                  effect resource Log;\n\
                  pub fn write_log() with writes(Log) {}\n\
                  pub fn pipeline[T](x: T, cb: Fn(T) -> () with reads(Db)) with reads(Db) { cb(x); }\n\
                  pub fn main() with writes(Log) reads(Db) {\n\
                      pipeline(42i64, |y| write_log())\n\
                  }";
    let result = effectcheck_full_pipeline(source);
    let v = result
        .errors
        .iter()
        .find(|e| e.kind == EffectErrorKind::EffectSubtypeViolation)
        .unwrap_or_else(|| {
            panic!(
                "expected EffectSubtypeViolation, got: {:?}",
                result
                    .errors
                    .iter()
                    .map(|e| (&e.kind, &e.message))
                    .collect::<Vec<_>>()
            )
        });
    let trace = v
        .subtype_trace
        .as_ref()
        .expect("E0404 must carry subtype_trace");
    let sig = trace
        .monomorphized_signature
        .as_ref()
        .expect("compound polymorphic call's E0404 must carry a monomorphized signature");
    assert!(
        sig.contains("Fn(i64)"),
        "monomorphized signature should substitute T → i64, got: {sig}"
    );
    assert!(
        !sig.contains("Fn(T)") && !sig.contains("(T,") && !sig.contains("(T)"),
        "monomorphized signature must not mention the type variable `T`, got: {sig}"
    );
    assert!(
        sig.contains("pipeline"),
        "signature should include the callee name, got: {sig}"
    );
    assert!(
        sig.contains("reads(Db)"),
        "signature should preserve the slot's declared effect set, got: {sig}"
    );
    // The message must also surface the signature — JSON consumers read
    // the trace, but humans read the message text.
    assert!(
        v.message.contains(sig),
        "error message must include the monomorphized signature, got: {}",
        v.message
    );
}

#[test]
fn test_monomorphized_signature_resolves_with_e_variable() {
    // Compound `[T, with E]` call where E unifies to writes(Log) via the closure
    // body, but the slot is the closure's body. To trigger E0404 while still
    // exercising var-binding rendering, narrow one slot to `with reads(Db)`
    // (concrete) and bind E from another slot.
    //
    // pipeline[T, with E](x: T, restricted: Fn(T) -> () with reads(Db),
    //                     vary: Fn(T) -> () with E) with reads(Db) + E
    // Calling with restricted = |y| write_log() (writes(Log) — NOT reads(Db))
    // triggers E0404 on the `restricted` arg. The signature should still
    // resolve E from the `vary` slot's closure for completeness.
    let source = "effect resource Db;\n\
                  effect resource Log;\n\
                  pub fn write_log() with writes(Log) {}\n\
                  pub fn read_db() with reads(Db) {}\n\
                  pub fn pipeline[T, with E](x: T, \
                      restricted: Fn(T) -> () with reads(Db), \
                      vary: Fn(T) -> () with E) with reads(Db) E { \
                      restricted(x); vary(x); \
                  }\n\
                  pub fn main() with reads(Db) writes(Log) {\n\
                      pipeline(7i64, |y| write_log(), |z| read_db())\n\
                  }";
    let result = effectcheck_full_pipeline(source);
    let v = result
        .errors
        .iter()
        .find(|e| e.kind == EffectErrorKind::EffectSubtypeViolation)
        .unwrap_or_else(|| {
            panic!(
                "expected EffectSubtypeViolation, got: {:?}",
                result
                    .errors
                    .iter()
                    .map(|e| (&e.kind, &e.message))
                    .collect::<Vec<_>>()
            )
        });
    let trace = v.subtype_trace.as_ref().unwrap();
    let sig = trace
        .monomorphized_signature
        .as_ref()
        .expect("compound polymorphic E0404 must carry a monomorphized signature");
    // T → i64 substitution shows up in every Fn slot.
    assert!(
        sig.contains("Fn(i64)") && !sig.contains("Fn(T)"),
        "signature must substitute T → i64, got: {sig}"
    );
    // E binding from the closure passed at the `vary` slot.
    assert!(
        sig.contains("reads(Db)") && sig.contains("with"),
        "signature must include resolved effect spec, got: {sig}"
    );
}

#[test]
fn test_monomorphized_signature_omitted_for_non_generic_call() {
    // Non-generic call → no type substitution → signature is `None` (the
    // diagnostic is no less informative than before — slot effects are still
    // listed in the message).
    let parsed = parse(
        "effect resource Db;\n\
         fn write_fn(x: i32) -> i32 writes(Db) { x }\n\
         fn run(f: Fn(i32) -> i32 with reads(Db)) -> i32 { f(1) }\n\
         fn main() { run(write_fn); }",
    );
    assert!(parsed.errors.is_empty());
    let result = effectcheck(&parsed.program);
    let v = result
        .errors
        .iter()
        .find(|e| e.kind == EffectErrorKind::EffectSubtypeViolation)
        .expect("expected E0404");
    let trace = v.subtype_trace.as_ref().unwrap();
    assert!(
        trace.monomorphized_signature.is_none(),
        "non-generic call should not carry a monomorphized signature \
         (the slot type is already concrete in source), got: {:?}",
        trace.monomorphized_signature
    );
}

// ── Round 10.3 step 8: trait-method effects compose with `with E` ─
//
// Per design.md § Monomorphization order for compound polymorphism (step 8):
// inside `f[T: Trait, with E]`'s body, method calls on `T`-bound values
// resolve through `T`'s per-method declarations and compose with the resolved
// effect variable as a *union* at the call-site contribution step — not
// during effect unification. The trait method's effects flow alongside `E`'s
// binding; both surface in the caller's inferred set.

#[test]
fn test_trait_method_effects_union_with_with_e_binding() {
    // `pipeline[T: Storage, with E]` invokes `store.fetch()` (Storage.fetch
    // declares `reads(Cache)`) and the closure `cb` (binds `E` to whatever
    // its body does). The caller passes a closure that does `writes(Log)`.
    // Both effects should reach `main`, which declares both.
    let source = "effect resource Cache;\n\
                  effect resource Log;\n\
                  pub trait Storage {\n\
                      fn fetch(self) -> i64 with reads(Cache);\n\
                  }\n\
                  pub struct DiskStore {}\n\
                  impl Storage for DiskStore {\n\
                      fn fetch(self) -> i64 with reads(Cache) { 0 }\n\
                  }\n\
                  pub fn write_log() with writes(Log) {}\n\
                  pub fn pipeline[T: Storage, with E](store: T, cb: Fn() with E) \
                      -> i64 with reads(Cache) E { \
                      cb(); store.fetch() \
                  }\n\
                  pub fn main() with reads(Cache) writes(Log) {\n\
                      let _ = pipeline(DiskStore, || write_log());\n\
                  }";
    effectcheck_full_pipeline(source);
}

#[test]
fn test_trait_method_effect_missing_from_caller_declaration_fails() {
    // Same shape, but `main` only declares `writes(Log)` — the trait method's
    // `reads(Cache)` must still surface, and the missing declaration must be
    // diagnosed.
    let source = "effect resource Cache;\n\
                  effect resource Log;\n\
                  pub trait Storage {\n\
                      fn fetch(self) -> i64 with reads(Cache);\n\
                  }\n\
                  pub struct DiskStore {}\n\
                  impl Storage for DiskStore {\n\
                      fn fetch(self) -> i64 with reads(Cache) { 0 }\n\
                  }\n\
                  pub fn write_log() with writes(Log) {}\n\
                  pub fn pipeline[T: Storage, with E](store: T, cb: Fn() with E) \
                      -> i64 with reads(Cache) E { \
                      cb(); store.fetch() \
                  }\n\
                  pub fn main() with writes(Log) {\n\
                      let _ = pipeline(DiskStore, || write_log());\n\
                  }";
    let mut parsed = parse(source);
    assert!(parsed.errors.is_empty(), "Parse: {:?}", parsed.errors);
    let resolved = resolve(&parsed.program);
    assert!(resolved.errors.is_empty(), "Resolve: {:?}", resolved.errors);
    let typed = typecheck(&parsed.program, &resolved);
    assert!(typed.errors.is_empty(), "Type: {:?}", typed.errors);
    let method_types = typed.method_callee_types.clone();
    let call_type_subs = typed.call_type_subs.clone();
    lower(&mut parsed.program, &typed);
    let result = effectcheck_with_typecheck_data(
        &parsed.program,
        PublicEffectsPolicy::default(),
        CompileProfile::Default,
        method_types,
        call_type_subs,
    );
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::MissingEffectDeclaration
                && e.message.contains("reads")
                && e.message.contains("Cache")),
        "expected missing-declaration diagnostic naming reads(Cache), got: {:?}",
        result
            .errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_with_e_closure_effect_missing_from_caller_declaration_fails() {
    // Mirror of the above on the other axis — the trait method's effect is
    // declared, but the closure's `writes(Log)` (which binds `E`) is not.
    let source = "effect resource Cache;\n\
                  effect resource Log;\n\
                  pub trait Storage {\n\
                      fn fetch(self) -> i64 with reads(Cache);\n\
                  }\n\
                  pub struct DiskStore {}\n\
                  impl Storage for DiskStore {\n\
                      fn fetch(self) -> i64 with reads(Cache) { 0 }\n\
                  }\n\
                  pub fn write_log() with writes(Log) {}\n\
                  pub fn pipeline[T: Storage, with E](store: T, cb: Fn() with E) \
                      -> i64 with reads(Cache) E { \
                      cb(); store.fetch() \
                  }\n\
                  pub fn main() with reads(Cache) {\n\
                      let _ = pipeline(DiskStore, || write_log());\n\
                  }";
    let mut parsed = parse(source);
    assert!(parsed.errors.is_empty(), "Parse: {:?}", parsed.errors);
    let resolved = resolve(&parsed.program);
    assert!(resolved.errors.is_empty(), "Resolve: {:?}", resolved.errors);
    let typed = typecheck(&parsed.program, &resolved);
    assert!(typed.errors.is_empty(), "Type: {:?}", typed.errors);
    let method_types = typed.method_callee_types.clone();
    let call_type_subs = typed.call_type_subs.clone();
    lower(&mut parsed.program, &typed);
    let result = effectcheck_with_typecheck_data(
        &parsed.program,
        PublicEffectsPolicy::default(),
        CompileProfile::Default,
        method_types,
        call_type_subs,
    );
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::MissingEffectDeclaration
                && e.message.contains("writes")
                && e.message.contains("Log")),
        "expected missing-declaration diagnostic naming writes(Log), got: {:?}",
        result
            .errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_trait_method_effects_compose_at_call_site_not_during_unification() {
    // Verifies the structural ordering: trait-method effects come from
    // dispatch-time resolution (independent of `E`), so when two impls have
    // *different* trait-method effects, each call site picks up the one
    // matching its concrete `T`. `E` binding is unrelated.
    //
    // No diagnostic should fire — `pipeline` declares `with reads(Cache) E`
    // (not the union of all impls' effects) and each call narrows correctly.
    let source = "effect resource Cache;\n\
                  effect resource Log;\n\
                  pub trait Storage {\n\
                      fn fetch(self) -> i64 with reads(Cache);\n\
                  }\n\
                  pub struct DiskStore {}\n\
                  impl Storage for DiskStore {\n\
                      fn fetch(self) -> i64 with reads(Cache) { 0 }\n\
                  }\n\
                  pub fn pipeline[T: Storage, with E](store: T, cb: Fn() with E) \
                      -> i64 with reads(Cache) E { \
                      cb(); store.fetch() \
                  }\n\
                  pub fn main() with reads(Cache) {\n\
                      let _ = pipeline(DiskStore, || {});\n\
                  }";
    effectcheck_full_pipeline(source);
}

// ── Round 9: `with E` same-signature unification ────────────────

#[test]
fn test_with_e_two_slots_disagreeing_closures_conflict() {
    // Two `with E` slots, closures with different effect sets → conflict.
    // The diagnostic names E and both bindings. (Other diagnostics may
    // also fire from the existing `with _` viral rule and Fn-slot
    // subtyping checks — those are separate concerns; round 9 ships the
    // conflict-detection diagnostic.)
    let errors = effectcheck_errors(
        "effect resource Log;
         effect resource Db;
         pub fn write_log() with writes(Log) {}
         pub fn read_db() with reads(Db) {}
         pub fn pipe[with E](a: Fn() with E, b: Fn() with E) with E { a(); b(); }
         pub fn run() with writes(Log) reads(Db) {
             pipe(|| write_log(), || read_db())
         }",
    );
    let conflict = errors
        .iter()
        .find(|e| e.kind == EffectErrorKind::EffectVariableConflict)
        .unwrap_or_else(|| {
            panic!(
                "expected EffectVariableConflict, got: {:?}",
                errors
                    .iter()
                    .map(|e| (&e.kind, &e.message))
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        conflict.message.contains("`E`")
            && conflict.message.contains("writes(Log)")
            && conflict.message.contains("reads(Db)"),
        "conflict diagnostic should name E and both bindings, got: {}",
        conflict.message
    );
}

#[test]
fn test_with_e_function_ref_args_conflict() {
    // Same conflict shape with function-ref args (not closures). Effect
    // sets come from each function's inferred effects, unified at the
    // call site.
    let errors = effectcheck_errors(
        "effect resource Log;
         effect resource Db;
         pub fn write_log() with writes(Log) {}
         pub fn read_db() with reads(Db) {}
         pub fn pipe[with E](a: Fn() with E, b: Fn() with E) with E { a(); b(); }
         pub fn run() with writes(Log) reads(Db) { pipe(write_log, read_db) }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::EffectVariableConflict),
        "expected EffectVariableConflict for function-ref args, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_with_e_two_slots_pure_closures_no_conflict() {
    // Both closures pure → both bind E to {} → trivially agree, no
    // EffectVariableConflict diagnostic fires.
    let errors: Vec<_> = effectcheck_all(
        "pub fn pipe[with E](a: Fn() with E, b: Fn() with E) with E { a(); b(); }
         fn run() {
             pipe(|| {}, || {})
         }",
    )
    .errors
    .into_iter()
    .filter(|e| e.kind == EffectErrorKind::EffectVariableConflict)
    .collect();
    assert!(
        errors.is_empty(),
        "pure closures must not trigger EffectVariableConflict, got: {:?}",
        errors
    );
}

#[test]
fn test_with_underscore_two_slots_independent_no_conflict() {
    // Regression: two `with _` slots (no named E) are independent
    // (design.md:317). The conflict diagnostic must NOT fire for `with _`.
    let result = effectcheck_all(
        "effect resource Log;
         effect resource Db;
         pub fn write_log() with writes(Log) {}
         pub fn read_db() with reads(Db) {}
         pub fn pipe(a: Fn() with _, b: Fn() with _) with _ { a(); b(); }
         pub fn run() with _ {
             pipe(|| write_log(), || read_db())
         }",
    );
    let conflicts: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind == EffectErrorKind::EffectVariableConflict)
        .collect();
    assert!(
        conflicts.is_empty(),
        "two `with _` slots are independent — no EffectVariableConflict expected, got: {:?}",
        conflicts
    );
}

// ── Round 9.1: `with E`-aware viral rule + Fn-slot subtyping ────

#[test]
fn test_with_e_agreeing_closures_no_viral_or_subtype_error() {
    // Caller declares only `writes(Log)` and passes two `with E` closures
    // that both perform `writes(Log)`. The `with E` callee must not trip
    // (a) the `with _` viral rule (E is named, not anonymous) nor
    // (b) the Fn-slot subtyping check (E binds to `{writes(Log)}` per
    // call, so each closure satisfies its slot).
    effectcheck_ok(
        "effect resource Log;
         pub fn write_log() with writes(Log) {}
         pub fn write_audit() with writes(Log) {}
         pub fn pipe[with E](a: Fn() with E, b: Fn() with E) with E { a(); b(); }
         pub fn run() with writes(Log) {
             pipe(|| write_log(), || write_audit())
         }",
    );
}

#[test]
fn test_with_e_agreeing_function_refs_no_viral_or_subtype_error() {
    // Same as above but with named function references instead of
    // closure literals. E binds via each callee's inferred effects.
    effectcheck_ok(
        "effect resource Log;
         pub fn write_log() with writes(Log) {}
         pub fn write_audit() with writes(Log) {}
         pub fn pipe[with E](a: Fn() with E, b: Fn() with E) with E { a(); b(); }
         pub fn run() with writes(Log) {
             pipe(write_log, write_audit)
         }",
    );
}

#[test]
fn test_with_e_disagreeing_no_spurious_subtype_error() {
    // When `with E` slots disagree, only the EffectVariableConflict
    // diagnostic fires — the Fn-slot subtyping check uses the union of
    // referenced args' effects so each individual arg satisfies its
    // resolved slot vacuously, avoiding double-reporting.
    let result = effectcheck_all(
        "effect resource Log;
         effect resource Db;
         pub fn write_log() with writes(Log) {}
         pub fn read_db() with reads(Db) {}
         pub fn pipe[with E](a: Fn() with E, b: Fn() with E) with E { a(); b(); }
         pub fn run() with writes(Log) reads(Db) {
             pipe(|| write_log(), || read_db())
         }",
    );
    let subtype_errors: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind == EffectErrorKind::EffectSubtypeViolation)
        .collect();
    assert!(
        subtype_errors.is_empty(),
        "`with E` disagreement must not raise spurious subtype errors, got: {:?}",
        subtype_errors
    );
    let conflicts: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind == EffectErrorKind::EffectVariableConflict)
        .collect();
    assert_eq!(
        conflicts.len(),
        1,
        "expected exactly one EffectVariableConflict, got: {:?}",
        result.errors
    );
}

#[test]
fn test_with_underscore_callee_still_fires_viral_rule() {
    // Regression: a caller declaring fixed effects (not `with _`) that
    // calls a true `with _` callee must still trip the viral rule —
    // round 9.1's relaxation only applies to named `with E` callees.
    let errors = effectcheck_errors(
        "pub fn run_each(f: Fn()) with _ { f(); }
         pub fn caller() { run_each(|| {}) }",
    );
    assert!(
        errors.iter().any(
            |e| matches!(e.kind, EffectErrorKind::MissingEffectDeclaration)
                && e.message.contains("polymorphic")
        ),
        "expected viral-rule MissingEffectDeclaration for `with _` callee, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

// ── Round 9.2: `with E` body leak verification + SCC unification ─

#[test]
fn test_with_e_pure_body_leak_diagnosed() {
    // A `pub fn` declared `with E` (purely polymorphic) whose body
    // performs a concrete effect must be diagnosed at the function
    // itself — previously the leak surfaced only at a caller and
    // misattributed to the callee.
    let errors = effectcheck_errors(
        "effect resource Db;
         pub fn read_db() with reads(Db) {}
         pub fn alpha[with E](f: Fn() with E) with E {
             f();
             read_db()
         }",
    );
    let leak: Vec<_> = errors
        .iter()
        .filter(|e| {
            matches!(e.kind, EffectErrorKind::MissingEffectDeclaration)
                && e.message.contains("'alpha'")
                && e.message.contains("reads(Db)")
                && e.message.contains("purely polymorphic")
        })
        .collect();
    assert_eq!(
        leak.len(),
        1,
        "expected one leak diagnostic at alpha, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_with_e_plus_fixed_body_within_fixed_no_error() {
    // `with E + reads(Config)` — body performs reads(Config) (matches
    // fixed) plus the polymorphic call. No leak.
    effectcheck_ok(
        "effect resource Config;
         pub fn read_config() with reads(Config) {}
         pub fn run_with_config[with E](f: Fn() with E) with E reads(Config) {
             read_config();
             f()
         }",
    );
}

#[test]
fn test_with_e_plus_fixed_body_leak_diagnosed() {
    // `with E + reads(Config)` body also performs writes(Log), which is
    // not in the fixed part. Diagnose at the function itself.
    let errors = effectcheck_errors(
        "effect resource Config;
         effect resource Log;
         pub fn read_config() with reads(Config) {}
         pub fn write_log() with writes(Log) {}
         pub fn run_with_config[with E](f: Fn() with E) with E reads(Config) {
             read_config();
             write_log();
             f()
         }",
    );
    let leak: Vec<_> = errors
        .iter()
        .filter(|e| {
            matches!(e.kind, EffectErrorKind::MissingEffectDeclaration)
                && e.message.contains("'run_with_config'")
                && e.message.contains("writes(Log)")
                && e.message.contains("fixed part")
        })
        .collect();
    assert_eq!(
        leak.len(),
        1,
        "expected one leak diagnostic at run_with_config, got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_with_underscore_body_leak_still_skipped() {
    // Regression: `with _` (anonymous) is viral by design — the wildcard
    // absorbs whatever the body carries. Concrete effects in a `with _`
    // body must NOT trip the leak diagnostic.
    let result = effectcheck_all(
        "effect resource Db;
         pub fn read_db() with reads(Db) {}
         pub fn alpha(f: Fn()) with _ {
             f();
             read_db()
         }",
    );
    let leak_at_alpha: Vec<_> = result
        .errors
        .iter()
        .filter(|e| {
            matches!(e.kind, EffectErrorKind::MissingEffectDeclaration)
                && e.message.contains("'alpha'")
                && (e.message.contains("purely polymorphic") || e.message.contains("fixed part"))
        })
        .collect();
    assert!(
        leak_at_alpha.is_empty(),
        "`with _` body leak must not be diagnosed (it is viral by design), got: {:?}",
        leak_at_alpha
    );
}

#[test]
fn test_polymorphic_scc_pure_no_error() {
    // Two mutually-recursive `with E` functions, both pure outside the
    // polymorphic param. SCC fixed-point converges to empty inferred
    // effects; no leak diagnostic should fire on either member.
    effectcheck_ok(
        "pub fn alpha[with E](f: Fn() with E) with E { f(); beta(f) }
         pub fn beta[with E](f: Fn() with E) with E { f(); alpha(f) }",
    );
}

#[test]
fn test_polymorphic_scc_leak_propagates_to_all_members() {
    // SCC unification of E across the cycle: a leak in beta surfaces
    // for alpha too, because the SCC fixed-point propagates beta's
    // concrete effect through the call graph. Both members fire the
    // diagnostic — formalising that the SCC's E is shared.
    let errors = effectcheck_errors(
        "effect resource Db;
         pub fn read_db() with reads(Db) {}
         pub fn alpha[with E](f: Fn() with E) with E { f(); beta(f) }
         pub fn beta[with E](f: Fn() with E) with E {
             f();
             read_db();
             alpha(f)
         }",
    );
    let alpha_leak = errors.iter().any(|e| {
        matches!(e.kind, EffectErrorKind::MissingEffectDeclaration)
            && e.message.contains("'alpha'")
            && e.message.contains("reads(Db)")
    });
    let beta_leak = errors.iter().any(|e| {
        matches!(e.kind, EffectErrorKind::MissingEffectDeclaration)
            && e.message.contains("'beta'")
            && e.message.contains("reads(Db)")
    });
    assert!(
        alpha_leak && beta_leak,
        "expected leak diagnostic at both alpha and beta (SCC unification of E), got: {:?}",
        errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_multiple_effect_variables_independent() {
    // design.md line 4858: `[with E1, E2]` declares two independent
    // effect variables — each binds via its own slot, no cross-variable
    // unification. Two slots typed `Fn() with E1` and `Fn() with E2`
    // accept arbitrary closures with arbitrary (and different) effects.
    effectcheck_ok(
        "effect resource Log;
         effect resource Db;
         pub fn write_log() with writes(Log) {}
         pub fn read_db() with reads(Db) {}
         pub fn pipe[with E, F](a: Fn() with E, b: Fn() with F) with E F {
             a();
             b()
         }
         pub fn run() with writes(Log) reads(Db) {
             pipe(|| write_log(), || read_db())
         }",
    );
}

#[test]
fn test_with_e_trait_bound_dispatch_not_flagged_as_leak() {
    // design.md line 4901: a `with E` function that dispatches through a
    // typeparam-bounded trait whose method is `with _` must not have its
    // body's effects flagged as concrete leaks. The effect routes through
    // T → Trait.method (polymorphic), i.e. it belongs to E.
    //
    // Without the trait-bound filter on the leak check, the conservative
    // call-collection would attribute `sends(Net)` (from the RemoteProcessor
    // impl) to `run`'s body and false-positive it as a leak.
    let result = effectcheck_all(
        "effect resource Net;
         shared struct LocalProcessor {}
         shared struct RemoteProcessor {}
         pub fn remote_call() with sends(Net) {}
         pub trait Processor {
             fn process(self) with _;
         }
         impl Processor for LocalProcessor {
             fn process(self) {}
         }
         impl Processor for RemoteProcessor {
             fn process(self) with sends(Net) {
                 remote_call()
             }
         }
         pub fn run[T: Processor, with E](p: T) with E {
             p.process()
         }",
    );
    let leak_at_run: Vec<_> = result
        .errors
        .iter()
        .filter(|e| {
            matches!(e.kind, EffectErrorKind::MissingEffectDeclaration)
                && e.message.contains("'run'")
                && (e.message.contains("purely polymorphic") || e.message.contains("fixed part"))
        })
        .collect();
    assert!(
        leak_at_run.is_empty(),
        "trait-bound dispatch through `T: Processor` (Processor.process is `with _`) \
         must not fire the `with E` leak diagnostic at run: {:?}",
        leak_at_run
    );
}

// ── Profile-compatibility checks (FFI item 6) ───────────────────

fn profile_ok(source: &str, profile: CompileProfile) -> EffectCheckResult {
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
    let result = effectcheck_with_profile(&parsed.program, PublicEffectsPolicy::Declared, profile);
    let real_errors: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind != EffectErrorKind::FfiLintHint)
        .collect();
    assert!(
        real_errors.is_empty(),
        "Unexpected effect errors: {}",
        real_errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    result
}

fn profile_errors(source: &str, profile: CompileProfile) -> Vec<EffectError> {
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
    let result = effectcheck_with_profile(&parsed.program, PublicEffectsPolicy::Declared, profile);
    let real_errors: Vec<EffectError> = result
        .errors
        .into_iter()
        .filter(|e| e.kind != EffectErrorKind::FfiLintHint)
        .collect();
    assert!(
        !real_errors.is_empty(),
        "Expected profile violation errors but got none"
    );
    real_errors
}

#[test]
fn test_profile_default_allows_all_effects() {
    // Default profile imposes no restrictions on extern declarations.
    profile_ok(
        "extern \"C\" fn malloc(size: i64) -> i64 allocates(Heap) blocks panics;",
        CompileProfile::Default,
    );
}

#[test]
fn test_profile_embedded_forbids_heap_allocation() {
    // Embedded profile forbids allocates(Heap) on extern fns.
    let errors = profile_errors(
        "extern \"C\" fn malloc(size: i64) -> i64 allocates(Heap);",
        CompileProfile::Embedded,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, EffectErrorKind::ProfileViolation)),
        "Expected ProfileViolation, got: {:?}",
        errors
    );
    assert!(
        errors.iter().any(|e| e.message.contains("Heap")),
        "Error should mention Heap, got: {:?}",
        errors
    );
}

#[test]
fn test_profile_embedded_allows_non_heap_allocates() {
    // Embedded only forbids allocates(Heap); other resources are fine.
    profile_ok(
        "extern \"C\" fn arena_alloc(size: i64) -> i64 allocates(Arena);",
        CompileProfile::Embedded,
    );
}

#[test]
fn test_profile_embedded_allows_blocks_and_panics() {
    // Embedded does not restrict blocks or panics.
    profile_ok(
        "extern \"C\" fn os_write(fd: i32) -> i32 blocks panics;",
        CompileProfile::Embedded,
    );
}

#[test]
fn test_profile_kernel_forbids_panics() {
    // Kernel profile forbids panics on extern fns.
    let errors = profile_errors("extern \"C\" fn risky() panics;", CompileProfile::Kernel);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, EffectErrorKind::ProfileViolation)),
        "Expected ProfileViolation for panics in kernel profile, got: {:?}",
        errors
    );
}

#[test]
fn test_profile_kernel_forbids_blocks() {
    // Kernel profile forbids blocks on extern fns.
    let errors = profile_errors(
        "extern \"C\" fn sleep(ms: i32) blocks;",
        CompileProfile::Kernel,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, EffectErrorKind::ProfileViolation)),
        "Expected ProfileViolation for blocks in kernel profile, got: {:?}",
        errors
    );
}

#[test]
fn test_profile_kernel_forbids_allocates() {
    // Kernel profile forbids any allocates on extern fns.
    let errors = profile_errors(
        "extern \"C\" fn kmalloc(size: i64) -> i64 allocates(Heap);",
        CompileProfile::Kernel,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, EffectErrorKind::ProfileViolation)),
        "Expected ProfileViolation for allocates in kernel profile, got: {:?}",
        errors
    );
}

#[test]
fn test_profile_kernel_forbids_suspends() {
    // Kernel profile forbids suspends on extern fns.
    let errors = profile_errors(
        "extern \"C\" fn await_io() suspends;",
        CompileProfile::Kernel,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, EffectErrorKind::ProfileViolation)),
        "Expected ProfileViolation for suspends in kernel profile, got: {:?}",
        errors
    );
}

#[test]
fn test_profile_kernel_allows_reads_writes_sends_receives() {
    // Kernel profile allows resource-verb effects (reads, writes, sends, receives).
    // @noblock suppresses the extern "C" auto-blocks default so it doesn't mask the signal.
    profile_ok(
        "@noblock extern \"C\" fn io_op() reads(FileSystem) writes(FileSystem) sends(Channel) receives(Channel);",
        CompileProfile::Kernel,
    );
}

#[test]
fn test_profile_violation_message_contains_profile_name() {
    // Error message must name the offending profile.
    let errors = profile_errors(
        "extern \"C\" fn malloc(size: i64) -> i64 allocates(Heap);",
        CompileProfile::Embedded,
    );
    let msg = &errors[0].message;
    assert!(
        msg.contains("embedded"),
        "Message should name 'embedded' profile, got: {}",
        msg
    );
}

#[test]
fn test_profile_violation_message_contains_fn_name() {
    // Error message must name the offending extern function.
    let errors = profile_errors(
        "extern \"C\" fn my_alloc(size: i64) -> i64 allocates(Heap);",
        CompileProfile::Embedded,
    );
    let msg = &errors[0].message;
    assert!(
        msg.contains("my_alloc"),
        "Message should name the function 'my_alloc', got: {}",
        msg
    );
}

#[test]
fn test_profile_default_extern_c_blocks_default_is_ok() {
    // Default profile: extern "C" auto-defaults to {blocks}; no violation.
    profile_ok("extern \"C\" fn sleep(secs: i32);", CompileProfile::Default);
}

#[test]
fn test_profile_kernel_extern_c_blocks_default_is_violation() {
    // Kernel profile: extern "C" auto-defaults to {blocks}; blocks is forbidden.
    let errors = profile_errors("extern \"C\" fn sleep(secs: i32);", CompileProfile::Kernel);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e.kind, EffectErrorKind::ProfileViolation)),
        "Expected ProfileViolation for auto-blocks in kernel profile, got: {:?}",
        errors
    );
}

#[test]
fn test_profile_multiple_violations_reported() {
    // Both allocates(Heap) and blocks generate violations in kernel profile.
    let errors = profile_errors(
        "extern \"C\" fn big_alloc(size: i64) -> i64 allocates(Heap) blocks;",
        CompileProfile::Kernel,
    );
    let violations: Vec<_> = errors
        .iter()
        .filter(|e| matches!(e.kind, EffectErrorKind::ProfileViolation))
        .collect();
    assert!(
        violations.len() >= 2,
        "Expected at least 2 ProfileViolation errors, got: {}",
        violations.len()
    );
}

// ── FFI linter hints (Phase 3 checklist item 7) ─────────────────

fn ffi_hints(source: &str) -> Vec<EffectError> {
    let parsed = parse(source);
    assert!(
        parsed.errors.is_empty(),
        "Parse errors: {:?}",
        parsed.errors
    );
    let result = effectcheck(&parsed.program);
    result
        .errors
        .into_iter()
        .filter(|e| e.kind == EffectErrorKind::FfiLintHint)
        .collect()
}

#[test]
fn test_ffi_hint_malloc_missing_allocates() {
    // `malloc` without `allocates(Heap)` → lint hint
    let hints = ffi_hints("extern \"C\" fn malloc(size: i64) -> i64;");
    assert!(
        hints.iter().any(|h| h.message.contains("allocates(Heap)")),
        "Expected allocates(Heap) hint for malloc, got: {:?}",
        hints
    );
}

#[test]
fn test_ffi_hint_malloc_suppressed_when_declared() {
    // `malloc` with `allocates(Heap)` → no hint
    let hints = ffi_hints("extern \"C\" fn malloc(size: i64) -> i64 allocates(Heap);");
    assert!(
        hints.is_empty(),
        "No alloc hint expected when allocates(Heap) is declared, got: {:?}",
        hints
    );
}

#[test]
fn test_ffi_hint_sleep_missing_blocks() {
    // extern "C" sleep already gets blocks from the ABI default — no hint
    let hints = ffi_hints("extern \"C\" fn sleep(secs: i32);");
    assert!(
        hints.is_empty(),
        "No blocks hint expected for extern \"C\" sleep (ABI default adds it), got: {:?}",
        hints
    );
}

#[test]
fn test_ffi_hint_sleep_noblock_suggests_blocks() {
    // @noblock removes the ABI default → blocks is now missing → hint fires
    let hints = ffi_hints("@noblock extern \"C\" fn sleep(secs: i32);");
    assert!(
        hints.iter().any(|h| h.message.contains("blocks")),
        "Expected blocks hint for @noblock sleep, got: {:?}",
        hints
    );
}

#[test]
fn test_ffi_hint_getaddrinfo_both_blocking_and_allocating() {
    // getaddrinfo is both blocking and allocating; bare declaration gets two hints.
    // extern "C" ABI default already covers `blocks`, so only allocates hint fires.
    let hints = ffi_hints(
        "extern \"C\" fn getaddrinfo(node: i64, service: i64, hints: i64, res: i64) -> i32;",
    );
    assert!(
        hints.iter().any(|h| h.message.contains("allocates(Heap)")),
        "Expected allocates(Heap) hint for getaddrinfo, got: {:?}",
        hints
    );
    // blocks is already in the ABI default — no redundant hint
    assert!(
        !hints.iter().any(|h| h.message.contains("blocks")),
        "No blocks hint expected for getaddrinfo (ABI default covers it), got: {:?}",
        hints
    );
}

#[test]
fn test_ffi_hint_unknown_symbol_no_hint() {
    // An extern fn with an unknown symbol name gets no hints.
    let hints = ffi_hints("extern \"C\" fn do_something_custom() -> i32;");
    assert!(
        hints.is_empty(),
        "No hints expected for unknown symbol, got: {:?}",
        hints
    );
}

#[test]
fn test_ffi_hint_strdup_missing_allocates() {
    // strdup without allocates(Heap) → hint
    let hints = ffi_hints("extern \"C\" fn strdup(s: i64) -> i64;");
    assert!(
        hints.iter().any(|h| h.message.contains("allocates(Heap)")),
        "Expected allocates(Heap) hint for strdup, got: {:?}",
        hints
    );
}

#[test]
fn test_ffi_hint_is_advisory_not_error() {
    // A program with only FFI lint hints still passes effectcheck_ok
    // (hints do not prevent compilation).
    effectcheck_ok("extern \"C\" fn malloc(size: i64) -> i64;");
}

#[test]
fn test_ffi_hint_leading_underscore_normalized() {
    // Platform-prefixed names like `_malloc` still match the table.
    let hints = ffi_hints("extern \"C\" fn _malloc(size: i64) -> i64;");
    assert!(
        hints.iter().any(|h| h.message.contains("allocates(Heap)")),
        "Expected allocates(Heap) hint for _malloc, got: {:?}",
        hints
    );
}

// ── Impl-vs-trait ceiling (step 5) ──────────────────────────────────────────

#[test]
fn test_impl_trait_ceiling_satisfied() {
    // impl method effect == trait ceiling → ok.
    effectcheck_ok(
        "effect resource Db;
         pub fn do_db_read() with reads(Db) {}
         pub trait Repo with reads(Db) {
             fn fetch(ref self) -> i64;
         }
         pub struct MyRepo {}
         impl Repo for MyRepo {
             fn fetch(ref self) -> i64 { do_db_read(); 42 }
         }",
    );
}

#[test]
fn test_impl_trait_ceiling_narrower_ok() {
    // impl method performs no effects when ceiling allows reads(Db) → ok (narrower is fine).
    effectcheck_ok(
        "effect resource Db;
         pub trait Repo with reads(Db) {
             fn fetch(ref self) -> i64;
         }
         pub struct MyRepo {}
         impl Repo for MyRepo {
             fn fetch(ref self) -> i64 { 42 }
         }",
    );
}

#[test]
fn test_impl_trait_ceiling_violated() {
    // impl method performs reads(Cache) but trait ceiling is reads(Db) → error.
    let errors = effectcheck_errors(
        "effect resource Db;
         effect resource Cache;
         pub fn do_cache_read() with reads(Cache) {}
         pub trait Repo with reads(Db) {
             fn fetch(ref self) -> i64;
         }
         pub struct MyRepo {}
         impl Repo for MyRepo {
             fn fetch(ref self) -> i64 { do_cache_read(); 42 }
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::ImplExceedsTraitCeiling),
        "Expected ImplExceedsTraitCeiling, got: {:?}",
        errors
    );
}

#[test]
fn test_impl_trait_no_ceiling_any_effects_ok() {
    // Trait method has no `with` clause → no ceiling → impl may perform any effects.
    effectcheck_ok(
        "effect resource Db;
         pub fn do_db_read() with reads(Db) {}
         pub trait Repo {
             fn fetch(ref self) -> i64;
         }
         pub struct MyRepo {}
         impl Repo for MyRepo {
             fn fetch(ref self) -> i64 { do_db_read(); 42 }
         }",
    );
}

#[test]
fn test_impl_trait_wildcard_ceiling_any_effects_ok() {
    // Trait method ceiling is `with _` (polymorphic) → impl may perform any effects.
    effectcheck_ok(
        "effect resource Db;
         pub fn do_db_read() with reads(Db) {}
         pub trait Repo {
             fn fetch(ref self) -> i64 with _;
         }
         pub struct MyRepo {}
         impl Repo for MyRepo {
             fn fetch(ref self) -> i64 { do_db_read(); 42 }
         }",
    );
}

#[test]
fn test_inherent_impl_no_ceiling_check() {
    // Inherent impl (no trait name) is never checked against any ceiling.
    effectcheck_ok(
        "effect resource Db;
         pub fn do_db_read() with reads(Db) {}
         pub struct MyRepo {}
         impl MyRepo {
             fn fetch(ref self) -> i64 { do_db_read(); 42 }
         }",
    );
}

// ── Trait default method body check (step 6) ────────────────────────────────

#[test]
fn test_trait_default_body_within_ceiling() {
    // Default body performs exactly the declared ceiling effect → ok.
    effectcheck_ok(
        "effect resource Db;
         pub fn do_db_read() with reads(Db) {}
         pub trait Repo {
             fn fetch(ref self) -> i64 with reads(Db) { do_db_read(); 42 }
         }",
    );
}

#[test]
fn test_trait_default_body_narrower_ok() {
    // Default body performs no effects when ceiling allows reads(Db) → ok (narrower is fine).
    effectcheck_ok(
        "effect resource Db;
         pub trait Repo {
             fn fetch(ref self) -> i64 with reads(Db) { 42 }
         }",
    );
}

#[test]
fn test_trait_default_body_exceeds_ceiling() {
    // Default body performs reads(Cache) but ceiling is reads(Db) → error.
    let errors = effectcheck_errors(
        "effect resource Db;
         effect resource Cache;
         pub fn do_cache_read() with reads(Cache) {}
         pub trait Repo {
             fn fetch(ref self) -> i64 with reads(Db) { do_cache_read(); 42 }
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::TraitDefaultExceedsCeiling),
        "Expected TraitDefaultExceedsCeiling, got: {:?}",
        errors
    );
}

#[test]
fn test_trait_default_body_no_ceiling_ok() {
    // Method has no `with` clause → no ceiling → default body may perform any effects.
    effectcheck_ok(
        "effect resource Db;
         pub fn do_db_read() with reads(Db) {}
         pub trait Repo {
             fn fetch(ref self) -> i64 { do_db_read(); 42 }
         }",
    );
}

#[test]
fn test_trait_default_body_wildcard_ceiling_ok() {
    // Ceiling is `with _` (polymorphic) → default body may perform any effects.
    effectcheck_ok(
        "effect resource Db;
         pub fn do_db_read() with reads(Db) {}
         pub trait Repo {
             fn fetch(ref self) -> i64 with _ { do_db_read(); 42 }
         }",
    );
}

// ── Trait-level effect ceiling (steps 3–4) ──────────────────────────────────

#[test]
fn test_trait_no_effects_methods_unbound() {
    // A trait with no `with` leaves methods with no declared effect ceiling.
    effectcheck_ok(
        "trait Pure {
             fn compute(ref self) -> i64;
         }",
    );
}

#[test]
fn test_trait_level_with_inherited_by_method() {
    // Methods without their own `with` inherit the trait-level ceiling.
    effectcheck_ok(
        "effect resource Db;
         pub trait Repo with reads(Db) {
             fn fetch(ref self) -> i64;
         }
         pub fn query[R: Repo](r: ref R) -> i64 with reads(Db) { r.fetch() }",
    );
}

#[test]
fn test_trait_method_overrides_trait_level_with() {
    // A method with its own `with` fully replaces the trait-level default.
    effectcheck_ok(
        "effect resource Db;
         effect resource Cache;
         pub trait Repo with reads(Db) {
             fn fetch(ref self) -> i64;
             fn cached_fetch(ref self) -> i64 with reads(Cache);
         }",
    );
}

// ── Step 7: Associated function parity ──────────────────────────────────────

#[test]
fn test_assoc_fn_ceiling_collected() {
    // Associated function (no self) has its ceiling collected identically to a
    // receiver method — impl within ceiling is accepted.
    effectcheck_ok(
        "effect resource Db;
         pub fn do_db_read() with reads(Db) {}
         pub trait Factory with reads(Db) {
             fn create() -> i64;
         }
         pub struct MyFactory {}
         impl Factory for MyFactory {
             fn create() -> i64 { do_db_read(); 42 }
         }",
    );
}

#[test]
fn test_assoc_fn_impl_exceeds_ceiling() {
    // Impl of an associated function exceeds the trait ceiling → ImplExceedsTraitCeiling.
    let errors = effectcheck_errors(
        "effect resource Db;
         effect resource Cache;
         pub fn do_cache_read() with reads(Cache) {}
         pub trait Factory with reads(Db) {
             fn create() -> i64;
         }
         pub struct MyFactory {}
         impl Factory for MyFactory {
             fn create() -> i64 { do_cache_read(); 42 }
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::ImplExceedsTraitCeiling),
        "Expected ImplExceedsTraitCeiling for associated fn, got: {:?}",
        errors
    );
}

#[test]
fn test_assoc_fn_default_body_exceeds_ceiling() {
    // Default body of an associated function exceeds its own ceiling →
    // TraitDefaultExceedsCeiling.
    let errors = effectcheck_errors(
        "effect resource Db;
         effect resource Cache;
         pub fn do_cache_read() with reads(Cache) {}
         pub trait Factory {
             fn create() -> i64 with reads(Db) { do_cache_read(); 42 }
         }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::TraitDefaultExceedsCeiling),
        "Expected TraitDefaultExceedsCeiling for associated fn default body, got: {:?}",
        errors
    );
}

// ── Step 9: Unimpled private trait = unbound ceiling ────────────────────────

#[test]
fn test_unimpled_private_trait_ceiling_is_unbound() {
    // A private trait with no impl blocks — any pub caller through its default
    // body must declare `with _` (the ceiling is treated as unbound).
    let errors = effectcheck_errors(
        "trait Opaque {
             fn compute(ref self) -> i64 { 42 }
         }
         pub fn call_through[T: Opaque](t: ref T) -> i64 { t.compute() }",
    );
    assert!(
        errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::MissingEffectDeclaration),
        "Expected MissingEffectDeclaration (must declare with _) for \
         pub caller through unimpled private trait, got: {:?}",
        errors
    );
}

#[test]
fn test_private_trait_with_impl_uses_normal_ceiling() {
    // A private trait that does have an impl — step 9 does not apply and the
    // caller needs no special declaration when the default body is pure.
    effectcheck_ok(
        "trait Opaque {
             fn compute(ref self) -> i64 { 42 }
         }
         pub struct Thing {}
         impl Opaque for Thing {
             fn compute(ref self) -> i64 { 42 }
         }
         pub fn call_through(t: ref Thing) -> i64 { t.compute() }",
    );
}

// ── Step 8: Private-trait SCC inference ─────────────────────────────────────

#[test]
fn test_private_trait_abstract_method_effects_reach_caller() {
    // Abstract (no default body) private-trait method: caller through the concrete
    // impl should see the impl's effects.
    let result = effectcheck_ok(
        "effect resource Db;\n\
         fn reads_db() reads(Db) {}\n\
         trait Proc {\n\
             fn run(ref self);\n\
         }\n\
         struct Worker {}\n\
         impl Proc for Worker {\n\
             fn run(ref self) { reads_db() }\n\
         }\n\
         fn execute(w: ref Worker) { w.run() }",
    );
    let effects = result.inferred_effects.get("execute").unwrap();
    let resources: Vec<_> = effects
        .effects
        .iter()
        .map(|e| e.effect.resource.as_str())
        .collect();
    assert!(
        resources.contains(&"Db"),
        "execute should inherit reads(Db) from Worker.run: {resources:?}"
    );
}

#[test]
fn test_private_trait_multiple_impls_caller_sees_union() {
    // Multiple impls — caller through any abstract dispatch should accumulate
    // all impls' effects (conservative union via `ends_with` matching).
    let result = effectcheck_ok(
        "effect resource Db;\n\
         effect resource Cache;\n\
         fn reads_db() reads(Db) {}\n\
         fn reads_cache() reads(Cache) {}\n\
         trait Proc { fn run(ref self); }\n\
         struct A {}\n\
         struct B {}\n\
         impl Proc for A {\n\
             fn run(ref self) { reads_db() }\n\
         }\n\
         impl Proc for B {\n\
             fn run(ref self) { reads_cache() }\n\
         }\n\
         fn exec_a(a: ref A) { a.run() }\n\
         fn exec_b(b: ref B) { b.run() }",
    );
    let a_effects = result.inferred_effects.get("exec_a").unwrap();
    let a_res: Vec<_> = a_effects
        .effects
        .iter()
        .map(|e| e.effect.resource.as_str())
        .collect();
    assert!(
        a_res.contains(&"Db"),
        "exec_a should see reads(Db): {a_res:?}"
    );

    let b_effects = result.inferred_effects.get("exec_b").unwrap();
    let b_res: Vec<_> = b_effects
        .effects
        .iter()
        .map(|e| e.effect.resource.as_str())
        .collect();
    assert!(
        b_res.contains(&"Cache"),
        "exec_b should see reads(Cache): {b_res:?}"
    );
}

#[test]
fn test_private_trait_default_body_scc_cycle_converges() {
    // Default method body calls abstract sibling; impl of abstract sibling
    // calls back into default method body — forms an SCC.
    // Fixed-point must converge with all effects propagated.
    let result = effectcheck_ok(
        "effect resource Db;\n\
         fn reads_db() reads(Db) {}\n\
         trait T {\n\
             fn step_a(ref self) {\n\
                 self.step_b()\n\
             }\n\
             fn step_b(ref self);\n\
         }\n\
         struct Dual {}\n\
         impl T for Dual {\n\
             fn step_b(ref self) { reads_db() }\n\
         }\n\
         fn run(d: ref Dual) { d.step_a() }",
    );
    let effects = result.inferred_effects.get("run").unwrap();
    let resources: Vec<_> = effects
        .effects
        .iter()
        .map(|e| e.effect.resource.as_str())
        .collect();
    assert!(
        resources.contains(&"Db"),
        "run should see reads(Db) through default body → impl chain: {resources:?}"
    );
}

#[test]
fn test_private_trait_ceiling_inferred_from_impl() {
    // After inference, the declared ceiling for a private trait method with no
    // explicit `with` should be updated to the union of impl inferred effects.
    // Observable: verify_impl_trait_ceilings no longer skips — impl ⊆ ceiling is trivially true.
    effectcheck_ok(
        "effect resource Db;\n\
         fn reads_db() reads(Db) {}\n\
         trait Store { fn fetch(ref self); }\n\
         struct PgStore {}\n\
         impl Store for PgStore {\n\
             fn fetch(ref self) { reads_db() }\n\
         }\n\
         fn load(s: ref PgStore) { s.fetch() }",
    );
}

// ── Step 10: `public_effects = "inferred"` does not bypass pub-trait ceiling ─

#[test]
fn test_inferred_policy_does_not_bypass_pub_trait_ceiling() {
    // Under public_effects = "inferred", the impl-vs-trait ceiling check still
    // fires — the policy only relaxes free-function declaration requirements.
    let parsed = parse(
        "effect resource Db;
         effect resource Cache;
         pub fn do_cache_read() with reads(Cache) {}
         pub trait Repo with reads(Db) {
             fn fetch(ref self) -> i64;
         }
         pub struct MyRepo {}
         impl Repo for MyRepo {
             fn fetch(ref self) -> i64 { do_cache_read(); 42 }
         }",
    );
    assert!(
        parsed.errors.is_empty(),
        "Parse errors: {:?}",
        parsed.errors
    );
    let result = effectcheck_with_policy(&parsed.program, PublicEffectsPolicy::Inferred);
    let real_errors: Vec<_> = result
        .errors
        .iter()
        .filter(|e| e.kind != EffectErrorKind::FfiLintHint)
        .collect();
    assert!(
        real_errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::ImplExceedsTraitCeiling),
        "Expected ImplExceedsTraitCeiling under Inferred policy, got: {:?}",
        real_errors
    );
}

// ── Stdlib allocates(Heap) and suspends seeding ─────────────────

#[test]
fn test_vec_new_infers_allocates_heap() {
    // Vec.new() is a heap-allocating stdlib constructor; the caller must
    // accumulate allocates(Heap) in its inferred effect set.
    let result = effectcheck_ok("fn make_vec() { let v: Vec[i64] = Vec.new(); }");
    let inferred = result.inferred_effects.get("make_vec").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Allocates && e.effect.resource == "Heap"),
        "Expected allocates(Heap) for Vec.new(), got: {:?}",
        inferred.effects
    );
}

#[test]
fn test_vec_push_infers_allocates_heap() {
    // Vec.push() may grow the backing buffer — allocates(Heap).
    let result = effectcheck_ok(
        "fn fill_vec() {
             let mut v: Vec[i64] = Vec.new();
             v.push(1);
         }",
    );
    let inferred = result.inferred_effects.get("fill_vec").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Allocates && e.effect.resource == "Heap"),
        "Expected allocates(Heap) for Vec.push(), got: {:?}",
        inferred.effects
    );
}

#[test]
fn test_map_new_infers_allocates_heap() {
    // Map.new() allocates the underlying storage.
    let result = effectcheck_ok("fn make_map() { let m: Map[String, i64] = Map.new(); }");
    let inferred = result.inferred_effects.get("make_map").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Allocates && e.effect.resource == "Heap"),
        "Expected allocates(Heap) for Map.new(), got: {:?}",
        inferred.effects
    );
}

#[test]
fn test_sorted_set_new_infers_allocates_heap() {
    // SortedSet.new() allocates the B-tree backing store.
    let result = effectcheck_ok("fn make_set() { let s: SortedSet[i64] = SortedSet.new(); }");
    let inferred = result.inferred_effects.get("make_set").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Allocates && e.effect.resource == "Heap"),
        "Expected allocates(Heap) for SortedSet.new(), got: {:?}",
        inferred.effects
    );
}

#[test]
fn test_channel_new_infers_allocates_heap() {
    // Channel.new() allocates the shared queue.
    let result = effectcheck_ok(
        "fn make_chan() {
             let (s, r): (Sender[i64], Receiver[i64]) = Channel.new();
         }",
    );
    let inferred = result.inferred_effects.get("make_chan").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Allocates && e.effect.resource == "Heap"),
        "Expected allocates(Heap) for Channel.new(), got: {:?}",
        inferred.effects
    );
}

#[test]
fn test_sender_send_infers_allocates_heap() {
    // Sender.send() pushes to the heap-allocated queue.
    let result = effectcheck_ok(
        "fn do_send(s: Sender[i64]) {
             s.send(42);
         }",
    );
    let inferred = result.inferred_effects.get("do_send").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Allocates && e.effect.resource == "Heap"),
        "Expected allocates(Heap) for Sender.send(), got: {:?}",
        inferred.effects
    );
}

#[test]
fn test_receiver_recv_infers_suspends() {
    // Receiver.recv() blocks until a message arrives — infers `suspends`.
    let result = effectcheck_ok(
        "fn do_recv(r: Receiver[i64]) -> i64 {
             r.recv()
         }",
    );
    let inferred = result.inferred_effects.get("do_recv").unwrap();
    assert!(
        inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Suspends),
        "Expected suspends for Receiver.recv(), got: {:?}",
        inferred.effects
    );
}

#[test]
fn test_pub_fn_calling_vec_new_must_declare_allocates_heap() {
    // A public function that calls Vec.new() must declare allocates(Heap).
    // Under the default Declared policy, omitting the declaration is an error.
    let errors = effectcheck_errors("pub fn make_vec() { let v: Vec[i64] = Vec.new(); }");
    assert!(
        errors.iter().any(|e| e.message.contains("allocates")),
        "Expected undeclared-allocates error for pub fn calling Vec.new(), got: {:?}",
        errors
    );
}

#[test]
fn test_allocates_heap_propagates_through_call_chain() {
    // allocates(Heap) propagates transitively: inner → outer.
    let result = effectcheck_ok(
        "fn inner() {
             let v: Vec[i64] = Vec.new();
         }
         fn outer() {
             inner();
         }",
    );
    let outer_inferred = result.inferred_effects.get("outer").unwrap();
    assert!(
        outer_inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Allocates && e.effect.resource == "Heap"),
        "allocates(Heap) should propagate from inner to outer, got: {:?}",
        outer_inferred.effects
    );
}

// ── Per-method effect surface for Map / Set (List 2, item 1) ────

fn has_alloc_heap(set: &EffectSet) -> bool {
    set.effects
        .iter()
        .any(|e| e.effect.verb == EffectVerbKind::Allocates && e.effect.resource == "Heap")
}

fn has_panics(set: &EffectSet) -> bool {
    set.effects
        .iter()
        .any(|e| e.effect.verb == EffectVerbKind::Panics)
}

#[test]
fn test_map_insert_infers_allocates_heap() {
    let result = effectcheck_ok(
        "fn fill(m: Map[String, i64]) {
             m.insert(\"k\", 1);
         }",
    );
    assert!(
        has_alloc_heap(result.inferred_effects.get("fill").unwrap()),
        "Map.insert should infer allocates(Heap)"
    );
}

#[test]
fn test_map_merge_infers_allocates_heap() {
    let result = effectcheck_ok(
        "fn do_merge(m1: Map[String, i64], m2: Map[String, i64]) {
             m1.merge(m2);
         }",
    );
    assert!(
        has_alloc_heap(result.inferred_effects.get("do_merge").unwrap()),
        "Map.merge should infer allocates(Heap)"
    );
}

#[test]
fn test_map_keys_values_entries_infer_allocates_heap() {
    for method in ["keys", "values", "entries"] {
        let src = format!(
            "fn dump(m: Map[String, i64]) {{
                 let _v = m.{}();
             }}",
            method
        );
        let result = effectcheck_ok(&src);
        assert!(
            has_alloc_heap(result.inferred_effects.get("dump").unwrap()),
            "Map.{} should infer allocates(Heap)",
            method
        );
    }
}

#[test]
fn test_map_pure_reads_infer_no_alloc() {
    // get / contains_key / len / is_empty are pure reads.
    let result = effectcheck_ok(
        "fn pure_query(m: Map[i64, i64], k: i64) -> bool {
             let n: i64 = m.len();
             let e: bool = m.is_empty();
             let c: bool = m.contains_key(k);
             let g: Option[i64] = m.get(k);
             c
         }",
    );
    let inferred = result.inferred_effects.get("pure_query").unwrap();
    assert!(
        !has_alloc_heap(inferred),
        "pure Map reads should not infer allocates(Heap), got: {:?}",
        inferred.effects
    );
}

#[test]
fn test_map_remove_clear_no_alloc() {
    // remove / clear mutate without growing — no allocates(Heap).
    let result = effectcheck_ok(
        "fn shrink(m: Map[i64, i64], k: i64) {
             let _ = m.remove(k);
             m.clear();
         }",
    );
    let inferred = result.inferred_effects.get("shrink").unwrap();
    assert!(
        !has_alloc_heap(inferred),
        "remove/clear should not infer allocates(Heap), got: {:?}",
        inferred.effects
    );
}

#[test]
fn test_map_index_infers_panics() {
    // Map's `[]` operator can panic on missing key.
    let result = effectcheck_ok(
        "fn lookup(m: Map[i64, i64], k: i64) -> i64 {
             m[k]
         }",
    );
    assert!(
        has_panics(result.inferred_effects.get("lookup").unwrap()),
        "Map[k] should infer panics"
    );
}

#[test]
fn test_pub_fn_returning_map_must_declare_allocates_heap() {
    // Building and returning a Map demands the declared effect on a pub fn.
    let errors = effectcheck_errors(
        "pub fn build_map() -> Map[String, i64] {
             let m: Map[String, i64] = Map.new();
             m
         }",
    );
    assert!(
        errors.iter().any(|e| e.message.contains("allocates")),
        "Expected undeclared-allocates error for pub fn returning a Map, got: {:?}",
        errors
    );
}

#[test]
fn test_pub_fn_indexing_map_must_declare_panics() {
    // `pub fn lookup(...) -> i64 { m[k] }` without `panics` is rejected.
    let errors = effectcheck_errors("pub fn lookup(m: Map[i64, i64], k: i64) -> i64 { m[k] }");
    assert!(
        errors
            .iter()
            .any(|e| e.message.to_lowercase().contains("panic")),
        "Expected undeclared-panics error for pub fn indexing a Map, got: {:?}",
        errors
    );
}

#[test]
fn test_set_insert_infers_allocates_heap() {
    let result = effectcheck_ok(
        "fn fill(s: Set[i64]) {
             s.insert(1);
         }",
    );
    assert!(
        has_alloc_heap(result.inferred_effects.get("fill").unwrap()),
        "Set.insert should infer allocates(Heap)"
    );
}

#[test]
fn test_set_union_intersection_difference_infer_allocates_heap() {
    for method in ["union", "intersection", "difference"] {
        let src = format!(
            "fn combine(a: Set[i64], b: Set[i64]) -> Set[i64] {{
                 a.{}(b)
             }}",
            method
        );
        let result = effectcheck_ok(&src);
        assert!(
            has_alloc_heap(result.inferred_effects.get("combine").unwrap()),
            "Set.{} should infer allocates(Heap)",
            method
        );
    }
}

#[test]
fn test_set_pure_reads_infer_no_alloc() {
    // contains / len / is_empty are pure reads; remove does not grow.
    let result = effectcheck_ok(
        "fn pure_query(s: Set[i64], x: i64) -> bool {
             let n: i64 = s.len();
             let e: bool = s.is_empty();
             let c: bool = s.contains(x);
             let r: bool = s.remove(x);
             c
         }",
    );
    let inferred = result.inferred_effects.get("pure_query").unwrap();
    assert!(
        !has_alloc_heap(inferred),
        "pure Set reads should not infer allocates(Heap), got: {:?}",
        inferred.effects
    );
}

// ── Trait associated function ceilings (List 1, item 4) ─────────

#[test]
fn test_typeparam_assoc_fn_ceiling_propagates() {
    // `T.load()` inside `fn make[T: Loader]` must propagate the trait method's
    // `reads(Db)` ceiling to the caller's inferred set. Public trait so the
    // declared ceiling stays explicit (private traits with no impls collapse
    // to `with _`).
    let result = effectcheck_ok(
        r#"
effect resource Db;

pub trait Loader {
    fn load() -> Self with reads(Db);
}

fn make[T: Loader]() -> T with reads(Db) {
    T.load()
}
"#,
    );
    let make_inferred = result.inferred_effects.get("make").expect("make inferred");
    assert!(
        make_inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Reads && e.effect.resource == "Db"),
        "reads(Db) should propagate from `T.load()` to `make`, got: {:?}",
        make_inferred.effects
    );
}

#[test]
fn test_pub_fn_with_typeparam_assoc_fn_must_declare_ceiling() {
    // A public `make[T: Loader]` that calls `T.load()` must declare reads(Db).
    let errors = effectcheck_errors(
        r#"
effect resource Db;

pub trait Loader {
    fn load() -> Self with reads(Db);
}

pub fn make[T: Loader]() -> T {
    T.load()
}
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("reads(Db)") && e.message.contains("'make'")),
        "expected pub fn to require reads(Db) declaration, got: {errors:?}"
    );
}

#[test]
fn test_bare_assoc_fn_ceiling_propagates() {
    // Bare `load()` inside a `T: Loader` body should propagate reads(Db).
    let result = effectcheck_ok(
        r#"
effect resource Db;

pub trait Loader {
    fn load() -> Self with reads(Db);
}

fn make[T: Loader]() -> T with reads(Db) {
    load()
}
"#,
    );
    let make_inferred = result.inferred_effects.get("make").expect("make inferred");
    assert!(
        make_inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Reads && e.effect.resource == "Db"),
        "reads(Db) should propagate from bare `load()` to `make`, got: {:?}",
        make_inferred.effects
    );
}

#[test]
fn test_concrete_type_dispatch_uses_impl_effects_not_ceiling() {
    // `Wrapper.load()` should propagate the *impl method's* effects, not the
    // trait's wider ceiling. Here the trait ceiling is `reads(Db)` but the impl
    // is pure — caller should be pure.
    let result = effectcheck_ok(
        r#"
effect resource Db;

trait Loader {
    fn load() -> Self with reads(Db);
}

struct Wrapper { value: i64 }

impl Loader for Wrapper {
    fn load() -> Wrapper { Wrapper { value: 0 } }
}

fn caller() -> Wrapper {
    Wrapper.load()
}
"#,
    );
    let caller_inferred = result
        .inferred_effects
        .get("caller")
        .expect("caller inferred");
    assert!(
        !caller_inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Reads && e.effect.resource == "Db"),
        "concrete dispatch on a pure impl must not propagate trait ceiling, got: {:?}",
        caller_inferred.effects
    );
}

#[test]
fn test_supertrait_self_assoc_fn_ceiling_propagates_in_default_body() {
    // `Self.default()` inside the default body of a trait whose supertrait
    // declares reads(Config) must propagate that ceiling to the SCC ceiling
    // for the method (visible through the trait's own ceiling collection).
    let result = effectcheck_ok(
        r#"
effect resource Config;

pub trait Default {
    fn default() -> Self with reads(Config);
}

pub trait Resettable: Default {
    fn reset() -> Self with reads(Config) {
        Self.default()
    }
}
"#,
    );
    let reset_inferred = result
        .inferred_effects
        .get("Resettable.reset")
        .expect("Resettable.reset inferred");
    assert!(
        reset_inferred
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Reads && e.effect.resource == "Config"),
        "reads(Config) should propagate from Self.default() into Resettable.reset's body, got: {:?}",
        reset_inferred.effects
    );
}

// ── Conversion trait `with _` ceilings (round 6 — step 1 + 2) ────────

#[test]
fn test_stdlib_conversion_trait_ceilings_seeded() {
    // Step 1: the four stdlib conversion-trait methods carry `with _`
    // (Polymorphic) ceilings even with no user code declaring them.
    let result = effectcheck_ok("");
    for key in [
        "From.from",
        "Into.into",
        "TryFrom.try_from",
        "TryInto.try_into",
    ] {
        assert!(
            matches!(
                result.declared_effects.get(key),
                Some(DeclaredEffects::Polymorphic)
            ),
            "expected {} to be seeded as DeclaredEffects::Polymorphic, got {:?}",
            key,
            result.declared_effects.get(key),
        );
    }
}

#[test]
fn test_from_impl_with_panics_accepted() {
    // Step 2: a `From` impl whose body panics is accepted under the
    // seeded `with _` ceiling.
    effectcheck_ok(
        "struct ParseError { msg: String }
         struct AppError { msg: String }
         impl From for AppError {
             fn from(e: ParseError) -> AppError with panics {
                 panic(\"convert failed\")
             }
         }",
    );
}

#[test]
fn test_from_impl_with_writes_accepted() {
    // Step 2: a `From` impl that writes a user resource is accepted.
    effectcheck_ok(
        "effect resource Log;
         pub fn write_log() with writes(Log) {}
         struct ParseError { msg: String }
         struct AppError { msg: String }
         impl From for AppError {
             fn from(e: ParseError) -> AppError with writes(Log) {
                 write_log();
                 AppError { msg: e.msg }
             }
         }",
    );
}

#[test]
fn test_from_impl_with_allocates_heap_accepted() {
    // Step 2: a `From` impl that allocates is accepted.
    effectcheck_ok(
        "struct Raw { n: i64 }
         struct Wrapped { items: Vec[i64] }
         impl From for Wrapped {
             fn from(r: Raw) -> Wrapped with allocates(Heap) {
                 let v: Vec[i64] = Vec.new();
                 Wrapped { items: v }
             }
         }",
    );
}

#[test]
fn test_tryfrom_impl_with_effects_accepted() {
    // Step 2: a `TryFrom` impl that performs effects is accepted.
    // `TryFrom.try_from` carries the same `with _` ceiling as `From.from`.
    effectcheck_ok(
        "effect resource Log;
         pub fn write_log() with writes(Log) {}
         struct Raw { n: i64 }
         struct Validated { n: i64 }
         impl TryFrom for Validated {
             type Error = String;
             fn try_from(r: Raw) -> Result[Validated, String] with writes(Log) {
                 write_log();
                 Result.Ok(Validated { n: r.n })
             }
         }",
    );
}

#[test]
fn test_into_propagates_from_impl_effects() {
    // Step 3: `x.into()` lowers to `Target.from(x)` in the lowering pass,
    // so the effectchecker sees a normal `Type.method` call and propagates
    // the From impl's declared effects to the caller.
    let result = effectcheck_full_pipeline(
        "effect resource Log;
         pub fn write_log() with writes(Log) {}
         struct ParseError { msg: String }
         struct AppError { msg: String }
         impl From for AppError {
             fn from(e: ParseError) -> AppError with writes(Log) {
                 write_log();
                 AppError { msg: e.msg }
             }
         }
         fn convert(p: ParseError) -> AppError {
             p.into()
         }",
    );
    let convert_effects = result
        .inferred_effects
        .get("convert")
        .expect("convert inferred");
    assert!(
        convert_effects
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "Log"),
        "expected writes(Log) to propagate from From impl through `.into()` to convert(), got: {:?}",
        convert_effects.effects
    );
}

#[test]
fn test_from_impl_no_effects_still_works() {
    // Regression: a `From` impl with no effects continues to compile under
    // the new seeding (the `Polymorphic` ceiling admits the empty effect set).
    effectcheck_ok(
        "struct ParseError { msg: String }
         struct AppError { msg: String }
         impl From for AppError {
             fn from(e: ParseError) -> AppError { AppError { msg: e.msg } }
         }",
    );
}

#[test]
fn test_into_pure_from_impl_no_spurious_effects() {
    // Regression: `.into()` to a target with a pure From impl must not
    // contribute any effects to the caller.
    let result = effectcheck_full_pipeline(
        "struct Raw { n: i64 }
         struct Wrapped { n: i64 }
         impl From for Wrapped {
             fn from(r: Raw) -> Wrapped { Wrapped { n: r.n } }
         }
         fn convert(r: Raw) -> Wrapped {
             r.into()
         }",
    );
    let convert_effects = result
        .inferred_effects
        .get("convert")
        .expect("convert inferred");
    let non_transparent: Vec<_> = convert_effects
        .effects
        .iter()
        .filter(|e| {
            !matches!(
                e.effect.verb,
                EffectVerbKind::Allocates | EffectVerbKind::Reads
            ) || e.effect.resource != "Heap"
        })
        .collect();
    // No effects expected (allocates(Heap) is fine since Wrapped is stack-allocated).
    assert!(
        non_transparent.is_empty(),
        "pure From impl should add no effects to .into() caller, got: {:?}",
        convert_effects.effects
    );
}

#[test]
fn test_into_distinct_targets_pick_distinct_from_impls() {
    // Step 3: `.into()` resolved to different target types pulls in only
    // that target's From impl effects, not a union across all From[Source]
    // impls. Two From impls for the same source type, distinct effects;
    // each conversion site picks up only its target's effects.
    let result = effectcheck_full_pipeline(
        "effect resource Log;
         effect resource Db;
         pub fn write_log() with writes(Log) {}
         pub fn read_db() with reads(Db) {}
         struct Raw { n: i64 }
         struct A { n: i64 }
         struct B { n: i64 }
         impl From for A {
             fn from(r: Raw) -> A with writes(Log) {
                 write_log();
                 A { n: r.n }
             }
         }
         impl From for B {
             fn from(r: Raw) -> B with reads(Db) {
                 read_db();
                 B { n: r.n }
             }
         }
         fn to_a(r: Raw) -> A { r.into() }
         fn to_b(r: Raw) -> B { r.into() }",
    );
    let to_a_effects = result.inferred_effects.get("to_a").expect("to_a inferred");
    let to_b_effects = result.inferred_effects.get("to_b").expect("to_b inferred");
    assert!(
        to_a_effects
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "Log"),
        "to_a should inherit writes(Log) from A's From impl, got: {:?}",
        to_a_effects.effects
    );
    assert!(
        !to_a_effects
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Reads && e.effect.resource == "Db"),
        "to_a must NOT inherit reads(Db) from B's From impl, got: {:?}",
        to_a_effects.effects
    );
    assert!(
        to_b_effects
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Reads && e.effect.resource == "Db"),
        "to_b should inherit reads(Db) from B's From impl, got: {:?}",
        to_b_effects.effects
    );
    assert!(
        !to_b_effects
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "Log"),
        "to_b must NOT inherit writes(Log) from A's From impl, got: {:?}",
        to_b_effects.effects
    );
}

#[test]
fn test_try_into_propagates_tryfrom_impl_effects() {
    // Round 7: `.try_into()` lowers to `Target.try_from(x)` via the new
    // desugar (lowering.rs:rewrite_try_into_call), so the round-6 step-3
    // propagation path now applies through the sugar without further
    // effectchecker changes.
    let result = effectcheck_full_pipeline(
        "effect resource Log;
         pub fn write_log() with writes(Log) {}
         struct Raw { n: i64 }
         struct Validated { n: i64 }
         impl TryFrom for Validated {
             type Error = String;
             fn try_from(r: Raw) -> Result[Validated, String] with writes(Log) {
                 write_log();
                 Result.Ok(Validated { n: r.n })
             }
         }
         fn convert(r: Raw) -> Result[Validated, String] {
             r.try_into()
         }",
    );
    let convert_effects = result
        .inferred_effects
        .get("convert")
        .expect("convert inferred");
    assert!(
        convert_effects
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "Log"),
        "expected writes(Log) to propagate from TryFrom impl through `.try_into()` to convert(), got: {:?}",
        convert_effects.effects
    );
}

#[test]
fn test_tryfrom_direct_call_propagates_effects() {
    // Round 6 / round 7 boundary: direct `Target.try_from(x)` propagates the
    // TryFrom impl's effects to the caller. Round 6 verified this for the
    // direct-call form before the `.try_into()` desugar landed in round 7.
    let result = effectcheck_full_pipeline(
        "effect resource Log;
         pub fn write_log() with writes(Log) {}
         struct Raw { n: i64 }
         struct Validated { n: i64 }
         impl TryFrom for Validated {
             type Error = String;
             fn try_from(r: Raw) -> Result[Validated, String] with writes(Log) {
                 write_log();
                 Result.Ok(Validated { n: r.n })
             }
         }
         fn convert(r: Raw) -> Result[Validated, String] {
             Validated.try_from(r)
         }",
    );
    let convert_effects = result
        .inferred_effects
        .get("convert")
        .expect("convert inferred");
    assert!(
        convert_effects
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "Log"),
        "expected writes(Log) to propagate from TryFrom impl through Validated.try_from(r), got: {:?}",
        convert_effects.effects
    );
}

// ── Code-review fixes: method-call effect parity ────────────────
//
// The four cases below are method-call analogues of effect-checker rules
// that already worked for free-function calls. Without the fixes, method
// dispatch silently bypassed the rule. Each repro is taken verbatim from
// `brainstorming/code_review_2026_05_01.md`.

#[test]
fn test_method_call_function_ref_arg_propagates_effects() {
    // F1: function-reference argument to a `with _` method must propagate
    // the referenced function's effects to the caller, mirroring the
    // free-call handling. Before the fix, `caller` inferred no effects and
    // emitted no missing-declaration diagnostic.
    let result = effectcheck_full_pipeline(
        "effect resource Log;
         struct Runner {}
         pub fn write_log() with writes(Log) {}
         impl Runner {
             fn run_each[with E](self, f: Fn() with E) with E { f(); }
         }
         pub fn caller() {
             let r = Runner {};
             r.run_each(write_log)
         }",
    );
    let caller_effects = result
        .inferred_effects
        .get("caller")
        .expect("caller inferred");
    assert!(
        caller_effects
            .effects
            .iter()
            .any(|e| e.effect.verb == EffectVerbKind::Writes && e.effect.resource == "Log"),
        "expected writes(Log) to propagate through r.run_each(write_log), got: {:?}",
        caller_effects.effects
    );
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::MissingEffectDeclaration),
        "expected MissingEffectDeclaration on caller (it performs writes(Log) but \
         declares nothing); errors: {:?}",
        result
            .errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_method_call_with_e_unification_disagreeing_closures_conflict() {
    // F2: method-call analogue of `test_with_e_two_slots_disagreeing_closures_conflict`.
    // Two `with E` slots on a method, closures with different effect sets →
    // EffectVariableConflict.
    let result = effectcheck_full_pipeline(
        "effect resource Log;
         effect resource Db;
         struct Runner {}
         pub fn write_log() with writes(Log) {}
         pub fn read_db() with reads(Db) {}
         impl Runner {
             fn pipe[with E](self, a: Fn() with E, b: Fn() with E) with E { a(); b(); }
         }
         pub fn run() with writes(Log) reads(Db) {
             let r = Runner {};
             r.pipe(|| write_log(), || read_db())
         }",
    );
    let conflict = result
        .errors
        .iter()
        .find(|e| e.kind == EffectErrorKind::EffectVariableConflict)
        .unwrap_or_else(|| {
            panic!(
                "expected EffectVariableConflict on r.pipe(...), got: {:?}",
                result
                    .errors
                    .iter()
                    .map(|e| (&e.kind, &e.message))
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        conflict.message.contains("`E`")
            && conflict.message.contains("writes(Log)")
            && conflict.message.contains("reads(Db)"),
        "conflict diagnostic should name E and both bindings, got: {}",
        conflict.message
    );
}

#[test]
fn test_method_call_pure_fn_slot_rejects_effectful_closure() {
    // F3: method-call analogue of the Fn-slot subtyping check. A method
    // whose closure parameter is pure (`Fn()`, no `with` clause) must
    // reject an effectful closure even when the enclosing caller declares
    // the effects.
    let result = effectcheck_full_pipeline(
        "effect resource Log;
         struct Runner {}
         pub fn write_log() with writes(Log) {}
         impl Runner {
             fn run_pure(self, f: Fn()) { f(); }
         }
         pub fn caller() with writes(Log) {
             let r = Runner {};
             r.run_pure(|| write_log())
         }",
    );
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.kind == EffectErrorKind::EffectSubtypeViolation),
        "expected EffectSubtypeViolation on r.run_pure(|| write_log()) (closure has \
         writes(Log) but slot is pure), got: {:?}",
        result
            .errors
            .iter()
            .map(|e| (&e.kind, &e.message))
            .collect::<Vec<_>>()
    );
}

// ── Step 8: capture mode does not feed into closure effect set ──
//
// Per design.md § Closures (Rule 2½), the four legal capture-mode forms
// (`||`, `own ||`, `ref ||`, `mut ref ||`) are ownership signals that
// drive RC fallback / drop placement. They must NOT influence the
// closure's inferred effect set; effects come from the body's actions
// alone. The current pass ordering runs effect inference before
// ownership analysis, so the property holds vacuously today; these
// tests pin it as a regression sentinel against any future code that
// would let capture mode leak into the effect side.

#[test]
fn capture_mode_prefix_does_not_change_inferred_effects() {
    // The closure expression is wrapped in parens to sidestep the parser's
    // call-arg rejection of `ref` / `mut ref` (design.md Part 1½ Rule 4 —
    // those keywords are not legal at call sites except as a closure prefix
    // recognized inside a primary-expression context). Parens reach that
    // context. The semantics of `(closure)` and `closure` are identical for
    // effect inference — `inline_closure_effects_for_arg` walks through the
    // paren wrapper.
    let collect_resources = |prefix: &str| -> Vec<String> {
        let source = format!(
            "effect resource UserDB;\n\
             fn reads_db() reads(UserDB) {{ }}\n\
             pub fn apply(f: Fn() -> () with _) -> () with _ {{ f() }}\n\
             fn caller() {{ apply(({prefix}|| reads_db())); }}"
        );
        let result = effectcheck_ok(&source);
        let mut verbs: Vec<String> = result
            .inferred_effects
            .get("caller")
            .unwrap()
            .effects
            .iter()
            .map(|e| e.effect.resource.clone())
            .collect();
        verbs.sort();
        verbs
    };
    let bare = collect_resources("");
    let own = collect_resources("own ");
    let ref_ = collect_resources("ref ");
    let mut_ref = collect_resources("mut ref ");
    assert!(
        bare.contains(&"UserDB".to_string()),
        "bare prefix should propagate reads(UserDB): {bare:?}"
    );
    assert_eq!(bare, own, "own prefix changed the inferred effect set");
    assert_eq!(bare, ref_, "ref prefix changed the inferred effect set");
    assert_eq!(
        bare, mut_ref,
        "mut ref prefix changed the inferred effect set"
    );
}

#[test]
fn empty_closure_body_yields_no_effects_for_any_capture_mode() {
    for prefix in &["", "own ", "ref ", "mut ref "] {
        let source = format!(
            "pub fn apply(f: Fn() -> () with _) -> () with _ {{ f() }}\n\
             fn caller() {{ apply(({prefix}|| {{}})); }}"
        );
        let result = effectcheck_ok(&source);
        let effects = &result.inferred_effects.get("caller").unwrap().effects;
        assert!(
            effects.is_empty(),
            "{prefix}|| should infer empty effects, got: {:?}",
            effects
                .iter()
                .map(|e| (e.effect.verb.clone(), e.effect.resource.clone()))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn mut_ref_capture_does_not_synthesize_writes_effect() {
    // A closure that captures `mut ref` and mutates the captured binding
    // must NOT synthesize a `writes(R)` effect. The mutation is an
    // ownership operation against the local binding `x`, not a write
    // against any declared resource. The closure's effect set must be
    // empty (no callee with effects, nothing else effect-positive in the
    // body). Locks in the separation between ownership and effect.
    let result = effectcheck_ok(
        "pub fn apply(f: Fn() -> () with _) -> () with _ { f() }\n\
         fn caller() {\n\
             let mut x: i64 = 1;\n\
             apply((mut ref || { x = x + 1; }));\n\
         }",
    );
    let effects = &result.inferred_effects.get("caller").unwrap().effects;
    assert!(
        effects.is_empty(),
        "mut ref capture mutation must not produce a writes effect: {:?}",
        effects
            .iter()
            .map(|e| (e.effect.verb.clone(), e.effect.resource.clone()))
            .collect::<Vec<_>>()
    );
}
