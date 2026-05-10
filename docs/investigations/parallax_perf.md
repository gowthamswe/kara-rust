# Parallax bench — perf investigation

**Status:** open. **Started:** 2026-05-09. **Owner:** unassigned.

This doc captures the diagnostic framing for the throughput gaps
surfaced by [Slice E's verification run](../../examples/parallax/bench/README.md)
and lays out concrete next-step probes. The numbers landed; *what
they mean* did not. This is where that work lives.

Cross-refs:
- Bench harness scaffolding: `ea1d26d`. Verification run: `4f7b72d`.
  HTTP handler ABI trampoline (predecessor): `5f4cbcc`.
- Design record: [`docs/demo_ideas.md § Slice E`](../demo_ideas.md).
  The "Out of scope" → "Closing the Kāra-vs-Rust gap" sub-section
  enumerates a 3-step closure path *for the trampoline overhead* (F3-
  conditional). This investigation covers the same gap from a
  broader root-cause angle — the trampoline is one candidate, not
  necessarily the dominant one.

---

## Tooling primer — flamegraphs

Many of the probes below produce **flamegraphs** as their output. If
the term is unfamiliar, this section is the read-once orientation;
skip if you've used them before.

A flamegraph is a visualization of *where a program spent its CPU
time*, built from sampled stack traces. The profiler interrupts the
program N times per second (typically 99–999 Hz), records the full
call stack at that moment, then aggregates: any function that
appears on the stack often → wide block; any function that doesn't
→ narrow or invisible. Brendan Gregg's reference write-up is the
canonical source: <https://www.brendangregg.com/flamegraphs.html>.

**How to read one.**

```
                        ┌──────────────┐
                        │     main     │
                        └──────────────┘
                ┌───────────────────────────────────┐
                │           handle_request          │
                └───────────────────────────────────┘
        ┌──────────────────┐ ┌────────────────────┐
        │   parse_request  │ │   get_dashboard    │
        └──────────────────┘ └────────────────────┘
                              ┌────────┐ ┌────────┐
                              │ fetch_a│ │ karac_ │
                              └────────┘ │par_run │
                                         └────────┘
```

- **x-axis: sample frequency, NOT time.** Width = how often this
  function appeared on the stack across all samples = how much CPU
  time was spent there. Horizontal order is alphabetical, *not*
  chronological — a flamegraph does not show what ran first.
- **y-axis: stack depth.** Lower blocks call upper blocks. The
  bottom is `main` or the entry point; each block above represents
  the caller of the block beneath it.
- **Width matters; height doesn't.** A tall, narrow tower is fine
  (deep call stack, but rarely on-CPU). A short *wide plateau* at
  the top of the stack is your hot spot — that's where the program
  is actually spending its cycles.

**Why this is the right tool here.** A 30-second flamegraph of the
Kāra binary under `wrk` load answers most of H1–H4 *simultaneously*,
in one picture:
- Wide `karac_par_run` plateau → worker-pool dispatch is the
  bottleneck (H1).
- Wide `karac_runtime_http_request_path` / per-request `String::*`
  block → trampoline + path-string allocations (H2 / H3).
- A *single* wide `busy_loop` at the top (rather than four narrower
  ones aggregating across cores) → fan-out is serializing instead
  of parallelizing (H1 again, from a different angle).
- A wide `__pthread_*` / `_dispatch_*` block → most of the time is
  spent in scheduler / thread-creation, not in user code at all.

Without a flamegraph, every hypothesis stays informed-guessing.

**How to render one (this machine, macOS / arm64).**

- **`cargo flamegraph`** (recommended). `cargo install flamegraph`,
  then `cargo flamegraph -p <pid>` or `cargo flamegraph --bin
  <name>` — wraps `dtrace` on macOS, `perf` on Linux. Renders an
  interactive SVG. Requires `sudo` for `dtrace` (it modifies kernel
  probe state); `cargo flamegraph --root` handles the prompt.
- **`samply`** (alternative, no sudo). `cargo install samply`, then
  `samply record ./binary` or `samply record -p <pid>`. Works on
  Apple silicon without root because it uses the OS-provided sampling
  syscall (`task_threads` + `thread_get_state`); produces an
  interactive HTML profile rather than a Brendan-Gregg SVG.
- **Apple Instruments** (UI). Time Profiler → "Heaviest Stack Trace"
  view. More mature on Apple silicon than the cli tools but harder
  to script and store as a checked-in artifact.

The output is an interactive SVG (cargo flamegraph) or HTML profile
(samply). Click any block to zoom in; search by function name;
hover for sample counts.

---

## What was measured

`GET /dashboard/1` — four CPU-bound busy loops per request, fanned
out + joined into a `Dashboard` struct, JSON-encoded into the
response body. Loop sizes calibrated for "modern x86-64" 2 / 5 / 8 /
12 ms (`examples/parallax/bench/kara/server.kara:49-52`); same shape
mirrored across all four impls.

Driver: `wrk -t4 -c100 -d10s` warmup + `-d30s` measure, sequential
per-impl. Hardware: Apple M5 Pro / 18 logical CPUs (10P + 8E) /
64 GB / macOS 26.4.1 / wrk 4.2.0.

| Impl | req/s | p99 | Notes |
|---|---|---|---|
| Rust | 45,731.33 | 5.16 ms | tokio + hyper + `tokio::join!` (perf ceiling reference) |
| Go | 7,695.31 | 58.58 ms | `net/http` + goroutines + `sync.WaitGroup` |
| Kāra | 1,089.99 | 438.18 ms | auto-par fan-out via `karac_par_run` |
| Node | 92.55 | 1.10 s | single-process `Promise.all` (F4 footnote) |

**Two surprises** are load-bearing:
1. **Rust ÷ Go ≈ 6×** — wider than typical for web-server benches
   (usually 1.5-3×). Suggests the bench's CPU-bound + JSON-heavy
   shape amplifies Go-specific tax we don't normally see.
2. **Rust ÷ Kāra ≈ 42×** — large, and the p99 of 438 ms vs designed
   critical path of ~12 ms (or ~2-4 ms after the M5/arm64 hardware-
   calibration adjustment) is the strongest single signal that
   *something is serializing*. First time the auto-par stack has been
   exercised under HTTP-level concurrency.

Node is a known-asymmetric reference per F4 — single-process by
design — and is not a focus of this investigation.

---

## Hardware calibration caveat (read first)

The loop sizes were chosen for x86-64. This run is arm64 / M5 Pro.
The kernel is `i = i + 1; sum = sum + i` — two register adds per
iter, ≈ 0.3-0.5 ns/iter on M5. The 12-ms-designed loop probably
finishes in **2-4 ms wall-clock** on this hardware.

This matters for two reasons:
- Rust hitting 45 K req/s is consistent with a ~3-4 ms critical path
  + 18-core fan-out, not the designed 12 ms. (At 12 ms critical path,
  18 cores cap throughput around ≈ 1.5 K req/s, which is below
  Rust's measured number — so the loops *must* be shorter than
  designed.)
- Conclusions about *fan-out efficiency* (the Kāra story) are still
  valid, because all four impls run the same loops on the same
  hardware. The relative ordering reflects each runtime's fan-out
  efficiency, even if the absolute "work per request" is smaller
  than the design intended.

A future iteration could re-calibrate the loop sizes for arm64
(target ≈ 100 ms total per request, asymmetric across the four) so
the bench surfaces fan-out behavior more cleanly. Not in scope here.

---

## Hypotheses — Rust ÷ Kāra gap

Ranked by **suspected impact × tractability of probing**. The Kāra
gap is the load-bearing diagnostic; the Go gap is a separate section
below.

### H1 — `karac_par_run` worker-pool serialization under HTTP concurrency

**Claim.** With 100 concurrent connections × 4 fan-out tasks per
request = up to 400 outstanding workers needed. If `karac_par_run`'s
worker pool is bounded near `num_cpus` (18), the fan-out serializes:
each handler waits for workers, the four reads queue up rather than
running concurrently. p99 of 438 ms — orders of magnitude above the
critical path — is consistent with this.

**Why this is the top suspect.** It explains the *shape* of the
slowdown (high p99, low throughput) directly. The auto-par mechanism
was previously exercised in `parallax_lite` (a writes-only
microbench, single-threaded driver) — never under HTTP concurrency
with 100 in-flight requests competing for the pool.

**How to probe (cheap, do this first).**
1. **Read `karac_par_run` source.** Likely at `runtime/src/lib.rs`
   or a `runtime/src/par.rs` adjacent. Look for: pool sizing logic,
   whether workers are created per-call or pooled, queueing
   behavior under contention. Confirms or kills H1 without needing
   to instrument anything.
2. **Run with `KARAC_AUTO_PAR=0`** (per the env var referenced in
   `kara/server.kara:125-127`). This serializes the four reads.
   Compare throughput.
   - If Kāra-with-auto-par is *similar* to Kāra-without — fan-out
     isn't actually happening, H1 is plausible (or upstream of the
     pool — see H2).
   - If Kāra-with-auto-par is *materially better* — fan-out is
     working at low concurrency, H1 may be a contention-only effect.
     Re-run at lower `wrk -c` (e.g., `-c4` or `-c8`) to test.
