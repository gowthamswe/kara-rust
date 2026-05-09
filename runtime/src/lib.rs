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
//!
//! ## Debugger Contract (design.md § Debugger Contract)
//!
//! The four-piece contract surface that gives slice 5's
//! `std.runtime::list_par_blocks()` / `list_tasks()` / `has_debug_metadata()`
//! and the future `std.panic` crash-report `parallel_context` field a stable
//! shape to read against:
//!
//! 1. **`SpawnSiteId` metadata table** — `KARAC_SPAWN_SITES` /
//!    `KARAC_SPAWN_SITES_LEN` / `KARAC_SPAWN_SITES_ENABLED` globals emitted
//!    by codegen (slice 3, `c6d8b44`). Per-binary stable IDs for every
//!    `par {}` block (explicit + inferred) joined to `(file, line, col)`.
//! 2. **Parent-frame reference on worker frames** — `KaracFrame::parent`
//!    (slice 4): every worker frame produced by `karac_par_run` carries a
//!    pointer back to the frame that created it; root tasks have `null`.
//!    Slice 5 walks this graph to reconstruct the structured-concurrency
//!    tree.
//! 3. **Await-chain pointer on suspended tasks** — `KaracFrame::wait_target`
//!    (slice 4 contract surface only; v1 always populates `KaracWaitTarget::None`).
//!    Real values land when Phase 6.3's network event loop ships and registers
//!    `WaitTarget`s at I/O-effect-boundary operations.
//! 4. **Crash-report `parallel_context` field** — co-developed with these
//!    globals, lands with `std.panic` (separate Phase 8 entry).

mod clone;
mod map;

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;
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

// ── Debugger Contract — frame tracking (slice 4) ───────────────────────────
//
// See module-level doc-comment for the four-piece contract overview. This
// section ships pieces (2) and (3): per-worker `KaracFrame`s carrying a
// parent-frame pointer + a `wait_target` field, and the cross-thread
// `ACTIVE_FRAMES` registry slice 5 will enumerate.

/// Wait-target discriminator on `KaracFrame`. Item (3) of the four-piece
/// Debugger Contract; see module-level doc.
///
/// **v1 ships single-variant `None`.** The `wait_target` field exists on
/// every `KaracFrame` and the enum's name is stable, but no other variants
/// are defined yet because v1's blocking runtime has no real suspension to
/// track — `Receiver.recv()` returns `Unit` on empty rather than blocking,
/// no event loop exists yet. Phase 6.3's network event loop will add
/// `PeerTask { task: *const KaracFrame }` and `IoHandle { handle: *const c_void }`
/// variants additively (non-breaking under `#[non_exhaustive]` per
/// design.md § Stability) once it registers real `WaitTarget`s at
/// I/O-effect-boundary operations.
///
/// `#[repr(u8)]` pins the discriminant width at 1 byte for stable FFI.
/// The single-variant v1 form is `{ tag: u8 }` (one byte total — see the
/// `test_wait_target_size_pinned` runtime test). When Phase 6.3 adds
/// payload-carrying variants, the representation upgrades to `#[repr(C, u8)]`
/// (C-style tagged union with `u8` discriminant) — additive change, since
/// the existing single-variant `None` keeps discriminant 0 and the C-style
/// upgrade is wire-compatible for that variant. Rustc rejects `#[repr(C, u8)]`
/// on a no-payload enum (`E0566 conflicting representation hints`), so v1
/// uses `#[repr(u8)]` standalone; the plan-side spec said `#[repr(C, u8)]`
/// but the no-payload form requires a single repr hint.
#[repr(u8)]
#[non_exhaustive]
pub enum KaracWaitTarget {
    /// Worker is running (or, in v1, always — until Phase 6.3 lights up).
    None,
}

/// Per-worker frame produced by `karac_par_run`. Item (2) of the four-piece
/// Debugger Contract; see module-level doc.
///
/// Allocated on the worker thread's stack inside the `thread::scope` body,
/// so `*const KaracFrame` pointers are valid for the lifetime of that
/// worker's branch invocation. Pointers stored in `ACTIVE_FRAMES` are
/// removed at frame teardown (success or panic, via `FrameGuard`'s `Drop`)
/// before the stack frame deallocates. Pointers stored as a child's
/// `parent` field are safe because Rust's `thread::scope` guarantees the
/// parent thread's stack outlives all scope-spawned children.
///
/// Slice 5's `std.runtime::list_par_blocks()` joins `spawn_site_id` against
/// the slice-3 `KARAC_SPAWN_SITES` table to fill `(file, line, col)`; the
/// future `std.panic` crash-report reads the same fields for its
/// `parallel_context` block.
#[repr(C)]
pub struct KaracFrame {
    /// Frame of the worker that spawned this one, or `null` for root tasks.
    /// Walked by slice 5 to reconstruct the structured-concurrency tree.
    pub parent: *const KaracFrame,
    /// Index into the slice-3 `KARAC_SPAWN_SITES` table — identifies the
    /// `par {}` site (file, line, col, worker_count) this frame was forked
    /// from.
    pub spawn_site_id: u32,
    /// 0-based branch index within the par block — first branch is 0,
    /// second is 1, etc.
    pub worker_index: u32,
    /// What this worker is currently waiting on. Always `KaracWaitTarget::None`
    /// in v1 (no real suspension exists yet); Phase 6.3's event loop will set
    /// real values at I/O-effect-boundary operations.
    pub wait_target: KaracWaitTarget,
}

// Per-thread current-frame pointer. Workers set this to their
// stack-allocated `KaracFrame` for the duration of their branch invocation;
// root tasks (and threads outside any par-block context) read `null`.
//
// **`Cell`, not `RefCell`** — the inner value is `*const KaracFrame`
// (a `Copy` raw pointer), so `Cell::set` / `Cell::get` is sufficient and
// avoids `RefCell` borrow-tracking overhead.
//
// **TLS-during-atexit caveat does not apply.** The `karac_error_trace_*`
// section above (line ~115) explains why `thread_local!` is unsafe to read
// during `atexit` (TLS destructors run during thread shutdown, *before*
// the C runtime's atexit handlers, so reads from inside `atexit` panic).
// Slice 4's reads happen inside live Kāra code via
// `karac_runtime_get_current_frame`, never inside an atexit handler, so
// the constraint doesn't apply here. Future readers conflating the two
// surfaces should re-check this comment before redirecting frame tracking
// through a global mutex.
thread_local! {
    static CURRENT_FRAME: Cell<*const KaracFrame> = const { Cell::new(ptr::null()) };
}

/// Newtype around `*const KaracFrame` that opts into `Send + Sync` for
/// storage in the cross-thread `ACTIVE_FRAMES` registry. Raw pointers are
/// `!Send` by default; the soundness comes from the structured-concurrency
/// invariant that `thread::scope` joins all workers before `karac_par_run`
/// returns, and `FrameGuard::drop` removes each entry from the registry
/// before its stack frame deallocates. Iteration via
/// `karac_runtime_for_each_active_frame` is gated on the registry lock to
/// rule out reading-while-deregistering races.
#[derive(Copy, Clone, PartialEq, Eq)]
struct FramePtr(*const KaracFrame);

// SAFETY: see the doc-comment above. The runtime is the only writer to
// `ACTIVE_FRAMES`; pointers are valid by construction (stack-allocated
// inside a `thread::scope`-bounded worker) and removed before invalidation.
unsafe impl Send for FramePtr {}
unsafe impl Sync for FramePtr {}

/// Cross-thread registry of currently-active worker frames. Slice 5's
/// `karac_runtime_for_each_active_frame` enumerates this list under the
/// lock to materialize `Vec[ParBlockInfo]` for `std.runtime::list_par_blocks()`.
///
/// `Mutex<Vec<FramePtr>>` chosen over `RwLock<HashMap<ThreadId, _>>` because
/// slice 5 doesn't query by thread (it just enumerates), v1 has few
/// par-blocks (a linear `retain` on deregister is fine), and write/read
/// frequencies are roughly balanced (each fork = 1 lock at register +
/// 1 lock at deregister; iteration is rare). `RwLock` is worth its overhead
/// only when reads dominate writes ~10x+.
///
/// **Pointer lifetime constraint.** Entries point into worker thread stacks.
/// They are valid only while the worker is running its branch —
/// `FrameGuard::drop` removes the entry before the stack frame deallocates.
/// Slice 5's iteration **must** happen while holding the registry lock so a
/// worker can't exit and invalidate an entry between the enumerator's read
/// and the consumer's dereference. `karac_runtime_for_each_active_frame`'s
/// callback API enforces this by firing the callback under the lock.
static ACTIVE_FRAMES: Mutex<Vec<FramePtr>> = Mutex::new(Vec::new());

