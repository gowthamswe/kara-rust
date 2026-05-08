//! Integration tests for par block codegen (Phase 7).
//!
//! These tests verify:
//! - IR-level: par blocks lower to a `karac_par_run` call with the correct
//!   number of branch function pointers.
//! - End-to-end: a compiled par program statically links the runtime, spawns
//!   real threads, and produces output from every branch.
//!
//! The end-to-end tests build the runtime crate on first use via
//! `cargo build -p karac-runtime --release`. If that build fails (e.g., no
//! Cargo available in the test environment) the tests soft-skip by returning
//! early, matching the pattern in tests/codegen.rs.

#[cfg(feature = "llvm")]
mod par_codegen_tests {
    use karac::codegen::compile_to_ir;
    use std::path::PathBuf;
    use std::sync::Once;

    static RUNTIME_BUILT: Once = Once::new();
    static mut RUNTIME_PATH: Option<PathBuf> = None;

    /// Build the runtime static library once per test process and return its
    /// path. Returns None if the build fails — callers soft-skip.
    #[allow(static_mut_refs)]
    fn runtime_path() -> Option<PathBuf> {
        RUNTIME_BUILT.call_once(|| {
            let output = std::process::Command::new("cargo")
                .args(["build", "-p", "karac-runtime", "--release"])
                .output();
            if let Ok(out) = output {
                if out.status.success() {
                    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("target/release/libkarac_runtime.a");
                    if p.exists() {
                        unsafe {
                            RUNTIME_PATH = Some(p);
                        }
                    }
                }
            }
        });
        unsafe { RUNTIME_PATH.clone() }
    }

    fn ir_for(src: &str) -> String {
        let parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        compile_to_ir(&parsed.program, None, None).expect("codegen failed")
    }

    /// Like `ir_for` but runs the full analysis pipeline first so the
    /// `Program.callee_effectful` side-table is populated. Required for the
    /// par-branch cancel-check narrowing — without effect-check info every
    /// callee is unknown and the check fires conservatively.
    fn ir_for_with_pipeline(src: &str) -> String {
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
        // Mirror `Pipeline::effectcheck`: a callee is "effectful" iff its
        // inferred or declared set contains reads/writes/sends/receives.
        use karac::effectchecker::DeclaredEffects;
        fn set_eff(s: &karac::effectchecker::EffectSet) -> bool {
            s.effects.iter().any(|t| {
                matches!(
                    t.effect.verb,
                    karac::ast::EffectVerbKind::Reads
                        | karac::ast::EffectVerbKind::Writes
                        | karac::ast::EffectVerbKind::Sends
                        | karac::ast::EffectVerbKind::Receives
                )
            })
        }
        let mut table = std::collections::HashMap::new();
        for (name, set) in &effects.inferred_effects {
            table.insert(name.clone(), set_eff(set));
        }
        for (name, decl) in &effects.declared_effects {
            let eff = match decl {
                DeclaredEffects::Explicit(s) => set_eff(s),
                DeclaredEffects::Polymorphic | DeclaredEffects::PolymorphicWithFixed(_) => true,
                DeclaredEffects::None => false,
            };
            table
                .entry(name.clone())
                .and_modify(|v| *v = *v || eff)
                .or_insert(eff);
        }
        parsed.program.callee_effectful = table;
        compile_to_ir(&parsed.program, None, None).expect("codegen failed")
    }

    /// Compile, link with the runtime, and run the program. Returns stdout
    /// on success, None if link/exec fails (legitimate soft-skip when the
    /// runtime archive is missing). Parse and codegen failures panic — those
    /// are programming bugs, not environment issues.
    fn run_program(src: &str) -> Option<String> {
        use karac::codegen::{compile_to_object, link_executable};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let rt = runtime_path()?;
        std::env::set_var("KARAC_RUNTIME", &rt);

        let parsed = karac::parse(src);
        if !parsed.errors.is_empty() {
            let mut msg = String::from("test source failed to parse:\n");
            for e in &parsed.errors {
                msg.push_str(&format!("  {:?}\n", e));
            }
            panic!("{}", msg);
        }

        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let obj_path = format!("/tmp/karac_par_e2e_{}_{}.o", std::process::id(), id);
        let exe_path = format!("/tmp/karac_par_e2e_{}_{}", std::process::id(), id);

        if let Err(e) = compile_to_object(&parsed.program, &obj_path, None, None) {
            panic!("codegen failed for test program: {}", e);
        }
        link_executable(&obj_path, &exe_path).ok()?;

        let output = std::process::Command::new(&exe_path).output().ok()?;

        let _ = std::fs::remove_file(&obj_path);
        let _ = std::fs::remove_file(&exe_path);

        Some(String::from_utf8_lossy(&output.stdout).to_string())
    }

