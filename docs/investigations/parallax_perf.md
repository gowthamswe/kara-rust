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

_(empty — fill in as probes run; date each entry, link to commits
or supporting artifacts.)_