/// Lazy gating helper — read `KARAC_RUNTIME_DEBUG_METADATA` once and cache.
/// Mirrors codegen's `read_runtime_debug_metadata_env` exactly; both sides
/// independently honor the same env var.
///
/// - `Ok("0")` → `false` (gate explicitly off).
/// - `Ok(_)`   → `true` (any other value, including empty).
/// - `Err(_)`  → `true` (dev default; profile-aware defaults land in
///   Phase 8.5 Track 2).
///
/// The result is cached for the process lifetime via `OnceLock`. Tests that
/// flip the env var between runs can't observe a re-read once the cache is
/// initialized — they go through `runtime_debug_metadata_enabled_uncached`
/// (cfg(test)) instead.
fn runtime_debug_metadata_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(read_runtime_debug_metadata_env)
}

fn read_runtime_debug_metadata_env() -> bool {
    !matches!(std::env::var("KARAC_RUNTIME_DEBUG_METADATA"), Ok(v) if v == "0")
}

/// Test-only re-read of the gating env var that bypasses the `OnceLock`
/// cache used by `runtime_debug_metadata_enabled`. Used by
/// `test_runtime_debug_metadata_disabled_skips_tracking` so the test's
/// env-var mutation actually takes effect — otherwise the first slice-4
/// test to fire would freeze the cache to `true` and the disabled-path
/// test would silently pass against the wrong code path.
///
/// Tests serialize on `FRAME_TRACKING_ENV_LOCK` to prevent races on the
/// env var.
#[cfg(test)]
fn runtime_debug_metadata_enabled_uncached() -> bool {
    read_runtime_debug_metadata_env()
}

/// RAII guard that registers a frame in `ACTIVE_FRAMES` + `CURRENT_FRAME`
/// on construction and deregisters on `Drop`. Drop runs on both normal
/// return *and* unwind, so a panicking branch fn still cleanly removes its
/// entry from the registry — pinned by `test_frame_deregistered_on_panic`.
///
/// Hand-rolled rather than pulling in `scopeguard` to keep runtime deps
/// minimal (zero-heavy-deps policy; runtime is no_std-adjacent).
struct FrameGuard {
    frame_ptr: FramePtr,
    prev_current: *const KaracFrame,
}