    // ── IR-level tests ────────────────────────────────────────────

    #[test]
    fn test_ir_par_block_emits_runtime_call() {
        let ir = ir_for(
            r#"
fn main() {
    par {
        println(100);
        println(200);
    }
}
"#,
        );
        assert!(
            ir.contains("declare void @karac_par_run"),
            "IR should declare karac_par_run; got:\n{ir}"
        );
        assert!(
            ir.contains("call void @karac_par_run"),
            "IR should call karac_par_run; got:\n{ir}"
        );
        assert!(
            ir.contains("__par_branch_0_0"),
            "IR should define first branch fn"
        );
        assert!(
            ir.contains("__par_branch_0_1"),
            "IR should define second branch fn"
        );
    }

    #[test]
    fn test_ir_par_single_stmt_no_runtime_call() {
        // Par with one statement is optimized to sequential — no runtime call.
        let ir = ir_for(
            r#"
fn main() {
    par {
        println(42);
    }
}
"#,
        );
        assert!(
            !ir.contains("call void @karac_par_run"),
            "single-stmt par should not call runtime; got:\n{ir}"
        );
    }

    #[test]
    fn test_ir_par_empty_block_no_runtime_call() {
        let ir = ir_for(
            r#"
fn main() {
    par {
    }
}
"#,
        );
        assert!(
            !ir.contains("call void @karac_par_run"),
            "empty par should not call runtime; got:\n{ir}"
        );
    }

    // ── Mid-branch cooperative cancellation ───────────────────────────────

    /// Count mid-branch cancel checks across every par-branch function in
    /// the IR. Each top-level statement inside `par { }` lowers to its own
    /// branch fn (e.g. `__par_branch_0_0`, `__par_branch_0_1`), so per-call
    /// narrowing is observed by aggregating across all of them. We key on
    /// `call.cancel.flag = load` — the unique atomic-flag load instruction
    /// that opens each mid-branch check (the entry-time check uses a
    /// different `%cancel` SSA name).
    fn count_branch_cancel_checks(ir: &str) -> usize {
        let mut total = 0;
        let mut cursor = 0;
        while let Some(off) = ir[cursor..].find("define void @__par_branch_") {
            let start = cursor + off;
            let end = ir[start + 1..]
                .find("define ")
                .map(|i| start + 1 + i)
                .unwrap_or(ir.len());
            total += ir[start..end].matches("call.cancel.flag = load").count();
            cursor = end;
        }
        total
    }

    #[test]
    fn test_ir_par_branch_emits_cancel_check_per_effectful_call() {
        // Each call to an effectful helper inside a par branch should emit a
        // mid-branch cancel check (load-and-branch on the runtime atomic).
        let ir = ir_for_with_pipeline(
            r#"
effect resource Log;
fn helper(n: i64) -> i64 writes(Log) { n + 1 }
fn main() {
    par {
        let _ = helper(1_i64);
        let _ = helper(2_i64);
        let _ = helper(3_i64);
    }
}
"#,
        );
        let total = count_branch_cancel_checks(&ir);
        assert!(
            total >= 3,
            "expected ≥3 mid-branch cancel checks before effectful helper() calls across all \
             par branches, found {total}; IR:\n{ir}"
        );
    }

