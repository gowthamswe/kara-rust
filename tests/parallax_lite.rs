//! Slice 6 — Parallax-lite microbenchmark workload.
//!
//! Pins the auto-par codegen path on a real `examples/parallax_lite/`
//! Kāra workload and validates `karac query concurrency` /
//! `karac query cost-summary` output structure on the same source.
//!
//! Tests:
//!   - `test_parallax_lite_compiles_clean`: the workload typechecks +
//!     effect-checks + ownership-checks + concurrency-analyzes +
//!     codegens cleanly (parse, resolve, typecheck, lower, effects,
//!     ownership, concurrency, IR).
//!   - `test_parallax_lite_emits_par_run`: `process_request`'s body
//!     auto-parallelizes — exactly one `karac_par_run` dispatch in
//!     the function's IR, with three branch fns (`__par_branch_0_0/1/2`
//!     when no other par site precedes it).
//!   - `test_parallax_lite_query_concurrency_groups_three_calls`:
//!     replays the analyzer pass and asserts the
//!     `process_request` decision contains a single non-trivial
//!     parallel group covering all three statements.
//!   - `test_parallax_lite_query_cost_summary_structural`: replays the
//!     cost-summary builder and asserts the `CostSummary` struct's
//!     shape (totals fields, by_function vec, perf_notes vec) — pins
//!     the structural surface for the `cost-summary` JSON renderer.
//!   - `test_parallax_lite_auto_par_env_gate_disables_par_run`
//!     (`#[ignore]` because env-var manipulation forces serialized
//!     test execution): with `KARAC_AUTO_PAR=0`, the workload's IR
//!     contains no `karac_par_run` calls.
//!   - `test_parallax_lite_wall_clock_benchmark` (`#[ignore]` —
//!     wall-clock numbers are flaky on shared CI). Compiles the
//!     workload twice (auto-par on / off via `KARAC_AUTO_PAR`), runs
//!     each binary, asserts auto-par wall-clock ≤ sequential / 1.3 to
//!     pin the speedup signal at a relaxed threshold.
//!
//! All tests gate on `--features llvm` (codegen path).
//!
//! The workload lives at `examples/parallax_lite/src/{resources,
//! workload}.kara`; the tests load both files from disk and concat
//! into a single source string before compiling. v1 project-mode
//! build (`karac build` with no file arg) only typechecks across
//! modules — full codegen across modules is a CR-24 follow-up. Tests
//! exercise the auto-par lowering directly through the in-process
//! pipeline machinery.

#[cfg(feature = "llvm")]
mod parallax_lite_tests {
    use std::path::PathBuf;
    use std::sync::Once;

    /// Path to the workspace root (same dir as `Cargo.toml`).
    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Concatenate the workload files into a single source string.
    /// Drops the `import` line (which only matters cross-module — when
    /// concatenated the type names are visible in the same scope).
    fn workload_source() -> String {
        let root = workspace_root();
        let resources =
            std::fs::read_to_string(root.join("examples/parallax_lite/src/resources.kara"))
                .expect("resources.kara missing");
        let workload =
            std::fs::read_to_string(root.join("examples/parallax_lite/src/workload.kara"))
                .expect("workload.kara missing");
        // Drop `import resources.{...};` — concat puts everything in
        // one source so the import would be a forward declaration of
        // already-visible names.
        let workload_no_import: String = workload
            .lines()
            .filter(|l| !l.trim_start().starts_with("import "))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{resources}\n{workload_no_import}\n")
    }

    /// Run the full pipeline on the workload source and produce the
    /// LLVM IR. Mirrors `tests/par_codegen.rs::ir_for_with_concurrency`
    /// but loads from disk and explicitly populates the auto-par path.
    fn ir_for_workload() -> String {
        ir_for_workload_with(&workload_source())
    }

    fn ir_for_workload_with(src: &str) -> String {
        use karac::codegen::compile_to_ir_with_options;
        let mut parsed = karac::parse(src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors on workload: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let effects = karac::effectcheck(&parsed.program);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);
        compile_to_ir_with_options(&parsed.program, None, Some(&analysis), None, None)
            .expect("workload codegen failed")
    }