impl FrameGuard {
    /// Register `frame` as the current frame on this thread and add it to
    /// `ACTIVE_FRAMES`. Caller must keep the underlying `KaracFrame` alive
    /// (e.g. on the worker's stack) until the guard drops.
    fn new(frame: &KaracFrame) -> Self {
        let frame_ptr = FramePtr(frame as *const KaracFrame);
        let prev_current = CURRENT_FRAME.with(|c| c.replace(frame_ptr.0));
        // Lock-poison handling: a poisoned mutex still has a valid Vec
        // inside; recover the inner state and proceed (matches the
        // `print_trace_at_exit` pattern above).
        let mut guard = ACTIVE_FRAMES.lock().unwrap_or_else(|p| p.into_inner());
        guard.push(frame_ptr);
        drop(guard);
        FrameGuard {
            frame_ptr,
            prev_current,
        }
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        let mut guard = ACTIVE_FRAMES.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|&p| p != self.frame_ptr);
        drop(guard);
        CURRENT_FRAME.with(|c| c.set(self.prev_current));
    }
}

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
/// **Frame tracking (Debugger Contract slice 4).** When
/// `runtime_debug_metadata_enabled()` is `true`, each branch runs inside a
/// `FrameGuard` that stack-allocates a `KaracFrame { parent, spawn_site_id,
/// worker_index, wait_target: KaracWaitTarget::None }` and registers it in
/// `ACTIVE_FRAMES` for slice 5's enumeration surface. `parent` is captured
/// from the calling thread's `CURRENT_FRAME` before `thread::scope` starts,
/// so workers spawned inside another worker's branch see the outer worker's
/// frame as their parent (nested-par chain). When the gate is off the
/// function runs the existing thread::scope loop unchanged — no allocation,
/// no bookkeeping.
///
/// **Result collection**: not yet implemented — branches return void.
/// Error propagation via typed results is a Phase 6.2 follow-up.
///
/// # Parameters
///
/// - `branches` / `count`: array of `KaracBranch` descriptors (one per
///   parallel statement in the source `par {}` block).
/// - `spawn_site_id`: identifies the par site for slice 4's `KaracFrame`
///   metadata. Indexes into the slice-3 `KARAC_SPAWN_SITES` table emitted
///   by codegen so slice 5 can join `(file, line, col)`. Ignored when
///   `runtime_debug_metadata_enabled() == false`.
///
/// # Safety
///
/// `branches` must point to `count` valid `KaracBranch` values; each
/// branch's `func` must be a valid function pointer and `ctx` must be a
/// pointer the `func` is prepared to receive. The compiler always satisfies
/// these preconditions.
#[no_mangle]
pub unsafe extern "C" fn karac_par_run(
    branches: *const KaracBranch,
    count: usize,
    spawn_site_id: u32,
) {
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
    let track_frames = runtime_debug_metadata_enabled();
    // Capture the calling thread's current frame *before* `thread::scope`
    // — children's `parent` field points at this. Cast through `usize` so
    // the closure captures a `Send + Sync` integer rather than the raw
    // pointer (which Rust's auto-trait inference flags as `!Send`).
    // Soundness: `thread::scope` guarantees the calling thread's stack
    // outlives all scope-spawned children, so the address remains valid
    // for the duration of the join.
    let parent_addr: usize = if track_frames {
        CURRENT_FRAME.with(|c| c.get()) as usize
    } else {
        0
    };

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
                            if track_frames {
                                let frame = KaracFrame {
                                    parent: parent_addr as *const KaracFrame,
                                    spawn_site_id,
                                    worker_index: idx as u32,
                                    wait_target: KaracWaitTarget::None,
                                };
                                let _guard = FrameGuard::new(&frame);
                                func(ctx as *mut c_void, &cancel as *const AtomicBool);
                                // `_guard` drops here on normal return,
                                // deregistering the frame. On panic the
                                // unwind path still runs Drop.
                            } else {
                                func(ctx as *mut c_void, &cancel as *const AtomicBool);
                            }
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

/// Public extern getter for slice 5 / tests. Returns the current thread's
/// active worker frame, or `null` for root tasks (and any thread outside a
/// par-block context, including any thread when
/// `runtime_debug_metadata_enabled() == false`).
///
/// Slice 5's `std.runtime::list_tasks()` reads through this symbol to find
/// the calling task's position in the structured-concurrency tree, then
/// walks `KaracFrame::parent` to enumerate ancestors.
///
/// # Safety
///
/// The returned pointer is valid only while the worker thread that owns
/// the frame is alive — that is, while the `karac_par_run` call that
/// produced the frame has not yet returned. Callers must not store the
/// pointer beyond the current par-block's join boundary. Slice 5's wrapper
/// dereferences-and-copies inside the same call frame, so this constraint
/// is naturally upheld.
#[no_mangle]
pub extern "C" fn karac_runtime_get_current_frame() -> *const KaracFrame {
    CURRENT_FRAME.with(|c| c.get())
}

/// Public extern iteration callback for slice 5. Invokes `callback` once
/// per currently-active worker frame, passing the frame pointer plus the
/// caller's opaque `userdata`. Slice 5's wrapper builds its
/// `Vec[ParBlockInfo]` inside the callback.
///
/// **Hold-the-lock-during-iteration is intentional.** `*const KaracFrame`
/// lifetimes are tied to the worker thread's stack; releasing the lock
/// before the slice-5-side reader finishes inspecting could let a worker
/// exit and invalidate the pointer (its `FrameGuard` deregisters on Drop,
/// then the stack frame deallocates). Callbacks fire under the
/// `ACTIVE_FRAMES` mutex.
///
/// # Safety
///
/// `callback` must be a valid function pointer with the documented
/// signature; it is invoked synchronously from the calling thread.
/// Callbacks MUST NOT call back into the runtime in ways that would
/// re-enter `ACTIVE_FRAMES` (e.g. spawning a new par block) — that would
/// deadlock. Read-only inspection of the `KaracFrame` fields is safe.
#[no_mangle]
pub unsafe extern "C" fn karac_runtime_for_each_active_frame(
    callback: unsafe extern "C" fn(*const KaracFrame, *mut c_void),
    userdata: *mut c_void,
) {
    let guard = ACTIVE_FRAMES.lock().unwrap_or_else(|p| p.into_inner());
    for &frame in guard.iter() {
        callback(frame.0, userdata);
    }
}

// ── Debugger Contract — `std.runtime` introspection (slice 5) ──────────────
//
// Item (4) of the four-piece contract per `design.md § Debugger Contract`.
// Materializes slice 3's `KARAC_SPAWN_SITES` LLVM globals + slice 4's
// `ACTIVE_FRAMES` registry as Kāra-callable APIs through the
// `Runtime.has_debug_metadata()` / `Runtime.list_par_blocks()` /
// `Runtime.list_tasks()` surface declared in `runtime/stdlib/runtime.kara`.
//
// **Linkage choice (cross-checked against `cat rust-toolchain.toml`).**
// The slice plan flagged a fork between `#[linkage = "extern_weak"]`
// (nightly-only via `#![feature(linkage)]`) and strong linkage on stable
// Rust. The project pins stable Rust (no `rust-toolchain.toml`; cargo
// 1.95.0 stable), so this section takes the **strong-linkage** path:
// slice 3's `emit_spawn_sites_metadata` always emits the globals (even
// the gate-off form ships `LEN = 0`, `ENABLED = false`, empty array), so
// extern declarations without `#[linkage]` resolve at link time on every
// karac binary. Hard-stop trigger 1 is satisfied: weak linkage is only
// needed when some build path skips the emission, which slice 3 never
// does.
//
// **Vec materialization (sub-step f, hard-stop trigger 3).** Slice 5 takes
// the runtime-side full Vec materialization path: `karac_runtime_list_par_blocks_into`
// allocates the `Vec[ParBlockInfo]` element buffer, populates each entry
// (including per-entry String allocation for the file-path field), and
// writes the final `{data, len, cap}` Vec descriptor into a slot the
// codegen alloca'd. Trade-off: the runtime carries Kāra Vec + String
// layout knowledge (already present from `clone.rs::karac_string_clone`)
// and the compiler-side ParBlockInfo struct layout (matched via `#[repr(C)]`
// with explicit padding). Codegen-side complexity drops from ~80 lines of
// inline-IR loop to a single call + load. The alternative (codegen emits
// the iteration + per-entry String clone in inline IR) is the
// plan-recommended path; slice 5 deviates because the Kāra-side `String`
// allocation surface for inline-IR construction (hard-stop trigger 4) is
// not directly exposed at the relevant abstraction level.

// Strong-linkage extern declarations of slice 3's globals. Gated on
// `#[cfg(not(test))]` so the runtime crate's own unit tests can provide
// stand-in definitions (see the `#[cfg(test)]` block at the bottom of
// this file) — codegen-emitted globals only enter the link in real karac
// builds, never in the runtime crate's standalone test binary.
#[cfg(not(test))]
extern "C" {
    /// Slice 3 emits `KARAC_SPAWN_SITES_ENABLED` as an LLVM `i1`
    /// (booltype) global. On every supported target the `i1` lowers to
    /// a 1-byte storage cell (the LLVM data layout's `i1` alignment is
    /// 1, and the value-bit lives in the low bit), so reading it
    /// through a `u8` extern static is the stable way to recover the
    /// boolean: any non-zero low bit means `true`.
    static KARAC_SPAWN_SITES_ENABLED: u8;
    /// Slice 3 emits this as an `i32` global; row count of the
    /// `KARAC_SPAWN_SITES` array (`0` when the gate is off).
    static KARAC_SPAWN_SITES_LEN: u32;
    /// Slice 3 emits this as a `[N x SpawnSiteEntry]` array global.
    /// `KaracSpawnSiteEntry` below mirrors the LLVM struct layout
    /// `{ i32 id, ptr file_cstr, i32 line, i32 col, i32 worker_count, i32 reserved }`.
    static KARAC_SPAWN_SITES: KaracSpawnSiteEntry;
}

/// One row of slice 3's `KARAC_SPAWN_SITES` LLVM array. The layout must
/// match `Codegen::emit_spawn_sites_metadata`'s
/// `{ i32 id, ptr file_cstr, i32 line, i32 col, i32 worker_count, i32 reserved }`
/// struct exactly: `#[repr(C)]` + 8-byte alignment for the `file_cstr`
/// pointer puts a 4-byte gap after `id` and a 4-byte gap after
/// `_reserved`, total 32 bytes per entry. `mem::size_of` /
/// `mem::offset_of` are pinned in `tests::test_spawn_site_entry_layout_pinned`
/// so any future codegen-side rearrangement triggers a runtime-test
/// failure rather than a silent ABI break.
#[repr(C)]
struct KaracSpawnSiteEntry {
    id: u32,
    _pad0: u32, // alignment padding before pointer
    file_cstr: *const std::os::raw::c_char,
    line: u32,
    col: u32,
    worker_count: u32,
    _reserved: u32,
}

/// Layout-compatible view of a Kāra `String` value `{ ptr data, i64 len, i64 cap }`.
/// Mirrors `clone.rs::KaracString` — duplicated here rather than imported
/// because `clone.rs` defines it with crate-private visibility for the
/// `karac_string_clone` symbol; lifting it to a shared module is a
/// post-slice-5 refactor.
#[repr(C)]
struct RuntimeKaracString {
    data: *mut u8,
    len: i64,
    cap: i64,
}

/// Layout-compatible view of a Kāra `Vec[T]` value `{ ptr data, i64 len, i64 cap }`.
/// Element type is opaque at this level — the slice 5 `_into` writers
/// allocate `count * size_of::<KaracParBlockInfo>()` bytes and stride by
/// the same element size when filling.
///
/// Public so the `karac_runtime_list_par_blocks_into` extern fn can name
/// the type in its parameter list. Field semantics match Kāra's `Vec[T]`
/// codegen — `data` is heap-allocated (`std::alloc::alloc` here, freed at
/// scope exit by user-side codegen), `len` / `cap` are i64 element counts.
#[repr(C)]
pub struct KaracVec {
    pub data: *mut u8,
    pub len: i64,
    pub cap: i64,
}

/// Layout-compatible view of the Kāra `ParBlockInfo` struct declared in
/// `runtime/stdlib/runtime.kara`:
///
/// ```text
/// pub struct ParBlockInfo {
///     spawn_site_id: u32,
///     file: String,        // {ptr, i64 len, i64 cap}
///     line: u32,
///     col: u32,
///     worker_count: u32,
/// }
/// ```
///
/// LLVM's natural layout for `{ i32, {ptr, i64, i64}, i32, i32, i32 }`
/// on 64-bit targets:
///
///   - offset 0..4:   spawn_site_id (i32)
///   - offset 4..8:   padding (alignment to 8 for the inner String)
///   - offset 8..32:  file (24 bytes)
///   - offset 32..36: line (i32)
///   - offset 36..40: col (i32)
///   - offset 40..44: worker_count (i32)
///   - offset 44..48: trailing padding (struct alignment 8)
///   - total size:    48 bytes
///
/// Rust's `#[repr(C)]` produces the identical layout because the field
/// order, alignments, and trailing-padding rules match LLVM's
/// `target-data-layout`-driven defaults on every supported target. The
/// `_pad0` / `_pad1` fields are explicit so the layout reads identically
/// to the LLVM struct in source — `tests::test_par_block_info_layout_pinned`
/// asserts size and field offsets at runtime.
#[repr(C)]
struct KaracParBlockInfo {
    spawn_site_id: u32,
    _pad0: u32,
    file: RuntimeKaracString,
    line: u32,
    col: u32,
    worker_count: u32,
    _pad1: u32,
}

/// Slice 5 of the Debugger Contract — public extern reading
/// `KARAC_SPAWN_SITES_ENABLED` from the binary's LLVM globals.
/// `runtime/stdlib/runtime.kara`'s `Runtime.has_debug_metadata()`
/// `#[compiler_builtin]` shim dispatches to this through codegen.
///
/// Slice 3 always emits the symbol (gate-off form is `0`), so the read
/// is unconditionally safe under strong linkage.
#[no_mangle]
pub extern "C" fn karac_runtime_has_debug_metadata() -> bool {
    // SAFETY: KARAC_SPAWN_SITES_ENABLED is always emitted by codegen
    // (slice 3, `c6d8b44`) — even the gate-off form ships the symbol
    // with value 0. Strong linkage resolves the address at link time;
    // the load is a single byte read. The `i1` LLVM type lowers to
    // 1-byte storage with the boolean value in the low bit, so any
    // non-zero byte means `true`.
    //
    // The `unsafe` block is required only in non-test builds where the
    // symbol resolves through an `extern "C"` decl; in test builds the
    // stand-in is a regular Rust `static u8` and the `unsafe` would be
    // unnecessary, so we cfg-gate accordingly.
    #[cfg(not(test))]
    {
        unsafe { KARAC_SPAWN_SITES_ENABLED != 0 }
    }
    #[cfg(test)]
    {
        KARAC_SPAWN_SITES_ENABLED != 0
    }
}

/// Build a Kāra `Vec[ParBlockInfo]` snapshot of currently-active
/// `par {}` blocks across all OS threads. Writes the resulting
/// `{data, len, cap}` Vec descriptor into `*out`.
///
/// Joins slice 4's `ACTIVE_FRAMES` registry against slice 3's
/// `KARAC_SPAWN_SITES` table: each active `KaracFrame::spawn_site_id`
/// indexes into `KARAC_SPAWN_SITES[id]` to look up `(file, line, col,
/// worker_count)`. The lookup is bounds-checked — frames whose id is
/// out-of-range (which would indicate a metadata mismatch between
/// runtime and codegen) are skipped rather than panicking, on the
/// "introspection should never crash the program" principle.
///
/// **Iteration holds the registry lock.** `karac_runtime_for_each_active_frame`'s
/// callback API is reused so that frame-pointer dereferences happen
/// while the lock is held — slice-4-style soundness for the `*const
/// KaracFrame` reads. The two-call snapshot race the slice plan worried
/// about (`_count` then `_fill`) is avoided entirely because we go from
/// active-frames → final Vec in a single function call.
///
/// Allocates two heap regions: the element buffer
/// (`count * size_of::<KaracParBlockInfo>()` bytes via `std::alloc::alloc`,
/// the same allocator the rest of the runtime uses) and one
/// `RuntimeKaracString` heap copy per entry's file path (also via
/// `std::alloc::alloc`). Empty result (`count == 0` or
/// `runtime_debug_metadata_enabled()` is false) writes `{null, 0, 0}` —
/// no allocation, matching Kāra's `Vec.new()` convention so scope-exit
/// cleanup is a no-op.
///
/// # Safety
///
/// `out` must point to a writable `{ptr, i64, i64}` slot. Codegen
/// always allocas this on the caller's stack before invoking. The
/// returned Vec's `cap` matches `len`, so when scope-exit cleanup
/// `free`s the buffer it sees a complete Kāra-shape allocation.
#[no_mangle]
pub unsafe extern "C" fn karac_runtime_list_par_blocks_into(out: *mut KaracVec) {
    if out.is_null() {
        return;
    }
    // Empty fast path: gate off, or no active frames. Either way write
    // the canonical empty `{null, 0, 0}` Vec.
    if !runtime_debug_metadata_enabled() {
        (*out) = KaracVec {
            data: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        };
        return;
    }

    // Snapshot active frames under the lock; copy out the (id, parent,
    // worker_index) triples so we can release the lock before doing
    // String allocations.
    struct FrameSnapshot {
        spawn_site_id: u32,
    }
    let frames: Vec<FrameSnapshot> = {
        let guard = ACTIVE_FRAMES.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .iter()
            .map(|fp| FrameSnapshot {
                // SAFETY: pointers in ACTIVE_FRAMES are valid while the
                // lock is held — `FrameGuard::drop` deregisters before
                // the stack frame deallocates, and we read the field
                // through the lock.
                spawn_site_id: (*fp.0).spawn_site_id,
            })
            .collect()
    };

    let count = frames.len();
    if count == 0 {
        (*out) = KaracVec {
            data: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        };
        return;
    }

    // Slice 3's KARAC_SPAWN_SITES table — bounds-check each spawn_site_id
    // against KARAC_SPAWN_SITES_LEN before indexing. Address cast goes
    // through a `*const ()` intermediate so the test-mode stand-in type
    // (`SpawnSiteEntryStandIn`, a `#[repr(transparent)]` wrapper around
    // `KaracSpawnSiteEntry`) and the production extern type both lower
    // to a raw byte address.
    let sites_len = KARAC_SPAWN_SITES_LEN as usize;
    let sites_base: *const KaracSpawnSiteEntry =
        &KARAC_SPAWN_SITES as *const _ as *const KaracSpawnSiteEntry;

    let elem_size = std::mem::size_of::<KaracParBlockInfo>();
    let layout = std::alloc::Layout::from_size_align(elem_size * count, 8)
        .expect("ParBlockInfo array layout");
    let buf = std::alloc::alloc(layout) as *mut KaracParBlockInfo;
    if buf.is_null() {
        std::alloc::handle_alloc_error(layout);
    }

    let mut filled: usize = 0;
    for snap in &frames {
        let id = snap.spawn_site_id as usize;
        let (file_str, line, col, worker_count) = if id < sites_len {
            let entry = &*sites_base.add(id);
            let file = if entry.file_cstr.is_null() {
                RuntimeKaracString {
                    data: std::ptr::null_mut(),
                    len: 0,
                    cap: 0,
                }
            } else {
                let cstr = std::ffi::CStr::from_ptr(entry.file_cstr);
                let bytes = cstr.to_bytes();
                if bytes.is_empty() {
                    RuntimeKaracString {
                        data: std::ptr::null_mut(),
                        len: 0,
                        cap: 0,
                    }
                } else {
                    let str_layout = std::alloc::Layout::array::<u8>(bytes.len()).unwrap();
                    let str_buf = std::alloc::alloc(str_layout);
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), str_buf, bytes.len());
                    RuntimeKaracString {
                        data: str_buf,
                        len: bytes.len() as i64,
                        cap: bytes.len() as i64,
                    }
                }
            };
            (file, entry.line, entry.col, entry.worker_count)
        } else {
            // Spawn-site ID out of range — metadata mismatch (e.g. table
            // emitted with gate off). Skip rather than crash.
            continue;
        };

        let entry_ptr = buf.add(filled);
        std::ptr::write(
            entry_ptr,
            KaracParBlockInfo {
                spawn_site_id: snap.spawn_site_id,
                _pad0: 0,
                file: file_str,
                line,
                col,
                worker_count,
                _pad1: 0,
            },
        );
        filled += 1;
    }

    (*out) = KaracVec {
        data: buf as *mut u8,
        len: filled as i64,
        cap: count as i64,
    };
}