    #[test]
    fn test_ir_par_branch_skips_cancel_check_for_pure_callees() {
        // Pure callees (no reads/writes/sends/receives) should have their
        // mid-branch cancel checks elided per the v1 narrowing — the
        // observable behavior is unchanged because a cooperative cancel
        // can't observe a mid-state through a side-effect-free call.
        let ir = ir_for_with_pipeline(
            r#"
fn pure_helper(n: i64) -> i64 { n + 1 }
fn main() {
    par {
        let _ = pure_helper(1_i64);
        let _ = pure_helper(2_i64);
        let _ = pure_helper(3_i64);
    }
}
"#,
        );
        let total = count_branch_cancel_checks(&ir);
        assert_eq!(
            total, 0,
            "pure helpers should not emit mid-branch cancel checks; found {total}; IR:\n{ir}"
        );
    }

    #[test]
    fn test_ir_par_branch_mixed_pure_and_effectful() {
        // In a par block that mixes pure and effectful calls, only the
        // effectful calls should carry the cancel check.
        let ir = ir_for_with_pipeline(
            r#"
effect resource Log;
fn pure_helper(n: i64) -> i64 { n + 1 }
fn effectful_helper(n: i64) -> i64 writes(Log) { n + 1 }
fn main() {
    par {
        let _ = pure_helper(1_i64);
        let _ = effectful_helper(2_i64);
        let _ = pure_helper(3_i64);
        let _ = effectful_helper(4_i64);
    }
}
"#,
        );
        let total = count_branch_cancel_checks(&ir);
        assert_eq!(
            total, 2,
            "expected exactly 2 mid-branch cancel checks (one per effectful call); \
             found {total}; IR:\n{ir}"
        );
    }

    #[test]
    fn test_ir_par_branch_skips_method_check_for_pure_callee() {
        // A method whose body has no observable effects (no reads/writes/
        // sends/receives) should not emit a mid-branch cancel check at the
        // call site, mirroring the narrowing already in place for free
        // functions and `Type.assoc` calls. (`pure` is a reserved keyword
        // for future use, so the method is named `compute` here.)
        let ir = ir_for_with_pipeline(
            r#"
struct Counter { n: i64 }
impl Counter {
    fn compute(ref self) -> i64 { self.n + 1 }
}
fn main() {
    let c = Counter { n: 1 };
    par {
        let _ = c.compute();
        let _ = c.compute();
    }
}
"#,
        );
        let total = count_branch_cancel_checks(&ir);
        assert_eq!(
            total, 0,
            "pure method calls should not emit mid-branch cancel checks; \
             found {total}; IR:\n{ir}"
        );
    }

    #[test]
    fn test_ir_par_branch_emits_method_check_for_effectful_callee() {
        // A method that writes a resource is observably effectful — the
        // mid-branch cancel check must fire before each call site.
        let ir = ir_for_with_pipeline(
            r#"
effect resource Log;
struct Counter { n: i64 }
impl Counter {
    fn effectful(ref self) -> i64 writes(Log) { self.n + 1 }
}
fn main() {
    let c = Counter { n: 1 };
    par {
        let _ = c.effectful();
        let _ = c.effectful();
    }
}
"#,
        );
        let total = count_branch_cancel_checks(&ir);
        assert_eq!(
            total, 2,
            "expected exactly 2 mid-branch cancel checks (one per effectful method call); \
             found {total}; IR:\n{ir}"
        );
    }

    #[test]
    fn test_ir_non_par_function_no_cancel_check_per_call() {
        // Functions outside par blocks should NOT carry mid-call cancel
        // checks — the cancel pointer isn't even in scope.
        let ir = ir_for(
            r#"
fn helper(n: i64) -> i64 { n + 1 }
fn main() {
    let _ = helper(1_i64);
    let _ = helper(2_i64);
}
"#,
        );
        // `call.cancel.bb` is the cancel-block label emitted only by the
        // mid-branch helper. It must not appear in @main's IR.
        let start = ir.find("define i32 @main").unwrap_or(0);
        let after_main = &ir[start..];
        assert!(
            !after_main.contains("call.cancel.bb"),
            "non-par function should not emit mid-branch cancel check blocks; IR:\n{after_main}"
        );
    }

    // ── End-to-end tests ──────────────────────────────────────────

    #[test]
    fn test_e2e_par_both_branches_run() {
        let out = run_program(
            r#"
fn main() {
    par {
        println(100);
        println(200);
    }
}
"#,
        );
        if let Some(out) = out {
            // Branches may interleave — just verify both tokens appear.
            assert!(
                out.contains("100"),
                "first branch should have printed 100; got {out:?}"
            );
            assert!(
                out.contains("200"),
                "second branch should have printed 200; got {out:?}"
            );
        }
    }

