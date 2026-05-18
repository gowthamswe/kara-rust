//! Network event loop. Cross-platform abstraction over the OS-level
//! fd-readiness facilities — `epoll` on Linux, `kqueue` on macOS / BSD,
//! `IOCP` on Windows — via the `mio` crate.
//!
//! See `docs/design.md § Network Event Loop and State-Machine Transform`
//! and `docs/implementation_checklist/phase-6-runtime.md` line 15.
//!
//! ## v1 architectural commitments (per phase-6-runtime.md line 15)
//!
//! - **One event loop per process.** v1 runs exactly one loop; M2 / M3
//!   may shard across multiple loops to reach the 1M+ idle-connection
//!   target. The type is `Sync` — shared via `Arc<EventLoop>` from any
//!   thread — with two interior Mutexes that split the polling and
//!   registration code paths so a long-blocking `run_once` (held by
//!   the background poller thread, slice 3) does not block concurrent
//!   register / deregister calls.
//! - **Registration / de-registration are crate-internal.** The public
//!   language surface stays effect-typed (`sends(Network)` /
//!   `receives(Network)`); codegen lowers those effects into runtime
//!   calls. End-user Kāra code never sees this module's API.
//! - **Per-fd state holds the parked-task pointer + I/O direction +
//!   optional deadline.** Readiness wakeups carry the parked pointer
//!   back to the scheduler so it can resume the right task. The
//!   `deadline` field is stored so the timer-wheel work (M2 polish
//!   layer) can drive expiry-based wakeups without re-shaping the
//!   per-fd state.
//! - **Cross-thread wakeup via `mio::Waker`.** Under the hood `mio` uses
//!   `eventfd` on Linux, a pipe pair on BSD / macOS, and a posted IOCP
//!   completion packet on Windows — the three OS primitives the
//!   phase-6 entry calls out by name.

use mio::{Events, Interest, Poll, Token};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Direction(s) of I/O readiness we are polling on a given fd.
///
/// `repr(u8)` pins the discriminant width for stable FFI when the
/// codegen parking-emit path (phase-6 line 17) starts passing this
/// enum across the runtime boundary.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IoDirection {
    Read = 0,
    Write = 1,
    ReadWrite = 2,
}

impl IoDirection {
    fn to_interest(self) -> Interest {
        match self {
            IoDirection::Read => Interest::READABLE,
            IoDirection::Write => Interest::WRITABLE,
            IoDirection::ReadWrite => Interest::READABLE.add(Interest::WRITABLE),
        }
    }
}

/// Opaque handle returned by [`EventLoop::register`]. Caller hands it
/// back to [`EventLoop::deregister`] to remove the registration and
/// receives it through [`Wakeup`] when the fd becomes ready.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct RegistrationToken(usize);

/// Per-fd state stored inside the loop.
///
/// `parked` is type-erased to `*mut c_void` because the runtime
/// representation of a parked task is owned by the codegen parking
/// path (phase-6 line 17 — effect-routed task parking), not by the
/// event loop. The loop only stores and forwards it; correlating it
/// back to a concrete task type is the scheduler's responsibility.
struct FdState {
    parked: *mut c_void,
    direction: IoDirection,
    /// Optional deadline. Stored as part of the v1 per-fd state per
    /// the phase-6 line 15 spec. Timer-driven expiry is the M2 polish
    /// layer's work (timer wheel); v1 stores the field so M2 hooks
    /// into the existing per-fd state map without re-shaping it.
    #[allow(dead_code)]
    deadline: Option<Instant>,
}

// SAFETY: `parked` is a pointer owned by the codegen parking path /
// scheduler that registered the fd. The event loop never derefs it;
// it only stores it and hands it back through `Wakeup` when the fd
// becomes ready. The owner guarantees the pointer is valid from the
// `register` call until the corresponding `deregister` returns (or
// until the readiness wakeup is observed and consumed). `Send` is
// required because the `EventLoop` is shared across threads as
// `Arc<EventLoop>`, so the inner `fds` HashMap (storing `FdState`)
// must itself be `Send` to live inside the `Mutex`.
unsafe impl Send for FdState {}

/// A readiness wakeup surfaced by [`EventLoop::run_once`].
///
/// Carries the parked-task pointer the caller registered with so the
/// scheduler can resume the right task. The `direction` field reports
/// which side of a `ReadWrite` registration actually fired (`mio`
/// surfaces these independently — a `ReadWrite` registration can wake
/// up with just `Read` or just `Write` ready).
#[derive(Debug)]
pub struct Wakeup {
    pub token: RegistrationToken,
    pub parked: *mut c_void,
    pub direction: IoDirection,
}

/// Event loop. `Sync` — register / deregister / wake from any thread;
/// `run_once` serializes via an interior Mutex.
///
/// Per the v1 architectural commitment, exactly one loop runs per
/// process. The interior splits state into two independently-locked
/// halves so the long-blocking `run_once` (which holds the `poll`
/// Mutex through the entire `mio::Poll::poll` call) does **not**
/// block registration (which acquires only the `fds` Mutex briefly).
/// This is what makes the background-poller architecture (slice 3)
/// safe: the poller thread blocks indefinitely in `run_once` while
/// other threads continue to register / deregister fds against the
/// same loop.
pub struct EventLoop {
    /// Owned clone of `mio::Poll`'s registry. `mio::Registry` is
    /// `Sync`, so register / deregister calls from arbitrary threads
    /// hit the OS-level registration syscalls without external
    /// synchronization — the only thing we lock is the `fds`
    /// HashMap below.
    registry: mio::Registry,
    /// Cross-thread waker handle. `mio::Waker` is `Sync` (uses
    /// eventfd / pipe / IOCP-post under the hood).
    waker: Arc<mio::Waker>,
    /// Poll instance + events buffer. Only `run_once` touches this;
    /// the Mutex enforces single-polling-thread-at-a-time.
    poll: Mutex<EventLoopPoll>,
    /// Per-fd state + token allocator. Briefly locked by register /
    /// deregister, and during the post-poll wakeup-extraction phase
    /// of `run_once`.
    fds: Mutex<EventLoopFds>,
}

struct EventLoopPoll {
    poll: Poll,
    events: Events,
}

struct EventLoopFds {
    by_token: HashMap<Token, FdState>,
    /// Monotonically increasing source of unique tokens. Reserved
    /// values: `0` is the cross-thread waker (see [`WAKER_TOKEN`]);
    /// user-fd tokens start at `1`.
    next_token: usize,
}

const WAKER_TOKEN: Token = Token(0);

impl EventLoop {
    /// Construct a new event loop. Allocates the underlying `mio::Poll`,
    /// clones its registry handle, and registers the cross-thread waker.
    pub fn new() -> io::Result<Self> {
        let poll = Poll::new()?;
        let registry = poll.registry().try_clone()?;
        let waker = Arc::new(mio::Waker::new(poll.registry(), WAKER_TOKEN)?);
        Ok(EventLoop {
            registry,
            waker,
            poll: Mutex::new(EventLoopPoll {
                poll,
                events: Events::with_capacity(256),
            }),
            fds: Mutex::new(EventLoopFds {
                by_token: HashMap::new(),
                next_token: 1,
            }),
        })
    }

    /// Return a clonable handle that other threads can use to wake
    /// the loop. The handle holds the `mio::Waker` internally; clones
    /// are cheap (a single `Arc` bump).
    pub fn handle(&self) -> EventLoopHandle {
        EventLoopHandle {
            waker: Arc::clone(&self.waker),
        }
    }

    /// Register a source with the loop.
    ///
    /// `parked` is the opaque task pointer that will be returned
    /// through [`Wakeup`] when the fd becomes ready. The event loop
    /// stores it but does not deref it; lifetime is the caller's
    /// responsibility (the codegen parking path / scheduler).
    ///
    /// Acquires only the `fds` Mutex briefly — concurrent `run_once`
    /// calls are unaffected (different Mutex).
    pub fn register<S: mio::event::Source + ?Sized>(
        &self,
        source: &mut S,
        direction: IoDirection,
        deadline: Option<Instant>,
        parked: *mut c_void,
    ) -> io::Result<RegistrationToken> {
        let mut fds = self.fds.lock().unwrap_or_else(|p| p.into_inner());
        let token = Token(fds.next_token);
        fds.next_token = fds
            .next_token
            .checked_add(1)
            .expect("event loop token exhaustion (usize wrap)");
        // mio::Registry is Sync — safe to call without holding any
        // additional lock. We still hold the fds lock through this
        // call so the HashMap insert and OS-level registration appear
        // atomic to other threads.
        self.registry
            .register(source, token, direction.to_interest())?;
        fds.by_token.insert(
            token,
            FdState {
                parked,
                direction,
                deadline,
            },
        );
        Ok(RegistrationToken(token.0))
    }

