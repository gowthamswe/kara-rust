//! Slice E (2026-05-09) — Three-language Parallax bench harness.
//!
//! Two tests (gated on `--features llvm`):
//!
//! * `test_kara_bench_server_smoke` — compile + run the Kāra reference impl
//!   at `examples/parallax/bench/kara/server.kara`, GET `/dashboard/1`
//!   against the bound port, assert `200` + JSON body contains `"profile"`.
//!   Mirrors the `Server.serve` handler smoke pattern from
//!   `tests/http_server.rs`.
//!
//! * `test_bench_script_dry_run` — invoke
//!   `examples/parallax/bench/bench.sh --dry-run`, assert exit 0 + stdout
//!   names all four impls (kara, rust, go, node). Pins script syntactic
//!   correctness without paying the bench cost in CI.
//!
//! The Rust/Go/Node impls deliberately have no unit tests — they're
//! reference implementations, not karac code; their correctness is
//! validated by `bench.sh` returning numbers (broken impl → server
//! doesn't respond → wrk reports zero RPS → bench.sh exits non-zero).
//!
//! Per the slice plan: no throughput-number assertions in tests. The
//! numbers are the artifact, not a regression gate. CI doesn't run
//! `bench.sh`.

#[cfg(feature = "llvm")]
mod parallax_bench_tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::Once;
    use std::time::{Duration, Instant};

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    static RUNTIME_BUILT: Once = Once::new();
    static mut RUNTIME_PATH: Option<PathBuf> = None;

    /// Build the runtime static library once per test process and
    /// return its path; soft-skip on failure. Mirrors
    /// `tests/http_server.rs::runtime_path`.
    #[allow(static_mut_refs)]
    fn runtime_path() -> Option<PathBuf> {
        RUNTIME_BUILT.call_once(|| {
            let output = Command::new("cargo")
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

    /// Compile a Kāra source string to an executable. Same shape as
    /// `tests/http_server.rs::compile_and_link` — runs the in-process
    /// pipeline with the concurrency analysis hooked in so the
    /// `get_dashboard` parallel_group lowers through Slice A's slot
    /// ABI.
    fn compile_and_link(src: &str, exe_path: &Path) -> Result<(), String> {
        use karac::codegen::{compile_to_object_with_options, link_executable};
        let mut parsed = karac::parse(src);
        if !parsed.errors.is_empty() {
            return Err(format!("parse errors: {:?}", parsed.errors));
        }
        let resolved = karac::resolve(&parsed.program);
        if !resolved.errors.is_empty() {
            return Err(format!("resolve errors: {:?}", resolved.errors));
        }
        let typed = karac::typecheck(&parsed.program, &resolved);
        if !typed.errors.is_empty() {
            return Err(format!("typecheck errors: {:?}", typed.errors));
        }
        karac::lower(&mut parsed.program, &typed);
        let effects = karac::effectcheck(&parsed.program);
        let _ownership = karac::ownershipcheck(&parsed.program, &typed);
        let analysis = karac::concurrency_analyze(&parsed.program, &effects);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let obj = format!("/tmp/karac_bench_smoke_{pid}_{nanos}.o");
        compile_to_object_with_options(&parsed.program, &obj, None, Some(&analysis), None, None)
            .map_err(|e| format!("codegen failed: {e}"))?;
        link_executable(&obj, exe_path.to_str().unwrap())
            .map_err(|e| format!("link failed: {e}"))?;
        let _ = std::fs::remove_file(&obj);
        Ok(())
    }

    fn await_bound_port(
        stdout: std::process::ChildStdout,
        timeout: Duration,
    ) -> (Option<u16>, std::thread::JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::channel::<u16>();
        let handle = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let mut sent = false;
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if !sent {
                            if let Some(rest) = line.trim().strip_prefix("BOUND_PORT=") {
                                if let Ok(p) = rest.parse::<u16>() {
                                    let _ = tx.send(p);
                                    sent = true;
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let port = rx.recv_timeout(timeout).ok();
        (port, handle)
    }

    fn http_get(port: u16, path: &str) -> Result<(u16, String), String> {
        let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .map_err(|e| format!("connect failed: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("set_read_timeout failed: {e}"))?;
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream
            .write_all(req.as_bytes())
            .map_err(|e| format!("write failed: {e}"))?;
        let mut buf = Vec::new();
        stream
            .read_to_end(&mut buf)
            .map_err(|e| format!("read failed: {e}"))?;
        let text = String::from_utf8_lossy(&buf).into_owned();
        let mut parts = text.split("\r\n\r\n");
        let head = parts.next().unwrap_or("");
        let body = parts.collect::<Vec<_>>().join("\r\n\r\n");
        let first = head.lines().next().ok_or("empty response")?;
        let mut tokens = first.split_whitespace();
        let _proto = tokens.next();
        let status_str = tokens.next().ok_or("missing status code")?;
        let status: u16 = status_str
            .parse()
            .map_err(|e| format!("bad status code '{status_str}': {e}"))?;
        Ok((status, body))
    }

    /// Compile + run the Kāra bench server, GET `/dashboard/1`,
    /// assert `200` + body contains `"profile"`.
    ///
    /// **First-load discovery moment** per Slice E hard-stop trigger 4
    /// — if this fails, we've found a real bug (sustained-HTTP-load
    /// crash or similar). One-request smoke isn't sustained load
    /// (that's the bench's job), but it does prove the
    /// `Server.serve(handler)` + auto-par get_dashboard + JSON-build
    /// chain works end-to-end.
    #[test]
    fn test_kara_bench_server_smoke() {
        let Some(rt) = runtime_path() else {
            eprintln!(
                "skip: libkarac_runtime.a not built \
                 (run `cargo build -p karac-runtime --release`)"
            );
            return;
        };
        std::env::set_var("KARAC_RUNTIME", &rt);

        let src_path = workspace_root().join("examples/parallax/bench/kara/server.kara");
        let src = std::fs::read_to_string(&src_path)
            .unwrap_or_else(|e| panic!("missing fixture {}: {e}", src_path.display()));

        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let exe_path = PathBuf::from(format!("/tmp/karac_bench_smoke_{pid}_{nanos}"));

        if let Err(e) = compile_and_link(&src, &exe_path) {
            panic!("compile/link failed: {e}");
        }

        let mut child = Command::new(&exe_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn bench server binary");

        let stdout = child.stdout.take().expect("child stdout missing");
        let (port_opt, _join) = await_bound_port(stdout, Duration::from_secs(15));

        let port = match port_opt {
            Some(p) => p,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&exe_path);
                panic!("server did not emit BOUND_PORT line within timeout");
            }
        };
        assert!(port > 0, "BOUND_PORT must be a non-zero ephemeral port");

        let started = Instant::now();
        let mut last_err: Option<String> = None;
        let mut response: Option<(u16, String)> = None;
        for _ in 0..10 {
            match http_get(port, "/dashboard/1") {
                Ok(r) => {
                    response = Some(r);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            if started.elapsed() > Duration::from_secs(15) {
                break;
            }
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&exe_path);

        let (status, body) = match response {
            Some(r) => r,
            None => panic!(
                "GET /dashboard/1 against 127.0.0.1:{port} never succeeded; \
                 last error: {:?}",
                last_err
            ),
        };
        // 2xx success — exact status is bench-impl choice. Post-G1
        // (`5ef2ea6`), `handle()` folds three Dashboard i64 fields
        // into the response status (`200 + ((order ^ notif ^ rec) &
        // 1)`) so DCE can't elide the busy_loop calls. Status is
        // deterministically 200 or 201 depending on the kernel's
        // output for fixed user_id=1; both are valid for the smoke
        // test's purpose (server responded with a JSON body that
        // includes the expected fields).
        assert!(
            (200..300).contains(&status),
            "expected 2xx status; got={status}; body={body:?}"
        );
        assert!(
            body.contains("\"profile\""),
            "body should contain `\"profile\"` field; got: {body:?}"
        );
    }

    /// `bench.sh --dry-run` exits 0 and lists the four impl names on
    /// stdout. Doesn't actually start any server or invoke `wrk`.
    #[test]
    fn test_bench_script_dry_run() {
        let script = workspace_root().join("examples/parallax/bench/bench.sh");
        if !script.exists() {
            panic!("missing fixture: {}", script.display());
        }
        let output = Command::new("sh")
            .arg(&script)
            .arg("--dry-run")
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn bench.sh: {e}"));
        assert!(
            output.status.success(),
            "bench.sh --dry-run exited non-zero: status={:?}, stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        for impl_name in &["kara", "rust", "go", "node"] {
            assert!(
                stdout.contains(impl_name),
                "bench.sh --dry-run stdout should mention `{impl_name}`; got:\n{stdout}"
            );
        }
    }
}