// ── Provider stack (`with_provider[R]` trait-method dispatch) ──────────────
//
// Per-task linked list of `(resource_id, provider_data, vtable)` cells that
// `R.method(args)` dispatch walks innermost-first. Mirrors the interpreter's
// `eval_resource_method` semantics (src/interpreter.rs:7146) and the
// `design.md § Provider-Rooted Resources` ("Resource call desugaring",
// "Runtime mechanics", "with_provider and parameterized resources")
// paragraphs.
//
// **TLS-backed head pointer.** The slice plan recommended carrying the head
// pointer in `KaracFrame` to avoid `thread_local!` overhead, but root tasks
// (no par-block) have no `KaracFrame` — `karac_par_run` is the only site
// that allocates one. A thread-local works uniformly for root and spawned
// tasks; the per-`R.method()` cost is one TLS read, well within the cost
// model `design.md` already names ("thin Arc deref + one vtable
// indirection"). Cross-task inheritance (par-block branches): the env-struct
// emitted by codegen carries a `provider_stack_head` snapshot from the
// calling thread; each worker calls `karac_provider_set_stack_head` from
// the branch fn prologue to seed its TLS.
//
// **Frame ownership.** `ProviderFrame` storage is alloca'd by codegen at
// each `with_provider[R](p, ||body)` site; `karac_provider_push` populates
// the frame in-place and links it as the new head. `karac_provider_pop`
// unlinks the head (without deallocating — codegen owns the alloca). This
// matches the structured-concurrency invariant: every push has a matching
// pop on the same thread, balanced across normal and unwind paths.

/// FFI-safe handle to a trait vtable. Opaque from the runtime's
/// perspective — the runtime walks `vtable_ptr` only as far as following
/// the indirection; codegen generates the vtable layout (array of fn
/// pointers in trait-method-declaration order) and emits the indirect
/// call inline.
#[repr(C)]
pub struct VTable {
    _private: [u8; 0],
}

/// One entry in the per-task provider stack. Codegen alloca's storage for
/// these at each `with_provider[R](...)` site; `karac_provider_push`
/// populates them in-place.
///
/// `prev` chains to the previous head (innermost-first lookup); `null` for
/// the bottom frame. `resource_id` is the codegen-assigned u32 for the
/// resource trait `R`. `provider_data_ptr` is an opaque pointer to the
/// provider value's payload (codegen knows the layout); `vtable_ptr` is
/// the static vtable for `Provider's-impl-of-R::Provider`.
#[repr(C)]
pub struct ProviderFrame {
    pub prev: *const ProviderFrame,
    pub resource_id: u32,
    pub provider_data_ptr: *const u8,
    pub vtable_ptr: *const VTable,
}