    /// Remove a registration.
    ///
    /// `mio::Registry::deregister` takes the source itself (the OS
    /// needs the fd, not just our token), so the caller must hand
    /// back the source it registered. Removing the token from our
    /// internal map is unconditional — a `RegistrationToken` produced
    /// by this loop is always present unless it has already been
    /// deregistered, in which case removing again is a silent no-op.
    ///
    /// Acquires only the `fds` Mutex briefly.
    pub fn deregister<S: mio::event::Source + ?Sized>(
        &self,
        source: &mut S,
        token: RegistrationToken,
    ) -> io::Result<()> {
        let mut fds = self.fds.lock().unwrap_or_else(|p| p.into_inner());
        self.registry.deregister(source)?;
        fds.by_token.remove(&Token(token.0));
        Ok(())
    }

    /// Drive the loop once.
    ///
    /// Blocks until at least one fd is ready, the cross-thread waker
    /// fires, or `max_wait` elapses. Returns the readiness wakeups
    /// observed in this iteration.
    ///
    /// - `max_wait = None`: block until any wakeup.
    /// - `max_wait = Some(Duration::ZERO)`: poll without blocking.
    /// - `max_wait = Some(d)`: block up to `d`.
    ///
    /// Cross-thread waker events are filtered out of the returned
    /// `Vec` — they exist only to unblock `poll` so the caller can
    /// re-check any external state (new registrations queued by the
    /// scheduler, cancellation, shutdown). An empty return with no
    /// readiness wakeups indicates a waker or timeout wakeup.
    ///
    /// **Locking.** Holds the `poll` Mutex throughout (so only one
    /// thread polls at a time). Acquires the `fds` Mutex briefly
    /// after the poll syscall returns, to translate ready events
    /// into [`Wakeup`]s. Lock order is consistently poll → fds.
    pub fn run_once(&self, max_wait: Option<Duration>) -> io::Result<Vec<Wakeup>> {
        let mut poll_guard = self.poll.lock().unwrap_or_else(|p| p.into_inner());
        let EventLoopPoll {
            ref mut poll,
            ref mut events,
        } = *poll_guard;
        poll.poll(events, max_wait)?;
        let fds = self.fds.lock().unwrap_or_else(|p| p.into_inner());
        let mut wakeups = Vec::new();
        for event in events.iter() {
            if event.token() == WAKER_TOKEN {
                continue;
            }
            let Some(state) = fds.by_token.get(&event.token()) else {
                continue;
            };
            let direction = if event.is_readable() && event.is_writable() {
                IoDirection::ReadWrite
            } else if event.is_readable() {
                IoDirection::Read
            } else if event.is_writable() {
                IoDirection::Write
            } else {
                state.direction
            };
            wakeups.push(Wakeup {
                token: RegistrationToken(event.token().0),
                parked: state.parked,
                direction,
            });
        }
        Ok(wakeups)
    }

    #[cfg(test)]
    fn registered_count(&self) -> usize {
        self.fds
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .by_token
            .len()
    }
}

/// Cross-thread waker handle.
///
/// Clone freely across threads. Calling [`wake`](Self::wake) interrupts
/// the corresponding [`EventLoop::run_once`] call (or makes the next
/// call return immediately if the loop is not currently parked).
/// Multiple `wake` calls between polls coalesce into a single wakeup —
/// this is `mio::Waker`'s documented semantics, not an accident.
#[derive(Clone)]
pub struct EventLoopHandle {
    waker: Arc<mio::Waker>,
}

impl EventLoopHandle {
    pub fn wake(&self) -> io::Result<()> {
        self.waker.wake()
    }
}

// ── Process-global event loop + FFI surface ────────────────────────────────
//
// Phase 6 line 17 (effect-routed task parking) — slice 1: runtime FFI
// surface. The `extern "C"` entry points below are what codegen-emitted
// IR will call into when it lowers a network-effecting function call to
// "register fd with event loop, park current task, yield." The actual
// "park / yield" mechanism (state-machine transform — phase-6 line 18)
// and the scheduler-side wakeup-to-worker-queue glue land as follow-up
// slices; this slice exposes only the ABI codegen will emit against.
//
// **Threading model (v1, slice 3+).** The process-global event loop is
// stored as an `Arc<EventLoop>` and is itself `Sync`. Register /
// deregister calls from any thread acquire only the inner `fds` Mutex
// briefly; `run_once` acquires the inner `poll` Mutex for the duration
// of the blocking poll. Because the two locks are independent, a
// long-blocking poll (the background poller thread in slice 3+) does
// not block concurrent registrations.
//
// **Platform scope.** The fd-registration FFI fns are `#[cfg(unix)]`
// only — Linux / macOS / BSD. Windows IOCP uses a completion-based
// model rather than fd-readiness, so its FFI surface looks different
// and lands separately. The `poll` and `wake` fns are cross-platform
// (mio's `Poll` / `Waker` work everywhere).

/// Process-global event loop instance, lazily initialized.
/// Per the v1 architectural commitment: exactly one EventLoop per process.
static EVENT_LOOP: OnceLock<Arc<EventLoop>> = OnceLock::new();

/// Cached handle to the process-global event loop's waker. Populated
/// during the same `OnceLock::get_or_init` that constructs `EVENT_LOOP`,
/// so observing `EVENT_LOOP` initialized implies `EVENT_LOOP_HANDLE` is
/// also set.
static EVENT_LOOP_HANDLE: OnceLock<EventLoopHandle> = OnceLock::new();

fn global_event_loop() -> &'static Arc<EventLoop> {
    EVENT_LOOP.get_or_init(|| {
        let ev = EventLoop::new().expect("karac_runtime: process-global event loop init failed");
        let arc = Arc::new(ev);
        // `set` may already have been populated by a racing initializer if
        // two threads called this concurrently; the `OnceLock::get_or_init`
        // contract guarantees we are the unique initializer of `EVENT_LOOP`,
        // but the handle write is a separate `OnceLock`, so ignore a
        // duplicate-set error.
        let _ = EVENT_LOOP_HANDLE.set(arc.handle());
        arc
    })
}

/// Readiness wakeup entry written into the caller-allocated buffer by
/// [`karac_runtime_event_loop_poll`].
///
/// `direction` encoding matches the [`IoDirection`] discriminant:
/// 0 = Read, 1 = Write, 2 = ReadWrite. `repr(C)` pins the layout for
/// the codegen-emitted struct that consumes this on the Kāra side.
#[repr(C)]
pub struct KaracWakeup {
    pub token: u64,
    pub parked: *mut c_void,
    pub direction: u8,
}

// SAFETY: `parked` is opaque to the runtime — stored at register time
// and handed back through this value at wakeup time. The original
// caller (codegen parking path) owns the pointer's lifetime and any
// thread-safety concerns; the runtime moves `KaracWakeup` across
// threads only when the background poller (slice 3) queues a wakeup
// for consumption by a scheduler thread, and the pointer crosses
// unchanged.
unsafe impl Send for KaracWakeup {}

/// Register a raw fd with the process-global event loop.
///
/// `direction`: 0 = Read, 1 = Write, 2 = ReadWrite. Any other value
/// returns 0 (invalid input).
///
/// `parked`: opaque pointer the runtime stores and hands back through
/// [`KaracWakeup::parked`] on readiness. The runtime never derefs it;
/// the caller (codegen parking path) owns its lifetime.
///
/// Returns a non-zero registration token on success, 0 on failure.
/// Token 0 is reserved for the cross-thread waker and is never
/// returned by this fn even on success.
///
/// Unix-only — `mio::unix::SourceFd` is the cross-Unix raw-fd wrapper
/// that mio uses to talk to epoll / kqueue. Windows IOCP integration
/// is a separate slice (different fd model).
#[cfg(unix)]
#[no_mangle]
pub extern "C" fn karac_runtime_event_loop_register_fd(
    raw_fd: i32,
    direction: u8,
    parked: *mut c_void,
) -> u64 {
    let dir = match direction {
        0 => IoDirection::Read,
        1 => IoDirection::Write,
        2 => IoDirection::ReadWrite,
        _ => return 0,
    };
    let mut source = mio::unix::SourceFd(&raw_fd);
    let ev = global_event_loop();
    match ev.register(&mut source, dir, None, parked) {
        Ok(token) => token.0 as u64,
        Err(_) => 0,
    }
}

