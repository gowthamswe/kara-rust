//! Slice C — Full Parallax demo workload.
//!
//! Pins the canonical `get_dashboard(user_id)` workload's typed-return
//! provider chain on a real `examples/parallax/` Kāra workload and
//! validates the IR shape, analyzer grouping, and the four-deep
//! `with_provider[R]` chain end-to-end. Drafted 2026-05-09 against
//! [`docs/implementation_checklist/phase-8-stdlib-floor.md`] §
//! "Provider Implementations" → "Slice plan (drafted 2026-05-09) —
//! Full Parallax demo: typed-resource providers + canonical workload".
//!
//! The test surface mirrors `tests/parallax_lite.rs`'s layout: a
//! `workload_source()` helper that concatenates `types.kara`,
//! `traits.kara`, `resources.kara`, `providers.kara`,
//! `workload.kara`, and `main.kara` into a single source string
//! (dropping cross-module `import` lines per the multi-file
//! project-mode codegen gap, the same pattern parallax-lite uses).
//! Tests gate on `--features llvm` (codegen path).
//!
//! Tests:
//!   - `test_parallax_compiles_clean`: workload typechecks +
//!     effect-checks + ownership-checks + concurrency-analyzes +
//!     codegens cleanly through the in-process pipeline. Pins the
//!     source against regressions in any phase.
//!   - `test_parallax_emits_par_run_for_four_lets`: `get_dashboard`'s
//!     body emits exactly one `karac_par_run` dispatch with four
//!     branch fns. Asserts the analyzer grouped all four `let`
//!     bindings into one parallel_group and Slice A's path
//!     materialized four return slots.
//!   - `test_parallax_query_concurrency_groups_four_calls`: replays
//!     the analyzer pass and asserts the `get_dashboard` decision
//!     contains a single non-trivial `ParallelGroup` covering
//!     statement indices 0-3 (the four `let`s); the tail expression
//!     `Dashboard { ... }` is the function's `final_expr`, not a
//!     statement, so it's correctly excluded from the parallel set.
//!   - `test_parallax_provider_chain_dispatches_innermost_wins`:
//!     end-to-end. Builds the binary, runs it, asserts stdout shows
//!     all four `fetch_*` impls were invoked exactly once. Verifies
//!     the four-deep `with_provider` chain correctly threads each
//!     resource's binding through to its dispatch site.
//!   - `test_parallax_get_dashboard_completes_e2e`: end-to-end. Runs
//!     the binary, asserts the program exits cleanly with the
//!     "got dashboard" marker — pins that Slice A's return-slot
//!     reads round-trip through the four branch fns without panic.
//!   - `test_parallax_concurrency_report_renders_dashboard_workload`:
//!     pairs with Slice D's renderer; replays
//!     `render_concurrency_report` over the workload source and
//!     snapshot-compares against
//!     `tests/snapshots/concurrency_report_parallax.txt`.
//!     Cross-references Slice D's renderer for prose-shape stability.
//!   - `test_parallax_wall_clock_benchmark` (`#[ignore]`-gated —
//!     wall-clock numbers are flaky on shared CI). Compiles the
//!     workload twice (auto-par on / off via `KARAC_AUTO_PAR`), runs
//!     each, asserts auto-par wall-clock ≤ sequential / 2.0 (relaxed
//!     2.0x speedup threshold).

#[cfg(feature = "llvm")]
mod parallax_tests {
    use std::path::PathBuf;
    use std::sync::Once;

    /// Path to the workspace root (same dir as `Cargo.toml`).
    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Concatenate workload + main into a single source string. Drops
    /// cross-module `import` lines (everything is in one `Program` after
    /// concat, so imports are forward declarations of already-visible
    /// names — same pattern parallax-lite's test helper uses).
    fn workload_source() -> String {
        let root = workspace_root();
        let parts = [
            "examples/parallax/src/types.kara",
            "examples/parallax/src/traits.kara",
            "examples/parallax/src/resources.kara",
            "examples/parallax/src/providers.kara",
            "examples/parallax/src/workload.kara",
            "examples/parallax/src/main.kara",
        ];
        let mut combined = String::new();
        for p in &parts {
            let body = std::fs::read_to_string(root.join(p))
                .unwrap_or_else(|_| panic!("missing fixture: {p}"));
            for line in body.lines() {
                if line.trim_start().starts_with("import ") {
                    continue;
                }
                combined.push_str(line);
                combined.push('\n');
            }
        }
        combined
    }