3. **Step `wrk -c` from 1 → 100** in powers of 2. If throughput
   plateaus (or drops) early, the pool is saturating early.

**Prior art.** None on this stack — first-of-its-kind probe.

### H2 — Handler trampoline FFI overhead per request

**Claim.** The trampoline shipped at `5f4cbcc` converts each request:
hyper `Request` → Kāra `Request` (heap-allocates wrapper, copies path
bytes into a fresh `String`), runs handler, Kāra `Response` → hyper
`Response`. Per-request heap traffic + value-type packing/unpacking.

**Why this is suspect #2.** The design record at `demo_ideas.md §
Slice E` "Out of scope" already enumerates the closure path here:
(1) borrowed accessors → (2) inline trampoline → (3) `#[repr(C)]`
Request. That ranking suggests the design author already suspected
this is non-trivial overhead. But — it's a per-request *constant*,
not a contention effect, so it shouldn't produce a 438 ms p99. More
likely a contributor to the throughput-floor than the tail.

**How to probe.**
1. **Time profile under `wrk` load** (Instruments → Time Profiler on
   macOS, attach to running Kāra binary). Look for: time spent in
   `karac_runtime_http_request_path`, `String::from`, the trampoline
   dispatch shim, and Kāra→hyper Response packing. If trampoline
   functions are >10% of CPU, this is real; if <2%, dismiss.