/// Deregister a previously registered fd.
///
/// `raw_fd` must match the fd passed at register time. `token` is the
/// value returned by [`karac_runtime_event_loop_register_fd`].
///
/// Returns 0 on success, -1 on error.
#[cfg(unix)]
#[no_mangle]
pub extern "C" fn karac_runtime_event_loop_deregister_fd(raw_fd: i32, token: u64) -> i32 {
    let mut source = mio::unix::SourceFd(&raw_fd);
    let ev = global_event_loop();
    match ev.deregister(&mut source, RegistrationToken(token as usize)) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Drive the event loop once.
///
/// - `max_wait_nanos = -1`: block indefinitely until any wakeup.
/// - `max_wait_nanos = 0`: poll without blocking.
/// - `max_wait_nanos > 0`: wait up to `n` nanoseconds.
/// - Any other negative value: poll without blocking (treated as 0).
///
/// `wakeups_out` is a caller-allocated buffer of capacity `max_wakeups`.
/// Returns the number of wakeups written (bounded by `max_wakeups`).
/// If more than `max_wakeups` events are ready, the excess are dropped
/// — caller can re-poll with `max_wait_nanos = 0` to drain.
///
/// **Caller invariant:** only one thread calls this at a time —
/// typically the scheduler's dedicated event-loop thread. Concurrent
/// calls serialize through the global mutex (correct but contended).
///
/// # Safety
///
/// `wakeups_out` must point to a writable buffer of at least
/// `max_wakeups` × `sizeof(KaracWakeup)` bytes. `max_wakeups = 0`
/// with a null `wakeups_out` is permitted (no writes).
#[no_mangle]
pub unsafe extern "C" fn karac_runtime_event_loop_poll(
    max_wait_nanos: i64,
    wakeups_out: *mut KaracWakeup,
    max_wakeups: usize,
) -> usize {
    let max_wait = match max_wait_nanos {
        -1 => None,
        n if n > 0 => Some(Duration::from_nanos(n as u64)),
        _ => Some(Duration::ZERO),
    };
    // If the background poller thread is running it owns polling — direct
    // FFI poll callers get back an empty result so they fall through to
    // `karac_runtime_event_loop_take_wakeups` instead of contending for
    // the inner poll Mutex (which the background thread holds for the
    // duration of its blocking call).
    if BACKGROUND_POLLER
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_some()
    {
        return 0;
    }
    let ev = global_event_loop();
    let wakeups = match ev.run_once(max_wait) {
        Ok(w) => w,
        Err(_) => return 0,
    };
    let n = wakeups.len().min(max_wakeups);
    for (i, w) in wakeups.iter().take(n).enumerate() {
        // SAFETY: the caller's contract is that `wakeups_out` points
        // to a writable buffer of at least `max_wakeups` elements;
        // we write at offset `i < n <= max_wakeups`, so the offset
        // is in bounds.
        unsafe {
            wakeups_out.add(i).write(KaracWakeup {
                token: w.token.0 as u64,
                parked: w.parked,
                direction: w.direction as u8,
            });
        }
    }
    n
}

/// Wake the process-global event loop from a non-event-loop thread.
///
/// Returns 0 on success, -1 on error. Coalesces with other pending
/// wakes (`mio::Waker`'s documented behavior — multiple wakes between
/// polls produce one event).
///
/// Idempotent before init: if the event loop has not been initialized
/// yet, this triggers init and then wakes. (The new loop has nothing
/// parked, so the wake is a no-op for the next poll but is still
/// "successful.")
#[no_mangle]
pub extern "C" fn karac_runtime_event_loop_wake() -> i32 {
    // Ensure init so EVENT_LOOP_HANDLE is populated.
    let _ = global_event_loop();
    match EVENT_LOOP_HANDLE.get() {
        Some(h) => match h.wake() {
            Ok(()) => 0,
            Err(_) => -1,
        },
        None => -1,
    }
}

// ── Parked-task ABI (Phase 6 line 17 slice 2) ──────────────────────────────
//
// Repr-C ABI codegen-emitted state machines populate at the network-effect
// call boundary. `KaracParkedTask` mirrors `KaracBranch`'s shape — a
// function pointer + an opaque state pointer — but the function returns a
// `KaracPollResult` tag so the runtime can distinguish "task wants to be
// re-polled later" from "task is done." See design.md § Network Event
// Loop and State-Machine Transform > State-Machine Transform —
// Network-Boundary Functions for the lowering shape this ABI implements.

/// Discriminator returned by codegen-emitted poll functions.
///
/// `repr(u8)` pins the discriminant width for the codegen ABI: poll
/// functions return one of `0`, `1`, `2` and Kāra-side code (the future
/// scheduler integration) maps it back to this enum. `#[non_exhaustive]`
/// signals that variants may be added — `Cancelled` is a likely future
/// addition once cooperative cancellation lowering matures — without a
/// breaking ABI change (existing 0 / 1 / 2 keep their discriminants).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum KaracPollResult {
    /// Task wants to be re-polled later. The poll function has registered
    /// itself with the event loop (or a channel, or whatever it is
    /// waiting on) and stored the registration in its state struct.
    Pending = 0,
    /// Task completed successfully. The state struct's return-slot
    /// holds the task's value; the runtime can free the state struct.
    Ready = 1,
    /// Task completed with an error. The state struct's return-slot
    /// holds the error; the runtime can free the state struct.
    Err = 2,
}

/// Parked-task ABI value carrying a state machine's poll function plus
/// the opaque state pointer it operates on.
///
/// Codegen emits this struct at every network-effect call boundary as
/// part of the state-machine lowering (phase 6 line 18). The runtime
/// drives the task by calling `poll_fn(state, cancel)` and inspecting
/// the returned [`KaracPollResult`] discriminant. The state struct's
/// lifetime spans from the network-boundary function's entry until
/// `poll_fn` returns `Ready` or `Err` (or the task is cancelled —
/// codegen emits the state-struct destructor on every exit path).
///
/// **Field layout.** Two pointer-width fields, no padding, `repr(C)`
/// — matches `KaracBranch`'s shape exactly so the runtime can share
/// dispatch machinery between the two task representations when the
/// scheduler integration lands.
///
/// **Cancellation.** The `*const AtomicBool` cancel pointer passed at
/// each poll mirrors the [`KaracBranch::func`] convention. The poll
/// function reads it at every yield point to observe cooperative
/// cancellation; on a true read, the function unwinds via the state
/// struct's destructor and returns `Err` (or a future `Cancelled`
/// variant — see [`KaracPollResult`]'s `non_exhaustive`).
#[repr(C)]
pub struct KaracParkedTask {
    pub poll_fn: unsafe extern "C" fn(*mut c_void, *const std::sync::atomic::AtomicBool) -> u8,
    pub state: *mut c_void,
}

// SAFETY: KaracParkedTask is `Send` because the codegen-emitted state
// struct's captured locals are subject to the cross-task-safe check
// when the surrounding network-boundary function is itself called
// from a `par {}` / `spawn()` boundary (the existing structural
// cross-task-safe enumeration covers this — see design.md §
// Structured Concurrency Lifetime Guarantees). The state pointer is
// type-erased here; soundness comes from the codegen check, not from
// runtime inspection.
unsafe impl Send for KaracParkedTask {}

// ── Background event-loop poller + wakeup queue (Phase 6 line 17 slice 3) ──
//
// An opt-in background thread that owns event-loop polling. Once started,
// the thread loops on `EventLoop::run_once(None)` indefinitely (blocking
// in mio's poll inside the inner `poll` Mutex), depositing wakeups into
// an internal `VecDeque<KaracWakeup>` for consumption by a scheduler
// thread via `karac_runtime_event_loop_take_wakeups`.
//
// **No deadlock with registration.** The `EventLoop` refactor that
// landed alongside this section splits the inner state into two
// independent Mutexes — `poll` (held by the background thread for the
// duration of each blocking poll) and `fds` (held only briefly by
// register / deregister). Concurrent `karac_runtime_event_loop_register_fd`
// calls from any thread acquire only the `fds` Mutex, so the long-blocking
// poll does not stall registration.
//
// **Direct FFI poll coexistence.** While the background poller is running,
// direct `karac_runtime_event_loop_poll` callers short-circuit to return
// 0 immediately — the background poller has authoritative ownership of
// the polling channel and direct callers should drain via `take_wakeups`
// instead. Documented in `karac_runtime_event_loop_poll`'s body.
//
// **Shutdown protocol.** `karac_runtime_event_loop_shutdown_background_thread`
// sets the shutdown flag, fires the cross-thread `wake()` to unblock the
// current poll call, signals the queue's `Condvar` to release any
// waiting `take_wakeups` callers, joins the thread, and clears the
// global slot. Idempotent — calling on a non-running thread returns -1
// without side effects, so a re-start after shutdown is supported within
// the same process.

