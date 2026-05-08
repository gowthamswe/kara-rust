//! Kāra runtime library. Statically linked into every compiled Kāra binary.
//!
//! The compiler emits calls into this library for parallel execution, task
//! scheduling, and (eventually) event-loop integration and atomic primitives.
//! See design.md § Runtime Distribution.
//!
//! All public symbols are `extern "C"` — the compiler emits LLVM calls against
//! this ABI, so the surface must remain stable across compiler/runtime
//! versions built in lockstep and is NOT stable across independently built
//! pairs. Distribution is always compiler+runtime bundled.

mod clone;
mod map;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;

/// A single branch of a `par {}` block: a function pointer and its opaque
/// context. The context is heap-allocated by the compiler and freed by the
/// runtime after the branch returns.
#[repr(C)]
pub struct KaracBranch {
    pub func: unsafe extern "C" fn(*mut c_void, *const AtomicBool),
    pub ctx: *mut c_void,
}

// SAFETY: The compiler guarantees that each branch's ctx is exclusively owned
// by that branch for the duration of karac_par_run. Branches never share
// mutable state through ctx; any shared state goes through separately
// allocated Arc values (see Rc→Arc promotion in ownership.rs).
unsafe impl Send for KaracBranch {}

/// Execute branches concurrently using a fixed-size thread pool and join
/// before returning.
///
/// **Thread pool**: min(branch_count, available_parallelism) worker threads.
/// Each worker grabs the next branch index via an atomic counter —
/// simple work distribution without external dependencies.
///
/// **Fail-fast cancellation**: an internal `AtomicBool` cancel flag is set
/// when any branch panics. Workers check the flag before picking up new
/// branches, so remaining branches are skipped after a failure. Branches
/// already running complete (completion-wins at branch granularity).
///
/// **Result collection**: not yet implemented — branches return void.
/// Error propagation via typed results is a Phase 6.2 follow-up.
///
/// # Safety
///
/// `branches` must point to `count` valid `KaracBranch` values; each
/// branch's `func` must be a valid function pointer and `ctx` must be a
/// pointer the `func` is prepared to receive. The compiler always satisfies
/// these preconditions.
#[no_mangle]
pub unsafe extern "C" fn karac_par_run(branches: *const KaracBranch, count: usize) {
    if count == 0 {
        return;
    }

    let pool_size = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(count);

    // Copy branch descriptors so thread closures can capture them by reference.
    let copied: Vec<(unsafe extern "C" fn(*mut c_void, *const AtomicBool), usize)> = (0..count)
        .map(|i| {
            let b = &*branches.add(i);
            (b.func, b.ctx as usize)
        })
        .collect();

    let cancel = AtomicBool::new(false);
    let next_idx = AtomicUsize::new(0);

    thread::scope(|s| {
        for _ in 0..pool_size {
            s.spawn(|| {
                loop {
                    // Check cancel before picking up new work.
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                    if idx >= count {
                        break;
                    }
                    let (func, ctx) = copied[idx];
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                            func(ctx as *mut c_void, &cancel as *const AtomicBool);
                        }));
                    if result.is_err() {
                        // Fail-fast: signal other workers to stop.
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            });
        }
        // Implicit join at scope end — all workers finish before we return.
    });
}

// ── Error return trace ─────────────────────────────────────────────────────
//
// Mirrors the interpreter's `error_trace` (src/interpreter.rs:592). On each
// `?` failure site, the codegen emits a call to `karac_error_trace_push`
// before propagating the `Err` / `None`. On a `?` success, codegen emits a
// `karac_error_trace_clear` so a successful path doesn't leak frames into
// later failures.
//
// Storage: a single global `Mutex<ErrorTraceState>` (depth-64 ring buffer).
// We deliberately do NOT use a `thread_local!` here: Rust's TLS destructors
// run during thread shutdown, BEFORE the C runtime's atexit handlers, so
// reading TLS from inside `atexit` triggers a "cannot access a Thread Local
// Storage value during or after destruction" panic. A global mutex sidesteps
// that — it remains valid for the entire process lifetime, including during
// atexit.
//
// Multi-threaded `?` use (par branches doing their own propagation) writes
// to the same buffer; pushes serialize through the lock. For v1 this is
// acceptable — the typical workload has `?` in serial call chains, and par
// branches in the MVP runtime discard their `Err` returns anyway, so they
// never reach the trace surface.
//
// Output format: defaults to the interpreter's text mode (cli.rs:1651-1664):
//
//     Error return trace:
//       <file>:<line>:<col>
//       ... (trace truncated, max 64 frames)         (only when truncated)
//
// At process exit the printer consults the `KARAC_ERROR_TRACE_FORMAT` env
// var and dispatches to one of three emitters:
//
//   - `text`   (default, missing/unrecognized values fall back here): the
//              stderr lines shown above. Backwards-compatible with the
//              pre-env-var build.
//   - `json`   single-document pretty-ish JSON on stderr matching the
//              interpreter's `format_error_trace_json` shape: a bare array
//              `[{"file":"…","line":N,"column":N},…]` when not truncated,
//              or `{"frames":[…],"truncated":true}` when truncated.
//   - `jsonl`  line-delimited JSON (NDJSON), one event per line:
//              `{"type":"frame","file":"…","line":N,"column":N}` per frame
//              and an optional trailing `{"type":"truncated","max":64}`
//              line when the ring buffer dropped older frames.
//
// The env var is read once at atexit-time (after the printer wakes); the
// runtime never observes mid-process changes — out of scope per the slice
// plan. The atexit registration is lazy — the first `karac_error_trace_push`
// call arms it. Programs that never `?`-propagate pay zero atexit overhead.