2. **A/B with a no-op handler** — replace `get_dashboard(1)` with
   `Response { status: 200, body: "ok" }`. The req/s of *that* run
   is the trampoline-only ceiling. Distance from 45 K (Rust ceiling)
   tells us how much of the gap is trampoline vs everything else.

### H3 — String allocations on the hot path

**Claim.** `"Alice"` returned per call from `fetch_profile_name` —
if string literals aren't statically interned, that's a heap alloc
per fetch (× 4 fetches × N req/s = high allocator pressure). Same
for the 144-byte JSON body literal in `handle()`.

**How to probe.** Compile with `-C overflow-checks=on` is irrelevant
here. The right probe is:
1. Read codegen output for the four `fetch_*` fns. If they emit
   `karac_alloc` or equivalent for the string literal, H3 is real.
   If literals are `static`-promoted, H3 is dead.
2. Instruments → Allocations track during a 5s wrk run.

### H4 — `karac_par_run` is not inlined / no LLVM cross-FFI optimization

**Claim.** LLVM monomorphizes within the Kāra-generated module, but
`karac_par_run` is an external runtime symbol (Rust crate, separate
compilation unit). Calls to it don't get inlined; the four busy
loops dispatch through indirect calls; LLVM can't prove the work
units are independent → no auto-vectorization, no instruction-level
parallelism beyond what the busy loop itself exposes.

**How to probe.** Lower-priority — overhead vs Rust's inline
`tokio::join!` is real but unlikely to be the dominant gap. Worth
revisiting only if H1-H3 don't account for most of the 42×. Read
the LLVM IR for `get_dashboard` (`karac build --emit=llvm-ir`).

### H5 — Effect-tracking bookkeeping at runtime

**Claim.** Each `reads(R_i)` call may emit runtime checks (effect
verification, ownership-mode dispatch). I have not read the codegen
in this session — could be zero-cost (compile-time only), could
not.

**How to probe.** Read codegen output for a `reads(R)` annotated
function call vs a plain function call. If the IR is identical
(modulo metadata), H5 is dead; if there's runtime dispatch, measure
its frequency.

---

## Hypotheses — Rust ÷ Go gap

Less actionable for *us* (we don't control Go's runtime), but worth
documenting for the README narrative and for future bench
calibration. Probable contributors to the ~6× gap, in rough
suspected-impact order:

1. **`encoding/json` reflection** — runtime reflection per call;
   serde monomorphizes. Per-request 50-200 µs tax is plausible.
2. **GC pressure** — Go heap-allocates the four fetch results +
   `Dashboard` aggregate per request; Rust struct-by-value path is
   zero-alloc. STW-ish pauses contribute to p99 of 58 ms.
3. **Goroutine creation cost** — 4 fresh goroutines per request →
   ~1.6 K live under 400 in-flight. Tokio's `spawn_blocking` reuses
   a 512-thread pool; cold goroutines aren't free.
4. **Async preemption interrupts** — Go 1.14+ preempts CPU-bound
   goroutines every 10 ms. Designed loops straddle that; on M5 the
   loops are shorter than 10 ms, so this is probably *not* a major
   contributor on this hardware (worth verifying — could be (1) +
   (2) alone).

**How to probe.** Lower priority — only worth doing if the README
narrative needs more precision than "Rust ÷ Go is wider than usual
because CPU-bound + JSON-heavy". A flamegraph of the Go binary
under wrk load (`go tool pprof`) settles it in ~30 min.

---

## Suggested next-session pickup order