/// Internal poller state. Held inside `Arc` so the spawned thread can
/// share it with the global slot.
struct EventLoopPoller {
    event_loop: Arc<EventLoop>,
    queue: Mutex<VecDeque<KaracWakeup>>,
    notify: Condvar,
    shutdown: AtomicBool,
    /// `JoinHandle` for the spawned thread. Wrapped in `Mutex<Option<_>>`
    /// so the shutdown path can `take()` it independently of the rest
    /// of the poller state.
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

/// Global slot for the background poller. `None` until the first
/// `karac_runtime_event_loop_start_background_thread` call; cleared
/// back to `None` on shutdown so the thread can be re-started later
/// within the same process. `Mutex<Option<Arc<_>>>` rather than
/// `OnceLock` for exactly this restart capability.
static BACKGROUND_POLLER: Mutex<Option<Arc<EventLoopPoller>>> = Mutex::new(None);

fn lock_background_poller_slot() -> std::sync::MutexGuard<'static, Option<Arc<EventLoopPoller>>> {
    BACKGROUND_POLLER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn poller_thread_main(poller: Arc<EventLoopPoller>) {
    while !poller.shutdown.load(Ordering::Acquire) {
        let wakeups = match poller.event_loop.run_once(None) {
            Ok(w) => w,
            Err(_) => {
                // Treat transient poll errors as a yield — re-check
                // shutdown and continue.
                continue;
            }
        };
        if wakeups.is_empty() {
            continue;
        }
        let mut q = poller.queue.lock().unwrap_or_else(|p| p.into_inner());
        for w in wakeups {
            q.push_back(KaracWakeup {
                token: w.token.0 as u64,
                parked: w.parked,
                direction: w.direction as u8,
            });
        }
        drop(q);
        poller.notify.notify_all();
    }
}

/// Start the background event-loop poller thread.
///
/// Idempotent: a second call while the thread is already running
/// returns 0 without re-spawning. Returns 0 on success.
#[no_mangle]
pub extern "C" fn karac_runtime_event_loop_start_background_thread() -> i32 {
    let mut slot = lock_background_poller_slot();
    if slot.is_some() {
        return 0;
    }
    let event_loop = Arc::clone(global_event_loop());
    let poller = Arc::new(EventLoopPoller {
        event_loop,
        queue: Mutex::new(VecDeque::new()),
        notify: Condvar::new(),
        shutdown: AtomicBool::new(false),
        handle: Mutex::new(None),
    });
    let poller_for_thread = Arc::clone(&poller);
    let join = thread::Builder::new()
        .name("karac-event-loop".to_string())
        .spawn(move || poller_thread_main(poller_for_thread))
        .expect("karac_runtime: failed to spawn event-loop poller thread");
    *poller.handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(join);
    *slot = Some(poller);
    0
}

/// Drain up to `max` wakeups from the background poller's queue into
/// the caller's buffer.
///
/// `timeout_nanos`:
/// - `-1`: block indefinitely until at least one wakeup arrives.
/// - `0`: non-blocking — return immediately, even if the queue is empty.
/// - `n > 0`: block up to `n` nanoseconds.
/// - Any other negative value: treated as 0 (non-blocking).
///
/// Returns the number of wakeups written. 0 means "queue was empty at
/// timeout" (or the background thread is not running).
///
/// # Safety
///
/// `out` must point to a writable buffer of at least `max ×
/// sizeof(KaracWakeup)` bytes. `max = 0` with `out = null` is permitted
/// (no writes).
#[no_mangle]
pub unsafe extern "C" fn karac_runtime_event_loop_take_wakeups(
    out: *mut KaracWakeup,
    max: usize,
    timeout_nanos: i64,
) -> usize {
    let poller = {
        let slot = lock_background_poller_slot();
        match slot.as_ref() {
            Some(p) => Arc::clone(p),
            None => return 0,
        }
    };
    let mut q = poller.queue.lock().unwrap_or_else(|p| p.into_inner());
    if q.is_empty() {
        match timeout_nanos {
            -1 => {
                q = poller.notify.wait(q).unwrap_or_else(|p| p.into_inner());
            }
            n if n > 0 => {
                let (g, _) = poller
                    .notify
                    .wait_timeout(q, Duration::from_nanos(n as u64))
                    .unwrap_or_else(|p| p.into_inner());
                q = g;
            }
            _ => {
                // Non-blocking — return empty.
            }
        }
    }
    let mut n_out = 0;
    while n_out < max {
        match q.pop_front() {
            Some(w) => {
                // SAFETY: caller's contract — `out` is writable for
                // `max` entries; we write at offset `n_out < max`.
                unsafe {
                    out.add(n_out).write(w);
                }
                n_out += 1;
            }
            None => break,
        }
    }
    n_out
}

/// Signal the background poller thread to stop, unblock its `poll`
/// call via the cross-thread waker, join the thread, and clear the
/// global slot.
///
/// Returns 0 on success, -1 if no background thread is running.
/// A second shutdown after a successful shutdown returns -1 (the slot
/// is empty).
#[no_mangle]
pub extern "C" fn karac_runtime_event_loop_shutdown_background_thread() -> i32 {
    let poller = {
        let mut slot = lock_background_poller_slot();
        match slot.take() {
            Some(p) => p,
            None => return -1,
        }
    };
    poller.shutdown.store(true, Ordering::Release);
    let _ = karac_runtime_event_loop_wake();
    poller.notify.notify_all();
    let join = poller
        .handle
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    if let Some(h) = join {
        let _ = h.join();
    }
    // Drain any pending waker event. If the poller thread observed
    // the shutdown flag *before* our `wake()` was delivered to its
    // `poll()` call (i.e., the thread had already returned from one
    // `run_once` and was about to check the flag for the next loop
    // iteration), mio's edge-armed waker leaves the event pending —
    // the next thread to call `poll()` would receive it as a spurious
    // empty wakeup. A non-blocking `run_once` here consumes it and
    // leaves the event loop in a known-clean state for follow-up
    // callers. BACKGROUND_POLLER is already None at this point (we
    // took the Arc out at the top of this fn), so this `run_once`
    // doesn't compete with any background polling.
    let ev = global_event_loop();
    let _ = ev.run_once(Some(Duration::ZERO));
    0
}

// ── Scheduler dispatcher (Phase 6 line 17 slice 4) ────────────────────────
//
// A background dispatcher thread that drains the background poller's
// wakeup queue and invokes `(task.poll_fn)(task.state, cancel)` on
// each wakeup. The `parked` field of each `KaracWakeup` is interpreted
// as `*const KaracParkedTask` — this is the convention that codegen
// (when state-machine lowering for network-boundary functions lands —
// phase-6 line 18) will follow when registering fds with the event
// loop.
//
// **Pairing with the background poller.** The dispatcher is opt-in
// and requires the background poller to be running. Calling
// `karac_runtime_scheduler_start_dispatcher` will auto-start the
// poller if it isn't already running — see the body.
//
// **Cancel routing.** v1 ships with a single process-global "never
// cancelled" `AtomicBool` that the dispatcher passes to each
// `poll_fn` invocation. Per-par-block cancel routing (so a parked
// task inside a fail-fast `par {}` observes its block's cancel flag)
// is later integration work — the FFI surface stays stable because
// the cancel pointer comes from the task's own state, not from the
// dispatcher's signature.
//
// **Lifetime convention.** The codegen is responsible for keeping the
// `KaracParkedTask` alive — and its `state` struct alive — from the
// `register_fd` call until `poll_fn` returns `Ready` or `Err`. The
// dispatcher does no allocation or freeing of task / state structs;
// it only invokes `poll_fn` through the type-erased pointers.

/// Internal dispatcher state. Held inside `Arc` so the spawned thread
/// can share it with the global slot.
struct SchedulerDispatcher {
    shutdown: AtomicBool,
    /// Per-process "never cancelled" flag. v1 placeholder — passed to
    /// every `poll_fn` invocation. When per-par-block cancel routing
    /// lands, parked tasks will carry the appropriate per-block flag
    /// in their `state` struct and `poll_fn` will read it from there
    /// instead of (or in addition to) this arg.
    cancel: AtomicBool,
    /// Counters for test verification + diagnostics. Updated unsynchronized
    /// (Relaxed) — they only need monotonic-write visibility, not strict
    /// ordering against other operations.
    polls: std::sync::atomic::AtomicU64,
    ready_observations: std::sync::atomic::AtomicU64,
    err_observations: std::sync::atomic::AtomicU64,
    pending_observations: std::sync::atomic::AtomicU64,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

static SCHEDULER_DISPATCHER: Mutex<Option<Arc<SchedulerDispatcher>>> = Mutex::new(None);

fn lock_scheduler_dispatcher_slot(
) -> std::sync::MutexGuard<'static, Option<Arc<SchedulerDispatcher>>> {
    SCHEDULER_DISPATCHER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn dispatcher_thread_main(disp: Arc<SchedulerDispatcher>) {
    // Drain wakeups in small batches; a short timeout makes shutdown
    // responsive without busy-spinning.
    let mut buf: [KaracWakeup; 16] = unsafe { std::mem::zeroed() };
    while !disp.shutdown.load(Ordering::Acquire) {
        // 100ms timeout — bounded enough that shutdown takes effect
        // within a poll cycle, brief enough that the dispatcher
        // doesn't wake up needlessly when idle. The background poller
        // delivers wakeups via the queue's Condvar, so this isn't a
        // busy-loop.
        let n = unsafe {
            karac_runtime_event_loop_take_wakeups(buf.as_mut_ptr(), buf.len(), 100_000_000)
        };
        for i in 0..n {
            // SAFETY: indices 0..n were written by take_wakeups.
            let w = unsafe { std::ptr::read(buf.as_ptr().add(i)) };
            if w.parked.is_null() {
                // Wakeup with no associated parked task — e.g., a
                // pre-dispatcher-era test that registered with a raw
                // marker. Skip rather than crash.
                continue;
            }
            // SAFETY: the codegen convention is that `parked` carries
            // a `*const KaracParkedTask` whose state struct lives
            // until `poll_fn` returns Ready / Err. The dispatcher
            // invokes `poll_fn` but never derefs `state` itself.
            let task = unsafe { &*(w.parked as *const KaracParkedTask) };
            let result = unsafe { (task.poll_fn)(task.state, &disp.cancel) };
            disp.polls.fetch_add(1, Ordering::Relaxed);
            match result {
                0 => {
                    disp.pending_observations.fetch_add(1, Ordering::Relaxed);
                }
                1 => {
                    disp.ready_observations.fetch_add(1, Ordering::Relaxed);
                }
                2 => {
                    disp.err_observations.fetch_add(1, Ordering::Relaxed);
                }
                _ => {
                    // Unknown discriminant — treat as Err for
                    // accounting purposes.
                    disp.err_observations.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// Start the scheduler dispatcher thread.
///
/// Auto-starts the background poller if it isn't already running —
/// the dispatcher's `take_wakeups` calls would otherwise return 0
/// forever. Idempotent: a second call while running returns 0
/// without re-spawning.
///
/// Returns 0 on success.
#[no_mangle]
pub extern "C" fn karac_runtime_scheduler_start_dispatcher() -> i32 {
    let mut slot = lock_scheduler_dispatcher_slot();
    if slot.is_some() {
        return 0;
    }
    // Auto-start the background poller — take_wakeups depends on it.
    let _ = karac_runtime_event_loop_start_background_thread();

    let disp = Arc::new(SchedulerDispatcher {
        shutdown: AtomicBool::new(false),
        cancel: AtomicBool::new(false),
        polls: std::sync::atomic::AtomicU64::new(0),
        ready_observations: std::sync::atomic::AtomicU64::new(0),
        err_observations: std::sync::atomic::AtomicU64::new(0),
        pending_observations: std::sync::atomic::AtomicU64::new(0),
        handle: Mutex::new(None),
    });
    let disp_for_thread = Arc::clone(&disp);
    let join = thread::Builder::new()
        .name("karac-scheduler-dispatcher".to_string())
        .spawn(move || dispatcher_thread_main(disp_for_thread))
        .expect("karac_runtime: failed to spawn scheduler dispatcher thread");
    *disp.handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(join);
    *slot = Some(disp);
    0
}

/// Signal the dispatcher to stop, join the thread, clear the global
/// slot. Returns 0 on success, -1 if no dispatcher is running.
///
/// Does NOT stop the background poller; the poller has its own
/// shutdown FFI and may be used independently of the dispatcher.
#[no_mangle]
pub extern "C" fn karac_runtime_scheduler_shutdown_dispatcher() -> i32 {
    let disp = {
        let mut slot = lock_scheduler_dispatcher_slot();
        match slot.take() {
            Some(d) => d,
            None => return -1,
        }
    };
    disp.shutdown.store(true, Ordering::Release);
    // The dispatcher's `take_wakeups` call has a 100ms timeout, so
    // shutdown takes effect within one poll cycle without further
    // signaling. (No need to wake or notify here.)
    let join = disp.handle.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(h) = join {
        let _ = h.join();
    }
    0
}

/// Snapshot of the scheduler dispatcher's atomic counters.
///
/// `#[repr(C)]` pins the layout for callers reading through FFI.
/// Counter semantics:
/// - `polls`: total number of `poll_fn` invocations the dispatcher has
///   made since process start (cumulative; never decreases).
/// - `ready_observations`: count of poll calls that returned `Ready` (1).
/// - `err_observations`: count of poll calls that returned `Err` (2)
///   or any unknown non-zero discriminant.
/// - `pending_observations`: count of poll calls that returned
///   `Pending` (0).
///
/// Invariant: `polls == ready_observations + err_observations +
/// pending_observations`. The counters are read with `Relaxed`
/// ordering (each independently), so a snapshot can transiently
/// observe the sum mismatching the total by one if a poll completes
/// between reads. Treat the values as approximate for diagnostics;
/// don't rely on cross-counter consistency.
#[repr(C)]
pub struct KaracSchedulerStats {
    pub polls: u64,
    pub ready_observations: u64,
    pub err_observations: u64,
    pub pending_observations: u64,
}

/// Read the dispatcher's counter snapshot into the caller's buffer.
///
/// Returns 0 on success, -1 if the dispatcher is not running. On -1
/// the contents of `*out` are unspecified — callers must check the
/// return value before reading.
///
/// # Safety
///
/// `out` must point to a writable `KaracSchedulerStats`. The fn writes
/// the four counters as one atomic write per field (no struct-level
/// atomicity).
#[no_mangle]
pub unsafe extern "C" fn karac_runtime_scheduler_stats_snapshot(
    out: *mut KaracSchedulerStats,
) -> i32 {
    let disp = {
        let slot = lock_scheduler_dispatcher_slot();
        match slot.as_ref() {
            Some(d) => Arc::clone(d),
            None => return -1,
        }
    };
    let snapshot = KaracSchedulerStats {
        polls: disp.polls.load(Ordering::Relaxed),
        ready_observations: disp.ready_observations.load(Ordering::Relaxed),
        err_observations: disp.err_observations.load(Ordering::Relaxed),
        pending_observations: disp.pending_observations.load(Ordering::Relaxed),
    };
    // SAFETY: caller guarantees `out` is writable for one
    // `KaracSchedulerStats`.
    unsafe {
        out.write(snapshot);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio::net::TcpListener;
    use std::net::SocketAddr;
    use std::thread;

    /// Serializes FFI tests within a single test binary. The FFI entry
    /// points go through the **process-global** event loop, so two FFI
    /// tests running in parallel race on its state. Acquire this lock
    /// at the start of any test that touches the global event loop
    /// (`karac_runtime_event_loop_*` entries or the parked-task driver
    /// loop below).
    static FFI_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn ffi_test_guard() -> std::sync::MutexGuard<'static, ()> {
        FFI_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn new_succeeds() {
        let _ev = EventLoop::new().expect("new event loop");
    }

    #[test]
    fn cross_thread_wake_unblocks_poll() {
        let ev = EventLoop::new().unwrap();
        let handle = ev.handle();

        let woke = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            handle.wake().unwrap();
        });

        // Should return well before the 2s safety bound because the
        // waker fires at ~20ms.
        let start = Instant::now();
        let wakeups = ev.run_once(Some(Duration::from_secs(2))).unwrap();
        let elapsed = start.elapsed();

        // Waker tokens are filtered out, so the visible wakeups list
        // is empty; the fact that `poll` returned early is the proof.
        assert!(wakeups.is_empty());
        assert!(
            elapsed < Duration::from_secs(1),
            "expected cross-thread wake to unblock poll well under 1s, took {elapsed:?}"
        );

        woke.join().unwrap();
    }

    #[test]
    fn fd_readiness_carries_parked_pointer_back() {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = TcpListener::bind(bind_addr).unwrap();
        let local = listener.local_addr().unwrap();

        let ev = EventLoop::new().unwrap();

        // Use a stack-allocated u64 as the "parked task" stand-in. The
        // loop never derefs it; we just check round-trip identity.
        let marker: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let parked = std::ptr::addr_of!(marker) as *mut c_void;

        let token = ev
            .register(&mut listener, IoDirection::Read, None, parked)
            .unwrap();

        let connector = thread::spawn(move || {
            let _stream = std::net::TcpStream::connect(local).unwrap();
            thread::sleep(Duration::from_millis(50));
        });

        let wakeups = ev.run_once(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(wakeups.len(), 1, "exactly one fd-readiness wakeup expected");
        assert_eq!(wakeups[0].token, token);
        assert_eq!(wakeups[0].parked, parked);
        assert_eq!(wakeups[0].direction, IoDirection::Read);

        connector.join().unwrap();

        ev.deregister(&mut listener, token).unwrap();
        assert_eq!(
            ev.registered_count(),
            0,
            "deregister should remove the fd from internal state"
        );
    }

    #[test]
    fn poll_timeout_returns_empty_wakeups() {
        let ev = EventLoop::new().unwrap();
        let wakeups = ev.run_once(Some(Duration::from_millis(10))).unwrap();
        assert!(wakeups.is_empty(), "no fds registered → no wakeups");
    }

    #[test]
    fn tokens_are_distinct_across_registrations() {
        // Bind two listeners to different ports, register both, verify
        // tokens differ. Also checks `next_token` increments correctly.
        let mut l1 = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut l2 = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let ev = EventLoop::new().unwrap();
        let t1 = ev
            .register(&mut l1, IoDirection::Read, None, std::ptr::null_mut())
            .unwrap();
        let t2 = ev
            .register(&mut l2, IoDirection::Read, None, std::ptr::null_mut())
            .unwrap();
        assert_ne!(t1, t2);
        ev.deregister(&mut l1, t1).unwrap();
        ev.deregister(&mut l2, t2).unwrap();
    }

    #[test]
    fn io_direction_to_interest_covers_all_arms() {
        assert_eq!(IoDirection::Read.to_interest(), Interest::READABLE);
        assert_eq!(IoDirection::Write.to_interest(), Interest::WRITABLE);
        assert_eq!(
            IoDirection::ReadWrite.to_interest(),
            Interest::READABLE.add(Interest::WRITABLE)
        );
    }

    // ── FFI surface (Phase 6 line 17 slice 1) ─────────────────────────
    //
    // The FFI fns go through the **process-global** event loop. Multiple
    // FFI tests share that global, so each acquires `FFI_TEST_LOCK` at
    // entry to serialize within the test binary. Internal-API tests
    // above use locally constructed `EventLoop` instances so they don't
    // need the lock.

    #[cfg(unix)]
    #[test]
    fn ffi_round_trip_register_poll_deregister_wake() {
        let _guard = ffi_test_guard();
        use std::os::fd::AsRawFd;

        // Bind a std-lib listener so we can pull a raw fd; set it
        // non-blocking so accept calls from a worker don't strand on
        // a slow client.
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let local = std_listener.local_addr().unwrap();
        let raw_fd = std_listener.as_raw_fd();

        // Round-trip marker stored in the parked pointer.
        let marker: u64 = 0xC0DE_FACE_C0DE_FACE;
        let parked = std::ptr::addr_of!(marker) as *mut c_void;

        // Register READ direction.
        let token = karac_runtime_event_loop_register_fd(raw_fd, 0, parked);
        assert_ne!(token, 0, "register should return a non-zero token");

        // Invalid direction → returns 0.
        let bad_token = karac_runtime_event_loop_register_fd(raw_fd, 99, parked);
        assert_eq!(bad_token, 0, "invalid direction byte should return 0");

        // Trigger fd readability from another thread.
        let connector = thread::spawn(move || {
            let _stream = std::net::TcpStream::connect(local).unwrap();
            thread::sleep(Duration::from_millis(50));
        });

        // Poll into a caller-allocated buffer. SAFETY: buffer of 4
        // `KaracWakeup`s lives on the stack for the duration of the
        // call.
        let mut buf: [KaracWakeup; 4] = [
            KaracWakeup {
                token: 0,
                parked: std::ptr::null_mut(),
                direction: 0,
            },
            KaracWakeup {
                token: 0,
                parked: std::ptr::null_mut(),
                direction: 0,
            },
            KaracWakeup {
                token: 0,
                parked: std::ptr::null_mut(),
                direction: 0,
            },
            KaracWakeup {
                token: 0,
                parked: std::ptr::null_mut(),
                direction: 0,
            },
        ];
        let n = unsafe {
            karac_runtime_event_loop_poll(
                2_000_000_000, // 2 s safety bound
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        assert!(n >= 1, "expected at least one fd-readiness wakeup, got {n}");
        let w = &buf[0];
        assert_eq!(w.token, token);
        assert_eq!(w.parked, parked);
        assert_eq!(w.direction, IoDirection::Read as u8);

        connector.join().unwrap();

        // Deregister succeeds; second call on the same fd is harmless
        // at our layer (we silently remove our map entry; mio may error
        // depending on its own state).
        let dereg = karac_runtime_event_loop_deregister_fd(raw_fd, token);
        assert_eq!(dereg, 0, "deregister should report success");

        // Wake is callable; coalesces with any pending wake.
        let wake = karac_runtime_event_loop_wake();
        assert_eq!(wake, 0, "wake should report success");

        // A subsequent poll with a long max_wait should return very
        // quickly because the wake is pending. The returned count
        // may legitimately be 0 (the wake event is filtered out of
        // the wakeups buffer at the EventLoop layer).
        let start = Instant::now();
        let n2 =
            unsafe { karac_runtime_event_loop_poll(2_000_000_000, buf.as_mut_ptr(), buf.len()) };
        let elapsed = start.elapsed();
        assert_eq!(n2, 0, "wake event filtered → empty wakeups");
        assert!(
            elapsed < Duration::from_secs(1),
            "wake should unblock poll well under 1s, took {elapsed:?}"
        );

        // Non-blocking poll returns 0 immediately when nothing is
        // pending.
        let start = Instant::now();
        let n3 = unsafe { karac_runtime_event_loop_poll(0, buf.as_mut_ptr(), buf.len()) };
        let elapsed = start.elapsed();
        assert_eq!(n3, 0);
        assert!(
            elapsed < Duration::from_millis(100),
            "non-blocking poll should return immediately, took {elapsed:?}"
        );
    }

    // ── Parked-task ABI (Phase 6 line 17 slice 2) ─────────────────────

    #[test]
    fn karac_poll_result_discriminants_match_codegen_abi() {
        // Discriminants are part of the codegen ABI — codegen emits raw
        // `u8` returns that the runtime maps back through this enum.
        // Pinning them here catches accidental reordering.
        assert_eq!(KaracPollResult::Pending as u8, 0);
        assert_eq!(KaracPollResult::Ready as u8, 1);
        assert_eq!(KaracPollResult::Err as u8, 2);
        assert_eq!(std::mem::size_of::<KaracPollResult>(), 1);
    }

    #[test]
    fn karac_parked_task_layout_pinned() {
        // Two pointer-width fields, no padding — `repr(C)` shape that
        // codegen will emit a struct literal against.
        let ptr = std::mem::size_of::<usize>();
        assert_eq!(std::mem::size_of::<KaracParkedTask>(), 2 * ptr);
        assert_eq!(std::mem::align_of::<KaracParkedTask>(), ptr);
    }

    // ── End-to-end driver test ────────────────────────────────────────
    //
    // Hand-rolls a 2-state machine that simulates what codegen will emit
    // for a network-boundary function. State 0: register a fd with the
    // event loop, return `Pending`. State 1: deregister, return `Ready`.
    // The test drives the state machine through the FFI surface in a
    // tight loop, proving the full ABI works end-to-end without needing
    // a production scheduler integration.

    #[cfg(unix)]
    #[repr(C)]
    struct HandRolledState {
        tag: u8,
        listener_fd: i32,
        token: u64,
        ready_observed: bool,
    }

    #[cfg(unix)]
    unsafe extern "C" fn hand_rolled_poll_fn(
        state_ptr: *mut c_void,
        _cancel: *const std::sync::atomic::AtomicBool,
    ) -> u8 {
        // SAFETY: the test constructs `state_ptr` as the address of a
        // valid `HandRolledState` stack value living through the entire
        // driver loop below.
        let state = unsafe { &mut *(state_ptr as *mut HandRolledState) };
        match state.tag {
            0 => {
                // Register the listener fd for read readiness.
                let token = karac_runtime_event_loop_register_fd(
                    state.listener_fd,
                    0,
                    std::ptr::null_mut(),
                );
                assert_ne!(token, 0, "register should succeed");
                state.token = token;
                state.tag = 1;
                KaracPollResult::Pending as u8
            }
            1 => {
                // The driver has observed readiness and re-polled us.
                state.ready_observed = true;
                let dereg = karac_runtime_event_loop_deregister_fd(state.listener_fd, state.token);
                assert_eq!(dereg, 0, "deregister should succeed");
                KaracPollResult::Ready as u8
            }
            _ => KaracPollResult::Err as u8,
        }
    }

    #[cfg(unix)]
    #[test]
    fn parked_task_drives_to_completion_through_ffi_surface() {
        let _guard = ffi_test_guard();
        use std::os::fd::AsRawFd;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let local = listener.local_addr().unwrap();
        let listener_fd = listener.as_raw_fd();

        let mut state = HandRolledState {
            tag: 0,
            listener_fd,
            token: 0,
            ready_observed: false,
        };
        let task = KaracParkedTask {
            poll_fn: hand_rolled_poll_fn,
            state: &mut state as *mut HandRolledState as *mut c_void,
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);

        let connector = thread::spawn(move || {
            // Give the driver a moment to register before the connect
            // makes the listener readable.
            thread::sleep(Duration::from_millis(50));
            let _stream = std::net::TcpStream::connect(local).unwrap();
            thread::sleep(Duration::from_millis(50));
        });

        // Test-only driver loop: invoke poll_fn, pump the event loop on
        // Pending, repeat until Ready / Err. Bounded by an iteration
        // count so a broken state machine fails the test rather than
        // hanging it forever.
        let mut wakeup_buf: [KaracWakeup; 4] = unsafe { std::mem::zeroed() };
        let mut iterations = 0;
        let final_result = loop {
            iterations += 1;
            assert!(iterations <= 8, "driver loop ran more than 8 iterations");

            // SAFETY: poll_fn / state pair is valid for the lifetime of
            // this test fn; `cancel` lives on the stack throughout.
            let raw = unsafe { (task.poll_fn)(task.state, &cancel) };
            if raw == KaracPollResult::Ready as u8 || raw == KaracPollResult::Err as u8 {
                break raw;
            }
            // Pending — drive the event loop. SAFETY: wakeup_buf has
            // 4 entries; bound passed matches.
            let _ = unsafe {
                karac_runtime_event_loop_poll(
                    2_000_000_000,
                    wakeup_buf.as_mut_ptr(),
                    wakeup_buf.len(),
                )
            };
        };

        assert_eq!(final_result, KaracPollResult::Ready as u8);
        assert!(state.ready_observed, "state machine reached the ready arm");
        assert_eq!(state.tag, 1, "state machine ended in state 1");

        connector.join().unwrap();
    }

    // ── Background poller thread (Phase 6 line 17 slice 3) ─────────────

    /// Test-only guard that shuts down the background poller on drop.
    /// Holds the FFI test lock so background-poller tests serialize
    /// against the other FFI tests. The drop order matters: shutdown
    /// runs first (while the FFI lock is still held), then the FFI lock
    /// releases.
    struct BackgroundPollerTestGuard {
        _ffi: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for BackgroundPollerTestGuard {
        fn drop(&mut self) {
            let _ = karac_runtime_event_loop_shutdown_background_thread();
        }
    }

    fn start_background_poller_for_test() -> BackgroundPollerTestGuard {
        let _ffi = ffi_test_guard();
        // Ensure clean start: a prior test that aborted abnormally could
        // have left the thread running.
        let _ = karac_runtime_event_loop_shutdown_background_thread();
        let rc = karac_runtime_event_loop_start_background_thread();
        assert_eq!(rc, 0, "start_background_thread should report success");
        BackgroundPollerTestGuard { _ffi }
    }

    #[cfg(unix)]
    #[test]
    fn background_thread_drains_wakeups_via_take() {
        let _guard = start_background_poller_for_test();
        use std::os::fd::AsRawFd;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let local = listener.local_addr().unwrap();
        let raw_fd = listener.as_raw_fd();

        let marker: u64 = 0xBEEF_0F0F_BEEF_0F0F;
        let parked = std::ptr::addr_of!(marker) as *mut c_void;
        let token = karac_runtime_event_loop_register_fd(raw_fd, 0, parked);
        assert_ne!(token, 0, "register should return a non-zero token");

        let connector = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let _stream = std::net::TcpStream::connect(local).unwrap();
            thread::sleep(Duration::from_millis(50));
        });

        let mut buf: [KaracWakeup; 4] = unsafe { std::mem::zeroed() };
        let n = unsafe {
            karac_runtime_event_loop_take_wakeups(buf.as_mut_ptr(), buf.len(), 2_000_000_000)
        };
        assert!(
            n >= 1,
            "expected at least one wakeup via background thread, got {n}"
        );
        let w = &buf[0];
        assert_eq!(w.token, token);
        assert_eq!(w.parked, parked);
        assert_eq!(w.direction, IoDirection::Read as u8);

        connector.join().unwrap();
        let dereg = karac_runtime_event_loop_deregister_fd(raw_fd, token);
        assert_eq!(dereg, 0);
    }

    #[test]
    fn background_thread_take_nonblocking_returns_zero_on_empty_queue() {
        let _guard = start_background_poller_for_test();
        let mut buf: [KaracWakeup; 4] = unsafe { std::mem::zeroed() };
        let n = unsafe { karac_runtime_event_loop_take_wakeups(buf.as_mut_ptr(), buf.len(), 0) };
        assert_eq!(n, 0);
    }

    #[test]
    fn background_thread_take_with_timeout_unblocks_on_empty() {
        let _guard = start_background_poller_for_test();
        let mut buf: [KaracWakeup; 4] = unsafe { std::mem::zeroed() };
        let start = Instant::now();
        let n = unsafe {
            karac_runtime_event_loop_take_wakeups(buf.as_mut_ptr(), buf.len(), 100_000_000)
        };
        let elapsed = start.elapsed();
        assert_eq!(n, 0);
        assert!(
            elapsed >= Duration::from_millis(80),
            "should wait ~100ms before timing out, only waited {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "should not wait much longer than 100ms, waited {elapsed:?}"
        );
    }

    #[test]
    fn background_thread_start_is_idempotent() {
        let _guard = start_background_poller_for_test();
        let rc = karac_runtime_event_loop_start_background_thread();
        assert_eq!(rc, 0);
    }

    #[test]
    fn background_thread_shutdown_returns_minus_one_when_not_running() {
        let _guard = ffi_test_guard();
        let _ = karac_runtime_event_loop_shutdown_background_thread();
        let rc = karac_runtime_event_loop_shutdown_background_thread();
        assert_eq!(rc, -1);
    }

    #[cfg(unix)]
    #[test]
    fn direct_ffi_poll_short_circuits_when_background_is_running() {
        let _guard = start_background_poller_for_test();
        // With the background poller owning polling, direct FFI poll
        // returns 0 immediately so callers don't contend for the
        // inner poll Mutex.
        let mut buf: [KaracWakeup; 4] = unsafe { std::mem::zeroed() };
        let start = Instant::now();
        let n =
            unsafe { karac_runtime_event_loop_poll(2_000_000_000, buf.as_mut_ptr(), buf.len()) };
        let elapsed = start.elapsed();
        assert_eq!(n, 0);
        assert!(
            elapsed < Duration::from_millis(100),
            "direct poll should return immediately, took {elapsed:?}"
        );
    }

    // ── Scheduler dispatcher (Phase 6 line 17 slice 4) ─────────────────

    /// Test-only guard that shuts down the scheduler dispatcher AND
    /// the background poller on drop. Holds the FFI test lock so
    /// dispatcher tests serialize against the rest of the FFI tests.
    struct SchedulerTestGuard {
        _ffi: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for SchedulerTestGuard {
        fn drop(&mut self) {
            // Dispatcher first (depends on the poller's queue), then
            // poller. Both are idempotent on already-stopped state.
            let _ = karac_runtime_scheduler_shutdown_dispatcher();
            let _ = karac_runtime_event_loop_shutdown_background_thread();
        }
    }

    fn start_scheduler_for_test() -> SchedulerTestGuard {
        let _ffi = ffi_test_guard();
        // Ensure clean start.
        let _ = karac_runtime_scheduler_shutdown_dispatcher();
        let _ = karac_runtime_event_loop_shutdown_background_thread();
        let rc = karac_runtime_scheduler_start_dispatcher();
        assert_eq!(rc, 0, "scheduler dispatcher should start");
        SchedulerTestGuard { _ffi }
    }

    /// State for the end-to-end scheduler test. The state machine has
    /// two states: state 0 registers the listener's fd and returns
    /// Pending; state 1 sets `completed = true` and returns Ready.
    /// The initial poll (state 0) is invoked by the test thread; the
    /// re-poll (state 1) is invoked by the dispatcher.
    #[cfg(unix)]
    #[repr(C)]
    struct SchedulerTestState {
        tag: u8,
        listener_fd: i32,
        token: u64,
        completed: std::sync::atomic::AtomicBool,
    }

    #[cfg(unix)]
    unsafe extern "C" fn scheduler_test_poll_fn(
        state_ptr: *mut c_void,
        _cancel: *const std::sync::atomic::AtomicBool,
    ) -> u8 {
        // SAFETY: caller passes a valid `*mut SchedulerTestState` that
        // lives across both invocations. Both sequential, never
        // concurrent (initial call from test thread returns Pending
        // before the dispatcher starts polling).
        let state = unsafe { &mut *(state_ptr as *mut SchedulerTestState) };
        match state.tag {
            0 => {
                // Register the fd; the `parked` pointer points to the
                // KaracParkedTask wrapping this state — that's the
                // codegen convention slice 4 implements.
                let task_ptr = state as *mut SchedulerTestState as *mut c_void;
                // The actual parked pointer passed to register_fd
                // points to the KaracParkedTask, not the state. The
                // caller (test thread) supplies that pointer; we just
                // store the registration token so we can deregister
                // on Ready.
                state.tag = 1;
                let _ = task_ptr; // silence unused (used by caller via FFI)
                KaracPollResult::Pending as u8
            }
            1 => {
                // Cleanup: deregister the fd, signal completion.
                let _ = karac_runtime_event_loop_deregister_fd(state.listener_fd, state.token);
                state.completed.store(true, Ordering::Release);
                KaracPollResult::Ready as u8
            }
            _ => KaracPollResult::Err as u8,
        }
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_drives_parked_task_to_completion_on_wakeup() {
        let _guard = start_scheduler_for_test();
        use std::os::fd::AsRawFd;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let local = listener.local_addr().unwrap();
        let listener_fd = listener.as_raw_fd();

        // Build state + parked task. Box pins them so their address
        // doesn't move while the dispatcher holds the pointer.
        let mut state = Box::new(SchedulerTestState {
            tag: 0,
            listener_fd,
            token: 0,
            completed: std::sync::atomic::AtomicBool::new(false),
        });
        let task = Box::new(KaracParkedTask {
            poll_fn: scheduler_test_poll_fn,
            state: &mut *state as *mut SchedulerTestState as *mut c_void,
        });
        let task_ptr = &*task as *const KaracParkedTask as *mut c_void;

        // Register the listener fd with `parked = &task` per the
        // dispatcher convention.
        let token = karac_runtime_event_loop_register_fd(listener_fd, 0, task_ptr);
        assert_ne!(token, 0, "register should succeed");
        state.token = token;

        // Initial poll (state 0): just bumps tag to 1. (We register
        // BEFORE this in the test because the hand-rolled state-machine
        // doesn't have a tag-0 register step in this layout — the test
        // owns registration. In a real codegen-emitted state machine,
        // the tag-0 case would do the register itself.)
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let initial = unsafe { (task.poll_fn)(task.state, &cancel) };
        assert_eq!(initial, KaracPollResult::Pending as u8);

        // Trigger fd readability.
        let connector = thread::spawn(move || {
            let _stream = std::net::TcpStream::connect(local).unwrap();
            thread::sleep(Duration::from_millis(50));
        });

        // Spin-wait for the dispatcher to drive completion. Bounded
        // by 2s so a broken dispatcher fails the test instead of
        // hanging it.
        let start = Instant::now();
        while !state.completed.load(Ordering::Acquire) {
            if start.elapsed() > Duration::from_secs(2) {
                panic!("dispatcher did not drive task to completion within 2s");
            }
            thread::sleep(Duration::from_millis(10));
        }

        connector.join().unwrap();

        // Drop order: connector joined → state still pinned → task
        // still pinned. Now safe to drop in test cleanup.
        drop(task);
        drop(state);
    }

    #[test]
    fn dispatcher_start_is_idempotent() {
        let _guard = start_scheduler_for_test();
        let rc = karac_runtime_scheduler_start_dispatcher();
        assert_eq!(rc, 0);
    }

    #[test]
    fn dispatcher_shutdown_returns_minus_one_when_not_running() {
        let _guard = ffi_test_guard();
        let _ = karac_runtime_scheduler_shutdown_dispatcher();
        let rc = karac_runtime_scheduler_shutdown_dispatcher();
        assert_eq!(rc, -1);
    }

    #[test]
    fn scheduler_stats_snapshot_returns_minus_one_when_dispatcher_not_running() {
        let _guard = ffi_test_guard();
        let _ = karac_runtime_scheduler_shutdown_dispatcher();
        let mut stats = KaracSchedulerStats {
            polls: 0,
            ready_observations: 0,
            err_observations: 0,
            pending_observations: 0,
        };
        let rc = unsafe { karac_runtime_scheduler_stats_snapshot(&mut stats) };
        assert_eq!(rc, -1, "should report not-running");
    }

    #[cfg(unix)]
    #[test]
    fn scheduler_stats_track_dispatcher_polls() {
        let _guard = start_scheduler_for_test();
        use std::os::fd::AsRawFd;

        // Initial snapshot — dispatcher just started, counters at 0.
        let mut before = KaracSchedulerStats {
            polls: 0,
            ready_observations: 0,
            err_observations: 0,
            pending_observations: 0,
        };
        let rc = unsafe { karac_runtime_scheduler_stats_snapshot(&mut before) };
        assert_eq!(rc, 0);
        assert_eq!(before.polls, 0);
        assert_eq!(before.ready_observations, 0);

        // Drive one parked task to completion (same shape as the
        // earlier dispatcher test).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let local = listener.local_addr().unwrap();
        let listener_fd = listener.as_raw_fd();

        let mut state = Box::new(SchedulerTestState {
            tag: 0,
            listener_fd,
            token: 0,
            completed: std::sync::atomic::AtomicBool::new(false),
        });
        let task = Box::new(KaracParkedTask {
            poll_fn: scheduler_test_poll_fn,
            state: &mut *state as *mut SchedulerTestState as *mut c_void,
        });
        let task_ptr = &*task as *const KaracParkedTask as *mut c_void;
        let token = karac_runtime_event_loop_register_fd(listener_fd, 0, task_ptr);
        assert_ne!(token, 0);
        state.token = token;
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let _initial = unsafe { (task.poll_fn)(task.state, &cancel) };

        let connector = thread::spawn(move || {
            let _stream = std::net::TcpStream::connect(local).unwrap();
            thread::sleep(Duration::from_millis(50));
        });

        let start = Instant::now();
        while !state.completed.load(Ordering::Acquire) {
            if start.elapsed() > Duration::from_secs(2) {
                panic!("dispatcher did not drive task to completion within 2s");
            }
            thread::sleep(Duration::from_millis(10));
        }
        connector.join().unwrap();

        // After-snapshot. Dispatcher should have polled exactly once
        // (the resume after fd readiness), observing Ready. Counters
        // are monotonic, so we assert lower bounds rather than exact
        // equality — a spurious extra poll would be unusual but not
        // an outright bug.
        let mut after = KaracSchedulerStats {
            polls: 0,
            ready_observations: 0,
            err_observations: 0,
            pending_observations: 0,
        };
        let rc = unsafe { karac_runtime_scheduler_stats_snapshot(&mut after) };
        assert_eq!(rc, 0);
        assert!(
            after.polls >= 1,
            "dispatcher should have polled at least once, got {}",
            after.polls
        );
        assert!(
            after.ready_observations >= 1,
            "at least one Ready observation expected, got {}",
            after.ready_observations
        );
        // The total invariant — polls = ready + err + pending.
        assert_eq!(
            after.polls,
            after.ready_observations + after.err_observations + after.pending_observations,
            "polls should equal sum of category observations"
        );

        drop(task);
        drop(state);
    }
}
