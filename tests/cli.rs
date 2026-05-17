//! CLI and diagnostic output integration tests.
//!
//! These are golden-file snapshot tests that freeze the diagnostic output format.
//! If a test fails, either the output format has regressed (fix the code) or
//! the format intentionally changed (update the expected output).

use std::process::Command;

fn karac_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_karac"))
}

// ── Help & Version ──────────────────────────────────────────────

#[test]
fn test_help_output() {
    let out = karac_bin().arg("help").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac - The Kara language compiler"));
    assert!(stdout.contains("COMMANDS:"));
    assert!(stdout.contains("run"));
    assert!(stdout.contains("check"));
    assert!(stdout.contains("build"));
    assert!(stdout.contains("query"));
    assert!(stdout.contains("fmt"));
}

#[test]
fn test_version_output() {
    let out = karac_bin().arg("version").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac 0.1.0"));
}

#[test]
fn test_no_args_shows_help() {
    let out = karac_bin().output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("COMMANDS:"));
}

// ── Subcommand-scoped --help ────────────────────────────────────

#[test]
fn test_subcommand_help_init() {
    for flag in ["--help", "-h"] {
        let out = karac_bin().args(["init", flag]).output().unwrap();
        assert!(out.status.success(), "`karac init {flag}` should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("karac init"));
        assert!(stdout.contains("--bin"));
        assert!(stdout.contains("--lib"));
        assert!(stdout.contains("--force"));
    }
}

#[test]
fn test_subcommand_help_run() {
    let out = karac_bin().args(["run", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac run"));
    assert!(stdout.contains("--sequential"));
}

#[test]
fn test_subcommand_help_check() {
    let out = karac_bin().args(["check", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac check"));
    assert!(stdout.contains("--output=json"));
}

#[test]
fn test_subcommand_help_build() {
    let out = karac_bin().args(["build", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac build"));
    assert!(stdout.contains("kara.toml"));
}

#[test]
fn test_subcommand_help_query() {
    let out = karac_bin().args(["query", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac query"));
    assert!(stdout.contains("effects"));
    assert!(stdout.contains("ownership"));
    assert!(stdout.contains("concurrency"));
}

#[test]
fn test_subcommand_help_fmt() {
    let out = karac_bin().args(["fmt", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac fmt"));
}

#[test]
fn test_subcommand_help_does_not_scaffold() {
    // Guard against regressions where `karac init --help` would fall through
    // and actually run the scaffolder in the CWD.
    let tmp = std::env::temp_dir().join(format!(
        "karac-cli-init-help-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let out = karac_bin()
        .args(["init", "--help"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!tmp.join("kara.toml").exists());
    assert!(!tmp.join("src").exists());
    let _ = std::fs::remove_dir_all(&tmp);
}

// ── Run ─────────────────────────────────────────────────────────

#[test]
fn test_run_clean_program() {
    let out = karac_bin()
        .args(["run", "tests/snapshots/clean.kara"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("x = 42"));
}

#[test]
fn test_run_bare_file_shorthand() {
    let out = karac_bin()
        .arg("tests/snapshots/clean.kara")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("x = 42"));
}

#[test]
fn test_run_resolve_error_exits_nonzero() {
    let out = karac_bin()
        .args(["run", "tests/snapshots/resolve_error.kara"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error[resolve]"));
    assert!(stderr.contains("undefined name"));
}

// ── Check ───────────────────────────────────────────────────────

#[test]
fn test_check_clean_program() {
    let out = karac_bin()
        .args(["check", "tests/snapshots/clean.kara"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("All checks passed"));
}

#[test]
fn test_check_parse_error() {
    let out = karac_bin()
        .args(["check", "tests/snapshots/parse_error.kara"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error[parse]"));
    assert!(stderr.contains("2:13")); // line:column
}

#[test]
fn test_check_type_error() {
    let out = karac_bin()
        .args(["check", "tests/snapshots/type_error.kara"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error[typecheck]"));
    assert!(stderr.contains("expected 2 argument(s), found 3"));
}

#[test]
fn test_check_provider_escape_error() {
    let out = karac_bin()
        .args(["check", "tests/snapshots/provider_escape_error.kara"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error[provider_escape]"),
        "expected provider_escape error, got: {}",
        stderr
    );
    assert!(
        stderr.contains("Clock"),
        "expected resource name Clock in message, got: {}",
        stderr
    );
    assert!(
        stderr.contains("escapes its provider scope"),
        "expected escape message, got: {}",
        stderr
    );
}

#[test]
fn test_check_provider_escape_error_json() {
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/provider_escape_error.kara",
            "--output=json",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"phase\":\"provider_escape\""),
        "missing phase in JSON output: {}",
        stdout
    );
    assert!(
        stdout.contains("\"code\":\"E0600\""),
        "missing code E0600 in JSON output: {}",
        stdout
    );
    assert!(
        stdout.contains("Clock"),
        "missing resource name in JSON output: {}",
        stdout
    );
}

#[test]
fn test_run_rejects_provider_escape() {
    // `karac run` must abort on escape errors — the spec's "cannot
    // escape" rule breaks test isolation if we silently run.
    let out = karac_bin()
        .args(["run", "tests/snapshots/provider_escape_error.kara"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error[provider_escape]"),
        "expected provider_escape error, got: {}",
        stderr
    );
}

// ── JSON Output Snapshots ───────────────────────────────────────

#[test]
fn test_json_clean_program() {
    let out = karac_bin()
        .args(["check", "tests/snapshots/clean.kara", "--output=json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    // Verify structure
    assert!(json.starts_with('{'));
    assert!(json.contains("\"program_effects\""));
    assert!(json.contains("\"diagnostics\":[]"));
}

#[test]
fn test_json_parse_error_snapshot() {
    let out = karac_bin()
        .args(["check", "tests/snapshots/parse_error.kara", "--output=json"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    // Verify diagnostic fields per spec
    assert!(json.contains("\"severity\":\"error\""));
    assert!(json.contains("\"primary\":true"));
    assert!(json.contains("\"phase\":\"parse\""));
    assert!(json.contains("\"code\":\"E0001\""));
    assert!(json.contains("\"line\":2"));
    assert!(json.contains("\"column\":13"));
    assert!(json.contains("\"message\":"));
}

#[test]
fn test_json_resolve_error_snapshot() {
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/resolve_error.kara",
            "--output=json",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    assert!(json.contains("\"phase\":\"resolve\""));
    assert!(json.contains("\"code\":\"E0100\""));
    assert!(json.contains("undefined name"));
}

#[test]
fn test_json_type_error_snapshot() {
    let out = karac_bin()
        .args(["check", "tests/snapshots/type_error.kara", "--output=json"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    assert!(json.contains("\"phase\":\"typecheck\""));
    assert!(json.contains("\"code\":\"E0202\""));
    assert!(json.contains("expected 2 argument(s), found 3"));
}

#[test]
fn test_json_multiple_errors_snapshot() {
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/multiple_errors.kara",
            "--output=json",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    // Should have 3 diagnostics (d1, d2, d3)
    assert!(json.contains("\"id\":\"d1\""));
    assert!(json.contains("\"id\":\"d2\""));
    assert!(json.contains("\"id\":\"d3\""));
    // All are resolve errors
    assert!(json.matches("\"phase\":\"resolve\"").count() == 3);
}

#[test]
fn test_json_suggestion_in_hints() {
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/multiple_errors.kara",
            "--output=json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Suggestions should be present as hints
    assert!(stdout.contains("\"hints\":[{\"description\":"));
}

// ── JSONL Output Snapshots ──────────────────────────────────────

#[test]
fn test_jsonl_clean_program() {
    let out = karac_bin()
        .args(["check", "tests/snapshots/clean.kara", "--output=jsonl"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();

    // First line: build_start
    assert!(lines[0].contains("\"type\":\"build_start\""));

    // Should have phase_start/phase_complete pairs for each phase
    let phase_starts: Vec<&&str> = lines
        .iter()
        .filter(|l| l.contains("\"phase_start\""))
        .collect();
    assert!(phase_starts.len() >= 5); // lex, parse, resolve, typecheck, effect, ownership

    // Last line: build_complete
    let last = lines.last().unwrap();
    assert!(last.contains("\"type\":\"build_complete\""));
    assert!(last.contains("\"success\":true"));
    assert!(last.contains("\"total_errors\":0"));
}

#[test]
fn test_jsonl_parse_error_skips_phases() {
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/parse_error.kara",
            "--output=jsonl",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Should have phase_skipped for resolve, typecheck, effect, ownership
    assert!(stdout.contains("\"type\":\"phase_skipped\""));
    let skipped_count = stdout.matches("\"phase_skipped\"").count();
    assert_eq!(skipped_count, 4);

    // build_complete should show failure
    assert!(stdout.contains("\"success\":false"));
}

#[test]
fn test_jsonl_resolve_error_skips_later_phases() {
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/resolve_error.kara",
            "--output=jsonl",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    // resolve should complete (not be skipped)
    assert!(stdout.contains("\"phase\":\"resolve\""));
    // typecheck, effect, ownership should be skipped
    let skipped_count = stdout.matches("\"phase_skipped\"").count();
    assert_eq!(skipped_count, 3);
}

// ── Query ───────────────────────────────────────────────────────

#[test]
fn test_query_effects_pure_function() {
    let out = karac_bin()
        .args(["query", "effects", "tests/snapshots/type_error.kara.add"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"function\":\"add\""));
    assert!(stdout.contains("\"inferred_effects\":[]"));
    assert!(stdout.contains("\"declared_effects\":null"));
}

#[test]
fn test_query_ownership() {
    let out = karac_bin()
        .args(["query", "ownership", "tests/snapshots/type_error.kara.add"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"function\":\"add\""));
    assert!(stdout.contains("\"parameters\":["));
    assert!(stdout.contains("\"name\":\"a\""));
    assert!(stdout.contains("\"name\":\"b\""));
    // Round 12.25: closures array is always present (empty for
    // functions without closures).
    assert!(stdout.contains("\"closures\":[]"));
}

#[test]
fn test_query_ownership_surfaces_closures() {
    // Round 12.25: closures created inside the queried function are
    // listed with their inferred param modes and captures. The
    // snapshot has one consume-param closure (no capture) and one
    // bare-capture closure (no param).
    let out = karac_bin()
        .args([
            "query",
            "ownership",
            "tests/snapshots/closure_query.kara.main",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"function\":\"main\""));
    // First closure: |x: Data| take(x) — consume-param, no capture.
    assert!(
        stdout.contains("\"name\":\"x\",\"mode\":\"own\""),
        "expected consume-param entry; got: {stdout}"
    );
    // Second closure: || d.v — no params, ref-capture of d.
    assert!(
        stdout.contains("\"name\":\"d\",\"mode\":\"ref\""),
        "expected ref-capture entry; got: {stdout}"
    );
    // Each closure's source location is surfaced.
    assert!(
        stdout.contains("\"line\":7"),
        "expected line 7 for first closure; got: {stdout}"
    );
    assert!(
        stdout.contains("\"line\":8"),
        "expected line 8 for second closure; got: {stdout}"
    );
}

#[test]
fn test_query_ownership_closure_array_filtered_per_function() {
    // Querying a function with no closures must still emit the
    // closures array as `[]` even though the program contains
    // closures elsewhere (in `main`). Pins the per-function filter.
    let out = karac_bin()
        .args([
            "query",
            "ownership",
            "tests/snapshots/closure_query.kara.take",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"function\":\"take\""));
    assert!(
        stdout.contains("\"closures\":[]"),
        "take has no closures; expected empty array; got: {stdout}"
    );
}

// ── Formatter ───────────────────────────────────────────────────

#[test]
fn test_fmt_idempotent() {
    let out1 = karac_bin()
        .args(["fmt", "tests/snapshots/clean.kara"])
        .output()
        .unwrap();
    assert!(out1.status.success());
    let formatted = String::from_utf8_lossy(&out1.stdout);
    // Should contain the key constructs
    assert!(formatted.contains("fn main()"));
    assert!(formatted.contains("let x = 42"));
}

// ── Error Cases ─────────────────────────────────────────────────

#[test]
fn test_unknown_command() {
    let out = karac_bin().arg("foobar").output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown command"));
}

#[test]
fn test_missing_file() {
    let out = karac_bin()
        .args(["run", "nonexistent.kara"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot read"));
}

#[test]
fn test_unknown_output_mode() {
    let out = karac_bin()
        .args(["check", "tests/snapshots/clean.kara", "--output=xml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown output mode"));
}

// ── Project-mode build (CR-24 slices 2 + 3) ─────────────────────

fn scratch_project(tag: &str) -> std::path::PathBuf {
    let tmp = std::env::temp_dir().join(format!(
        "karac-cli-build-project-{}-{}-{}",
        std::process::id(),
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    tmp
}

fn write(path: &std::path::Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

#[test]
fn test_build_project_lists_discovered_modules() {
    let tmp = scratch_project("lists-modules");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(&tmp.join("src/greet.kara"), "pub fn greet() {}\n");
    write(&tmp.join("src/db/connection.kara"), "pub fn open() {}\n");

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        out.status.success(),
        "build failed: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("project: demo"));
    assert!(stdout.contains("entry:   bin"));
    assert!(stdout.contains("modules: 3"));
    assert!(stdout.contains("greet"));
    assert!(stdout.contains("db.connection"));
    assert!(stdout.contains("<crate root>"));
}

#[test]
fn test_build_project_mixed_entry_files_fails() {
    let tmp = scratch_project("mixed-entry");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(&tmp.join("src/lib.kara"), "pub fn add() {}\n");

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot contain both"));
}

#[test]
fn test_build_project_rejects_mod_kara() {
    let tmp = scratch_project("mod-kara");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(&tmp.join("src/db/mod.kara"), "\n");

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("mod.kara"));
}

#[test]
fn test_build_project_missing_src_dir_fails() {
    let tmp = scratch_project("no-src");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    // No src/ directory.

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no `src/` directory"));
}

#[test]
fn test_build_project_json_output() {
    let tmp = scratch_project("json");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(&tmp.join("src/greet.kara"), "pub fn greet() {}\n");

    let out = karac_bin()
        .current_dir(&tmp)
        .args(["build", "--output=json"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"status\":\"ok\""));
    assert!(stdout.contains("\"entry\":\"bin\""));
    assert!(stdout.contains("\"modules\":["));
    assert!(stdout.contains("\"role\":\"entry\""));
    assert!(stdout.contains("\"role\":\"ordinary\""));
    assert!(stdout.contains("\"path\":\"greet\""));
}

// ── Slice 4 integration: parse errors + cycle detection hooks ───

#[test]
fn test_build_project_surfaces_per_file_parse_errors() {
    let tmp = scratch_project("parse-errors");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    // Intentional parse error: unterminated function signature.
    write(&tmp.join("src/broken.kara"), "fn oops(\n");

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error[parse]"),
        "expected parse error diagnostic, got stderr={stderr}",
    );
    assert!(
        stderr.contains("broken.kara"),
        "parse error should identify the offending file, got stderr={stderr}",
    );
}

#[test]
fn test_build_project_requires_entry_file() {
    let tmp = scratch_project("no-entry");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    // Only a nested module, no main.kara / lib.kara.
    write(&tmp.join("src/helper.kara"), "pub fn hi() {}\n");

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no entry file"),
        "expected 'no entry file' diagnostic, got stderr={stderr}",
    );
}

#[test]
fn test_build_project_empty_src_dir_fails() {
    let tmp = scratch_project("empty-src");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    std::fs::create_dir_all(tmp.join("src")).unwrap();

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no compilable"),
        "expected 'no compilable files' diagnostic, got stderr={stderr}",
    );
}

#[test]
fn test_build_project_emits_built_line_or_no_llvm_note() {
    // Theme 4 (2026-05-10): the project-mode build now drives codegen
    // through to a linked executable. Under `cfg(feature = "llvm")`,
    // stdout carries `Built: <path>`; without the llvm feature, stderr
    // carries the no-llvm fallback note. Either way, the build should
    // report a successful exit for a trivially-correct project.
    let tmp = scratch_project("built-line");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let built = stdout.contains("Built: ") || stderr.contains("requires the llvm feature");
    assert!(
        built,
        "expected `Built: ...` (llvm path) or no-llvm fallback note; stdout={stdout} stderr={stderr}",
    );
}

// ── Theme 4: multi-file project-mode codegen ────────────────────
//
// Theme 4 wires `cmd_build_project` through the existing single-file
// codegen path by concatenating all module items (in topological order,
// dropping `import` declarations + the synthetic prelude) into a single
// super-program and driving it through `lower` → `effect` → `ownership`
// → `concurrency` → codegen → link. Symbol mangling is deferred to v2;
// cross-module function-name collisions surface as resolve-time errors
// against the merged super-program (clear diagnostic, no mangling
// ambiguity).
//
// All four tests below are gated `#[cfg(feature = "llvm")]` because the
// codegen output they verify only exists when llvm is built in.

#[cfg(feature = "llvm")]
#[test]
fn test_build_project_codegen_two_files_runs() {
    let tmp = scratch_project("codegen-two-files");
    write(
        &tmp.join("kara.toml"),
        "[package]\nname = \"two_file_demo\"\n",
    );
    write(
        &tmp.join("src/main.kara"),
        "import greet.add;\n\
         fn main() {\n    println(add(2, 3));\n}\n",
    );
    write(
        &tmp.join("src/greet.kara"),
        "pub fn add(x: i64, y: i64) -> i64 { x + y }\n",
    );

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    assert!(
        out.status.success(),
        "build failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Built: "),
        "expected `Built: ...` line; stdout={stdout}",
    );

    let exe_path = tmp.join("two_file_demo");
    let run = std::process::Command::new(&exe_path).output();
    let _ = std::fs::remove_dir_all(&tmp);
    let run = run.expect("executable should be runnable");
    assert!(
        run.status.success(),
        "executable failed: stderr={}",
        String::from_utf8_lossy(&run.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "5");
}

#[cfg(feature = "llvm")]
#[test]
fn test_build_project_codegen_three_module_chain_runs() {
    // Pins topological emission order — `db.users` must declare its
    // symbols before `db` references them, which must declare before
    // `main`. The super-program builder concatenates in
    // dependency-first order via `module::emission_order`.
    let tmp = scratch_project("codegen-three-chain");
    write(&tmp.join("kara.toml"), "[package]\nname = \"chain_demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "import db.fetch_count;\n\
         fn main() { println(fetch_count()); }\n",
    );
    write(
        &tmp.join("src/db.kara"),
        "import db.users.user_count;\n\
         pub fn fetch_count() -> i64 { user_count() + 1 }\n",
    );
    write(
        &tmp.join("src/db/users.kara"),
        "pub fn user_count() -> i64 { 42 }\n",
    );

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    assert!(
        out.status.success(),
        "build failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let exe_path = tmp.join("chain_demo");
    let run = std::process::Command::new(&exe_path).output();
    let _ = std::fs::remove_dir_all(&tmp);
    let run = run.expect("executable should be runnable");
    assert!(
        run.status.success(),
        "executable failed: stderr={}",
        String::from_utf8_lossy(&run.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "43");
}

#[cfg(feature = "llvm")]
#[test]
fn test_build_project_codegen_cross_module_collision_diagnostic() {
    // Symbol mangling is deferred to v2 (per the Theme 4 plan revision);
    // for now, cross-module function-name collisions fall out as a
    // resolve-time error against the merged super-program. The
    // diagnostic is structured (mentions the colliding name) but
    // file-context is absent until the v2 follow-up adds per-module
    // span threading. Pins the no-silent-overwrite invariant.
    let tmp = scratch_project("codegen-collision");
    write(
        &tmp.join("kara.toml"),
        "[package]\nname = \"collide_demo\"\n",
    );
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(
        &tmp.join("src/a.kara"),
        "pub fn shared_name() -> i64 { 1 }\n",
    );
    write(
        &tmp.join("src/b.kara"),
        "pub fn shared_name() -> i64 { 2 }\n",
    );

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        !out.status.success(),
        "build should fail on cross-module collision",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("shared_name") && stderr.contains("already defined"),
        "expected collision diagnostic naming `shared_name`; stderr={stderr}",
    );
}

#[cfg(feature = "llvm")]
#[test]
fn test_build_project_late_phase_diagnostic_includes_file_context() {
    // 2026-05-12 close-out for the wip-staging carry-forward
    // "Per-module diagnostics for late-phase failures in
    // `cmd_build_project`". The super-program approach concatenates
    // all module items, so post-typecheck phases lose the file-of-
    // origin context that the per-module typecheck path retains.
    // The `ModuleSpanTable` built at concat time in
    // `run_multi_file_codegen` (src/span_visitor.rs) restores that
    // context: each error's span is looked up against the table
    // and, when it resolves to exactly one module, the diagnostic
    // line is prefixed with `file:line:col`. This test triggers an
    // ownership use-after-move in a helper module and asserts the
    // build diagnostic surfaces `src/helper.kara` in the error
    // output. Use-after-move fires only in the ownership pass,
    // which runs over the super-program — typecheck-per-module
    // upstream lets the program through.
    let tmp = scratch_project("late-phase-file-context");
    write(
        &tmp.join("kara.toml"),
        "[package]\nname = \"late_phase_demo\"\n",
    );
    write(
        &tmp.join("src/main.kara"),
        "import helper.bad;\n\
         fn main() { let _ = bad(); }\n",
    );
    write(
        &tmp.join("src/helper.kara"),
        "struct Data { value: i64 }\n\
         fn consume(d: Data) -> i64 { d.value }\n\
         pub fn bad() -> i64 {\n\
             let d = Data { value: 7 };\n\
             let a = consume(d);\n\
             let b = consume(d);\n\
             a + b\n\
         }\n",
    );

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        !out.status.success(),
        "build should fail on use-after-move in helper.kara",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("src/helper.kara") || stderr.contains("helper.kara"),
        "expected helper.kara file context in stderr; stderr={stderr}",
    );
    // Sanity: also confirm the diagnostic phase name is included.
    assert!(
        stderr.contains("ownership") || stderr.contains("moved"),
        "expected an ownership-shaped diagnostic; stderr={stderr}",
    );
}

#[cfg(feature = "llvm")]
#[test]
fn test_build_project_codegen_providers_as_module_name() {
    // Theme 4 follow-up (2026-05-10): the lexer no longer reserves
    // `providers` as a global keyword. The bareword now lexes as a
    // regular identifier, so module names like `src/providers.kara`
    // and import paths `import providers.{...}` parse cleanly. The
    // parser dispatches to the `providers { R => e } in { body }`
    // block contextually — only when `providers` (as an Identifier
    // expression) is followed by `{`. This pins the demo path that
    // motivated the contextual-keyword change (`examples/parallax/`).
    let tmp = scratch_project("providers-module-name");
    write(
        &tmp.join("kara.toml"),
        "[package]\nname = \"providers_module\"\n",
    );
    write(
        &tmp.join("src/main.kara"),
        "import providers.canned_value;\n\
         fn main() { println(canned_value()); }\n",
    );
    write(
        &tmp.join("src/providers.kara"),
        "pub fn canned_value() -> i64 { 314 }\n",
    );

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    assert!(
        out.status.success(),
        "build failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let exe_path = tmp.join("providers_module");
    let run = std::process::Command::new(&exe_path).output();
    let _ = std::fs::remove_dir_all(&tmp);
    let run = run.expect("executable should be runnable");
    assert!(
        run.status.success(),
        "executable failed: stderr={}",
        String::from_utf8_lossy(&run.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "314");
}

#[cfg(feature = "llvm")]
#[test]
fn test_build_project_codegen_manifest_name_becomes_binary_name() {
    let tmp = scratch_project("codegen-binname");
    write(
        &tmp.join("kara.toml"),
        "[package]\nname = \"my_renamed_app\"\n",
    );
    write(&tmp.join("src/main.kara"), "fn main() { println(7); }\n");

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    assert!(
        out.status.success(),
        "build failed: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );

    let exe_path = tmp.join("my_renamed_app");
    let exe_exists = exe_path.exists();
    let run = std::process::Command::new(&exe_path).output();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        exe_exists,
        "expected binary at <project>/my_renamed_app derived from manifest name",
    );
    let run = run.expect("executable should be runnable");
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "7");
}

#[test]
fn test_build_project_surfaces_unknown_module_e0224() {
    let tmp = scratch_project("unknown-module");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "import greeet.hello;\nfn main() {}\n",
    );
    write(&tmp.join("src/greet.kara"), "pub fn hello() {}\n");

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(!out.status.success(), "build should fail on unknown module");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error[E0224]"),
        "expected E0224 in stderr, got {stderr}",
    );
    assert!(
        stderr.contains("greet"),
        "expected 'did you mean greet' suggestion, got {stderr}",
    );
}

#[test]
fn test_build_project_surfaces_unknown_item_e0225() {
    let tmp = scratch_project("unknown-item");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "import greet.helllo;\nfn main() {}\n",
    );
    write(&tmp.join("src/greet.kara"), "pub fn hello() {}\n");

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(!out.status.success(), "build should fail on unknown item");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error[E0225]"),
        "expected E0225 in stderr, got {stderr}",
    );
}

#[test]
fn test_build_project_json_includes_resolve_diagnostics() {
    let tmp = scratch_project("json-resolve");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "import nope.thing;\nfn main() {}\n",
    );

    let out = karac_bin()
        .current_dir(&tmp)
        .arg("build")
        .arg("--output=json")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"code\":\"E0224\""),
        "JSON output should carry E0224, got {stdout}",
    );
    assert!(
        stdout.contains("\"phase\":\"resolve\""),
        "JSON diagnostic should tag phase=resolve, got {stdout}",
    );
}

#[test]
fn test_build_project_surfaces_e0222_across_directories() {
    let tmp = scratch_project("private-cross-dir");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "import db.helper.secret;\nfn main() {}\n",
    );
    write(&tmp.join("src/db/helper.kara"), "private fn secret() {}\n");

    let out = karac_bin().current_dir(&tmp).arg("build").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        !out.status.success(),
        "build should fail on cross-dir private access",
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error[E0222]"),
        "expected E0222 in stderr, got {stderr}",
    );
}

// ── karac test (CR-24 follow-up slice 1) ────────────────────────

/// Filter stdout to test-runner JSONL events. Build-pipeline diagnostics
/// share the same `"type"` discriminator key, so the filter restricts to
/// the five test-runner event types defined at
/// `docs/design.md § Testing › Test runner output format`.
fn jsonl_lines(s: &str) -> Vec<&str> {
    s.lines()
        .filter(|l| {
            matches!(
                event_kind(l),
                Some("run_start" | "test_pass" | "test_fail" | "test_skip" | "summary")
            )
        })
        .collect()
}

fn event_kind(line: &str) -> Option<&str> {
    // Lines look like `{"type":"test_pass",...}` — slice between the first
    // pair of double-quotes after `:`.
    let after_event = line.strip_prefix("{\"type\":\"")?;
    let end = after_event.find('"')?;
    Some(&after_event[..end])
}

#[test]
fn test_subcommand_help_test() {
    let out = karac_bin().args(["test", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac test"));
    assert!(stdout.contains("JSONL"));
    assert!(stdout.contains("EXIT CODE"));
}

#[test]
fn test_test_all_passing() {
    let tmp = scratch_project("test-all-pass");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "fn main() {}\nfn add(a: i64, b: i64) -> i64 { a + b }\n",
    );
    write(
        &tmp.join("src/main_test.kara"),
        "fn test_add() { assert_eq(add(1, 2), 3); }\nfn test_zero() { assert_eq(add(0, 0), 0); }\n",
    );

    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "expected exit 0; stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    assert_eq!(event_kind(lines[0]), Some("run_start"));
    assert!(lines[0].contains("\"total_tests\":2"));
    let pass_count = lines
        .iter()
        .filter(|l| event_kind(l) == Some("test_pass"))
        .count();
    assert_eq!(pass_count, 2, "expected 2 test_pass events; got: {lines:?}");
    let summary = lines.last().unwrap();
    assert_eq!(event_kind(summary), Some("summary"));
    assert!(summary.contains("\"passed\":2"));
    assert!(summary.contains("\"failed\":0"));
}

#[test]
fn test_test_failure_emits_left_right_and_location() {
    let tmp = scratch_project("test-failure-detail");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "fn main() {}\nfn add(a: i64, b: i64) -> i64 { a + b }\n",
    );
    // `assert_eq(add(2, 2), 5)` — left=4, right=5, line 1 col 23 of the
    // test file (the call is the second statement after the comment-free
    // first line).
    write(
        &tmp.join("src/main_test.kara"),
        "fn test_failing() { assert_eq(add(2, 2), 5); }\n",
    );

    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "expected non-zero exit; stdout:\n{stdout}"
    );
    let lines = jsonl_lines(&stdout);
    let fail_line = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_fail"))
        .unwrap_or_else(|| panic!("expected a test_fail event in:\n{lines:?}"));
    assert!(fail_line.contains("\"test\":\"<root>::test_failing\""));
    assert!(fail_line.contains("\"left\":\"4\""));
    assert!(fail_line.contains("\"right\":\"5\""));
    assert!(fail_line.contains("\"location\":{\"file\":"));
    assert!(fail_line.contains("main_test.kara"));
    let summary = lines.last().unwrap();
    assert!(summary.contains("\"failed\":1"));
}

#[test]
fn test_test_filter_narrows_to_substring_match() {
    let tmp = scratch_project("test-filter");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(
        &tmp.join("src/main_test.kara"),
        "fn test_alpha() { assert(true); }\nfn test_beta() { assert(true); }\nfn test_gamma() { assert(true); }\n",
    );

    let out = karac_bin()
        .current_dir(&tmp)
        .args(["test", "beta"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    assert!(lines[0].contains("\"total_tests\":1"));
    let pass_lines: Vec<&&str> = lines
        .iter()
        .filter(|l| event_kind(l) == Some("test_pass"))
        .collect();
    assert_eq!(pass_lines.len(), 1);
    assert!(pass_lines[0].contains("test_beta"));
}

#[test]
fn test_test_filter_no_matches_runs_zero_tests() {
    let tmp = scratch_project("test-filter-zero");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(
        &tmp.join("src/main_test.kara"),
        "fn test_alpha() { assert(true); }\n",
    );

    let out = karac_bin()
        .current_dir(&tmp)
        .args(["test", "no_such_test_anywhere"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    assert!(lines[0].contains("\"total_tests\":0"));
    let summary = lines.last().unwrap();
    assert!(summary.contains("\"total\":0"));
    assert!(summary.contains("\"passed\":0"));
    assert!(summary.contains("\"failed\":0"));
}

#[test]
fn test_test_no_test_files_runs_zero_tests() {
    let tmp = scratch_project("test-no-tests");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "fn main() {}\nfn helper() -> i64 { 42 }\n",
    );
    // No _test.kara file at all.

    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    assert!(lines[0].contains("\"total_tests\":0"));
}

#[test]
fn test_test_compile_error_exits_nonzero_without_summary() {
    let tmp = scratch_project("test-compile-err");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    // Use of an undefined name in the test file — surfaces as a resolve
    // error and the runner aborts before emitting any test events.
    write(
        &tmp.join("src/main_test.kara"),
        "fn test_a() { undefined_function(); }\n",
    );

    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "stdout:\n{stdout}");
    // Compile-failure path emits diagnostic events from the existing
    // build-pipeline JSONL helpers (with `"type":` keying), but no
    // `run_start` / `summary` test events should fire.
    let test_events = jsonl_lines(&stdout);
    assert!(
        test_events.is_empty(),
        "expected no test events; got: {test_events:?}"
    );
    assert!(stdout.contains("undefined_function"));
}

#[test]
fn test_test_test_only_module_with_no_production_sibling() {
    // A `_test.kara` file with no production sibling — design.md § Three-
    // level visibility says it shares the directory of the module it would
    // test. The walker still classifies it as Test role; the merge produces
    // a Module containing only test items.
    let tmp = scratch_project("test-only-module");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    // No src/standalone.kara — only the test file.
    write(
        &tmp.join("src/standalone_test.kara"),
        "fn test_in_standalone_module() { assert(true); }\n",
    );

    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    assert!(lines[0].contains("\"total_tests\":1"));
    let pass = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_pass"))
        .unwrap();
    assert!(pass.contains("standalone::test_in_standalone_module"));
}

#[test]
fn test_test_helper_in_test_file_not_run() {
    // Functions in a `_test.kara` file whose name does NOT start with `test_`
    // are helpers, not tests (per design.md § Testing > Unit tests).
    let tmp = scratch_project("test-helper-skip");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(
        &tmp.join("src/main_test.kara"),
        "fn make_pair() -> (i64, i64) { (1, 2) }\nfn test_uses_helper() { let p = make_pair(); assert_eq(p.0, 1); }\n",
    );

    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    assert!(lines[0].contains("\"total_tests\":1"));
    let pass = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_pass"))
        .unwrap();
    assert!(pass.contains("test_uses_helper"));
    assert!(!stdout.contains("make_pair"));
}

#[test]
fn test_test_test_prefixed_function_in_production_code_is_not_a_test() {
    // A `test_` prefix in production code (not a `_test.kara` file) is just
    // a regular function. It should NOT be picked up as a test.
    let tmp = scratch_project("test-prefix-prod");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "fn main() {}\nfn test_helper_named_like_a_test() -> i64 { 42 }\n",
    );
    // No `_test.kara` companion — so `test_helper_named_like_a_test`
    // lives in production code only and has no business being a test.

    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    assert!(lines[0].contains("\"total_tests\":0"));
}

#[test]
fn test_test_unknown_flag_rejected() {
    let out = karac_bin()
        .args(["test", "--no-such-flag"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown flag"));
}

#[test]
fn test_test_two_positional_args_rejected() {
    let out = karac_bin()
        .args(["test", "first_filter", "second_filter"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("at most one"));
}

#[test]
fn test_test_test_outside_project_emits_manifest_error() {
    let tmp = scratch_project("test-no-manifest");
    // No kara.toml at all — manifest discovery should fail.
    write(&tmp.join("src/main.kara"), "fn main() {}\n");

    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("manifest_error"));
}

// ── karac test (CR-24 follow-up slice 2: requires-gating) ───────

#[test]
fn test_test_requires_skips_when_resource_unavailable() {
    let tmp = scratch_project("test-requires-skip");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(
        &tmp.join("src/main_test.kara"),
        "#[test(requires = [karac_slice2_skipcase.fake_db])]\nfn test_needs_db() { assert(false); }\n",
    );

    // Env var KARA_RESOURCE_KARAC_SLICE2_SKIPCASE_FAKE_DB is unset; the
    // assertion in the body is unreachable because the test must be
    // skipped before execution. Exit 0 since skips don't fail by default.
    let out = karac_bin()
        .current_dir(&tmp)
        .arg("test")
        .env_remove("KARA_RESOURCE_KARAC_SLICE2_SKIPCASE_FAKE_DB")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "expected exit 0; stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    let skip = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_skip"))
        .unwrap_or_else(|| panic!("expected a test_skip line; got: {lines:?}"));
    assert!(skip.contains("\"reason\":\"unsatisfied_requires\""));
    assert!(skip.contains("\"resources\":[\"karac_slice2_skipcase.fake_db\"]"));
    assert!(skip.contains("test_needs_db"));
    let summary = lines.last().unwrap();
    assert!(summary.contains("\"skipped\":1"));
    assert!(summary.contains("\"failed\":0"));
    assert!(summary.contains("\"passed\":0"));
}

#[test]
fn test_test_requires_runs_when_env_var_set() {
    let tmp = scratch_project("test-requires-runs");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(
        &tmp.join("src/main_test.kara"),
        "#[test(requires = [karac_slice2_runcase.fake_db])]\nfn test_needs_db() { assert(true); }\n",
    );

    let out = karac_bin()
        .current_dir(&tmp)
        .arg("test")
        .env("KARA_RESOURCE_KARAC_SLICE2_RUNCASE_FAKE_DB", "1")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "expected exit 0; stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    assert!(
        lines.iter().any(|l| event_kind(l) == Some("test_pass")),
        "expected a test_pass line; got: {lines:?}",
    );
    assert!(
        lines.iter().all(|l| event_kind(l) != Some("test_skip")),
        "expected no test_skip line; got: {lines:?}",
    );
}

#[test]
fn test_test_requires_health_check_overrides_env_var() {
    // Even when the env var IS set, a `[test.resources]` shell command
    // takes precedence — and a failing command means the resource is
    // considered unavailable. Verifies the order-of-precedence rule
    // documented in `docs/design.md § Testing`.
    let tmp = scratch_project("test-requires-healthcheck");
    write(
        &tmp.join("kara.toml"),
        "[package]\nname = \"demo\"\n\n[test.resources]\n\"karac_slice2_healthcase.fake_db\" = \"false\"\n",
    );
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(
        &tmp.join("src/main_test.kara"),
        "#[test(requires = [karac_slice2_healthcase.fake_db])]\nfn test_needs_db() { assert(true); }\n",
    );

    // env var IS set — but the failing health-check should take precedence.
    let out = karac_bin()
        .current_dir(&tmp)
        .arg("test")
        .env("KARA_RESOURCE_KARAC_SLICE2_HEALTHCASE_FAKE_DB", "1")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "expected exit 0; stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    assert!(
        lines.iter().any(|l| event_kind(l) == Some("test_skip")),
        "health check exit-1 should win over env var; got: {lines:?}",
    );
}

#[test]
fn test_test_requires_health_check_success_runs_test() {
    let tmp = scratch_project("test-requires-healthok");
    write(
        &tmp.join("kara.toml"),
        "[package]\nname = \"demo\"\n\n[test.resources]\n\"karac_slice2_healthok.fake_db\" = \"true\"\n",
    );
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(
        &tmp.join("src/main_test.kara"),
        "#[test(requires = [karac_slice2_healthok.fake_db])]\nfn test_needs_db() { assert(true); }\n",
    );

    // No env var set — only the (passing) health check matters.
    let out = karac_bin()
        .current_dir(&tmp)
        .arg("test")
        .env_remove("KARA_RESOURCE_KARAC_SLICE2_HEALTHOK_FAKE_DB")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "expected exit 0; stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    assert!(
        lines.iter().any(|l| event_kind(l) == Some("test_pass")),
        "passing health check should let the test run; got: {lines:?}",
    );
}

#[test]
fn test_test_requires_partial_satisfied_lists_only_missing() {
    let tmp = scratch_project("test-requires-partial");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    // Two requires; A is set in env, B is not. Skip event must list only B.
    write(
        &tmp.join("src/main_test.kara"),
        "#[test(requires = [karac_slice2_partial.have_a, karac_slice2_partial.miss_b])]\nfn test_needs_both() { assert(true); }\n",
    );

    let out = karac_bin()
        .current_dir(&tmp)
        .arg("test")
        .env("KARA_RESOURCE_KARAC_SLICE2_PARTIAL_HAVE_A", "1")
        .env_remove("KARA_RESOURCE_KARAC_SLICE2_PARTIAL_MISS_B")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines = jsonl_lines(&stdout);
    let skip = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_skip"))
        .unwrap_or_else(|| panic!("expected test_skip; got: {lines:?}"));
    assert!(
        skip.contains("\"resources\":[\"karac_slice2_partial.miss_b\"]"),
        "missing-only list should contain only miss_b; got: {skip}",
    );
    assert!(
        !skip.contains("have_a"),
        "satisfied resource should not appear in missing list; got: {skip}",
    );
}

#[test]
fn test_test_all_promotes_skip_to_failure() {
    let tmp = scratch_project("test-all-flag");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(&tmp.join("src/main.kara"), "fn main() {}\n");
    write(
        &tmp.join("src/main_test.kara"),
        "#[test(requires = [karac_slice2_allcase.fake_db])]\nfn test_needs_db() { assert(true); }\n",
    );

    let out = karac_bin()
        .current_dir(&tmp)
        .args(["test", "--all"])
        .env_remove("KARA_RESOURCE_KARAC_SLICE2_ALLCASE_FAKE_DB")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "expected non-zero exit under --all when a skip occurs; stdout:\n{stdout}",
    );
    let lines = jsonl_lines(&stdout);
    let fail = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_fail"))
        .unwrap_or_else(|| panic!("expected test_fail under --all; got: {lines:?}"));
    assert!(fail.contains("\"reason\":\"unsatisfied_requires\""));
    assert!(fail.contains("\"resources\":[\"karac_slice2_allcase.fake_db\"]"));
    assert!(
        lines.iter().all(|l| event_kind(l) != Some("test_skip")),
        "no test_skip events should be emitted under --all; got: {lines:?}",
    );
    let summary = lines.last().unwrap();
    assert!(summary.contains("\"failed\":1"));
    assert!(summary.contains("\"skipped\":0"));
}

#[test]
fn test_test_requires_subcommand_help_documents_all_flag() {
    let out = karac_bin().args(["test", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--all"),
        "test --help should document --all; got:\n{stdout}",
    );
    assert!(
        stdout.contains("KARA_RESOURCE_"),
        "test --help should document the env-var probe; got:\n{stdout}",
    );
}

// ── karac test (CR-C: `#[with_provider]` fixture) ────────────────

/// Emit a small project with a Clock effect resource, a FakeClock
/// struct, and a single test. The caller supplies the test function
/// body and any attributes so each test scenario can tweak behavior.
fn with_provider_project(tag: &str, test_attrs_and_body: &str) -> std::path::PathBuf {
    let tmp = scratch_project(tag);
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "effect resource Clock;\n\
         struct FakeClock { t: i64 }\n\
         impl FakeClock { fn now(self) -> i64 { self.t } }\n\
         fn main() {}\n",
    );
    write(&tmp.join("src/main_test.kara"), test_attrs_and_body);
    tmp
}

#[test]
fn test_with_provider_fixture_pushes_frame_for_test() {
    let body = "#[with_provider(Clock, FakeClock { t: 42 })]\n\
                fn test_reads_injected_clock() {\n\
                    assert_eq(Clock.now(), 42);\n\
                }\n";
    let tmp = with_provider_project("test-with-provider-basic", body);
    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "expected pass; stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    let pass = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_pass"))
        .unwrap_or_else(|| panic!("expected test_pass; got:\n{lines:?}"));
    assert!(pass.contains("test_reads_injected_clock"));
}

#[test]
fn test_with_provider_constructor_failure_emits_structured_fail() {
    // Constructor calls `boom()` which hits `unreachable()` — a runtime
    // error surfaces before the frame is pushed, and the runner emits
    // `provider_construction_failed` instead of running the test body.
    let body = "fn boom() -> FakeClock { unreachable() }\n\
                #[with_provider(Clock, boom())]\n\
                fn test_broken_fixture() {\n\
                    assert_eq(1, 1);\n\
                }\n";
    let tmp = with_provider_project("test-with-provider-ctor-fail", body);
    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "expected non-zero exit; stdout:\n{stdout}"
    );
    let lines = jsonl_lines(&stdout);
    let fail = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_fail"))
        .unwrap_or_else(|| panic!("expected test_fail; got:\n{lines:?}"));
    assert!(
        fail.contains("\"reason\":\"provider_construction_failed\""),
        "missing reason in fail event: {fail}"
    );
    assert!(
        fail.contains("\"resource\":\"Clock\""),
        "missing resource in fail event: {fail}"
    );
    assert!(
        fail.contains("\"duration_ms\":0"),
        "duration_ms should be 0 when test body never ran: {fail}"
    );
}

#[test]
fn test_with_provider_and_requires_conflict_rejected_at_discovery() {
    // Same resource in both `requires` and `with_provider` — design.md
    // rejects with `requires_and_with_provider_conflict`.
    let body = "#[test(requires = [Clock])]\n\
                #[with_provider(Clock, FakeClock { t: 0 })]\n\
                fn test_contradictory_fixture() {\n\
                    assert_eq(1, 1);\n\
                }\n";
    let tmp = with_provider_project("test-with-provider-conflict", body);
    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    let fail = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_fail"))
        .unwrap_or_else(|| panic!("expected test_fail; got:\n{lines:?}"));
    assert!(
        fail.contains("\"reason\":\"requires_and_with_provider_conflict\""),
        "missing conflict reason: {fail}"
    );
    assert!(
        fail.contains("\"resources\":[\"Clock\"]"),
        "missing conflict resource list: {fail}"
    );
}

#[test]
fn test_with_provider_fail_event_grows_providers_field() {
    // The test body fails; the `test_fail` event should carry a
    // `providers` array listing the active fixtures. Passing tests stay
    // lean and do not carry this field.
    let body = "#[with_provider(Clock, FakeClock { t: 7 })]\n\
                fn test_fails_with_fixture() {\n\
                    assert_eq(Clock.now(), 999);\n\
                }\n";
    let tmp = with_provider_project("test-with-provider-fail-providers-field", body);
    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    let fail = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_fail"))
        .unwrap_or_else(|| panic!("expected test_fail; got:\n{lines:?}"));
    assert!(
        fail.contains("\"providers\":[\"Clock\"]"),
        "missing providers field in fail event: {fail}"
    );
}

#[test]
fn test_multiple_with_provider_attributes_all_active_in_body() {
    // Two `#[with_provider]` attributes — both providers visible inside
    // the test body. Source order is outer-to-inner per spec.
    let tmp = scratch_project("test-with-provider-multi");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "effect resource Clock;\n\
         effect resource AuditLog;\n\
         struct FakeClock { t: i64 }\n\
         impl FakeClock { fn now(self) -> i64 { self.t } }\n\
         struct FakeLog { n: i64 }\n\
         impl FakeLog { fn count(self) -> i64 { self.n } }\n\
         fn main() {}\n",
    );
    write(
        &tmp.join("src/main_test.kara"),
        "#[with_provider(Clock, FakeClock { t: 42 })]\n\
         #[with_provider(AuditLog, FakeLog { n: 3 })]\n\
         fn test_two_providers() {\n\
             assert_eq(Clock.now(), 42);\n\
             assert_eq(AuditLog.count(), 3);\n\
         }\n",
    );
    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout:\n{stdout}");
    let lines = jsonl_lines(&stdout);
    let pass = lines
        .iter()
        .find(|l| event_kind(l) == Some("test_pass"))
        .unwrap_or_else(|| panic!("expected test_pass; got:\n{lines:?}"));
    // Pass events stay lean — no `providers` field.
    assert!(
        !pass.contains("\"providers\""),
        "pass event should not carry providers field: {pass}"
    );
}

#[test]
fn test_with_provider_frame_popped_between_tests() {
    // Test A uses `#[with_provider(Clock, ...)]` to install a FakeClock
    // returning a fixed value; test B has no fixture. After A exits, its
    // provider frame must be popped so B's bare `Clock.now()` falls back
    // to the ambient default provider (CR-A slice 3) and does NOT see
    // A's FakeClock value.
    let tmp = scratch_project("test-with-provider-isolation");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "struct FakeClock { t: i64 }\n\
         impl FakeClock { fn now(self) -> i64 { self.t } }\n\
         fn main() {}\n",
    );
    write(
        &tmp.join("src/main_test.kara"),
        "#[with_provider(Clock, FakeClock { t: 1 })]\n\
         fn test_a_has_clock() { assert_eq(Clock.now(), 1); }\n\
         fn test_b_no_fixture() { assert_ne(Clock.now(), 1); }\n",
    );
    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines = jsonl_lines(&stdout);
    let a = lines
        .iter()
        .find(|l| l.contains("test_a_has_clock"))
        .unwrap_or_else(|| panic!("expected test_a_has_clock event; got:\n{lines:?}"));
    assert_eq!(event_kind(a), Some("test_pass"));
    let b = lines
        .iter()
        .find(|l| l.contains("test_b_no_fixture"))
        .unwrap_or_else(|| panic!("expected test_b_no_fixture event; got:\n{lines:?}"));
    assert_eq!(event_kind(b), Some("test_pass"));
}

#[test]
fn test_with_provider_overrides_builtin_primitive_clock() {
    // CR-C coverage slot: `#[with_provider(Clock, ...)]` overrides the
    // ambient default provider for the test's duration. Built-in
    // primitives use the same push-on-stack mechanism as user-declared
    // resources (design.md § Testing: "Built-in primitives use the same
    // mechanism"). The second test proves the override is scoped to the
    // attributed test — without the fixture, Clock.now() returns the
    // ambient default (system time, not the fake value 1700000000).
    let tmp = scratch_project("test-with-provider-builtin-override");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "struct FakeClock { t: i64 }\n\
         impl FakeClock { fn now(self) -> i64 { self.t } }\n\
         fn main() {}\n",
    );
    write(
        &tmp.join("src/main_test.kara"),
        "#[with_provider(Clock, FakeClock { t: 1_700_000_000 })]\n\
         fn test_clock_override_returns_fake() {\n\
             assert_eq(Clock.now(), 1_700_000_000);\n\
         }\n\
         fn test_clock_without_fixture_uses_ambient_default() {\n\
             assert_ne(Clock.now(), 1_700_000_000);\n\
         }\n",
    );
    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines = jsonl_lines(&stdout);
    let with_fixture = lines
        .iter()
        .find(|l| l.contains("test_clock_override_returns_fake"))
        .unwrap_or_else(|| panic!("expected override event; got:\n{lines:?}"));
    assert_eq!(event_kind(with_fixture), Some("test_pass"));
    let without_fixture = lines
        .iter()
        .find(|l| l.contains("test_clock_without_fixture_uses_ambient_default"))
        .unwrap_or_else(|| panic!("expected ambient-default event; got:\n{lines:?}"));
    assert_eq!(event_kind(without_fixture), Some("test_pass"));
}

// ── karac check --profiles ──────────────────────────────────────

#[test]
fn test_check_profiles_subcommand_help_lists_flag() {
    let out = karac_bin().args(["check", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--profiles"),
        "expected --profiles documented in `check --help`, got:\n{stdout}"
    );
}

#[test]
fn test_check_profiles_unknown_profile_rejected() {
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/clean.kara",
            "--profiles=embedded,does_not_exist",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected non-zero exit on unknown profile name"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown profile"),
        "expected 'unknown profile' diagnostic, got:\n{stderr}"
    );
    assert!(
        stderr.contains("does_not_exist"),
        "expected the offending profile name in error, got:\n{stderr}"
    );
}

#[test]
fn test_check_profiles_empty_list_rejected() {
    let out = karac_bin()
        .args(["check", "tests/snapshots/clean.kara", "--profiles="])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--profiles requires at least one profile"),
        "expected empty-list diagnostic, got:\n{stderr}"
    );
}

#[test]
fn test_check_profiles_all_clean_program_passes() {
    // A profile-neutral program (no `extern` declarations) compiles cleanly
    // under every profile — `--profiles=all` should exit 0 and emit a
    // grouped header per profile.
    let out = karac_bin()
        .args(["check", "tests/snapshots/clean.kara", "--profiles=all"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "expected clean program to pass all profiles, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("profile: default"));
    assert!(stderr.contains("profile: embedded"));
    assert!(stderr.contains("profile: kernel"));
    assert!(
        stderr.matches("All checks passed").count() == 3,
        "expected one pass message per profile, got:\n{stderr}"
    );
}

#[test]
fn test_check_profiles_per_profile_grouping_on_violation() {
    // The `allocates(Heap)` extern is fine under `default`, rejected under
    // `embedded` and `kernel`. Grouped output must keep the passing and
    // failing profiles separable.
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/profile_extern_heap.kara",
            "--profiles=all",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected non-zero exit when at least one profile fails"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("── profile: default ──"));
    assert!(stderr.contains("── profile: embedded ──"));
    assert!(stderr.contains("── profile: kernel ──"));
    assert!(
        stderr.contains("All checks passed under 'default' profile"),
        "expected default profile to pass; got:\n{stderr}"
    );
    assert!(
        stderr.contains("error(s) under 'embedded' profile"),
        "expected embedded profile to report errors; got:\n{stderr}"
    );
    assert!(
        stderr.contains("error(s) under 'kernel' profile"),
        "expected kernel profile to report errors; got:\n{stderr}"
    );
    // Profile name surfaces in the underlying E0405 message.
    assert!(stderr.contains("'embedded' profile"));
}

#[test]
fn test_check_profiles_subset_only_runs_named_profiles() {
    // `--profiles=embedded,kernel` should not include `default` in the
    // output, even when the file would have passed under it.
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/profile_extern_heap.kara",
            "--profiles=embedded,kernel",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("profile: embedded"));
    assert!(stderr.contains("profile: kernel"));
    assert!(
        !stderr.contains("profile: default"),
        "default profile should not appear when not in --profiles list, got:\n{stderr}"
    );
}

#[test]
fn test_check_profiles_dedupes_repeated_names() {
    // Specifying the same profile twice should run it once, not twice.
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/clean.kara",
            "--profiles=embedded,embedded",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("profile: embedded").count(),
        1,
        "duplicate profile name should be de-duplicated, got:\n{stderr}"
    );
}

#[test]
fn test_check_profiles_json_output() {
    // JSON output must wrap every profile run as a labeled object inside the
    // top-level `profiles` array, with an aggregate `success` field.
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/profile_extern_heap.kara",
            "--profiles=all",
            "--output=json",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    assert!(json.starts_with('{'));
    assert!(
        json.contains("\"profiles\":["),
        "expected top-level profiles array, got:\n{json}"
    );
    assert!(json.contains("\"profile\":\"default\""));
    assert!(json.contains("\"profile\":\"embedded\""));
    assert!(json.contains("\"profile\":\"kernel\""));
    // Aggregate success is false when any profile fails.
    assert!(json.contains("\"success\":false"));
    // E0405 (ProfileViolation) is the underlying effect-checker code.
    assert!(
        json.contains("\"code\":\"E0405\""),
        "expected E0405 ProfileViolation in JSON, got:\n{json}"
    );
}

#[test]
fn test_check_profiles_jsonl_emits_per_profile_events() {
    let out = karac_bin()
        .args([
            "check",
            "tests/snapshots/profile_extern_heap.kara",
            "--profiles=embedded,kernel",
            "--output=jsonl",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"type\":\"profile_start\""));
    assert!(stdout.contains("\"type\":\"profile_complete\""));
    assert!(stdout.contains("\"profile\":\"embedded\""));
    assert!(stdout.contains("\"profile\":\"kernel\""));
    // Each profile's pipeline emits its own build_complete frame.
    let build_completes = stdout.matches("\"type\":\"build_complete\"").count();
    assert_eq!(
        build_completes, 2,
        "expected one build_complete per profile, got {build_completes}"
    );
}

// ── karac query cost-summary ────────────────────────────────────

#[test]
fn test_query_cost_summary_clean_program_zeroes() {
    // A pure program with no RC fallbacks, no `with_provider` calls, and
    // no shared structs returns the all-zeros envelope and an empty
    // `by_function` list.
    let out = karac_bin()
        .args(["query", "cost-summary", "tests/snapshots/clean.kara"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "expected zero exit on clean program: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    assert!(json.starts_with('{'));
    assert!(json.contains("\"scope\":\"tests/snapshots/clean.kara\""));
    assert!(json.contains("\"rc_ops\":{\"count\":0,\"rc\":0,\"arc\":0}"));
    assert!(json.contains("\"arc_provider_wraps\":0"));
    assert!(json.contains("\"borrow_flag_fields\":0"));
    assert!(json.contains("\"partition_guard_sites\":0"));
    assert!(json.contains("\"auto_clone_insertions\":0"));
    assert!(json.contains("\"by_function\":[]"));
}

#[test]
fn test_query_cost_summary_rc_fallback_attributed_to_function() {
    // The fixture's `process` function consumes `d` in a match arm and
    // re-uses it afterward — a textbook RC fallback. The aggregator must
    // surface one rc_op against `process`, with a derivation entry that
    // names the binding and the trigger.
    let out = karac_bin()
        .args([
            "query",
            "cost-summary",
            "tests/snapshots/cost_summary_rc.kara",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    assert!(json.contains("\"rc_ops\":{\"count\":1,\"rc\":1,\"arc\":0}"));
    assert!(json.contains("\"function\":\"process\""));
    assert!(json.contains("\"rc_ops\":1"));
    assert!(
        json.contains("Rc fallback for `d`"),
        "expected derivation reason naming binding `d`, got: {json}"
    );
    assert!(json.contains("direct re-use after consume"));
    // Other categories must remain at zero — the fixture has no shared
    // struct and no `with_provider` calls.
    assert!(json.contains("\"arc_provider_wraps\":0"));
    assert!(json.contains("\"borrow_flag_fields\":0"));
}

#[test]
fn test_query_cost_summary_borrow_flags_count_shared_mut_fields() {
    // Two `mut` fields on a `shared struct`, plus one non-mut field, must
    // produce `borrow_flag_fields: 2` (only `mut` fields cost a flag).
    // The category is struct-attributable, so `by_function` stays empty.
    // The same definition is the trigger for the Tier 2
    // `perf[shared-struct-mut-field]` perf note: one entry per offending
    // struct (not per field), with the field names enumerated in the
    // message body so the migration target is visible without re-reading
    // the source.
    let out = karac_bin()
        .args([
            "query",
            "cost-summary",
            "tests/snapshots/cost_summary_shared.kara",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    assert!(json.contains("\"borrow_flag_fields\":2"));
    assert!(json.contains("\"by_function\":[]"));
    // Tier 2 perf note: exactly one entry, code stable, message names the
    // struct and both `mut` fields, site points at the struct definition.
    assert!(
        json.contains("\"code\":\"perf[shared-struct-mut-field]\""),
        "expected perf note code, got: {json}"
    );
    assert!(
        json.contains("`shared struct Counter`"),
        "expected struct name in perf note message, got: {json}"
    );
    assert!(
        json.contains("`hits`") && json.contains("`misses`"),
        "expected mut field names in perf note message, got: {json}"
    );
    // Exactly one note (one offending struct, one entry — not one per
    // field). Counted by occurrences of the stable code.
    let note_count = json.matches("perf[shared-struct-mut-field]").count();
    assert_eq!(
        note_count, 1,
        "expected exactly one perf note for the offending struct, got {note_count}: {json}"
    );
}

#[test]
fn test_query_cost_summary_no_perf_note_for_shared_without_mut() {
    // Negative-1: a `shared struct` with zero `mut` fields must NOT trigger
    // the Tier 2 perf note. The migration hint is predictive of *future*
    // concurrent-access cost, which depends on a `mut` field being present
    // — without one the borrow-flag and `par struct` migration framing both
    // collapse, so the note has nothing to predict.
    let src = "shared struct ReadOnly {\n    a: i64,\n    b: i64,\n}\nfn main() {}\n";
    let path = std::env::temp_dir().join(format!(
        "karac-perfnote-shared-no-mut-{}-{}.kara",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::write(&path, src).unwrap();
    let out = karac_bin()
        .args(["query", "cost-summary", path.to_str().unwrap()])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    assert!(json.contains("\"borrow_flag_fields\":0"));
    assert!(
        json.contains("\"perf_notes\":[]"),
        "expected empty perf_notes for shared struct without mut fields, got: {json}"
    );
}

#[test]
fn test_query_cost_summary_no_perf_note_for_plain_struct_with_mut() {
    // Negative-2: a plain (non-`shared`) struct with `mut` fields must NOT
    // trigger the Tier 2 perf note. The diagnostic is gated on
    // `kind == Shared` because it predicts the cost of a future
    // `shared struct` → `par struct` migration; a plain struct has no such
    // future, and `mut` on a plain-struct field is a parser-level shape that
    // doesn't pay a borrow-flag cost in the first place.
    let src = "struct Plain {\n    mut x: i64,\n    mut y: i64,\n}\nfn main() {}\n";
    let path = std::env::temp_dir().join(format!(
        "karac-perfnote-plain-mut-{}-{}.kara",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::write(&path, src).unwrap();
    let out = karac_bin()
        .args(["query", "cost-summary", path.to_str().unwrap()])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    assert!(json.contains("\"borrow_flag_fields\":0"));
    assert!(
        json.contains("\"perf_notes\":[]"),
        "expected empty perf_notes for plain struct with mut fields, got: {json}"
    );
}

#[test]
fn test_query_cost_summary_arc_provider_wraps_per_call_site() {
    // Two `with_provider[Clock](...)` call sites in two different functions
    // must each contribute one `arc_provider_wraps` and a derivation entry.
    let out = karac_bin()
        .args([
            "query",
            "cost-summary",
            "tests/snapshots/cost_summary_provider.kara",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout.trim();
    assert!(json.contains("\"arc_provider_wraps\":2"));
    assert!(json.contains("\"function\":\"run_one\""));
    assert!(json.contains("\"function\":\"run_two\""));
    assert!(json.contains("with_provider[Clock]"));
}

#[test]
fn test_query_cost_summary_rejects_function_dot_form() {
    // The cost-summary kind takes a bare file — appending `.fn_name` should
    // either be treated as the file path itself (which won't exist) or
    // surface an unhelpful error. Either way the exit must be non-zero.
    let out = karac_bin()
        .args([
            "query",
            "cost-summary",
            "tests/snapshots/clean.kara.process",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected non-zero exit on missing file"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot read"),
        "expected file-not-found error, got: {stderr}"
    );
}

#[test]
fn test_query_help_lists_cost_summary_kind() {
    let out = karac_bin().args(["query", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("cost-summary"),
        "expected cost-summary documented in `query --help`, got:\n{stdout}"
    );
}

#[test]
fn test_query_unknown_kind_lists_cost_summary_in_hint() {
    let out = karac_bin()
        .args(["query", "garbage", "tests/snapshots/clean.kara"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cost-summary"),
        "expected cost-summary listed in unknown-kind hint, got: {stderr}"
    );
}

// ── karac fix ──────────────────────────────────────────────────

/// Build a temp `.kara` file with the given source. Returns the path so
/// the caller can pass it to `karac fix` and assert on the resulting file
/// contents. Each call uses a fresh path keyed by pid + nanos so parallel
/// test runs don't collide.
fn fix_scratch_file(tag: &str, source: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "karac-fix-{}-{}-{}.kara",
        std::process::id(),
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::write(&path, source).expect("write scratch source");
    path
}

#[test]
fn test_fix_applies_did_you_mean_correction() {
    let path = fix_scratch_file(
        "applies",
        "fn helper() -> i64 { 42 }\nfn main() { println(helpr()); }\n",
    );
    let out = karac_bin()
        .args(["fix", path.to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "fix failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("applied 1 fix"));
    let rewritten = std::fs::read_to_string(&path).unwrap();
    assert!(
        rewritten.contains("println(helper())"),
        "expected `helpr` -> `helper`, got: {rewritten}"
    );
    assert!(
        !rewritten.contains("helpr"),
        "stale identifier still present: {rewritten}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_fix_dry_run_does_not_modify_file() {
    let original = "fn helper() -> i64 { 42 }\nfn main() { println(helpr()); }\n";
    let path = fix_scratch_file("dryrun", original);
    let out = karac_bin()
        .args(["fix", path.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("would apply 1 fix"));
    assert!(stdout.contains("`helpr`"));
    assert!(stdout.contains("`helper`"));
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, original, "dry-run must not write to disk");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_fix_reports_when_no_fixable_diagnostics() {
    // A program with no diagnostics — fix prints the no-op marker and
    // exits 0.
    let path = fix_scratch_file("noop", "fn main() { let x = 42; println(x); }\n");
    let out = karac_bin()
        .args(["fix", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no fixable diagnostics"),
        "expected no-op message, got: {stdout}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_fix_applies_multiple_corrections_in_one_file() {
    // Two typo'd identifiers in one file. Both should be fixed in a
    // single invocation; the reverse-offset ordering keeps later edits'
    // offsets valid.
    let path = fix_scratch_file(
        "multi",
        "fn alpha() -> i64 { 1 }\n\
         fn beta() -> i64 { 2 }\n\
         fn main() { println(alphq() + betq()); }\n",
    );
    let out = karac_bin()
        .args(["fix", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("applied 2 fix"));
    let rewritten = std::fs::read_to_string(&path).unwrap();
    assert!(rewritten.contains("alpha()"));
    assert!(rewritten.contains("beta()"));
    assert!(!rewritten.contains("alphq"));
    assert!(!rewritten.contains("betq"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_fix_help_text_lists_dry_run() {
    let out = karac_bin().args(["fix", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac fix"));
    assert!(stdout.contains("--dry-run"));
}

#[test]
fn test_fix_unknown_flag_rejected() {
    let out = karac_bin().args(["fix", "--garbage"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown flag"));
}

#[test]
fn test_fix_missing_file_arg_rejected() {
    let out = karac_bin().arg("fix").output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing file argument"));
}

#[test]
fn test_fix_applies_n0507_mut_ref_to_ref() {
    // Round 12.32: `karac fix` now runs through ownership and applies
    // the closure-prefix rewrite from N0507 (UnusedMutCaptureNote).
    // The source compiles cleanly (the note is a perf note, not an
    // error), so without ownership-side harvesting `karac fix` would
    // print "no fixable diagnostics" — pinning that the new path
    // collects ownership replacements alongside resolver ones.
    let path = fix_scratch_file(
        "n0507-apply",
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = mut ref || o.x + 1;\n\
             let _ = f();\n\
         }\n",
    );
    let out = karac_bin()
        .args(["fix", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fix failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("applied 1 fix"),
        "expected one fix applied, got: {stdout}"
    );
    let rewritten = std::fs::read_to_string(&path).unwrap();
    assert!(
        rewritten.contains("let f = ref || o.x + 1;"),
        "expected `mut ref` swapped for `ref`, got: {rewritten}"
    );
    assert!(
        !rewritten.contains("mut ref"),
        "stale `mut ref` still present: {rewritten}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_fix_dry_run_previews_n0507_without_writing() {
    // Dry-run companion to the apply test: --dry-run prints the
    // would-be rewrite (`mut ref` → `ref`) but does not modify the
    // file. The source is unchanged on disk after the command exits.
    let original = "struct Owned { x: i64 }\n\
                    fn main() {\n\
                        let o = Owned { x: 1 };\n\
                        let f = mut ref || o.x + 1;\n\
                        let _ = f();\n\
                    }\n";
    let path = fix_scratch_file("n0507-dryrun", original);
    let out = karac_bin()
        .args(["fix", path.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("would apply 1 fix"),
        "expected dry-run header, got: {stdout}"
    );
    assert!(
        stdout.contains("`mut ref`"),
        "expected dry-run to mention old text `mut ref`, got: {stdout}"
    );
    assert!(
        stdout.contains("`ref`"),
        "expected dry-run to mention new text `ref`, got: {stdout}"
    );
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, original, "dry-run must not write to disk");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_fix_aggregates_resolver_and_ownership_in_one_pass() {
    // A source that triggers BOTH a resolver `did you mean` (typo'd
    // identifier) AND an N0507 perf note (mut ref over a read-only
    // capture). One `karac fix` invocation should apply both.
    // Pinned because the cmd_fix collection step concatenates edits
    // from multiple phases — a regression that forgot one phase
    // would still apply the other and silently leave the source
    // half-rewritten.
    //
    // NOTE: the resolver-typo case (`helpr` vs `helper`) blocks
    // typecheck/ownership downstream because resolve has errors.
    // To exercise both phases in one fix pass we use a non-blocking
    // resolver class — but currently all resolver replacement
    // classes ARE errors that block typecheck. So this test instead
    // pins the negative case: a source with only the N0507 note
    // (resolve clean) gets the ownership fix applied, and the
    // resolver-only multi-fix test above (`test_fix_applies_multiple_corrections_in_one_file`)
    // pins the resolver-only path. The genuine cross-phase case will
    // arrive when a non-error resolver class gains replacement
    // metadata, or when ownership errors (not just notes) gain it.
    let path = fix_scratch_file(
        "n0507-clean-resolve",
        "struct Owned { x: i64 }\n\
         fn helper() -> i64 { 42 }\n\
         fn main() {\n\
             let _ = helper();\n\
             let o = Owned { x: 1 };\n\
             let f = mut ref || o.x + 1;\n\
             let _ = f();\n\
         }\n",
    );
    let out = karac_bin()
        .args(["fix", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rewritten = std::fs::read_to_string(&path).unwrap();
    assert!(rewritten.contains("let f = ref || o.x + 1;"));
    assert!(rewritten.contains("fn helper() -> i64 { 42 }"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_main_help_lists_fix_command() {
    let out = karac_bin().arg("help").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("fix <file>"));
}

#[test]
fn test_check_json_includes_replacement_metadata() {
    // The JSON envelope produced by `karac check` should include a
    // `"replacement"` payload alongside the `"hints"` field for
    // diagnostics that carry a machine-applicable edit (here:
    // resolver UndefinedName with a `did you mean` suggestion).
    let path = fix_scratch_file(
        "json-replacement",
        "fn helper() -> i64 { 42 }\nfn main() { println(helpr()); }\n",
    );
    let out = karac_bin()
        .args(["check", path.to_str().unwrap(), "--output=json"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected non-zero exit on undefined name"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"replacement\":"),
        "expected `replacement` field in JSON, got: {stdout}"
    );
    assert!(
        stdout.contains("\"text\":\"helper\""),
        "expected replacement text `helper`, got: {stdout}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_build_project_json_includes_replacement_for_unknown_module() {
    // Round 12.30: the project-mode JSON envelope must surface the
    // round-12.29 replacement payload for E0223 (UnknownModule).
    // Single-file `karac check` already includes it; the multi-file
    // path through `resolve_errors_json` previously dropped the
    // payload, making it unreachable from IDE consumers using
    // `karac build --output=json` against real projects (where E0223
    // can fire because imports are traversed). This test pins the
    // emission so the asymmetry can't silently regress.
    let tmp = scratch_project("json-replacement-e0223");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "import greeet.hello;\nfn main() {}\n",
    );
    write(&tmp.join("src/greet.kara"), "pub fn hello() {}\n");

    let out = karac_bin()
        .current_dir(&tmp)
        .args(["build", "--output=json"])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        !out.status.success(),
        "expected non-zero exit on unknown module"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"code\":\"E0224\""),
        "expected E0224 (UnknownModule) in JSON output, got: {stdout}"
    );
    assert!(
        stdout.contains("\"replacement\":"),
        "expected `replacement` field in JSON, got: {stdout}"
    );
    assert!(
        stdout.contains("\"text\":\"greet\""),
        "expected replacement text `greet`, got: {stdout}"
    );
}

#[test]
fn test_test_project_jsonl_includes_replacement_for_unknown_item() {
    // Round 12.30 companion to the JSON test: the JSONL envelope
    // emitted by `karac test` (one event per line — `run_start`,
    // `test_pass`, `test_fail`, `resolve_error`, etc.) must also carry
    // the replacement payload when a `resolve_error` event surfaces
    // a fixable diagnostic. The misspelled brace-list item exercises
    // E0225 (UnknownItemInModule) which flows through
    // `resolve_errors_jsonl`. `karac test` always emits JSONL on stdout
    // (no `--output` flag); when compilation fails, the runner exits
    // before the test pipeline starts and dumps the diagnostic events.
    let tmp = scratch_project("jsonl-replacement-e0225");
    write(&tmp.join("kara.toml"), "[package]\nname = \"demo\"\n");
    write(
        &tmp.join("src/main.kara"),
        "import greet.helllo;\nfn main() {}\n",
    );
    write(&tmp.join("src/greet.kara"), "pub fn hello() {}\n");

    let out = karac_bin().current_dir(&tmp).arg("test").output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        !out.status.success(),
        "expected non-zero exit when compile fails before tests run"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"code\":\"E0225\""),
        "expected E0225 (UnknownItemInModule) in JSONL output, got: {stdout}"
    );
    assert!(
        stdout.contains("\"replacement\":"),
        "expected `replacement` field in JSONL, got: {stdout}"
    );
    assert!(
        stdout.contains("\"text\":\"hello\""),
        "expected replacement text `hello`, got: {stdout}"
    );
}

#[test]
fn test_check_json_includes_replacement_for_n0507_note() {
    // Round 12.31: the ownership-checker N0507 (UnusedMutCaptureNote) now
    // carries machine-applicable `replacement` metadata covering the
    // closure prefix tokens (`mut ref` → `ref`). The single-file
    // `karac check --output=json` path renders ownership notes through
    // the same `extra_json` slot used for resolver replacement payloads,
    // so IDE quick-fix UIs see the same JSON shape across diagnostic
    // phases. First non-resolver class to gain replacement metadata.
    let path = fix_scratch_file(
        "json-replacement-n0507",
        "struct Owned { x: i64 }\n\
         fn main() {\n\
             let o = Owned { x: 1 };\n\
             let f = mut ref || o.x + 1;\n\
             let _ = f();\n\
         }\n",
    );
    let out = karac_bin()
        .args(["check", path.to_str().unwrap(), "--output=json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"code\":\"N0507\""),
        "expected N0507 in JSON output, got: {stdout}"
    );
    assert!(
        stdout.contains("\"replacement\":"),
        "expected `replacement` field on the N0507 note, got: {stdout}"
    );
    assert!(
        stdout.contains("\"text\":\"ref\""),
        "expected replacement text `ref`, got: {stdout}"
    );
    let _ = std::fs::remove_file(&path);
}

// ── karac clean / install / vendor (new subcommands) ────────────────

#[test]
fn test_clean_bare_idempotent_when_no_dist() {
    // `karac clean` against a tempdir with no `dist/` should exit 0
    // and report "already absent" — bare form is idempotent.
    let dir = std::env::temp_dir().join("kara-clean-bare-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = karac_bin().arg("clean").current_dir(&dir).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "`karac clean` should exit 0 on missing dist/"
    );
    assert!(
        stdout.contains("already absent"),
        "expected `already absent` notice, got: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_clean_bare_removes_existing_dist() {
    // `karac clean` should rm -rf the project-local `dist/` directory
    // and report what was removed.
    let dir = std::env::temp_dir().join("kara-clean-remove-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("dist").join("nested")).unwrap();
    std::fs::write(dir.join("dist").join("artifact.txt"), b"old").unwrap();

    let out = karac_bin().arg("clean").current_dir(&dir).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "`karac clean` should exit 0 (stdout: {stdout})"
    );
    assert!(stdout.contains("removed"));
    assert!(stdout.contains("project dist/"));
    assert!(!dir.join("dist").exists(), "dist/ should have been removed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_clean_global_targets_kara_cache() {
    // `karac clean --global` should resolve `$HOME/.kara/cache/` even
    // when the directory doesn't exist (idempotent) and reference the
    // canonical path in its output.
    let fake_home = std::env::temp_dir().join("kara-clean-global-test");
    let _ = std::fs::remove_dir_all(&fake_home);
    std::fs::create_dir_all(&fake_home).unwrap();
    let out = karac_bin()
        .args(["clean", "--global"])
        .env("HOME", &fake_home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout: {stdout}");
    assert!(stdout.contains(".kara/cache"));
    assert!(stdout.contains("global cache"));
    let _ = std::fs::remove_dir_all(&fake_home);
}

#[test]
fn test_install_requires_spec() {
    let out = karac_bin().arg("install").output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "`karac install` without spec must error"
    );
    assert!(stderr.contains("requires a <bin-spec>"));
}

#[test]
fn test_install_with_spec_emits_not_yet_wired_notice() {
    let out = karac_bin()
        .args(["install", "path=./tools/my-tool"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "v1 placeholder should exit non-zero so CI scripts notice the gap"
    );
    assert!(stderr.contains("not yet wired"));
    assert!(
        stderr.contains("path=./tools/my-tool"),
        "diagnostic must name the spec back, got: {stderr}"
    );
}

#[test]
fn test_vendor_emits_not_yet_wired_notice() {
    let out = karac_bin().arg("vendor").output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "v1 placeholder should exit non-zero");
    assert!(stderr.contains("not yet wired"));
    assert!(stderr.contains("./vendor/"));
}

#[test]
fn test_vendor_rejects_extra_args() {
    let out = karac_bin().args(["vendor", "extra"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("takes no arguments"));
}

#[test]
fn test_subcommand_help_clean() {
    for flag in ["--help", "-h"] {
        let out = karac_bin().args(["clean", flag]).output().unwrap();
        assert!(out.status.success(), "`karac clean {flag}` should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("karac clean"));
        assert!(stdout.contains("--global"));
    }
}

#[test]
fn test_subcommand_help_install() {
    let out = karac_bin().args(["install", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac install"));
    assert!(stdout.contains("<bin-spec>"));
}

#[test]
fn test_subcommand_help_vendor() {
    let out = karac_bin().args(["vendor", "--help"]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac vendor"));
    assert!(stdout.contains("--offline"));
}

// ── karac explain --concept=<name> ──────────────────────────────

#[test]
fn test_main_help_lists_explain_command() {
    let out = karac_bin().arg("help").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("explain --concept=NAME"),
        "top-level help should advertise the explain subcommand"
    );
    assert!(
        stdout.contains("closures"),
        "top-level help should list the supported explain concepts"
    );
}

#[test]
fn test_subcommand_help_explain() {
    for flag in ["--help", "-h"] {
        let out = karac_bin().args(["explain", flag]).output().unwrap();
        assert!(out.status.success(), "`karac explain {flag}` should exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("karac explain"));
        assert!(stdout.contains("--concept=NAME"));
        assert!(stdout.contains("closures"));
        assert!(
            stdout.contains("karac query ownership"),
            "scoped help should point at the per-function inspection surface"
        );
    }
}

#[test]
fn test_explain_concept_closures_renders_page() {
    let out = karac_bin()
        .args(["explain", "--concept=closures"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "explain --concept=closures should exit 0; stderr was: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Section headers from the page.
    assert!(stdout.contains("Closures: parameter modes, capture, and escape"));
    assert!(stdout.contains("Rule 2 first-use inference"));
    assert!(stdout.contains("Explicit prefixes: own | ref | mut ref"));
    assert!(stdout.contains("K2 conflict table"));
}

#[test]
fn test_explain_concept_closures_pins_first_use_classification() {
    // The Rule 2 mapping (read → ref, mutate → mut ref, consume → own)
    // is the load-bearing concept the page exists to teach. Pin all
    // three rows so a future copyedit cannot silently drop one.
    let out = karac_bin()
        .args(["explain", "--concept=closures"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("first use is a read"));
    assert!(stdout.contains("first use is a mutate"));
    assert!(stdout.contains("first use is a consume"));
    assert!(stdout.contains("`ref < mut ref < own`"));
}

#[test]
fn test_explain_concept_closures_pins_k2_ref_consume_redirect() {
    // Pin the *exact* diagnostic redirect wording the ownership checker
    // emits for the `ref` + consume K2 violation (see slice 1 of
    // phase-5-diagnostics.md § Closure default capture mode, and
    // src/ownership/expr_check.rs line ~1004). If the diagnostic gets
    // rephrased, this page must move with it or the user reading the
    // page after hitting the error will not see matching guidance.
    let out = karac_bin()
        .args(["explain", "--concept=closures"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("drop the `ref` prefix (use `own` or bare) or remove the consume"),
        "page must pin the exact `ref` + consume K2 redirect string the ownership checker emits"
    );
}

#[test]
fn test_explain_concept_closures_pins_k2_mut_ref_consume_redirect() {
    // Symmetric pin for the `mut ref` + consume K2 violation
    // (src/ownership/expr_check.rs line ~1007).
    let out = karac_bin()
        .args(["explain", "--concept=closures"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("drop the `mut ref` prefix and use `own`"),
        "page must pin the exact `mut ref` + consume K2 redirect string"
    );
}

#[test]
fn test_explain_concept_closures_pins_unused_mut_capture_note() {
    // The `mut ref` declared / read-only used perf note is the one
    // non-error K2 row that fires a diagnostic — pin its code name
    // so the page tracks the checker.
    let out = karac_bin()
        .args(["explain", "--concept=closures"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("perf[unused-mut-capture]"));
}

#[test]
fn test_explain_concept_closures_cross_references_disjoint_capture() {
    // Slice 2 spec requirement (b): cross-reference the disjoint
    // capture (Rule 2¼) extension so a reader knows the per-name
    // granularity is temporary and where the per-path future lives.
    let out = karac_bin()
        .args(["explain", "--concept=closures"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Rule 2¼"));
    assert!(stdout.contains("Disjoint closure capture"));
}

#[test]
fn test_explain_concept_closures_links_to_query_ownership() {
    // Slice 2 spec requirement (c): link to `karac query ownership`
    // as the per-function inspection surface. The page's final
    // section is the user's entry point for inspecting inferred
    // modes against a real source file.
    let out = karac_bin()
        .args(["explain", "--concept=closures"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("karac query ownership <file>.<function>"));
    assert!(stdout.contains("Inspecting inferred capture modes"));
}

#[test]
fn test_explain_concept_closures_describes_outer_scope_rc_routing() {
    // Slice 1 of phase-5-diagnostics.md § Closure default capture mode
    // documents the outer-scope routing case: a bare body that consumes
    // a captured root does NOT produce a use-after-move on a post-closure
    // outer-scope use — the binding promotes via RC fallback trigger 2
    // (RcTrigger::ClosureCaptureWithOuterUse). This is the most
    // common surprise the page exists to demystify.
    let out = karac_bin()
        .args(["explain", "--concept=closures"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("RcTrigger::ClosureCaptureWithOuterUse"));
    assert!(stdout.contains("Outer-scope routing"));
}

#[test]
fn test_explain_requires_concept_flag() {
    let out = karac_bin().arg("explain").output().unwrap();
    assert!(
        !out.status.success(),
        "bare `karac explain` should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--concept"));
    assert!(stderr.contains("closures"));
}

#[test]
fn test_explain_rejects_unknown_concept_with_supported_set() {
    let out = karac_bin()
        .args(["explain", "--concept=galaxy_brain"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown concept 'galaxy_brain'"),
        "stderr should name the rejected concept; got: {stderr}"
    );
    assert!(
        stderr.contains("Supported:") && stderr.contains("closures"),
        "stderr should list the supported concept set; got: {stderr}"
    );
}

#[test]
fn test_explain_rejects_empty_concept_value() {
    let out = karac_bin()
        .args(["explain", "--concept="])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--concept requires a name"));
}

#[test]
fn test_explain_rejects_duplicate_concept_flags() {
    let out = karac_bin()
        .args(["explain", "--concept=closures", "--concept=closures"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--concept may only be specified once"));
}

#[test]
fn test_explain_rejects_positional_argument() {
    let out = karac_bin().args(["explain", "closures"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--concept=NAME"));
}

#[test]
fn test_explain_rejects_unknown_flag() {
    let out = karac_bin()
        .args(["explain", "--concept=closures", "--banana"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown flag '--banana'"));
}