If a single short session lands first, do **H1 step 1** (read
`karac_par_run` source) — it's a single file read, gives strong
signal on the dominant hypothesis, and informs whether to invest in
H1 step 2-3 (env-var A/B + concurrency sweep) or jump to H2.

If a longer session is available, run all H1 probes end-to-end and
write up findings inline below ("Findings" section, dated).

If perf budget is tight and we just want directional improvements:
the design record's closure path (borrowed accessors → inline
trampoline → `#[repr(C)]` Request) is well-scoped and worth
shipping *regardless* of what root-cause analysis turns up. H1's
investigation tells us whether to *also* invest in worker-pool
sizing — a parallel track to the design's enumerated closure path.

---

## Out of scope (for this investigation)

- **Re-running the bench at different hardware-calibrated loop
  sizes.** Worthwhile separately for cleaner numbers but not load-
  bearing for root-cause analysis.
- **Cluster-mode Node.** F4 footnote stands; not a perf
  investigation question.
- **Production HTTP perf concerns** — TLS overhead, real DB I/O,
  request size variance, keep-alive vs connection-per-request,
  HTTP/2 framing. None of these are exercised by the bench; all are
  Phase 11 long-tail.
- **Comparing against other Rust web frameworks** (actix, rocket,
  warp). hyper is the apples-to-apples baseline because Kāra's
  runtime sits on hyper.

---

## Findings

### 2026-05-09 — H1 confirmed (stronger form than hypothesized)

**Probe:** samply 0.13.1 sampling at 1000 Hz for 30 s, with `wrk -t4
-c100` driving the Kāra bench server. Sudo-free path on macOS arm64
(`samply record -s -d 35 -- ./server`); the `cargo flamegraph`
default path requires `dtrace` + `sudo` and is interactive-prompty
under autonomy, so we used samply. Throughput during the profiled run
was 1,088 req/s — within noise of the unprofiled 1,090 (the profiler
is not perturbing the measurement). Profile artifact:
`examples/parallax/bench/profile_kara.json.gz` (gitignored — re-
generate via the steps above; analysis script
`examples/parallax/bench/analyze_profile.py`).

**Symbol resolution gap (caveat).** samply could not resolve symbols
in the locally-built Kāra binary or in `libsystem_kernel.dylib`
(the latter is normal — it ships without public symbol info on
macOS); the analysis script reports raw addresses. To map back, the
findings below were resolved manually by running `nm` against the
binary + the system dylib and matching the largest address ≤ each
hot offset. This is approximate (some matches may be intra-function
addresses) but sufficient for hypothesis discrimination.

**Top SELF (where the CPU is actually executing).**

| % | Symbol (resolved) | Library |
|---|---|---|
| 60.1 | `_mach_vm_protect +0x30` | libsystem_kernel.dylib |
| 24.8 | (kernel syscall stub at 0xbb0) | libsystem_kernel.dylib |
|  7.5 | `_busy_loop +0x10` | server (user code) |
|  3.5 | `_vm_copy +0xb8` | libsystem_kernel.dylib |
|  2.1 | (kernel) | libsystem_kernel.dylib |

**Top INCLUSIVE (where time is spent, including blocked-in-callee).**

| % | Symbol (resolved) | Notes |
|---|---|---|
| 98.3 | pthread thread entry | every sample lands inside a thread |
| 90.0 | tokio runtime IO loop / `__rust_begin_short_backtrace` | all threads alive in tokio |
| 58.2 | `tokio::runtime::context::runtime_mt::current_enter_context` | tokio task entry |
| 31.1 | `tokio::runtime::scheduler::multi_thread::worker::run` | tokio worker run loop |
| 28.3 | `std::sys::sync::condvar::pthread::Condvar::wait_timeout` | waiting on condvars |
| 27.2 | `_karac_par_run +0x1f7` | inside the auto-par fan-out |
| 27.2 | `_get_dashboard +0x67` | inside the user handler — same magnitude as par_run |

**The smoking gun: 3,344 unique threads** during the 30 s recording.
With the bench running at ~1,090 req/s × 4 fan-out tasks per request
= ~4,360 fan-out tasks/sec needing a worker, and 3,344 threads seen
in 30 s of profiling, the only consistent explanation is that
`karac_par_run` is creating **fresh OS threads per call** (or close
to it) rather than dispatching work onto a long-lived worker pool.

**Why this confirms H1, in a stronger form than hypothesized.**

The original H1 framing was "bounded worker pool sized near
`num_cpus`, contention serializes fan-out under HTTP concurrency."
Reality is more pointed: there is no pool at all, or it's a
per-invocation pool that's torn down between calls. Three pieces of
evidence, all pointing the same direction:

1. **3,344 threads** in 30 s — far above any reasonable steady-state
   pool size. A pooled implementation would show ≈ `num_cpus` (18)
   long-lived workers, plus the tokio runtime threads.
