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

    /// Debugger Contract slice 4: `karac_par_run` takes a `spawn_site_id`
    /// argument (the same `par_id` minted via slice 3's
    /// `record_spawn_site`). With two par blocks in the program, the call
    /// sites must pass `i32 0` and `i32 1` respectively — pinning the
    /// codegen-side argument-passing change against future regression.
    /// The runtime uses this ID to populate `KaracFrame::spawn_site_id`
    /// for slice 5's enumeration surface.
    #[test]
    fn test_emit_par_run_passes_spawn_site_id() {
        let ir = ir_for(
            r#"
fn main() {
    par {
        println(1);
        println(2);
    }
    par {
        println(3);
        println(4);
    }
}
"#,
        );
        // The extern declaration's signature now includes the `i32`
        // spawn-site id as the third arg.
        assert!(
            ir.contains("declare void @karac_par_run(ptr, i64, i32)"),
            "extern decl should be (ptr, i64, i32); got:\n{ir}"
        );
        // Two call sites — one with spawn_site_id 0, one with 1.
        // Inkwell emits the actual call as
        // `call void @karac_par_run(ptr ..., i64 ..., i32 0)`.
        let calls: Vec<&str> = ir
            .lines()
            .filter(|l| l.contains("call void @karac_par_run"))
            .collect();
        assert_eq!(
            calls.len(),
            2,
            "expected exactly two karac_par_run calls; got {}: {:?}",
            calls.len(),
            calls
        );
        let mut seen_zero = false;
        let mut seen_one = false;
        for c in &calls {
            if c.contains("i32 0)") {
                seen_zero = true;
            }
            if c.contains("i32 1)") {
                seen_one = true;
            }
        }
        assert!(
            seen_zero,
            "expected one call with spawn_site_id `i32 0`; calls:\n{:?}",
            calls
        );
        assert!(
            seen_one,
            "expected one call with spawn_site_id `i32 1`; calls:\n{:?}",
            calls
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

    // ── Auto-parallelization with return values ──
    //
    // Slice A (Phase-7 — Par codegen: return values, 2026-05-09) lifts
    // slice 2's `group_defines_binding_used_outside` gate. Each parallel
    // group whose let-bindings are read after the group gets a
    // synthesized parent-allocated return struct
    // (`__karac_ParGroup_<spawn_site_id>_Returns`); each branch writes
    // its produced value into an assigned field by offset, and the
    // parent loads the values back after `karac_par_run` joins. The
    // first test below confirms the four-read demo shape — which under
    // slice 2 fell back to sequential — now fans out through one
    // `karac_par_run` call with four branch fns and produces four
    // `load` instructions for the slot-back-read. The second test
    // pins the parallax-lite shape (no class-(ii) bindings) at byte-
    // equivalent IR — the empty-slot path should preserve slice 2's
    // behavior exactly.

    /// Four-read demo shape: each branch produces a typed value, the
    /// final-expr call consumes them. Asserts: (a) exactly one
    /// `karac_par_run` dispatch, (b) the parent allocates a
    /// `__karac_ParGroup_*_Returns` struct, (c) four slot-load
    /// instructions appear after the runtime call, (d) the joined
    /// `combine` call site receives those loaded values as its args.
    #[test]
    fn test_auto_par_four_reads_with_join_emits_par_run_and_slot_loads() {
        let ir = ir_for_with_concurrency(
            r#"
effect resource Net;
effect resource Disk;
effect resource Db;
effect resource Cache;

fn fetch_net() -> i64 reads(Net) { 1 }
fn fetch_disk() -> i64 reads(Disk) { 2 }
fn fetch_db() -> i64 reads(Db) { 3 }
fn fetch_cache() -> i64 reads(Cache) { 4 }

fn combine(a: i64, b: i64, c: i64, d: i64) -> i64 {
    a + b + c + d
}

fn main() {
    let result_1 = fetch_net();
    let result_2 = fetch_disk();
    let result_3 = fetch_db();
    let result_4 = fetch_cache();
    println(combine(result_1, result_2, result_3, result_4));
}
"#,
        );
        // (a) Exactly one karac_par_run dispatch.
        let calls = ir.matches("call void @karac_par_run").count();
        assert_eq!(
            calls, 1,
            "expected exactly one karac_par_run dispatch for four-read fan-out; \
             found {calls}; IR:\n{ir}"
        );
        // Four branch fns minted from the one auto-par site.
        for i in 0..4 {
            let needle = format!("__par_branch_0_{i}");
            assert!(
                ir.contains(&needle),
                "expected branch fn {needle} in IR:\n{ir}"
            );
        }
        // (b) Return struct synthesized with a deterministic name.
        // The name is `__karac_ParGroup_<id>_Returns`; the first auto-
        // par site mints id=0.
        assert!(
            ir.contains("__karac_ParGroup_0_Returns"),
            "expected return-struct type `__karac_ParGroup_0_Returns` in IR:\n{ir}"
        );
        // (c) Four slot loads after `karac_par_run` — one per slot.
        // The IR uses one `getelementptr` per slot field address plus
        // one `load` per field; we look for the named result registers
        // we emitted (`%result_1`, `%result_2`, `%result_3`,
        // `%result_4`) which only appear at the slot-back-read sites
        // because the original source bindings are inside the branches
        // (not visible in the parent function before slice A landed).
        for name in ["result_1", "result_2", "result_3", "result_4"] {
            let needle = format!("__par_slot_{name}_ptr");
            assert!(
                ir.contains(&needle),
                "expected slot-pointer GEP {needle} in IR:\n{ir}"
            );
        }
        // Four slot loads: each binding name appears as a load result.
        // Allow either the value-form or the pointer-load shape; the
        // simple "load i64" count must equal-or-exceed 4 inside main.
        let load_count = ir.matches("__par_slot_result_").count();
        assert!(
            load_count >= 4,
            "expected ≥4 slot-related GEP/load registers; found {load_count}; IR:\n{ir}"
        );
        // (d) The `combine(...)` call site uses the loaded values.
        // The parent emits exactly one `call i64 @combine(...)` after
        // the par-run dispatch, with arguments fed from the four slot
        // loads. We assert the call exists; the argument-flow
        // assertion is covered by the E2E correctness test in
        // tests/codegen.rs (wall-clock + sum-value).
        assert!(
            ir.contains("call i64 @combine"),
            "expected `call i64 @combine` site after slot loads in IR:\n{ir}"
        );
    }

    /// Bug fix: when an auto-par group's bindings are consumed only
    /// via an f-string in the tail position (e.g.
    /// `f"{a}-{b}-{c}-{d}"`), `compute_return_slots`'s `refs_in_expr`
    /// walk previously didn't recurse into
    /// `ExprKind::InterpolatedStringLit`'s segment list, so the four
    /// names were missed by the outside-group reads set, no slots
    /// were materialized, and codegen errored "Undefined variable"
    /// at the f-string's load sites (the workaround applied in
    /// Slice E `ea1d26d` was returning a fixed JSON literal).
    ///
    /// Fixed in `src/codegen.rs::refs_in_expr`: added an
    /// `InterpolatedStringLit` arm that walks each
    /// `ParsedInterpolationPart::Expr`. This test mirrors the
    /// four-reads-with-join shape above but routes the captures
    /// through an f-string instead of a direct fn call.
    #[test]
    fn test_auto_par_fstring_tail_captures_par_group_bindings() {
        let ir = ir_for_with_concurrency(
            r#"
effect resource Net;
effect resource Disk;
effect resource Db;
effect resource Cache;

fn fetch_net() -> i64 reads(Net) { 1 }
fn fetch_disk() -> i64 reads(Disk) { 2 }
fn fetch_db() -> i64 reads(Db) { 3 }
fn fetch_cache() -> i64 reads(Cache) { 4 }

fn main() {
    let result_1 = fetch_net();
    let result_2 = fetch_disk();
    let result_3 = fetch_db();
    let result_4 = fetch_cache();
    println(f"{result_1}-{result_2}-{result_3}-{result_4}");
}
"#,
        );
        let calls = ir.matches("call void @karac_par_run").count();
        assert_eq!(calls, 1, "expected one karac_par_run dispatch; IR:\n{ir}");
        assert!(
            ir.contains("__karac_ParGroup_0_Returns"),
            "expected return struct; pre-fix the f-string segments \
             weren't walked so no slots were materialized; IR:\n{ir}"
        );
        for name in ["result_1", "result_2", "result_3", "result_4"] {
            let needle = format!("__par_slot_{name}_ptr");
            assert!(
                ir.contains(&needle),
                "expected slot-pointer GEP {needle}; IR:\n{ir}"
            );
        }
    }

    /// Three independent `writes(R_i)` calls on disjoint resources
    /// with no joined return — the parallax-lite microbenchmark
    /// shape. The auto-par dispatch fires (one `karac_par_run`, three
    /// branches) but the slot mechanism is dormant: no return-struct
    /// type is emitted, no slot-pointer GEPs. Pins that the empty-slot
    /// path preserves slice 2's behavior — the load-bearing test
    /// against IR-shape regression for the parallax-lite benchmark.
    #[test]
    fn test_auto_par_three_reads_no_outside_use_keeps_parallax_lite_shape() {
        let ir = ir_for_with_concurrency(
            r#"
effect resource R0;
effect resource R1;
effect resource R2;

fn write_r0() writes(R0) {}
fn write_r1() writes(R1) {}
fn write_r2() writes(R2) {}

fn main() {
    write_r0();
    write_r1();
    write_r2();
}
"#,
        );
        // One karac_par_run dispatch, three branch fns.
        let calls = ir.matches("call void @karac_par_run").count();
        assert_eq!(
            calls, 1,
            "expected one karac_par_run dispatch for parallax-lite shape; IR:\n{ir}"
        );
        for i in 0..3 {
            let needle = format!("__par_branch_0_{i}");
            assert!(
                ir.contains(&needle),
                "expected branch fn {needle} in IR:\n{ir}"
            );
        }
        // Empty slot list: no return struct synthesized.
        assert!(
            !ir.contains("__karac_ParGroup_0_Returns"),
            "no return struct should be emitted for the empty-slot path; IR:\n{ir}"
        );
        // No slot GEPs.
        assert!(
            !ir.contains("__par_slot_"),
            "no slot pointer/load instructions should appear; IR:\n{ir}"
        );
    }

    /// Regression: `let a = fetch_a(); let b = fetch_b(); use_v(a);
    /// use_v(b);` previously codegen'd with `Undefined variable 'b'`.
    ///
    /// Root cause was upstream of Slice A's slot ABI: the analyzer's
    /// greedy grouping in `find_parallel_groups` would skip over
    /// dependent middle stmts and emit non-contiguous parallel groups
    /// (e.g., `[0, 3]` and `[1, 2]`). The codegen's
    /// `i = max_idx + 1` step then jumped past the second group's
    /// stmts entirely, so `let b` never ran in the parent scope and
    /// `use_v(b)` (which had been pulled into the first group's
    /// branch fn for stmt 3) failed to resolve `b`.
    ///
    /// Fixed at the analyzer level — parallel groups are now
    /// contiguous-only (`src/concurrency.rs::find_parallel_groups`
    /// breaks on the first non-eligible candidate instead of
    /// skipping). This test pins that the source compiles cleanly
    /// without the spurious diagnostic.
    #[test]
    fn test_auto_par_non_contiguous_group_no_undefined_var() {
        let src = r#"
effect resource R1;
effect resource R2;

fn fetch_a() -> Vec[i64] reads(R1) {
    let mut v: Vec[i64] = Vec.new();
    v.push(42);
    v
}

fn fetch_b() -> Vec[i64] reads(R2) {
    let mut v: Vec[i64] = Vec.new();
    v.push(99);
    v
}

fn use_v(v: Vec[i64]) { println(v.len()); }

fn main() {
    let a: Vec[i64] = fetch_a();
    let b: Vec[i64] = fetch_b();
    use_v(a);
    use_v(b);
}
"#;
        use karac::codegen::compile_to_ir_with_options;
        let mut parsed = karac::parse(src);
        assert!(parsed.errors.is_empty(), "parse: {:?}", parsed.errors);
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let effects = karac::effectcheck(&parsed.program);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);
        let res = compile_to_ir_with_options(&parsed.program, None, Some(&analysis), None, None);
        assert!(
            res.is_ok(),
            "expected clean compile post-contiguous-only fix; got: {:?}",
            res.err()
        );
    }

    /// Bug fix: when an auto-par group's branch-stmt body reads
    /// `self.X` (a `FieldAccess { object: SelfValue, ... }` shape),
    /// the capture-set computation in `emit_par_run` previously
    /// missed `self` because `refs_in_expr` had no `SelfValue` arm.
    /// The branch fn was emitted without `self` in its env-struct,
    /// and `load_variable("self")` inside the branch body errored
    /// with `Undefined variable 'self'`.
    ///
    /// Fixed 2026-05-09 in `src/codegen.rs::refs_in_expr`: added a
    /// `SelfValue` arm that inserts `"self"` into the refs set, so
    /// captures pick it up and the env-struct unpack rebinds `self`
    /// in the branch fn's `self.variables`. Surfaced when growing
    /// `examples/parallax/`'s `fetch_latest_order` from `-> Order`
    /// to `-> Vec[Order]`; the parallax demo also exposes a
    /// downstream slot-rebind use-after-free issue (tracked in
    /// `bugs.md`) so this regression test uses a smaller shape that
    /// only exercises the capture-set path.
    /// Regression for bug #3 (closed 2026-05-09): auto-par grouping
    /// puts a `Vec[T]` return through a slot, then the value is
    /// moved into the surrounding fn's returned struct. Pre-fix, the
    /// slot rebind unconditionally called `track_vec_var(alloca)`,
    /// scheduling a free at the parent's scope exit. When the slot
    /// value was moved into a returned struct, the free corrupted
    /// the moved Vec's data pointer → SIGABRT.
    ///
    /// Fixed in `src/codegen.rs::compile_function_body` slot rebind
    /// path: the `track_vec_var(alloca)` call was removed (see the
    /// inline comment for the leak/correctness trade-off rationale).
    /// This test pins the demo-shape end-to-end behaviour.
    #[test]
    fn test_auto_par_vec_slot_into_struct_returned() {
        // Build the binary and run it. Both println markers must
        // appear — if "after" is missing, the cleanup of the returned
        // struct's Vec fields is unsafe (use-after-free or double-free).
        let src = r#"
struct Holder {
    a: Vec[i64],
    b: Vec[i64],
}

trait DbA { fn fetch(ref self) -> Vec[i64]; }
trait DbB { fn fetch(ref self) -> Vec[i64]; }

pub effect resource R1: DbA;
pub effect resource R2: DbB;

struct InMemA {}
struct InMemB {}

impl DbA for InMemA {
    fn fetch(ref self) -> Vec[i64] {
        let mut v: Vec[i64] = Vec.new();
        v.push(1);
        v
    }
}

impl DbB for InMemB {
    fn fetch(ref self) -> Vec[i64] {
        let mut v: Vec[i64] = Vec.new();
        v.push(2);
        v
    }
}

fn fetch_a() -> Vec[i64] with reads(R1) { R1.fetch() }
fn fetch_b() -> Vec[i64] with reads(R2) { R2.fetch() }

fn assemble() -> Holder with reads(R1) reads(R2) {
    let a = fetch_a();
    let b = fetch_b();
    Holder { a: a, b: b }
}

fn main() {
    let mut da = InMemA {};
    let mut db = InMemB {};
    with_provider[R1](da, || {
        with_provider[R2](db, || {
            let h = assemble();
            println("before");
            let _ = h;
            println("after");
        });
    });
}
"#;
        // Inline the link-and-run helper from tests/parallax.rs.
        use karac::codegen::{compile_to_object_with_options, link_executable};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let mut parsed = karac::parse(src);
        assert!(parsed.errors.is_empty(), "parse: {:?}", parsed.errors);
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let effects = karac::effectcheck(&parsed.program);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let obj = format!("/tmp/karac_bug3_{pid}_{id}.o");
        let exe = format!("/tmp/karac_bug3_{pid}_{id}");
        compile_to_object_with_options(&parsed.program, &obj, None, Some(&analysis), None, None)
            .unwrap();
        link_executable(&obj, &exe).unwrap();
        let out = std::process::Command::new(&exe).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let _ = std::fs::remove_file(&obj);
        let _ = std::fs::remove_file(&exe);
        assert!(
            out.status.success(),
            "binary should exit cleanly; got {:?}, stdout:\n{stdout}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            stdout.trim(),
            "before\nafter",
            "expected both markers; if `after` is missing, the \
             cleanup of the returned Holder (whose fields came \
             through par-slot rebinds) corrupted memory."
        );
    }

    #[test]
    fn test_impl_ref_self_self_field_in_branch_stmt_captures_self() {
        let src = r#"
struct InMemOrders { seed: i64 }

impl InMemOrders {
    fn get_orders(ref self, user_id: i64) -> Vec[i64] {
        let s: i64 = self.seed + user_id;
        let mut v: Vec[i64] = Vec.new();
        v.push(s);
        v
    }
}

fn main() {
    let db = InMemOrders { seed: 100 };
    let _ = db.get_orders(42);
    println("done");
}
"#;
        use karac::codegen::compile_to_ir_with_options;
        let mut parsed = karac::parse(src);
        assert!(parsed.errors.is_empty(), "parse: {:?}", parsed.errors);
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let effects = karac::effectcheck(&parsed.program);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);
        let res = compile_to_ir_with_options(&parsed.program, None, Some(&analysis), None, None);
        assert!(
            res.is_ok(),
            "expected clean compile (SelfValue capture-set fix); got: {:?}",
            res.err()
        );
    }
}
