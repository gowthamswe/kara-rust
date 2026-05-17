//! Slice B (2026-05-09) — HTTP server FFI surface (minimal `std.http`).
//!
//! Pins the `Server.serve_static` smoke path end-to-end: compiles a
//! Kāra source file that calls `Server.serve_static("127.0.0.1:0",
//! body)`, links it against `libkarac_runtime.a` (which carries the
//! hyper-backed `karac_runtime_serve_http_static` export), spawns the
//! resulting binary as a subprocess, reads the bound port from the
//! binary's `BOUND_PORT=<n>` stdout line, performs a `GET /dashboard/1`
//! against `127.0.0.1:<port>`, and asserts the response is `200` with
//! the expected JSON body.
//!
//! **B3 fallback (b) taken.** Free-fn effect-set polymorphism on the
//! handler signature isn't typechecker-supported yet (Theme 6 settled
//! the trait-method shape but free-fn shape is the open delta). v1's
//! smoke surface uses `Server.serve_static(addr, body)` — a fixed-body
//! responder — so the v1 entry doesn't depend on either fn-pointer-as-
//! free-fn-arg codegen or effect-set-parameter syntax. Polymorphic
//! `serve[E]` + arbitrary handler dispatch lands in a follow-up.
//!
//! **Subprocess + port-from-stdout pattern.** Precedent in
//! `tests/memory_sanitizer.rs`. The binary writes `BOUND_PORT=<n>\n`
//! to stdout from inside `karac_runtime_serve_http_static` immediately
//! before entering the accept loop; the test harness reads stdout
//! until it observes that line, parses the port, and only then
//! attempts the HTTP request.
//!
//! **Effect-propagation pin (`#[ignore]` placeholder).** v1's smoke
//! path uses `Server.serve_static` which has no handler — there's no
//! handler-effect-set to propagate. The placeholder lives at the end
//! of this file as documentation of what the polymorphic-`serve[E]`
//! follow-up needs to test.