2. **60% of self-time in `mach_vm_protect`** — that syscall is the
   stack guard-page setup path inside `pthread_create`. Heavy
   `mach_vm_protect` traffic is the textbook signature of thread-
   creation churn, not steady-state thread work.
3. **`busy_loop` is only 7.5% of self-time.** With a healthy fan-
   out, the four busy loops should dominate self-time (they're the
   only meaningful CPU work in the program). Instead the kernel
   eats ~85% and user code eats < 10%. The CPU is being spent on
   thread management, not on the work the threads were created to
   do.

**H2 / H3 status.** The handler trampoline (`__karac_http_shim_
handle`) and HTTP request-path String-allocation
(`_karac_runtime_http_request_path`) are present in inclusive
samples but at much lower magnitudes (`__karac_http_shim_handle`
inclusive ≈ 27%, but its self-time is < 0.5%). The trampoline is
*on the call path* of every request — that's why its inclusive
count is high — but it's not where time is spent. **H2 and H3 are
behind H1's noise floor; addressing them before H1 would yield
diminishing returns.** The 3-step closure path enumerated in
`docs/demo_ideas.md § Slice E` "Out of scope" remains a valid
follow-up but is not the highest-impact next step.

**H4 / H5 status.** Neither was confirmed nor killed by this probe;
both would require codegen-output reads (`karac build --emit=llvm-
ir`) which are out of scope for this session. Re-evaluate after H1
is closed.

**Estimated headroom from fixing H1.** If 60% of CPU is `mach_vm_
protect` (thread creation) and 25% is other kernel sync (mostly
condvar wakes around thread join), then ≈ 85% of the CPU budget is
being spent on pthread orchestration rather than work. Switching to
a long-lived worker pool that amortizes thread creation across
requests should free most of that budget. Rough estimate: **3–5×
throughput improvement for the Kāra row** — bringing it from 1.1 K
req/s into the 3–5 K range. Still below Rust's ceiling (45 K), but
into the same order of magnitude as Go (7.7 K). The remaining gap
to Rust would then be H2 / H3 / H4 territory (the trampoline +
allocator + cross-FFI inlining) — at which point the design
record's existing closure path becomes the right next investment.

**Recommended next step.** Read `karac_par_run`'s source (likely
under `runtime/src/`) and confirm the lack of a pool. If confirmed,
prototype a fixed-size global worker pool (rayon-style, or hand-
rolled crossbeam channels) and re-run the bench. The expected
throughput after the fix is the validation criterion — if the Kāra
row jumps to 3 K+ req/s, the diagnosis was correct and we move on
to H2; if it doesn't, we have a different problem.

### 2026-05-09 — H1 fix landed, partial win (throughput plateau at ~1.06 K req/s)

**Source confirmation.** `runtime/src/lib.rs:345-347` (pre-fix) was the
smoking gun verbatim: `thread::scope(|s| { for _ in 0..pool_size {
s.spawn(|| { ... } })`. Per-call thread spawn confirmed.

**Fix landed.** `runtime/src/lib.rs` rewrite — long-lived global pool
(`OnceLock<Arc<Pool>>`, `Mutex<VecDeque<Task>>` + `Condvar`,
`available_parallelism()`-sized worker count). Caller blocks on a
per-call `Arc<ParCall>`'s `Condvar`; tasks decrement `remaining` on
completion; last task signals. Wait loop work-helps to prevent pool
exhaustion under nested-par. Design record at
[`phase-7-codegen.md § "karac_par_run: long-lived worker pool"`](../implementation_checklist/phase-7-codegen.md).
ABI unchanged; codegen unaffected; 802 tests + 21 runtime unit tests
green; clippy / fmt clean.

**Re-profile under same `wrk -t4 -c100` load** (`profile_kara_v2.json.gz`,
samply 30 s):

| Metric | Pre-fix (`4f7b72d`) | Post-fix | Change |
|---|---|---|---|
| Throughput (req/s) | 1,090 | 1,062 | -3 % (noise) |
| p50 latency | 75 ms | 90 ms | +20 % |
| p99 latency | 438 ms | 266 ms | **-39 %** |
| Unique threads in 30 s | 3,344 | **213** | **-94 %** |
| Top-of-stack `busy_loop` | 7.5 % | **66.7 %** | **+790 %** |
| Top-of-stack `mach_vm_protect` | 60.1 % | 26.6 % | **-56 %** |
| Top-of-stack other kernel | ~25 % | ~5 % | -80 % |

**Diagnosis:** H1 was *correctly identified* as a real problem and
the fix *fully resolves it*. Thread-creation overhead is gone; the
program now spends two-thirds of its CPU on the work it was designed
to do (vs < 8 % before). p99 dropped meaningfully (438 → 266 ms),
which is the user-visible quality-of-service win.