const ERROR_TRACE_MAX_DEPTH: usize = 64;

#[derive(Clone)]
struct ErrorTraceFrame {
    file: String,
    line: u32,
    col: u32,
}

struct ErrorTraceState {
    frames: Vec<ErrorTraceFrame>,
    truncated: bool,
}

impl ErrorTraceState {
    const fn new() -> Self {
        ErrorTraceState {
            frames: Vec::new(),
            truncated: false,
        }
    }
}

static ERROR_TRACE: Mutex<ErrorTraceState> = Mutex::new(ErrorTraceState::new());

extern "C" {
    /// POSIX `atexit(3)` — register a handler to run on normal program
    /// termination (return from main). Not invoked on `_exit` / `abort`.
    fn atexit(callback: extern "C" fn()) -> i32;
}

/// Push a frame onto the global error-return trace buffer. Called by
/// codegen at every `?` failure block before the early-return.
///
/// `file_ptr` / `file_len` describe a UTF-8 byte range identifying the
/// source file the `?` site lives in; the byte slice need not outlive this
/// call (the runtime copies into an owned `String`). Pass a null pointer or
/// zero length when the source filename is unavailable; the frame still
/// records line/col.
///
/// # Safety
///
/// `file_ptr` must either be null or point to `file_len` initialized,
/// readable bytes. The compiler always satisfies this — the slice lives in
/// the program's read-only string-pool section.
#[no_mangle]
pub unsafe extern "C" fn karac_error_trace_push(
    file_ptr: *const u8,
    file_len: usize,
    line: u32,
    col: u32,
) {
    register_trace_atexit_once();
    let file = if file_ptr.is_null() || file_len == 0 {
        String::new()
    } else {
        let bytes = std::slice::from_raw_parts(file_ptr, file_len);
        String::from_utf8_lossy(bytes).into_owned()
    };
    if let Ok(mut state) = ERROR_TRACE.lock() {
        if state.frames.len() >= ERROR_TRACE_MAX_DEPTH {
            state.frames.remove(0);
            state.truncated = true;
        }
        state.frames.push(ErrorTraceFrame { file, line, col });
    }
}

/// Reset the global error-return trace buffer. Called by codegen at every
/// `?` success site so subsequent failures don't include stale frames from
/// a recovered earlier propagation.
#[no_mangle]
pub extern "C" fn karac_error_trace_clear() {
    if let Ok(mut state) = ERROR_TRACE.lock() {
        state.frames.clear();
        state.truncated = false;
    }
}

/// Idempotently register the atexit-time printer the first time a `?` site
/// pushes a frame. Programs that never propagate via `?` skip the
/// registration entirely.
fn register_trace_atexit_once() {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        // SAFETY: `atexit` accepts an `extern "C" fn()` pointer. The
        // handler reads the global mutex-protected state (still valid
        // during atexit, unlike thread_local) and writes to stderr.
        // A non-zero return from `atexit` would mean registration failed;
        // we ignore that — the program continues, the trace silently
        // won't print.
        unsafe {
            let _ = atexit(print_trace_at_exit);
        }
    });
}

/// Output format selected by the `KARAC_ERROR_TRACE_FORMAT` env var.
/// `Text` is the default and preserves the pre-env-var behavior verbatim.
#[derive(Clone, Copy)]
enum TraceFormat {
    Text,
    Json,
    Jsonl,
}

impl TraceFormat {
    /// Parse the env var. Missing / empty / unrecognized values fall back
    /// to `Text` (no diagnostic — keeping startup quiet matches the
    /// "format-switching mid-process is out of scope" stance).
    fn from_env() -> Self {
        match std::env::var("KARAC_ERROR_TRACE_FORMAT")
            .unwrap_or_default()
            .as_str()
        {
            "json" => TraceFormat::Json,
            "jsonl" => TraceFormat::Jsonl,
            // Empty string, "text", or anything else → text.
            _ => TraceFormat::Text,
        }
    }
}