// SAFETY: ProviderFrame stores raw pointers but the per-thread invariant
// (push/pop balanced on the same thread, frame storage alloca'd in the
// caller's stack frame) means cross-thread sharing never happens through
// `PROVIDER_STACK_HEAD` directly. The env-struct snapshot mechanism
// (`karac_provider_set_stack_head`) is the only cross-thread transfer and
// it copies the head pointer at branch entry — not a shared cell.
unsafe impl Send for ProviderFrame {}
unsafe impl Sync for ProviderFrame {}

// Per-thread current-head pointer. `Cell` over `*const ProviderFrame` —
// see the slice-4 `CURRENT_FRAME` comment block for the TLS-during-atexit
// rationale; this surface is read only inside live Kāra code, never from
// `atexit`.
thread_local! {
    static PROVIDER_STACK_HEAD: Cell<*const ProviderFrame> = const { Cell::new(ptr::null()) };
}

/// FFI return type for `karac_provider_lookup`. Two-pointer struct so the
/// caller can branch on `data.is_null()` for the "no binding" panic path
/// without needing a separate boolean. `#[repr(C)]` pins the layout.
#[repr(C)]
pub struct ProviderLookupResult {
    pub data: *const u8,
    pub vtable: *const VTable,
}

/// Push `frame` onto the per-task provider stack. Caller (codegen) supplies
/// `frame` storage (typically an alloca'd `ProviderFrame`) so the runtime
/// doesn't allocate. Populates `frame` in-place with `prev = current_head,
/// resource_id, provider_data, vtable`, then sets the per-task head pointer
/// to `frame`.
///
/// # Safety
///
/// `frame` must point to writable `ProviderFrame` storage that outlives
/// the matching `karac_provider_pop()` call. Codegen alloca's the storage
/// inside the same function frame as the `with_provider` body, so this is
/// satisfied by construction. `provider_data` and `vtable` must remain
/// valid for the duration of the push/pop window (provider value alive,
/// vtable is a static global).
#[no_mangle]
pub unsafe extern "C" fn karac_provider_push(
    frame: *mut ProviderFrame,
    resource_id: u32,
    provider_data: *const u8,
    vtable: *const VTable,
) {
    let prev = PROVIDER_STACK_HEAD.with(|c| c.get());
    *frame = ProviderFrame {
        prev,
        resource_id,
        provider_data_ptr: provider_data,
        vtable_ptr: vtable,
    };
    PROVIDER_STACK_HEAD.with(|c| c.set(frame));
}

/// Pop the current head frame from the per-task provider stack, reverting
/// the head pointer to the `prev` link. The frame's storage is owned by
/// the caller (codegen alloca) — the runtime only updates the head pointer.
/// No-op if the stack is already empty (defensive against double-pop on
/// unwind paths, though codegen should never emit that shape).
#[no_mangle]
pub extern "C" fn karac_provider_pop() {
    PROVIDER_STACK_HEAD.with(|c| {
        let head = c.get();
        if !head.is_null() {
            // SAFETY: head is a valid ProviderFrame (alive until matching
            // pop, per the push contract); reading `.prev` is safe.
            let prev = unsafe { (*head).prev };
            c.set(prev);
        }
    });
}

/// Walk the per-task provider stack innermost-first, returning the first
/// frame whose `resource_id` matches the requested ID. Returns
/// `(null, null)` on miss; codegen emits the structured-panic call inline
/// per `design.md:7084-7095` ("Resource call: no provider bound...").
#[no_mangle]
pub extern "C" fn karac_provider_lookup(resource_id: u32) -> ProviderLookupResult {
    let mut cursor = PROVIDER_STACK_HEAD.with(|c| c.get());
    while !cursor.is_null() {
        // SAFETY: cursor was either the live head pointer or a `prev` link
        // from a live frame; both are valid for the duration of the lookup
        // because frames don't deallocate until matching pops on the same
        // thread.
        let frame = unsafe { &*cursor };
        if frame.resource_id == resource_id {
            return ProviderLookupResult {
                data: frame.provider_data_ptr,
                vtable: frame.vtable_ptr,
            };
        }
        cursor = frame.prev;
    }
    ProviderLookupResult {
        data: ptr::null(),
        vtable: ptr::null(),
    }
}

/// Set the per-task provider stack head to `head`. Used by par-block worker
/// branches at branch-fn prologue to inherit the parent thread's stack.
/// Codegen captures `karac_provider_get_stack_head()` into the env-struct
/// at par-block entry, then each worker calls this with the captured value
/// before executing the branch body.
///
/// # Safety
///
/// `head` must point to a `ProviderFrame` whose lifetime spans the entire
/// par-block (it's the parent's frame, which lives until `karac_par_run`
/// returns, which lives until all branches join — so the lifetime is
/// satisfied by `thread::scope`'s join guarantee).
#[no_mangle]
pub unsafe extern "C" fn karac_provider_set_stack_head(head: *const ProviderFrame) {
    PROVIDER_STACK_HEAD.with(|c| c.set(head));
}