**The throughput estimate was wrong.** The slice plan estimated
3-5 K req/s post-fix; reality is 1.06 K. Why: the bench is **wrk-
connection-bound, not pool-capacity-bound**. With `-c100` keep-alive
connections, throughput = connections / per-request-wall-clock-
latency = 100 / 0.09 s ≈ 1.1 K req/s, which matches the measured
number almost exactly. Pre-fix, the same identity held: 100 / 0.075
≈ 1.3 K, with p99 jitter pulling the average to 1.09 K. The pool fix
reduced p99 (because slow tail requests no longer pay the 4× thread-
spawn syscall cost) but didn't move p50 — so connection-throughput
math gives the same result. Higher p99 was *masking* the real
ceiling, not setting it.

**What's bounding p50 then.** Per-request wall-clock with the pool
fix is dominated by the four `busy_loop`s themselves (2-4 ms total
parallel). The remaining ~85 ms of p50 latency is everywhere else —
HTTP parse / response-build, mutex contention on `ACTIVE_FRAMES` (4
register + 4 deregister per request) and the global pool queue
(4 push + 4 pop per request), Arc allocation, the handler trampoline
(H2). None of these were investigated before the H1 fix because they
were behind H1's noise floor; now they're the surface.

**Hypotheses for the new throughput ceiling, ranked.** *(These
displace H2-H5 of the original ranking — H2 / H3 are still candidates
but their ranking changes now that we have post-fix data.)*

1. **`ACTIVE_FRAMES` mutex contention.** 4 register + 4 deregister
   per request × 1,000 req/s = 8,000 lock acquisitions/sec on a
   single global mutex, plus the 4-task-per-request worker contention
   on the same lock. Likely the dominant in-runtime cost now.
   Probe: `KARAC_RUNTIME_DEBUG_METADATA=0` env var disables frame
   tracking entirely (skipping the lock); if throughput jumps,
   confirmed. Mitigation: per-thread-local active-frame slot +
   periodic flush, or a sharded lock by thread-id hash.
2. **Global pool queue mutex contention.** Same shape as (1) but
   on `Pool::queue`. Probe: `lock_api`-style instrumentation, or
   replace with a lock-free MPMC queue (crossbeam-deque) and re-bench.
3. **Handler trampoline overhead (original H2).** Still on the
   call path of every request. The closure path enumerated in
   `docs/demo_ideas.md § Slice E` "Out of scope" remains valid —
   borrowed accessors first, inline trampoline second. Probe: a no-
   op-handler bench gives the trampoline-only ceiling.
4. **`block_in_place` + tokio worker churn.** The hyper service
   invokes the Kāra handler synchronously via `block_in_place`,
   which converts the current tokio worker into a blocking thread
   and tokio spawns a replacement. Under sustained `-c100` load,
   tokio's blocking pool churns. Probe: `tokio_unstable` runtime
   stats, or a non-`block_in_place` execution path for the handler
   (large change — defer until (1)-(3) are ruled out).

**Recommended next step.** Probe (1) — set
`KARAC_RUNTIME_DEBUG_METADATA=0` and re-run the bench. Single env
var, zero-LoC change, gives us instant signal on whether the
`ACTIVE_FRAMES` mutex is the next bottleneck. If yes, the fix is
either to make frame tracking lock-free or to gate it on a more
selective signal (debug builds only, off in `--release`).

### 2026-05-10 — Probe sweep rules out runtime, points to codegen

Three quick probes, all on the same hardware + bench config, ran
sequentially:

| Probe | Config | req/s | p50 | p99 | What it tells us |
|---|---|---|---|---|---|
| Baseline post-fix | Pool fix, auto-par on, frame tracking on | 1,054 | 90 ms | 238 ms | (reference) |
| Frame tracking off | `KARAC_RUNTIME_DEBUG_METADATA=0` | 1,074 | 90 ms | 237 ms | `ACTIVE_FRAMES` mutex is **NOT** the bottleneck |
| Auto-par off | `KARAC_AUTO_PAR=0` (compile-time, sequential fan-out) | 1,077 | 85 ms | 258 ms | Fan-out provides **zero** throughput benefit at this load |
| No-op handler | Removed `get_dashboard(1)` call from handler | **108,415** | **554 µs** | **55 ms** | Trampoline + HTTP path can do 108 K req/s — over 2× Rust's measured rate |

**Two conclusions, one bigger than the other.**