extern "C" fn print_trace_at_exit() {
    // `lock()` may fail only if a prior holder panicked. In that case we
    // can still try to print via `into_inner` on the poisoned guard.
    let state = match ERROR_TRACE.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if state.frames.is_empty() {
        return;
    }
    match TraceFormat::from_env() {
        TraceFormat::Text => emit_text(&state),
        TraceFormat::Json => emit_json(&state),
        TraceFormat::Jsonl => emit_jsonl(&state),
    }
}

fn emit_text(state: &ErrorTraceState) {
    eprintln!("Error return trace:");
    for f in &state.frames {
        let file_part = if f.file.is_empty() {
            String::new()
        } else {
            format!("{}:", f.file)
        };
        eprintln!("  {}{}:{}", file_part, f.line, f.col);
    }
    if state.truncated {
        eprintln!(
            "  ... (trace truncated, max {} frames)",
            ERROR_TRACE_MAX_DEPTH
        );
    }
}

/// Single-document JSON matching the interpreter's
/// `cli.rs::format_error_trace_json` shape verbatim:
///
/// - Not truncated: bare array `[{"file":"…","line":N,"column":N},…]`.
/// - Truncated:     `{"frames":[…],"truncated":true}`.
///
/// Emitted on stderr (peer to text mode — keeps the program's stdout
/// clean for downstream pipelines).
fn emit_json(state: &ErrorTraceState) {
    let mut frames = String::new();
    for (i, f) in state.frames.iter().enumerate() {
        if i > 0 {
            frames.push(',');
        }
        write_frame_object(&mut frames, f);
    }
    if state.truncated {
        eprintln!("{{\"frames\":[{}],\"truncated\":true}}", frames);
    } else {
        eprintln!("[{}]", frames);
    }
}

/// Line-delimited JSON (NDJSON): one event per line, each line a
/// self-contained JSON object. Frames carry `"type":"frame"`; a trailing
/// `{"type":"truncated","max":N}` line is emitted only when the ring
/// buffer dropped older entries. The shape matches the interpreter's
/// JSONL channel idiom (`emit_jsonl_event` in `cli.rs`).
fn emit_jsonl(state: &ErrorTraceState) {
    for f in &state.frames {
        let mut line = String::from("{\"type\":\"frame\",");
        write_frame_fields(&mut line, f);
        line.push('}');
        eprintln!("{}", line);
    }
    if state.truncated {
        eprintln!(
            "{{\"type\":\"truncated\",\"max\":{}}}",
            ERROR_TRACE_MAX_DEPTH
        );
    }
}

/// Append a `{"file":…,"line":N,"column":N}` object literal to `out`.
fn write_frame_object(out: &mut String, f: &ErrorTraceFrame) {
    out.push('{');
    write_frame_fields(out, f);
    out.push('}');
}

/// Append the bare `"file":…,"line":N,"column":N` field set (no braces)
/// so callers can splice extra fields like `"type":"frame"` alongside.
fn write_frame_fields(out: &mut String, f: &ErrorTraceFrame) {
    out.push_str("\"file\":");
    write_json_string(out, &f.file);
    out.push_str(",\"line\":");
    push_u32(out, f.line);
    out.push_str(",\"column\":");
    push_u32(out, f.col);
}

/// Hand-written JSON string escape — the runtime intentionally avoids a
/// `serde_json` dependency (zero-heavy-deps policy; runtime is no_std-
/// adjacent). Escapes match the interpreter's `cli.rs::json_string`:
/// `"`, `\`, `\n`, `\r`, `\t`, and any other control byte (`< 0x20`)
/// goes through `\u00XX`. Everything else passes through untouched —
/// including non-ASCII, since the source filename arrives as UTF-8 from
/// `karac_error_trace_push` and the output stream is byte-transparent.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // `\u00XX` for the remaining control bytes (BS, FF, etc.).
                let bytes = [
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    hex_nibble(((c as u32) >> 4) as u8),
                    hex_nibble((c as u32) as u8),
                ];
                // SAFETY: every byte produced above is ASCII (`\\`, `u`,
                // `0`, and two lowercase hex digits) so the slice is
                // valid UTF-8.
                out.push_str(std::str::from_utf8(&bytes).unwrap());
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn hex_nibble(b: u8) -> u8 {
    let n = b & 0x0F;
    if n < 10 {
        b'0' + n
    } else {
        b'a' + (n - 10)
    }
}

fn push_u32(out: &mut String, n: u32) {
    use std::fmt::Write;
    let _ = write!(out, "{}", n);
}