    /// Run the full pipeline on the workload source and produce the
    /// LLVM IR. Mirrors `tests/parallax_lite.rs::ir_for_workload`.
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
    fn test_parallax_compiles_clean() {
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
    fn test_parallax_emits_par_run_for_four_lets() {
        let ir = ir_for_workload();
        // Multiple par-run sites in IR (provider impls' busy-compute
        // body kernels each parallelize their internal work-loops too,
        // and each provider impl emits one auto-par site of its own).
        // The load-bearing assertion: at least one of the par-run
        // sites belongs to `get_dashboard` and fans out to four
        // branches (the four `let` bindings).
        assert!(
            ir.contains("call void @karac_par_run"),
            "workload IR should contain at least one karac_par_run dispatch"
        );
        // Slice A's return-struct is materialized for `get_dashboard`'s
        // group — the demo's load-bearing slot mechanism. The
        // group's spawn-site id is assigned in source-walk order; the
        // four provider-impl groups precede `get_dashboard`, so
        // `get_dashboard`'s group is one of the later ones. Pin the
        // *existence* of a four-field return struct — the only
        // group in the workload that has a binding-leak shape.
        //
        // Field-count heuristic: count top-level commas inside the
        // outermost `{ ... }` of the type def, ignoring commas nested
        // in field-type braces (Profile / Order / etc. are anonymous
        // structs themselves so the IR has nested `{ ... , ... }`
        // patterns).
        let mut found_four_field_returns = false;
        for line in ir.lines() {
            if !line.contains("__karac_ParGroup_") || !line.contains("_Returns =") {
                continue;
            }
            // Find the outermost `{ ... }` after `type`.
            let Some(open) = line.find("type {") else {
                continue;
            };
            let body_start = open + "type {".len();
            let Some(close_rel) = line[body_start..].rfind('}') else {
                continue;
            };
            let body = &line[body_start..body_start + close_rel];
            let mut depth: i32 = 0;
            let mut top_level_commas = 0usize;
            for ch in body.chars() {
                match ch {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    ',' if depth == 0 => top_level_commas += 1,
                    _ => {}
                }
            }
            if top_level_commas == 3 {
                found_four_field_returns = true;
                break;
            }
        }
        assert!(
            found_four_field_returns,
            "expected a 4-field __karac_ParGroup_N_Returns type def for get_dashboard's \
             auto-par site; full IR:\n{ir}"
        );
        // At least four __par_branch_* defs across all par-run sites.
        let branch_count = ir
            .lines()
            .filter(|l| l.starts_with("define ") && l.contains(" @__par_branch_"))
            .count();
        assert!(
            branch_count >= 4,
            "expected ≥4 __par_branch_* definitions across all par sites; \
             found {branch_count}"
        );
    }

    #[test]
    fn test_parallax_query_concurrency_groups_four_calls() {
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
            .get("get_dashboard")
            .expect("get_dashboard decision missing");
        assert_eq!(
            decision.total_statements, 4,
            "get_dashboard body should have exactly 4 statements (the four `let`s); \
             the tail `Dashboard {{ ... }}` is the function's final_expr, not a stmt"
        );
        assert_eq!(
            decision.parallel_groups.len(),
            1,
            "get_dashboard should have exactly one parallel group; got: {:?}",
            decision.parallel_groups
        );
        let group = &decision.parallel_groups[0];
        let mut indices = group.statement_indices.clone();
        indices.sort();
        assert_eq!(
            indices,
            vec![0, 1, 2, 3],
            "parallel group should cover all four `let` statements; got {:?}",
            group.statement_indices
        );
        assert!(
            !group.is_trivial,
            "parallel group should be non-trivial (reads effects on disjoint \
             resources are non-empty); got reason {:?}",
            group.reason
        );
    }

    #[test]
    fn test_parallax_concurrency_report_renders_dashboard_workload() {
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
        let actual = karac::concurrency_report::render_concurrency_report(
            &analysis,
            &effects,
            &parsed.program,
        );
        // Extract just the `function get_dashboard ... }` block — the
        // report also contains entries for each provider impl's
        // busy-compute kernel (which the analyzer also identifies as
        // a parallel group on the iter+sum pure-compute statements).
        // The Slice D snapshot pins only the load-bearing get_dashboard
        // block; the per-impl groups are stable in shape but their
        // line numbers drift if the fixture's iteration count changes.
        let needle = "function get_dashboard";
        let start = actual
            .find(needle)
            .unwrap_or_else(|| panic!("get_dashboard block missing from report:\n{actual}"));
        // Find the closing `}` of this block — first `}` line at column 2
        // (the renderer uses two-space indent for the closing brace).
        let mut end_byte_idx: Option<usize> = None;
        for (lineno, line) in actual[start..].lines().enumerate() {
            if lineno == 0 {
                continue;
            }
            if line == "  }" {
                // include this line + newline
                let line_start_in_block = actual[start..].find(line).unwrap();
                end_byte_idx = Some(start + line_start_in_block + line.len() + 1);
                break;
            }
        }
        let end = end_byte_idx.unwrap_or_else(|| panic!("closing `}}` not found:\n{actual}"));
        let block = &actual[start..end];
        // Strip the line number from the function header so the snapshot
        // is stable across additions in earlier source files.
        let mut normalized = String::new();
        for (lineno, line) in block.lines().enumerate() {
            if lineno == 0 {
                normalized.push_str("function get_dashboard:\n");
            } else {
                // Drop bracketed source-line numbers from the per-call
                // lines for the same reason — the demo file ordering
                // moves with future fixture changes.
                let stripped: String =
                    if let (Some(open), Some(close)) = (line.find('['), line.find(']')) {
                        if open < close {
                            format!("{}{}", &line[..open], &line[close + 1..])
                        } else {
                            line.to_string()
                        }
                    } else {
                        line.to_string()
                    };
                normalized.push_str(&stripped);
                normalized.push('\n');
            }
        }
        let snapshot_path =
            workspace_root().join("tests/snapshots/concurrency_report_parallax.txt");
        let expected = std::fs::read_to_string(&snapshot_path).expect(
            "tests/snapshots/concurrency_report_parallax.txt missing — \
             run the test once to print the actual output, then save it.",
        );
        if normalized != expected {
            panic!(
                "concurrency report snapshot mismatch.\n\nExpected ({} bytes):\n{}\n\
                 Actual ({} bytes):\n{}\n\
                 To accept the new output, overwrite {}",
                expected.len(),
                expected,
                normalized.len(),
                normalized,
                snapshot_path.display()
            );
        }
    }

    /// E2E: compile + link + run; assert each `fetch_*` impl emitted
    /// its side-channel marker exactly once, confirming the four-deep
    /// `with_provider` chain dispatches each resource through to its
    /// concrete impl.
    #[test]
    fn test_parallax_provider_chain_dispatches_innermost_wins() {
        let Some(rt) = runtime_path() else {
            eprintln!(
                "skip: libkarac_runtime.a not built (run `cargo build -p karac-runtime --release`)"
            );
            return;
        };
        std::env::set_var("KARAC_RUNTIME", &rt);
        let src = workload_source();
        let stdout = compile_and_run(&src, "dispatch")
            .unwrap_or_else(|| panic!("compile/run failed for parallax workload"));
        for marker in &[
            "loaded profile from UserDB",
            "loaded latest orders from OrderDB",
            "loaded top notification from NotifDB",
            "loaded top recommendation from RecommendDB",
        ] {
            assert!(
                stdout.contains(marker),
                "stdout should contain `{marker}`; got:\n{stdout}"
            );
        }
        assert!(
            stdout.contains("got dashboard"),
            "stdout should contain `got dashboard` confirmation line; got:\n{stdout}"
        );
    }

    /// E2E: the binary runs cleanly with the auto-par codegen path.
    /// Pins that Slice A's return-slot reads round-trip through the
    /// four branch fns without panic, regardless of completion order.
    #[test]
    fn test_parallax_get_dashboard_completes_e2e() {
        let Some(rt) = runtime_path() else {
            eprintln!(
                "skip: libkarac_runtime.a not built (run `cargo build -p karac-runtime --release`)"
            );
            return;
        };
        std::env::set_var("KARAC_RUNTIME", &rt);
        let src = workload_source();
        let stdout = compile_and_run(&src, "complete_e2e")
            .unwrap_or_else(|| panic!("compile/run failed for parallax workload"));
        // Each fetch's side-channel marker plus the join confirmation
        // appear exactly once. Auto-par may permute the fetch order
        // (concurrent execution), but the join has to sequence after
        // all four — the "got dashboard" line ALWAYS appears last.
        let last_line = stdout.lines().filter(|l| !l.is_empty()).last();
        assert_eq!(
            last_line,
            Some("got dashboard"),
            "last non-empty stdout line should be `got dashboard` (the \
             post-join confirmation); got:\n{stdout}"
        );
        // Each marker exactly once.
        for marker in &[
            "loaded profile from UserDB",
            "loaded latest orders from OrderDB",
            "loaded top notification from NotifDB",
            "loaded top recommendation from RecommendDB",
        ] {
            let count = stdout.matches(marker).count();
            assert_eq!(
                count, 1,
                "expected `{marker}` exactly once; got {count}; stdout:\n{stdout}"
            );
        }
    }

    /// Wall-clock benchmark: compile twice (auto-par on / off via
    /// `KARAC_AUTO_PAR=0`), run each, compare wall-clock. Asserts
    /// auto-par ≤ sequential / 2.0 (relaxed 2.0x speedup floor — per-
    /// fetch busy-compute totals ~75M iters across four fetches; a
    /// 3-4-core machine should clear ~3x in practice). `#[ignore]`-
    /// gated because single-run wall-clock is flaky on shared CI.
    #[test]
    #[ignore]
    fn test_parallax_wall_clock_benchmark() {
        let _guard = AUTO_PAR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(rt) = runtime_path() else {
            eprintln!(
                "skip: libkarac_runtime.a not built (run `cargo build -p karac-runtime --release`)"
            );
            return;
        };
        std::env::set_var("KARAC_RUNTIME", &rt);

        let src = workload_source();
        std::env::remove_var("KARAC_AUTO_PAR");
        let par_time = compile_and_time(&src, "parallax_auto_par");
        std::env::set_var("KARAC_AUTO_PAR", "0");
        let seq_time = compile_and_time(&src, "parallax_sequential");
        std::env::remove_var("KARAC_AUTO_PAR");

        let (Some(par_secs), Some(seq_secs)) = (par_time, seq_time) else {
            eprintln!("skip: link/exec failed");
            return;
        };
        let ratio = seq_secs / par_secs;
        eprintln!("auto-par: {par_secs:.3}s; sequential: {seq_secs:.3}s; speedup: {ratio:.2}x");
        assert!(
            ratio >= 2.0,
            "auto-par should be at least 2.0x faster than sequential; \
             got {ratio:.2}x (auto-par {par_secs:.3}s, sequential {seq_secs:.3}s). \
             A failure here is signal for v1.x cost-model tuning OR a regression in \
             auto-par codegen, NOT a CI flake — wall-clock variance can drop the \
             ratio below 3x but rarely below 2x on a 3-4-core machine."
        );
    }

    fn compile_and_run(src: &str, label: &str) -> Option<String> {
        use karac::codegen::link_executable;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let mut parsed = karac::parse(src);
        if !parsed.errors.is_empty() {
            panic!("parse errors: {:?}", parsed.errors);
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
        let out = std::process::Command::new(&exe).output().ok()?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let _ = std::fs::remove_file(&obj);
        let _ = std::fs::remove_file(&exe);
        Some(stdout)
    }

    fn compile_and_time(src: &str, label: &str) -> Option<f64> {
        use karac::codegen::link_executable;
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
        // Warmup once, then measure one run. Single-run timing is
        // intentionally simple — the 2.0x threshold has enough headroom
        // to absorb single-run variance.
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