**(a) The pool fix's expected throughput improvement was always
unreachable for this bench.** Sequential and parallel fan-out give
the *same* throughput (1,074 vs 1,054 — within noise). At 100
concurrent connections × 18 cores, the cores are fully utilized
serving sequential requests; fan-out *within* a request doesn't add
throughput because there's no spare CPU for it to use. (This is the
classic Amdahl/Gustafson saturation case.) The pool fix was correct
to ship — eliminating thread-creation overhead and improving p99 are
real wins — but the auto-par mechanism itself is invisible at
saturated load. Auto-par's value is at lower load (latency under
N < num_cpus connections, where intra-request parallelism *can* find
spare cores), or under workloads where individual branches block on
real I/O instead of CPU-bound busy loops.

**(b) Kāra's codegen for the busy loop runs ~25× slower than
optimal — and that single fact accounts for the entire Kāra-vs-Rust
gap.** Disassembly of `_busy_loop` in the kara binary
(`otool -tV examples/parallax/bench/kara/.bin/server`):

```
_busy_loop:
  sub  sp, sp, #0x20
  stp  x0, xzr, [sp, #0x8]   ; spill n, init i = 0
  str  xzr, [sp, #0x18]      ; init sum = 0
.L:
  ldp  x0, x8, [sp, #0x10]   ; reload sum, i  ← 2 stack loads
  ldr  x9, [sp, #0x8]        ; reload n      ← 1 stack load
  cmp  x8, x9                ; i < n ?
  b.ge .exit
  ldr  x8, [sp, #0x18]       ; redundant reload of i ← 1 stack load
  add  x9, x0, x8            ; sum += i
  add  x8, x8, #1            ; i += 1
  stp  x9, x8, [sp, #0x10]   ; spill sum, i  ← 2 stack stores
  b    .L
.exit: ...
```

12 instructions per iter, all serialized through the stack-spill
chain (every iter loads `i` and `sum` from memory, computes, stores
back). On M5 P-core's ~4 GHz, that's ~3 ns/iter best-case — and
measured 9.3 ns/iter (1077 req/s × 4 fetches × 2.275M average
iterations × 9.3 ns ≈ 91 ms p50, matches measurement) suggests
~12-15 cycles per iter, consistent with 1 inst/cycle dispatch
through the stack-aliasing dependency chain.

**Optimal codegen** keeps `i` and `sum` in registers for the
duration of the loop, hoisting the load + final-store outside —
LLVM's `mem2reg` + `loop-rotate` + `instcombine` passes do this
trivially. The optimal inner loop is 4 instructions:

```
.L:
  add sum, sum, i        ; 1 cycle
  add i,   i,   #1       ; 1 cycle
  cmp i,   n             ; 1 cycle (issue parallel with add)
  b.lt .L                ; 1 cycle (predicted)
```

≈ 1-2 cycles per iter via macro-op fusion + branch prediction →
0.25-0.5 ns/iter → 9.1 M iters in 2.3-4.5 ms. Rust's release-mode
codegen runs the loops at this speed (`get_dashboard` got fully
inlined into a single tokio-task closure; the busy-loop calls
disappear into register-only inner loops we can't even isolate
in the disassembly).

**Diagnosis.** Kāra's codegen is producing what looks like `-O0`
output for this kernel — local variables stay in stack slots, no
mem2reg promotion, no redundant-load elimination, no loop-invariant
code motion, no instruction-level scheduling. Either karac is not
running LLVM optimization passes at all, or it's running them with
metadata that prevents promotion (unlikely — these are standard
mid-end passes that should fire trivially on this IR).

**This subsumes H2 / H3 / H4 entirely.** The "Out of scope —
closing the Kāra-vs-Rust gap" path enumerated in
[`docs/demo_ideas.md § Slice E`](../demo_ideas.md) — borrowed
accessors → inline trampoline → `#[repr(C)]` Request — would not
have moved the needle in this configuration. The probes show:

- The trampoline can already do 108 K req/s. It is **not** the
  bottleneck.
- The handler-fan-out / auto-par dispatch isn't the bottleneck
  either (sequential and parallel are the same speed).
- Frame tracking (the `ACTIVE_FRAMES` mutex) isn't the bottleneck.

The bottleneck is **plain ARM codegen quality for tight integer
loops**. Until that's fixed, the Kāra row is bound at ~1 K req/s on
this bench regardless of any HTTP-path or runtime work.

**Recommended next step.** Confirm the codegen-passes hypothesis at
the source: read `src/codegen.rs` for the LLVM module construction
and look for an explicit pass-pipeline configuration. If passes
aren't being run, enabling `-O2`-equivalent (mem2reg, instcombine,
loop-rotate, LICM, indvars, deadcode-elim) should be a small
contained change. If passes *are* running, dump the pre-pass IR for
`busy_loop` (`karac build` with an LLVM-IR-emit flag if one exists,
or hack one in temporarily) and check what the IR looks like —
maybe the IR has a feature that defeats `mem2reg` (e.g., taking
addresses of locals).