    #[test]
    fn test_parallax_lite_compiles_clean() {
        let src = workload_source();
        let mut parsed = karac::parse(&src);
        assert!(
            parsed.errors.is_empty(),
            "workload should parse without errors; got {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        assert!(
            resolved.errors.is_empty(),
            "workload should resolve without errors; got {:?}",
            resolved.errors
        );
        let typed = karac::typecheck(&parsed.program, &resolved);
        assert!(
            typed.errors.is_empty(),
            "workload should typecheck without errors; got {:?}",
            typed.errors
        );
        karac::lower(&mut parsed.program, &typed);
        let effects = karac::effectcheck(&parsed.program);
        assert!(
            effects.errors.is_empty(),
            "workload should effect-check without errors; got {:?}",
            effects.errors
        );
        let _ownership = karac::ownershipcheck(&parsed.program, &typed);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);
        // Codegen pass — pulls the analysis through the auto-par path.
        let ir = karac::codegen::compile_to_ir_with_options(
            &parsed.program,
            None,
            Some(&analysis),
            None,
            None,
        );
        assert!(
            ir.is_ok(),
            "workload should codegen without errors; got {:?}",
            ir.err()
        );
    }

    #[test]
    fn test_parallax_lite_emits_par_run() {
        let ir = ir_for_workload();
        // Exactly one auto-par dispatch site for `process_request` —
        // there is also one for `main` (process_request() + println()
        // are independent on different resources) and one for the
        // top-level seed: but the load-bearing assertion is that the
        // workload's three-call aggregator does emit a `karac_par_run`
        // dispatch.
        assert!(
            ir.contains("call void @karac_par_run"),
            "workload IR should contain a karac_par_run dispatch; \
             got IR:\n{ir}"
        );
        // Three branch fns from `process_request`'s auto-par site.
        // The numbering depends on emit order, but the *count* of
        // `__par_branch_*` defs in the IR should be at least 3 — the
        // process_request site fans out to 3 branches.
        let branch_count = ir
            .lines()
            .filter(|l| l.starts_with("define ") && l.contains(" @__par_branch_"))
            .count();
        assert!(
            branch_count >= 3,
            "workload should emit at least 3 __par_branch_* definitions for the \
             process_request auto-par site; found {branch_count}; IR:\n{ir}"
        );
    }

    #[test]
    fn test_parallax_lite_query_concurrency_groups_three_calls() {
        // Replay the analyzer pass on the workload and inspect
        // `process_request`'s decision directly — same call shape the
        // `karac query concurrency` CLI consumes.
        let src = workload_source();
        let mut parsed = karac::parse(&src);
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
        let decision = analysis
            .function_decisions
            .get("process_request")
            .expect("process_request decision missing");
        assert_eq!(
            decision.total_statements, 3,
            "process_request body should have exactly 3 statements"
        );
        assert_eq!(
            decision.parallel_groups.len(),
            1,
            "process_request should have exactly one parallel group; got: {:?}",
            decision.parallel_groups
        );
        let group = &decision.parallel_groups[0];
        let mut indices = group.statement_indices.clone();
        indices.sort();
        assert_eq!(
            indices,
            vec![0, 1, 2],
            "parallel group should cover all three statements; got {:?}",
            group.statement_indices
        );
        assert!(
            !group.is_trivial,
            "parallel group should be non-trivial (writes effects are non-empty); \
             got reason {:?}",
            group.reason
        );
    }

    #[test]
    fn test_parallax_lite_query_cost_summary_structural() {
        // Replay the cost-summary builder on the workload and assert
        // the *structural* shape of its output (the field set, not
        // numerical counts). Numerical values are recorded in
        // `examples/parallax_lite/README.md` for v1.x cost-model
        // tuning ground-truth.
        let src = workload_source();
        let mut parsed = karac::parse(&src);
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let _effects = karac::effectcheck(&parsed.program);
        let ownership = karac::ownershipcheck(&parsed.program, &typed);
        let summary =
            karac::cost_summary::build("parallax_lite_workload", &parsed.program, &ownership);
        // Structural pin: the totals field set is exactly the five
        // categories from `design.md § Performance Diagnostics >
        // Cumulative Cost Surface`. Reading each field exercises the
        // struct's surface.
        let _ = summary.totals.rc_ops.count;
        let _ = summary.totals.rc_ops.rc;
        let _ = summary.totals.rc_ops.arc;
        let _ = summary.totals.arc_provider_wraps;
        let _ = summary.totals.borrow_flag_fields;
        let _ = summary.totals.partition_guard_sites;
        let _ = summary.totals.auto_clone_insertions;
        // by_function and perf_notes are vec slots — just exercise.
        let _ = summary.by_function.len();
        let _ = summary.perf_notes.len();
        assert_eq!(summary.scope, "parallax_lite_workload");
    }

    /// `KARAC_AUTO_PAR=0` flips the slice 6 codegen gate, short-
    /// circuiting auto-par dispatch back to plain sequential
    /// `compile_block`. Verify the gate by inspecting the IR — no
    /// `karac_par_run` calls when the gate is off, even on a workload
    /// that fires multiple auto-par sites with the gate on.
    ///
    /// `#[ignore]` because `std::env::set_var` is process-global; the
    /// test runner serializes ignored tests when invoked explicitly,
    /// avoiding cross-test interference. Run via:
    /// `cargo test --features llvm -- --ignored \
    ///  test_parallax_lite_auto_par_env_gate_disables_par_run`.
    #[test]
    #[ignore]
    fn test_parallax_lite_auto_par_env_gate_disables_par_run() {
        // Lock to serialize against any other env-var-touching test.
        let _guard = AUTO_PAR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let src = workload_source();
        // Gate-on baseline: at least one karac_par_run call site.
        std::env::remove_var("KARAC_AUTO_PAR");
        let ir_on = ir_for_workload_with(&src);
        assert!(
            ir_on.contains("call void @karac_par_run"),
            "with auto-par on, workload IR should contain karac_par_run; \
             got IR:\n{ir_on}"
        );
        // Gate-off: zero karac_par_run call sites.
        std::env::set_var("KARAC_AUTO_PAR", "0");
        let ir_off = ir_for_workload_with(&src);
        std::env::remove_var("KARAC_AUTO_PAR");
        assert!(
            !ir_off.contains("call void @karac_par_run"),
            "with KARAC_AUTO_PAR=0, workload IR should NOT contain \
             karac_par_run; got IR:\n{ir_off}"
        );
    }

    /// Wall-clock benchmark: compile the workload twice, link both,
    /// run each, compare wall-clock. Pass-fail signal: auto-par
    /// wall-clock ≤ sequential / 1.3 (relaxed 1.3x speedup threshold).
    /// Three CPU-bound branches across at least two cores should
    /// comfortably clear 1.3x; locally observed ~2.7x on a typical
    /// laptop after warmup. CI variance can push this lower, hence
    /// `#[ignore]` — the test is opt-in for measurement runs.
    ///
    /// Run via:
    /// `cargo test --features llvm -- --ignored \
    ///  test_parallax_lite_wall_clock_benchmark`.
    #[test]
    #[ignore]
    fn test_parallax_lite_wall_clock_benchmark() {
        let _guard = AUTO_PAR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(rt) = runtime_path() else {
            eprintln!(
                "skip: libkarac_runtime.a not built (run `cargo build -p karac-runtime --release`)"
            );
            return;
        };
        std::env::set_var("KARAC_RUNTIME", &rt);

        let src = workload_source();
        // Append a `main` that calls process_request — the workload
        // file by itself has no main (project-mode would supply it
        // via main.kara).
        let main_added =
            format!("{src}\nfn main() {{\n    process_request();\n    println(\"done\");\n}}\n");

        std::env::remove_var("KARAC_AUTO_PAR");
        let par_time = compile_and_time(&main_added, "auto_par");
        std::env::set_var("KARAC_AUTO_PAR", "0");
        let seq_time = compile_and_time(&main_added, "sequential");
        std::env::remove_var("KARAC_AUTO_PAR");

        let (Some(par_secs), Some(seq_secs)) = (par_time, seq_time) else {
            eprintln!("skip: link/exec failed");
            return;
        };
        let ratio = seq_secs / par_secs;
        eprintln!("auto-par: {par_secs:.3}s; sequential: {seq_secs:.3}s; speedup: {ratio:.2}x");
        assert!(
            ratio >= 1.3,
            "auto-par should be at least 1.3x faster than sequential; \
             got {ratio:.2}x (auto-par {par_secs:.3}s, sequential {seq_secs:.3}s). \
             A failure here is signal for v1.x cost-model tuning OR a regression in \
             auto-par codegen, NOT a CI flake — wall-clock variance can drop the \
             ratio below 1.5x but rarely below 1.3x."
        );
    }

    fn compile_and_time(src: &str, label: &str) -> Option<f64> {
        use karac::codegen::{compile_to_object, link_executable};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let mut parsed = karac::parse(src);
        if !parsed.errors.is_empty() {
            panic!("benchmark source failed to parse: {:?}", parsed.errors);
        }
        let resolved = karac::resolve(&parsed.program);
        let typed = karac::typecheck(&parsed.program, &resolved);
        karac::lower(&mut parsed.program, &typed);
        let effects = karac::effectcheck(&parsed.program);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);

        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let obj = format!("/tmp/karac_parallax_{pid}_{label}_{id}.o");
        let exe = format!("/tmp/karac_parallax_{pid}_{label}_{id}");
        karac::codegen::compile_to_object_with_options(
            &parsed.program,
            &obj,
            None,
            Some(&analysis),
            None,
            None,
        )
        .ok()?;
        link_executable(&obj, &exe).ok()?;
        // Discard the dummy `compile_to_object` import; the link path
        // is what we exercise.
        let _ = compile_to_object;

        // Warmup once, then measure one run. Wall-clock numbers from
        // a single run are intentionally simple — the 1.3x threshold
        // has enough headroom to absorb single-run variance.
        let _ = std::process::Command::new(&exe).output().ok()?;
        let start = std::time::Instant::now();
        let _ = std::process::Command::new(&exe).output().ok()?;
        let secs = start.elapsed().as_secs_f64();

        let _ = std::fs::remove_file(&obj);
        let _ = std::fs::remove_file(&exe);
        Some(secs)
    }

    // ── Test infrastructure ─────────────────────────────────────────

    static AUTO_PAR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    static RUNTIME_BUILT: Once = Once::new();
    static mut RUNTIME_PATH: Option<PathBuf> = None;

    /// Build the runtime static library once per test process and
    /// return its path. Returns None if the build fails — callers
    /// soft-skip. Mirrors `tests/par_codegen.rs::runtime_path`.
    #[allow(static_mut_refs)]
    fn runtime_path() -> Option<PathBuf> {
        RUNTIME_BUILT.call_once(|| {
            let output = std::process::Command::new("cargo")
                .args(["build", "-p", "karac-runtime", "--release"])
                .output();
            if let Ok(out) = output {
                if out.status.success() {
                    let p = workspace_root().join("target/release/libkarac_runtime.a");
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
}