/// Snapshot the current per-task provider stack head. Used by codegen at
/// par-block entry to copy into the env-struct so each spawned worker can
/// seed its TLS via `karac_provider_set_stack_head`.
#[no_mangle]
pub extern "C" fn karac_provider_get_stack_head() -> *const ProviderFrame {
    PROVIDER_STACK_HEAD.with(|c| c.get())
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

// ── Slice 5 test stand-ins for slice 3 globals ─────────────────────────────
//
// The runtime crate's `cargo test -p karac-runtime` binary has its own
// (test-only) symbol space — the LLVM globals `KARAC_SPAWN_SITES`,
// `KARAC_SPAWN_SITES_LEN`, `KARAC_SPAWN_SITES_ENABLED` emitted by codegen
// never enter the link. The `#[cfg(not(test))]` gate on the `extern "C"`
// block above means the runtime test binary has no extern decl to resolve
// — it instead reads the stand-in `static` definitions below directly.
//
// In real karac-build pipelines (compiler emits + runtime statically
// links), codegen's `emit_spawn_sites_metadata` provides the symbols with
// `External` linkage and the runtime's `extern "C"` block resolves to
// them. The two paths never collide because they're cfg-gated apart.
//
// `KARAC_SPAWN_SITES_ENABLED = 1` flips
// `karac_runtime_has_debug_metadata()` to `true` for the corresponding
// runtime test (`test_has_debug_metadata_reads_through_global`). `_LEN = 0`
// makes the `list_par_blocks_into` snapshot-from-table loop a no-op for
// tests that don't bind a real frame.
//
// `SpawnSiteEntryStandIn` wraps `KaracSpawnSiteEntry` so we can express
// `unsafe impl Sync` for the const-static stand-in (raw pointers are
// `!Sync` by default; the wrapper is sound because the entry is read-only
// and the pointer is the null sentinel).
#[cfg(test)]
#[repr(transparent)]
struct SpawnSiteEntryStandIn(KaracSpawnSiteEntry);

#[cfg(test)]
unsafe impl Sync for SpawnSiteEntryStandIn {}

#[cfg(test)]
#[no_mangle]
static KARAC_SPAWN_SITES_ENABLED: u8 = 1;

#[cfg(test)]
#[no_mangle]
static KARAC_SPAWN_SITES_LEN: u32 = 0;

#[cfg(test)]
#[no_mangle]
static KARAC_SPAWN_SITES: SpawnSiteEntryStandIn = SpawnSiteEntryStandIn(KaracSpawnSiteEntry {
    id: 0,
    _pad0: 0,
    file_cstr: std::ptr::null(),
    line: 0,
    col: 0,
    worker_count: 0,
    _reserved: 0,
});

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Runtime unit tests for the Debugger Contract slice 4 surface
    //! (parent-frame ref + `KaracWaitTarget`).
    //!
    //! **Frame-tracking test isolation.** Two distinct hazards force
    //! these tests to serialize on `FRAME_TRACKING_ENV_LOCK`:
    //!
    //! 1. **Env-var races on `KARAC_RUNTIME_DEBUG_METADATA`.** Cargo runs
    //!    tests in parallel, so any test that mutates the var races peers
    //!    reading it. Compounding this, `runtime_debug_metadata_enabled`
    //!    caches its result in a `OnceLock<bool>` — once initialized the
    //!    env read never repeats, so a test mutating the var after another
    //!    test has triggered initialization observes nothing.
    //! 2. **Shared-state races on `ACTIVE_FRAMES`.** The registry is a
    //!    process-global `static Mutex<Vec<FramePtr>>`, not thread-local.
    //!    Any test that pushes frames into it (directly via `FrameGuard`
    //!    or transitively by calling `karac_par_run`) or that reads it
    //!    (directly or via `karac_runtime_list_par_blocks_into` /
    //!    `karac_runtime_for_each_active_frame`) must hold the lock.
    //!    Without this, a reader test can run during another test's
    //!    barrier window and observe frames it shouldn't.
    //!
    //! Resolution: every frame-tracking test acquires
    //! `FRAME_TRACKING_ENV_LOCK` at entry, and the disabled-path test
    //! goes through `runtime_debug_metadata_enabled_uncached` (test-only
    //! re-read that bypasses the cache). This mirrors slice 3's
    //! `SPAWN_SITE_ENV_LOCK` pattern in `tests/codegen.rs`.
    //!
    //! Frame-pointer cross-thread shuttling uses `usize` casts so the
    //! `*const KaracFrame` (which is `!Send`) crosses the thread boundary
    //! as a plain integer; the runtime never relies on Rust's auto-Send
    //! inference for these pointers (the soundness comes from
    //! `thread::scope`'s lifetime guarantee, not Send).
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Barrier, Mutex};

    /// Serializes tests that touch the `KARAC_RUNTIME_DEBUG_METADATA`
    /// env var or the process-global `ACTIVE_FRAMES` registry (read or
    /// write). See the module-level comment for the two hazards.
    static FRAME_TRACKING_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// `KaracWaitTarget` v1 layout pin. Single-variant `None` under
    /// `#[repr(C, u8)]` is one byte total; future variants must be
    /// additive (non-breaking). If this assertion fails, slice 5 / FFI
    /// consumers built against the current layout would mis-read frames.
    #[test]
    fn test_wait_target_size_pinned() {
        assert_eq!(std::mem::size_of::<KaracWaitTarget>(), 1);
    }

    /// Outside any `par {}` block, `karac_runtime_get_current_frame()`
    /// returns null. Pins the root-task discriminator for slice 5.
    #[test]
    fn test_current_frame_null_at_root() {
        // Must run on a fresh thread so an earlier test (e.g.
        // `test_par_block_sets_worker_frame`) hasn't left state on this
        // thread's TLS. We can simply check the value on a freshly
        // spawned thread.
        let observed: usize = std::thread::spawn(|| karac_runtime_get_current_frame() as usize)
            .join()
            .unwrap();
        assert_eq!(observed, 0, "root task should observe null current_frame");
    }

    /// Synthesize a `KaracBranch` whose `func` captures the
    /// `karac_runtime_get_current_frame()` value at the moment the
    /// branch runs, then assert the captured frame has the expected
    /// shape (non-null, root parent, correct `spawn_site_id` /
    /// `worker_index`).
    #[test]
    fn test_par_block_sets_worker_frame() {
        let _guard = FRAME_TRACKING_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        // Captured frame fields per branch — `usize` to cross the Send
        // boundary cleanly. `(parent_addr, spawn_site_id, worker_index)`.
        struct Capture {
            slots: Mutex<Vec<Option<(usize, u32, u32)>>>,
        }
        let capture = Arc::new(Capture {
            slots: Mutex::new(vec![None, None]),
        });

        unsafe extern "C" fn branch_fn(ctx: *mut c_void, _cancel: *const AtomicBool) {
            let frame = karac_runtime_get_current_frame();
            assert!(!frame.is_null(), "worker should see non-null frame");
            let f = unsafe { &*frame };
            // ctx is a `*mut (Arc<Capture>, usize)` — index of this branch.
            let payload = unsafe { &*(ctx as *const (Arc<Capture>, usize)) };
            let mut slots = payload.0.slots.lock().unwrap();
            slots[payload.1] = Some((f.parent as usize, f.spawn_site_id, f.worker_index));
        }

        let mut payloads: Vec<(Arc<Capture>, usize)> =
            (0..2).map(|i| (capture.clone(), i)).collect();
        let branches: Vec<KaracBranch> = payloads
            .iter_mut()
            .map(|p| KaracBranch {
                func: branch_fn,
                ctx: p as *mut _ as *mut c_void,
            })
            .collect();

        unsafe {
            karac_par_run(branches.as_ptr(), branches.len(), 42);
        }

        let slots = capture.slots.lock().unwrap();
        let s0 = slots[0].expect("branch 0 captured no frame");
        let s1 = slots[1].expect("branch 1 captured no frame");
        // Both branches see root parent (null); spawn_site_id == 42 from
        // the call above. Worker indices are 0 and 1 in some order
        // (the work-stealing thread pool doesn't guarantee dispatch
        // order matches branch order, so check the set).
        assert_eq!(s0.0, 0, "branch 0 should have null parent");
        assert_eq!(s1.0, 0, "branch 1 should have null parent");
        assert_eq!(s0.1, 42);
        assert_eq!(s1.1, 42);
        let mut indices = [s0.2, s1.2];
        indices.sort();
        assert_eq!(indices, [0, 1]);
    }

    /// Inner par block invoked from inside an outer par block: the inner
    /// workers' `parent` should point at the outer worker's frame, not
    /// null. Pins the structured-concurrency tree shape that slice 5
    /// walks for `list_par_blocks()`.
    #[test]
    fn test_par_block_nested_parent_chain() {
        let _guard = FRAME_TRACKING_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        // Captured: outer-worker-frame address (the parent the inner
        // workers should observe) and the inner workers' captured
        // parents.
        struct Captures {
            outer_frame_addr: Mutex<Option<usize>>,
            inner_parent_addrs: Mutex<Vec<usize>>,
        }
        let captures = Arc::new(Captures {
            outer_frame_addr: Mutex::new(None),
            inner_parent_addrs: Mutex::new(Vec::new()),
        });

        unsafe extern "C" fn inner_branch_fn(ctx: *mut c_void, _cancel: *const AtomicBool) {
            let frame = karac_runtime_get_current_frame();
            assert!(!frame.is_null());
            let f = unsafe { &*frame };
            let cap = unsafe { &*(ctx as *const Arc<Captures>) };
            cap.inner_parent_addrs
                .lock()
                .unwrap()
                .push(f.parent as usize);
        }

        unsafe extern "C" fn outer_branch_fn(ctx: *mut c_void, _cancel: *const AtomicBool) {
            let frame = karac_runtime_get_current_frame();
            assert!(!frame.is_null());
            *((unsafe { &*(ctx as *const Arc<Captures>) }).outer_frame_addr)
                .lock()
                .unwrap() = Some(frame as usize);

            // Inner par block — two branches, both share the outer ctx.
            let cap = unsafe { &*(ctx as *const Arc<Captures>) };
            let inner_payloads: Vec<Arc<Captures>> = vec![cap.clone(), cap.clone()];
            let inner_branches: Vec<KaracBranch> = inner_payloads
                .iter()
                .map(|p| KaracBranch {
                    func: inner_branch_fn,
                    ctx: p as *const _ as *mut c_void,
                })
                .collect();
            unsafe {
                karac_par_run(inner_branches.as_ptr(), inner_branches.len(), 99);
            }
            // Keep payloads alive for the duration of the inner call.
            drop(inner_payloads);
        }

        // One-branch outer par so we get exactly one outer worker frame.
        // (`emit_par_run`'s codegen-side single-stmt skip doesn't apply
        // here — we're calling the runtime directly.)
        let payload = captures.clone();
        let outer_branches = [KaracBranch {
            func: outer_branch_fn,
            ctx: &payload as *const _ as *mut c_void,
        }];
        unsafe {
            karac_par_run(outer_branches.as_ptr(), outer_branches.len(), 7);
        }

        let outer_addr = captures
            .outer_frame_addr
            .lock()
            .unwrap()
            .expect("outer branch never ran");
        let inner_parents = captures.inner_parent_addrs.lock().unwrap().clone();
        assert_eq!(inner_parents.len(), 2);
        for p in &inner_parents {
            assert_eq!(
                *p, outer_addr,
                "inner worker's parent should match outer worker's frame address"
            );
        }
    }

    /// Long-running par block holds workers at a barrier so the main
    /// thread can call `karac_runtime_for_each_active_frame` and observe
    /// the registry mid-run. After the barrier releases and the par
    /// block joins, the registry must be empty again.
    #[test]
    fn test_active_frames_register_during_par() {
        let _guard = FRAME_TRACKING_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        // Three workers all wait on the same barrier (start: workers
        // wait; main thread observes registry; main thread releases).
        let barrier_workers = Arc::new(Barrier::new(4)); // 3 workers + 1 main
        let barrier_done = Arc::new(Barrier::new(4));

        struct Payload {
            start: Arc<Barrier>,
            done: Arc<Barrier>,
        }

        unsafe extern "C" fn branch_fn(ctx: *mut c_void, _cancel: *const AtomicBool) {
            let p = unsafe { &*(ctx as *const Payload) };
            p.start.wait();
            // Hold here until main signals via `done` so the registry
            // observation happens between the two barriers.
            p.done.wait();
        }

        let payloads: Vec<Payload> = (0..3)
            .map(|_| Payload {
                start: barrier_workers.clone(),
                done: barrier_done.clone(),
            })
            .collect();
        let branches: Vec<KaracBranch> = payloads
            .iter()
            .map(|p| KaracBranch {
                func: branch_fn,
                ctx: p as *const _ as *mut c_void,
            })
            .collect();

        // Run the par block on a side thread so this thread can observe
        // `ACTIVE_FRAMES` while it's populated.
        let branches_addr = branches.as_ptr() as usize;
        let count = branches.len();
        // Branches' `func` is fn-pointer (`Send`) and `ctx` points into
        // payloads which live for the test's stack frame; the side
        // thread joins before the test returns.
        let runner = std::thread::spawn(move || {
            // SAFETY: payloads / branches outlive this thread (joined
            // before the test function returns).
            unsafe {
                karac_par_run(branches_addr as *const KaracBranch, count, 11);
            }
        });

        // Wait for all workers to register their frames.
        barrier_workers.wait();

        // Count active frames via the iteration callback.
        struct Counter {
            count: u32,
        }
        unsafe extern "C" fn counter_cb(_frame: *const KaracFrame, ud: *mut c_void) {
            let c = unsafe { &mut *(ud as *mut Counter) };
            c.count += 1;
        }
        let mut counter = Counter { count: 0 };
        unsafe {
            karac_runtime_for_each_active_frame(counter_cb, &mut counter as *mut _ as *mut c_void);
        }
        assert_eq!(
            counter.count, 3,
            "expected 3 active frames during par run, got {}",
            counter.count
        );

        // Release workers and wait for join.
        barrier_done.wait();
        runner.join().unwrap();

        // Registry empty after join.
        let mut after = Counter { count: 0 };
        unsafe {
            karac_runtime_for_each_active_frame(counter_cb, &mut after as *mut _ as *mut c_void);
        }
        assert_eq!(
            after.count, 0,
            "expected empty active-frame registry after par join, got {}",
            after.count
        );

        // Keep payloads alive until here.
        drop(payloads);
    }

    /// `KARAC_RUNTIME_DEBUG_METADATA=0` flips the gate off — workers see
    /// null `current_frame` and `ACTIVE_FRAMES` stays empty. Goes through
    /// the test-only `runtime_debug_metadata_enabled_uncached` path so
    /// the env-var mutation actually takes effect (the production
    /// `OnceLock`-cached helper would freeze whichever value the first
    /// slice-4 test observed).
    #[test]
    fn test_runtime_debug_metadata_disabled_skips_tracking() {
        let _guard = FRAME_TRACKING_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let prior = std::env::var("KARAC_RUNTIME_DEBUG_METADATA").ok();
        std::env::set_var("KARAC_RUNTIME_DEBUG_METADATA", "0");
        let observed = runtime_debug_metadata_enabled_uncached();
        // Restore env var before any further code can observe it.
        match prior {
            Some(v) => std::env::set_var("KARAC_RUNTIME_DEBUG_METADATA", v),
            None => std::env::remove_var("KARAC_RUNTIME_DEBUG_METADATA"),
        }
        assert!(
            !observed,
            "expected runtime_debug_metadata_enabled_uncached() == false when env=0"
        );
    }

    /// `wait_target` is `None` for every v1 frame. Pins the contract —
    /// when Phase 6.3 ships real suspension and starts setting other
    /// variants, this test fails and signals the surface change.
    #[test]
    fn test_wait_target_always_none_in_v1() {
        let _guard = FRAME_TRACKING_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        struct Capture {
            tags: Mutex<Vec<u8>>,
        }
        let capture = Arc::new(Capture {
            tags: Mutex::new(Vec::new()),
        });

        unsafe extern "C" fn branch_fn(ctx: *mut c_void, _cancel: *const AtomicBool) {
            let frame = karac_runtime_get_current_frame();
            assert!(!frame.is_null());
            // Read the discriminant byte directly per the
            // `#[repr(C, u8)]` layout (tag at offset 0 of the
            // `wait_target` field). This is the FFI-stable read path
            // slice 5 / future debuggers will use, so a test that goes
            // through the discriminant byte verifies the same wire
            // shape.
            let f = unsafe { &*frame };
            let tag_byte = unsafe { *(&f.wait_target as *const KaracWaitTarget as *const u8) };
            unsafe { &*(ctx as *const Arc<Capture>) }
                .tags
                .lock()
                .unwrap()
                .push(tag_byte);
        }

        let payload = capture.clone();
        let branches = [
            KaracBranch {
                func: branch_fn,
                ctx: &payload as *const _ as *mut c_void,
            },
            KaracBranch {
                func: branch_fn,
                ctx: &payload as *const _ as *mut c_void,
            },
        ];
        unsafe {
            karac_par_run(branches.as_ptr(), branches.len(), 0);
        }

        let tags = capture.tags.lock().unwrap();
        assert_eq!(tags.len(), 2);
        for t in tags.iter() {
            // `KaracWaitTarget::None` is the only variant; under
            // `#[repr(C, u8)]` it has discriminant 0.
            assert_eq!(*t, 0, "v1 wait_target must always be KaracWaitTarget::None");
        }
    }

    /// `FrameGuard::drop` runs on the unwind path, so a frame is
    /// deregistered from `ACTIVE_FRAMES` even when the body between
    /// guard construction and guard drop panics. Pins the defer-style
    /// teardown against future regression.
    ///
    /// Note: we test `FrameGuard` directly rather than going through
    /// `karac_par_run` because the worker's `func` is `unsafe extern "C"`
    /// and Rust 1.81+ aborts on panics that cross a non-unwinding FFI
    /// boundary — codegen-emitted Kāra branches never panic across the
    /// FFI surface in practice (Kāra has its own panic protocol).
    /// What this test validates is the runtime-internal contract: if
    /// `FrameGuard` is alive and its scope unwinds, the registry is
    /// cleaned up. That's the whole reason for the RAII shape.
    #[test]
    fn test_frame_deregistered_on_panic() {
        let _guard = FRAME_TRACKING_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        struct Counter {
            count: u32,
        }
        unsafe extern "C" fn counter_cb(_frame: *const KaracFrame, ud: *mut c_void) {
            let c = unsafe { &mut *(ud as *mut Counter) };
            c.count += 1;
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let frame = KaracFrame {
                parent: std::ptr::null(),
                spawn_site_id: 99,
                worker_index: 0,
                wait_target: KaracWaitTarget::None,
            };
            let _g = FrameGuard::new(&frame);
            // While the guard is alive the registry should hold one
            // entry. Sanity-check before we panic.
            let mut mid = Counter { count: 0 };
            unsafe {
                karac_runtime_for_each_active_frame(counter_cb, &mut mid as *mut _ as *mut c_void);
            }
            assert_eq!(mid.count, 1, "guard alive; registry should hold 1 entry");
            panic!("intentional panic — `FrameGuard::drop` must still fire");
        }));
        assert!(
            result.is_err(),
            "expected panic to bubble out of catch_unwind"
        );

        // After the guard's scope unwinds, the registry is empty.
        let mut after = Counter { count: 0 };
        unsafe {
            karac_runtime_for_each_active_frame(counter_cb, &mut after as *mut _ as *mut c_void);
        }
        assert_eq!(
            after.count, 0,
            "FrameGuard::drop must run on unwind; found {} active after panic",
            after.count
        );
    }

    // ── Slice 5 layout pins ────────────────────────────────────────
    //
    // The `KaracParBlockInfo` `#[repr(C)]` layout must match what
    // user-side codegen would emit for the baked-stdlib `ParBlockInfo`
    // struct (`runtime/stdlib/runtime.kara`). LLVM lays out
    // `{ i32, {ptr, i64, i64}, i32, i32, i32 }` with explicit alignment
    // padding; if Rust's `#[repr(C)]` rules ever diverge from LLVM's
    // `target-data-layout` defaults on a supported target, the runtime
    // would silently mis-write entries and slice 5's `list_par_blocks()`
    // would return garbage. These two tests are the canary.

    #[test]
    fn test_par_block_info_size_pinned() {
        // Expected: { i32 (4) + 4 pad + KaracString (24) + 3*i32 (12) + 4 pad } = 48
        assert_eq!(
            std::mem::size_of::<KaracParBlockInfo>(),
            48,
            "KaracParBlockInfo size drift — codegen would mis-stride; \
             check field order vs `runtime/stdlib/runtime.kara`'s ParBlockInfo"
        );
    }

    #[test]
    fn test_par_block_info_field_offsets_pinned() {
        // Field offsets the LLVM layout produces:
        //   spawn_site_id: 0
        //   file:          8 (after 4 bytes of alignment padding)
        //   line:         32
        //   col:          36
        //   worker_count: 40
        assert_eq!(std::mem::offset_of!(KaracParBlockInfo, spawn_site_id), 0);
        assert_eq!(std::mem::offset_of!(KaracParBlockInfo, file), 8);
        assert_eq!(std::mem::offset_of!(KaracParBlockInfo, line), 32);
        assert_eq!(std::mem::offset_of!(KaracParBlockInfo, col), 36);
        assert_eq!(std::mem::offset_of!(KaracParBlockInfo, worker_count), 40);
    }

    #[test]
    fn test_spawn_site_entry_layout_pinned() {
        // Mirrors the LLVM struct layout in `Codegen::emit_spawn_sites_metadata`:
        //   { i32 id, ptr file_cstr, i32 line, i32 col, i32 worker_count, i32 reserved }
        // Expected total size 32 bytes (8-byte alignment from the pointer).
        assert_eq!(std::mem::size_of::<KaracSpawnSiteEntry>(), 32);
        assert_eq!(std::mem::offset_of!(KaracSpawnSiteEntry, id), 0);
        assert_eq!(std::mem::offset_of!(KaracSpawnSiteEntry, file_cstr), 8);
        assert_eq!(std::mem::offset_of!(KaracSpawnSiteEntry, line), 16);
        assert_eq!(std::mem::offset_of!(KaracSpawnSiteEntry, col), 20);
        assert_eq!(std::mem::offset_of!(KaracSpawnSiteEntry, worker_count), 24);
        assert_eq!(std::mem::offset_of!(KaracSpawnSiteEntry, _reserved), 28);
    }

    #[test]
    fn test_has_debug_metadata_reads_through_global() {
        // The runtime crate's `karac_runtime_has_debug_metadata` reads
        // `KARAC_SPAWN_SITES_ENABLED` directly. In the runtime test
        // binary we provide a strong-linkage definition of the slice-3
        // globals (see the `#[no_mangle]` block at the top of this
        // test module) so the reader resolves cleanly under
        // `cargo test -p karac-runtime`. The test confirms the value
        // we set flows through: 1 → true.
        let value = karac_runtime_has_debug_metadata();
        // The test-side stand-in below sets ENABLED to 1.
        assert!(
            value,
            "expected has_debug_metadata to read true via stand-in"
        );
    }

    #[test]
    fn test_list_par_blocks_into_empty_outside_par() {
        // Slice 5: `karac_runtime_list_par_blocks_into` writes
        // `{null, 0, 0}` when `ACTIVE_FRAMES` is empty. Validates the
        // empty-fast-path branch.
        //
        // Holds `FRAME_TRACKING_ENV_LOCK` because peer tests
        // (e.g. `test_active_frames_register_during_par`) push worker
        // frames into the process-global `ACTIVE_FRAMES` and park on a
        // barrier — without the lock this test races them and observes
        // a non-empty registry, taking the allocation path instead of
        // the fast path.
        let _guard = FRAME_TRACKING_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let mut out = KaracVec {
            data: std::ptr::null_mut(),
            len: -1,
            cap: -1,
        };
        unsafe {
            karac_runtime_list_par_blocks_into(&mut out as *mut _);
        }
        assert!(out.data.is_null(), "expected null data on empty");
        assert_eq!(out.len, 0, "expected len=0 on empty");
        assert_eq!(out.cap, 0, "expected cap=0 on empty");
    }

    #[test]
    fn test_list_par_blocks_into_null_out_safe() {
        // Defensive: passing `null` as the out-pointer is a no-op
        // rather than UB. The compiler always allocates the slot, so
        // this should never happen in practice — but the runtime
        // explicitly returns early to avoid a deref crash if a
        // future codegen bug regresses the alloca path.
        unsafe {
            karac_runtime_list_par_blocks_into(std::ptr::null_mut());
        }
        // No assertion — the test passes by not crashing.
    }

    // ── Provider stack tests (Theme 6 sub-step 1) ──────────────────────────

    /// `karac_provider_lookup` returns null + null when the per-task stack
    /// is empty — codegen branches on this for the structured-panic call.
    #[test]
    fn test_provider_lookup_returns_null_on_empty_stack() {
        // Defensive: any earlier test on this thread might have left the
        // stack non-empty. Pop until empty before asserting.
        while !PROVIDER_STACK_HEAD.with(|c| c.get()).is_null() {
            karac_provider_pop();
        }
        let result = karac_provider_lookup(42);
        assert!(result.data.is_null());
        assert!(result.vtable.is_null());
    }

    /// `push` / `lookup` / `pop` round-trip on a single frame: lookup
    /// finds the just-pushed frame; pop unlinks it; subsequent lookup
    /// misses.
    #[test]
    fn test_provider_push_lookup_pop_roundtrip() {
        while !PROVIDER_STACK_HEAD.with(|c| c.get()).is_null() {
            karac_provider_pop();
        }
        let mut frame = ProviderFrame {
            prev: std::ptr::null(),
            resource_id: 0,
            provider_data_ptr: std::ptr::null(),
            vtable_ptr: std::ptr::null(),
        };
        let data: u64 = 0xCAFE_BABE;
        unsafe {
            karac_provider_push(
                &mut frame as *mut ProviderFrame,
                7,
                &data as *const u64 as *const u8,
                std::ptr::null::<VTable>(),
            );
        }
        let hit = karac_provider_lookup(7);
        assert!(!hit.data.is_null());
        assert_eq!(hit.data as *const u64, &data as *const u64);

        karac_provider_pop();
        let miss = karac_provider_lookup(7);
        assert!(miss.data.is_null());
    }

    /// Nested pushes: lookup returns the innermost (most-recently-pushed)
    /// binding. Pop unwinds to the outer binding.
    #[test]
    fn test_provider_stack_innermost_wins() {
        while !PROVIDER_STACK_HEAD.with(|c| c.get()).is_null() {
            karac_provider_pop();
        }
        let outer_data: u64 = 100;
        let inner_data: u64 = 200;
        let mut outer = ProviderFrame {
            prev: std::ptr::null(),
            resource_id: 0,
            provider_data_ptr: std::ptr::null(),
            vtable_ptr: std::ptr::null(),
        };
        let mut inner = ProviderFrame {
            prev: std::ptr::null(),
            resource_id: 0,
            provider_data_ptr: std::ptr::null(),
            vtable_ptr: std::ptr::null(),
        };
        unsafe {
            karac_provider_push(
                &mut outer,
                3,
                &outer_data as *const u64 as *const u8,
                std::ptr::null::<VTable>(),
            );
            karac_provider_push(
                &mut inner,
                3,
                &inner_data as *const u64 as *const u8,
                std::ptr::null::<VTable>(),
            );
        }
        let hit = karac_provider_lookup(3);
        assert_eq!(hit.data as *const u64, &inner_data as *const u64);

        karac_provider_pop();
        let outer_hit = karac_provider_lookup(3);
        assert_eq!(outer_hit.data as *const u64, &outer_data as *const u64);

        karac_provider_pop();
        let miss = karac_provider_lookup(3);
        assert!(miss.data.is_null());
    }

    /// `set_stack_head` + `get_stack_head` round-trip the per-task head
    /// pointer — used by par-block worker branches to inherit the parent
    /// thread's stack.
    #[test]
    fn test_provider_set_and_get_stack_head() {
        while !PROVIDER_STACK_HEAD.with(|c| c.get()).is_null() {
            karac_provider_pop();
        }
        assert!(karac_provider_get_stack_head().is_null());

        let mut frame = ProviderFrame {
            prev: std::ptr::null(),
            resource_id: 0,
            provider_data_ptr: std::ptr::null(),
            vtable_ptr: std::ptr::null(),
        };
        unsafe {
            karac_provider_push(&mut frame, 1, std::ptr::null(), std::ptr::null::<VTable>());
        }
        let head = karac_provider_get_stack_head();
        assert!(!head.is_null());
        assert_eq!(head, &frame as *const ProviderFrame);

        unsafe {
            karac_provider_set_stack_head(std::ptr::null());
        }
        assert!(karac_provider_get_stack_head().is_null());

        // Restore for cleanup
        unsafe {
            karac_provider_set_stack_head(head);
        }
        karac_provider_pop();
    }
}