    #[test]
    fn test_e2e_par_three_branches_run() {
        let out = run_program(
            r#"
fn main() {
    par {
        println(1);
        println(2);
        println(3);
    }
}
"#,
        );
        if let Some(out) = out {
            for tok in ["1", "2", "3"] {
                assert!(
                    out.contains(tok),
                    "branch {tok} should have printed; got {out:?}"
                );
            }
        }
    }

    // ── Auto-parallelization of non-par regions ──

    /// Compile-time helper for slice 2's auto-par tests: runs the full
    /// pipeline (resolve → typecheck → lower → effectcheck →
    /// concurrency_analyze), threads the resulting `ConcurrencyAnalysis`
    /// into codegen via `compile_to_ir_with_options`, and returns the
    /// emitted IR. Mirrors `ir_for_with_pipeline` but additionally
    /// constructs the analysis object the auto-par codegen path consumes.
    fn ir_for_with_concurrency(src: &str) -> String {
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
        let effects = karac::effectcheck(&parsed.program);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);
        compile_to_ir_with_options(&parsed.program, None, Some(&analysis), None, None)
            .expect("codegen failed")
    }

    /// Three independent reads on disjoint resources — the analyzer
    /// groups all three as parallelizable, no binding leaks out (all
    /// `let _ = ...`), so the auto-par dispatch fires and the IR holds
    /// exactly one `karac_par_run` call site that fans out three branch
    /// fns.
    #[test]
    fn test_auto_par_three_independent_reads_emits_par_run() {
        let ir = ir_for_with_concurrency(
            r#"
effect resource Net;
effect resource Disk;
effect resource Db;

fn fetch_net() -> i64 reads(Net) { 1 }
fn fetch_disk() -> i64 reads(Disk) { 2 }
fn fetch_db() -> i64 reads(Db) { 3 }

fn main() {
    let _ = fetch_net();
    let _ = fetch_disk();
    let _ = fetch_db();
}
"#,
        );
        let calls = ir.matches("call void @karac_par_run").count();
        assert_eq!(
            calls, 1,
            "expected exactly one karac_par_run dispatch for three independent reads; \
             found {calls}; IR:\n{ir}"
        );
        // Three branch fns minted from one auto-par site. We use
        // par_id=0 because main's body is the first par site emitted.
        for i in 0..3 {
            let needle = format!("__par_branch_0_{i}");
            assert!(
                ir.contains(&needle),
                "expected branch fn {needle} in IR:\n{ir}"
            );
        }
    }

    /// Three pure top-level lets — the analyzer marks the group as
    /// `is_trivial = true` (no effects), and the codegen granularity
    /// gate emits sequentially with no `karac_par_run` call. Pins the
    /// `is_trivial` short-circuit in `compile_function_body`.
    #[test]
    fn test_auto_par_skips_trivial_pure_group() {
        let ir = ir_for_with_concurrency(
            r#"
fn main() {
    let _a = 1_i64;
    let _b = 2_i64;
    let _c = 3_i64;
}
"#,
        );
        assert!(
            !ir.contains("call void @karac_par_run"),
            "trivial pure group should not call karac_par_run; IR:\n{ir}"
        );
    }

    /// Two `writes(Disk)` calls on the same resource — the analyzer
    /// must not group them (effect conflict on the same resource), so
    /// the codegen emits sequentially. Pins that the lowering respects
    /// analyzer decisions and never speculatively parallelizes.
    #[test]
    fn test_auto_par_serializes_when_resources_conflict() {
        let ir = ir_for_with_concurrency(
            r#"
effect resource Disk;

fn write_a() -> i64 writes(Disk) { 1 }
fn write_b() -> i64 writes(Disk) { 2 }

fn main() {
    let _ = write_a();
    let _ = write_b();
}
"#,
        );
        assert!(
            !ir.contains("call void @karac_par_run"),
            "writes(Disk) ↔ writes(Disk) should serialize; IR:\n{ir}"
        );
    }
}