Bench validation criterion for the codegen fix: the Kāra row
should jump to **20-40 K req/s** (within striking distance of
Rust's 47 K), since the codegen overhead currently dominates and
the trampoline ceiling is already 108 K.

### 2026-05-10 — Codegen IR-opts landed, 92× throughput improvement

**Fix landed.** Wired `module.run_passes("default<O2>", &target_machine,
options)` into `compile_to_object_with_options` between IR
construction and object emission. Plus an `apply_optimization_passes`
helper, an env-var routing layer (`KARAC_OPT_LEVEL=0|1|2|3` for
opt-out / future tuning), and target-machine + pass-pipeline level
sync via `backend_optimization_level()`. ~120 LoC total in
`src/codegen.rs`. Design record at
[`phase-7-codegen.md § "Run LLVM mid-end optimization passes"`](../implementation_checklist/phase-7-codegen.md).

**Tests.** Full LLVM-feature suite green (~1,500 tests across codegen,
par_codegen, parallax, http_server, memory_sanitizer, etc.) — `-O2`
did not unmask any UB in our codegen. clippy + fmt clean.

**Bench (full 4-impl re-run on the same hardware as 2026-05-09):**

| Impl | req/s pre-O2 | req/s post-O2 | Change |
|---|---|---|---|
| Rust |  47,313 |  47,489 | +0.4% (noise — Rust was already optimized) |
| Go   |   7,599 |   7,728 | +1.7% (noise) |
| **Kāra** | **1,054** | **97,172** | **+92×** |
| Node |      94 |      93 | flat |

**Disassembly evidence.** `_busy_loop` post-fix is no longer a loop
at all — LLVM's `LoopIdiomRecognize` pass identified the body as the
triangular-number sum (`Σ_{i=0}^{n-1} i = n(n-1)/2`) and replaced
the entire loop with the closed-form arithmetic:

```
_busy_loop:
  subs  x8, x0, #0x1            ; x8 = n - 1
  b.lt  .return_zero             ; n < 1 → return 0
  sub   x9, x0, #0x2            ; x9 = n - 2
  mul   x10, x8, x9              ; (n-1)(n-2) low 64
  umulh x9,  x8, x9              ; (n-1)(n-2) high 64
  extr  x9,  x9, x10, #0x1       ; >> 1 — divide by 2
  add   x0,  x8, x9              ; (n-1) + (n-1)(n-2)/2 = n(n-1)/2
  ret
```

Better, the per-call result is dropped (`let _ = busy_loop(N)…` in
the bench source), so dead-code elimination further removed the
busy_loop *calls themselves* from the four `fetch_*` helpers — they
now compile to immediate-return-the-constant:

```
_fetch_latest_order_id:
  mov w0, #0x3e9   ; 1001
  ret
```

**This means the bench is no longer measuring fan-out work** — both
Kāra and Rust got the busy_loops fully elided by their optimizers.
The 97 K req/s the Kāra row reports is the **trampoline + HTTP-path
ceiling** (consistent with the earlier no-op-handler probe at 108 K
— the 11 K gap is per-request `get_dashboard` framing overhead that
DCE can't eliminate because of the `Dashboard` struct construction
+ FFI boundary).

**Reading the post-fix table.** Apples-to-oranges: the codegen fix
moved Kāra into a regime where its trampoline ceiling shows
through. Comparing 97 K (Kāra trampoline) to 47 K (Rust still doing
some real work — Rust's release codegen also elides the loops, but
hyper + tokio overhead on Rust's side is more substantial than on
Kāra's hand-rolled trampoline). The Kāra row is faster than Rust
*on this bench*, but it does not mean Kāra is faster than Rust at
real work — it means **the bench is no longer load-bearing** for
the comparison the design intended to set up (fan-out efficiency
under sustained CPU load).

**Recommended next step (separate slice).** Make the bench
optimization-resistant. Two paths:
1. Use `std::hint::black_box` (Rust) and a Kāra equivalent (TBD —
   likely a `karac_runtime_black_box` extern) around the busy_loop
   results so the optimizer can't elide them.
2. Weave `Dashboard`'s field values back into the response body —
   the `f-string`-codegen-gap follow-up enumerated in
   `examples/parallax/bench/kara/server.kara:99-114` would close
   this naturally, since the field values would have user-observable
   uses.

(2) is the better long-term path because it also closes the
"response body is a fixed JSON literal" v1 limitation noted in the
bench README; (1) is a quick fix that preserves the bench's
diagnostic shape without depending on the codegen-gap follow-up.

**The Kāra-vs-Rust gap is now closed (or inverted) for this
specific bench, but the closure is partly artifact-of-DCE rather
than actual runtime-speed parity.** The codegen fix is real and
correct (every production compiler does this); the bench needs the
follow-up above before its numbers become apples-to-apples again.