#[cfg(feature = "llvm")]
mod http_server_tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Mutex, Once};
    use std::time::{Duration, Instant};

    // Test isolation: every test in this module calls
    // `std::env::set_var("KARAC_RUNTIME", ...)` before linking the
    // generated binary, and env-var mutation is not thread-safe
    // (`std::env::set_var` is `unsafe` on edition 2024+ for exactly
    // this reason — concurrent set/read interleavings produce
    // intermittent link failures and `await_bound_port` timeouts
    // when several tests run on cargo's default test thread pool).
    // A shared `Mutex<()>` serializes the four tests within this
    // test binary; lock is acquired at the top of each test and
    // released on drop. Same precedent as
    // `tests/codegen.rs::SPAWN_SITE_ENV_LOCK`.
    static HTTP_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    static RUNTIME_BUILT: Once = Once::new();
    static mut RUNTIME_PATH: Option<PathBuf> = None;

    /// Build the runtime static library once per test process and
    /// return its path. Returns None if the build fails — callers
    /// soft-skip. Mirrors `tests/parallax_lite.rs::runtime_path`.
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

    /// Compile a Kāra source string to an executable at `exe_path`,
    /// returning Err with a diagnostic on failure. Mirrors the build
    /// path in `tests/parallax_lite.rs::compile_and_time` minus the
    /// concurrency-analysis hookup (the smoke source has no auto-par
    /// surface).
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
        let _effects = karac::effectcheck(&parsed.program);
        let _ownership = karac::ownershipcheck(&parsed.program, &typed);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let obj = format!("/tmp/karac_http_smoke_{pid}_{nanos}.o");
        compile_to_object_with_options(&parsed.program, &obj, None, None, None, None)
            .map_err(|e| format!("codegen failed: {e}"))?;
        link_executable(&obj, exe_path.to_str().unwrap())
            .map_err(|e| format!("link failed: {e}"))?;
        let _ = std::fs::remove_file(&obj);
        Ok(())
    }

    /// Read stdout line-by-line until we see `BOUND_PORT=<n>` (returns
    /// the parsed port), or until `timeout` elapses (returns `None`).
    /// The reader thread keeps the child's stdout drained so the OS
    /// pipe buffer doesn't fill up and block the binary.
    fn await_bound_port(
        stdout: std::process::ChildStdout,
        timeout: Duration,
    ) -> (Option<u16>, std::thread::JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::channel::<u16>();
        let handle = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            // Loop reading lines; on first BOUND_PORT match send port
            // through the channel, then keep draining stdout for the
            // remainder of the process lifetime (so the child doesn't
            // block on a full stdout pipe).
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

    /// Issue an HTTP/1.1 GET to `127.0.0.1:<port><path>` over a raw
    /// `TcpStream`. Returns `(status_code, body)`. Hand-rolled so the
    /// test doesn't pull in a heavier dev-dep — `ureq` is already a
    /// project dep but its blocking client adds startup cost we don't
    /// need for a single-request smoke.
    fn http_get(port: u16, path: &str) -> Result<(u16, String), String> {
        let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .map_err(|e| format!("connect failed: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
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
        // Parse status line: "HTTP/1.1 200 OK".
        let first = head.lines().next().ok_or("empty response")?;
        let mut tokens = first.split_whitespace();
        let _proto = tokens.next();
        let status_str = tokens.next().ok_or("missing status code")?;
        let status: u16 = status_str
            .parse()
            .map_err(|e| format!("bad status code '{status_str}': {e}"))?;
        Ok((status, body))
    }

    /// B5 smoke test (the primary deliverable).
    ///
    /// Compiles a minimal Kāra program that calls
    /// `Server.serve_static("127.0.0.1:0", "...")`, runs the binary,
    /// reads the bound port from stdout, performs a `GET /dashboard/1`,
    /// asserts the response is 200 with the expected JSON body.
    ///
    /// The test takes the **single-file concat workaround** for
    /// `examples/parallax/`'s multi-file project layout (per Slice B's
    /// hard-stop trigger 5 fallback): the binary the test compiles is
    /// a single inline source string, not a multi-file project build.
    /// `examples/parallax/` holds the source-of-truth multi-file form
    /// for human readers.
    #[test]
    fn test_http_server_serves_hardcoded_handler() {
        let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Build the runtime statically; soft-skip if it can't be
        // built (e.g. no internet for crate downloads on a sandboxed
        // host — same skip pattern as `tests/par_codegen.rs`).
        let Some(rt) = runtime_path() else {
            eprintln!(
                "skip: libkarac_runtime.a not built \
                 (run `cargo build -p karac-runtime --release`)"
            );
            return;
        };
        std::env::set_var("KARAC_RUNTIME", &rt);

        // Inline source: bind on 127.0.0.1:0 and serve a fixed JSON
        // body for every request. The body matches what the future
        // `get_dashboard(1)` handler would return — the test asserts
        // structural fields (`profile`, `latest_orders`,
        // `top_notification`, `top_recommendation`) so the smoke is
        // demo-shaped even though the handler itself is a fixed
        // string in v1.
        let src = r#"
            fn main() {
                let addr = "127.0.0.1:0";
                let body = "{\"profile\":{\"name\":\"Alice\"},\"latest_orders\":[],\"top_notification\":{},\"top_recommendation\":{}}";
                let _result = Server.serve_static(addr, body);
                println("server exited unexpectedly");
            }
        "#;

        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let exe_path = PathBuf::from(format!("/tmp/karac_http_smoke_{pid}_{nanos}"));

        if let Err(e) = compile_and_link(src, &exe_path) {
            panic!("compile/link failed: {e}");
        }

        let mut child = Command::new(&exe_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn server binary");

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

        // Allow the listener a tiny window to be ready for accept(2)
        // — the BOUND_PORT line is written *immediately before* the
        // accept loop is entered, so a connect attempted in the same
        // microsecond can race the listener's TcpListener::accept
        // call. Retry-with-backoff handles the race; we cap at 5
        // attempts ~spaced by 50ms.
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
            if started.elapsed() > Duration::from_secs(10) {
                break;
            }
        }

        let _ = child.kill();
        // Reap the child to avoid leaving a zombie process — clippy
        // flags spawned children that are never `wait()`-ed.
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
        assert_eq!(status, 200, "expected 200 status; body={body:?}");
        assert!(
            body.contains("\"profile\""),
            "body should contain `profile` field; got: {body:?}"
        );
        assert!(
            body.contains("\"latest_orders\""),
            "body should contain `latest_orders` field; got: {body:?}"
        );
        assert!(
            body.contains("\"top_notification\""),
            "body should contain `top_notification` field; got: {body:?}"
        );
        assert!(
            body.contains("\"top_recommendation\""),
            "body should contain `top_recommendation` field; got: {body:?}"
        );
    }

    /// Effect-propagation pin (placeholder).
    ///
    /// **Conditional on the polymorphic-`serve[E]` follow-up.** v1's
    /// `Server.serve_static` is a fixed-body responder with no handler,
    /// so there's no handler-effect-set to propagate. Once free-fn
    /// effect-set parameter syntax + fn-pointer-as-arg codegen lands
    /// (a Phase 7/8 follow-up), this test fills in to exercise the
    /// signature
    /// `Server.serve[E](handler: fn(Request) -> Response with E)`
    /// against a handler with explicit `with reads(MockResource),
    /// writes(MockResource)` annotations and asserts the typechecker
    /// admits the call without effect-set mismatch.
    ///
    /// Per Slice B's locked test surface (item 2): "stays as a
    /// `#[ignore]` placeholder until effect-set parameter syntax on
    /// free fns lands."
    #[test]
    #[ignore]
    fn test_http_server_handler_effects_propagate() {
        // Placeholder — see doc comment.
    }

    /// Slice B follow-up (2026-05-09) — `Server.serve(handler)` smoke.
    ///
    /// Compiles a Kāra program that calls `Server.serve(handle)` with a
    /// free-fn handler, runs the resulting binary, reads `BOUND_PORT`
    /// from stdout, performs a `GET /`, and asserts the runtime side
    /// of the handler-dispatch entry comes up cleanly. Sibling to
    /// `test_http_server_serves_hardcoded_handler` (the `serve_static`
    /// equivalent above).
    ///
    /// HTTP handler ABI trampoline (2026-05-09): the per-handler shim
    /// adapts between the user `fn handle(req: Request) -> Response`
    /// signature and the FFI extern's
    /// `extern "C" fn(*const KaracHttpRequest, *mut KaracHttpResponse)`.
    /// `Request` lowers as an opaque pointer (F2); `Response` status and
    /// body decompose into the runtime setters. Codegen-side caching is
    /// pinned by
    /// `tests/codegen.rs::test_server_serve_handler_shim_caches`;
    /// this test is the end-to-end runtime exercise.
    #[test]
    fn test_server_serve_handler_smoke() {
        let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let Some(rt) = runtime_path() else {
            eprintln!(
                "skip: libkarac_runtime.a not built \
                 (run `cargo build -p karac-runtime --release`)"
            );
            return;
        };
        std::env::set_var("KARAC_RUNTIME", &rt);

        // Response is defined inline because the codegen test pipeline
        // (`compile_and_link`) doesn't auto-inject stdlib structs; the
        // user code carries the body shape directly. The `Server.serve`
        // / `Request.path()` dispatch arms are registered as compiler
        // builtins regardless.
        let src = r#"
            struct Response { status: i64, body: String }

            fn handle(req: Request) -> Response {
                Response { status: 200, body: "{}" }
            }

            fn main() {
                let _result = Server.serve("127.0.0.1:0", handle);
                println("server exited unexpectedly");
            }
        "#;

        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let exe_path = PathBuf::from(format!("/tmp/karac_http_handler_{pid}_{nanos}"));

        if let Err(e) = compile_and_link(src, &exe_path) {
            panic!("compile/link failed: {e}");
        }

        let mut child = Command::new(&exe_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn server binary");

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
            match http_get(port, "/") {
                Ok(r) => {
                    response = Some(r);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            if started.elapsed() > Duration::from_secs(10) {
                break;
            }
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&exe_path);

        let (status, body) = match response {
            Some(r) => r,
            None => panic!(
                "GET / against 127.0.0.1:{port} never succeeded; \
                 last error: {:?}",
                last_err
            ),
        };
        assert_eq!(status, 200, "expected 200 status; body={body:?}");
        assert_eq!(body.trim(), "{}", "expected `{{}}` body; got: {body:?}");
    }

    // ── HTTP handler ABI trampoline ──
    //
    // The two tests below pin the F3 method surface end-to-end:
    // `Request.path()` and `Request.method()` round-trip through the
    // runtime externs and yield owned Strings the user handler can
    // return as the response body.

    /// Run a handler-using server inline against `path` and assert the
    /// response. Soft-skips when the runtime library isn't built.
    fn run_handler_smoke(src: &str, request_path: &str) -> Option<(u16, String)> {
        let rt = runtime_path()?;
        std::env::set_var("KARAC_RUNTIME", &rt);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let exe_path = PathBuf::from(format!("/tmp/karac_http_handler_run_{pid}_{nanos}"));
        if let Err(e) = compile_and_link(src, &exe_path) {
            panic!("compile/link failed: {e}");
        }
        let mut child = Command::new(&exe_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn server binary");
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
            match http_get(port, request_path) {
                Ok(r) => {
                    response = Some(r);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            if started.elapsed() > Duration::from_secs(10) {
                break;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&exe_path);
        match response {
            Some(r) => Some(r),
            None => panic!(
                "GET {request_path} against 127.0.0.1:{port} never succeeded; \
                 last error: {:?}",
                last_err
            ),
        }
    }

    /// `Request.path()` round-trips end-to-end: the user handler reads
    /// the path from the incoming request and returns it as the
    /// response body. Pins F2 (opaque-ptr Request shape) + F3
    /// (`path()` method dispatch) end-to-end.
    #[test]
    fn test_server_serve_handler_reads_path() {
        let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let src = r#"
            struct Response { status: i64, body: String }

            fn handle(req: Request) -> Response {
                Response { status: 200, body: req.path() }
            }

            fn main() {
                let _result = Server.serve("127.0.0.1:0", handle);
                println("server exited unexpectedly");
            }
        "#;
        let Some((status, body)) = run_handler_smoke(src, "/dashboard/42") else {
            return;
        };
        assert_eq!(status, 200, "expected 200 status; body={body:?}");
        assert!(
            body.contains("/dashboard/42"),
            "expected response body to echo path /dashboard/42; got: {body:?}"
        );
    }

    /// `Request.method()` round-trips end-to-end: the user handler reads
    /// the HTTP method verb and returns it as the response body. Pins
    /// the `method()` arm. (POST is not exercised in v1; the GET smoke
    /// shape is the only verb covered.)
    #[test]
    fn test_server_serve_handler_reads_method() {
        let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let src = r#"
            struct Response { status: i64, body: String }

            fn handle(req: Request) -> Response {
                Response { status: 200, body: req.method() }
            }

            fn main() {
                let _result = Server.serve("127.0.0.1:0", handle);
                println("server exited unexpectedly");
            }
        "#;
        let Some((status, body)) = run_handler_smoke(src, "/") else {
            return;
        };
        assert_eq!(status, 200, "expected 200 status; body={body:?}");
        assert_eq!(
            body.trim(),
            "GET",
            "expected response body to be `GET`; got: {body:?}"
        );
    }
}
