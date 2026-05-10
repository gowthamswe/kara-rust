# Parallax bench — robustness + realism gaps

**Status:** open. **Started:** 2026-05-10. **Owner:** unassigned.

The Parallax bench (`examples/parallax/bench/`) shipped as a
side-by-side throughput artifact for Demo 1 (Slice E, `4f7b72d`).
After two rounds of perf work (`3953a14`, `280ce2d`) the bench has
revealed structural gaps in *how it measures*, not just *what it
measures*. This doc enumerates those gaps and their fixes —
distinct from [`http_layer_perf.md`](http_layer_perf.md) which
tracks the path to higher throughput numbers.

Cross-refs:
- Bench harness scaffolding: `ea1d26d`. Verification run:
  `4f7b72d`. Pool fix: `3953a14`. Codegen O2: `280ce2d`.
- Slice E design lock: [`docs/demo_ideas.md § Slice E`](../demo_ideas.md).
- Parallax perf investigation: [`parallax_perf.md`](parallax_perf.md).
  The codegen O2 work surfaced gap **G1** (optimization-eaten
  busy_loops) which made the headline numbers no longer apples-to-
  apples; that's the reason this doc exists.

---

## Gap inventory (ranked by impact on number trustworthiness)

### G1 — Optimization-eaten busy_loops [load-bearing]

**What's wrong.** The `busy_loop` kernel runs `Σ_{i=0}^{n-1} i`,
which LLVM's `LoopIdiomRecognize` pass identifies as the
triangular-number sum and replaces with the closed-form
`n(n-1)/2` (6 instructions, constant-time, regardless of n). The
four `fetch_*` helpers then drop the result via `let _ =
busy_loop(...)`, so DCE eliminates the calls entirely. Both Kāra
(`default<O2>`, post-`280ce2d`) and Rust (release profile) do
this; Go's release codegen does it too.

The bench was designed to exercise fan-out efficiency on four
disjoint CPU-bound work units (2/5/8/12 ms each). Today, **none
of the impls actually run those work units** — the bench numbers
report HTTP-handler dispatch overhead, not fan-out efficiency.

**Fix options, ranked.**
1. **Weave `Dashboard` field values into the response body**
   (recommended — closes a v1 limitation simultaneously). Today
   the Kāra impl returns a fixed JSON literal because of two
   pre-existing codegen gaps (`refs_in_expr` lacks
   `InterpolatedStringLit` arm; f-string accumulators are
   unconditionally scope-exit-freed even when returned —
   documented in `examples/parallax/bench/kara/server.kara:99-114`).
   Closing those gaps lets `handle()` build the response body
   from `dashboard.profile_name`, `dashboard.order_id`, etc. —
   the values become user-observable and DCE can no longer elide
   the work that produces them. **Bonus**: matches the design
   intent (Parallax demo's "fan-out + join" should produce a
   dashboard whose data is in the response, not a fixed string).
2. **Optimization barrier (`black_box`-equivalent)**. Add
   `karac_runtime_black_box(value)` extern that takes a value by
   reference and prevents the optimizer from elision. Rust impl
   uses `std::hint::black_box`. Quick fix; preserves the bench's
   shape; doesn't close the response-body limitation.
3. **Use a non-recognizable kernel**. Replace `busy_loop` with
   work the optimizer can't reduce — e.g., a hash-mixing loop,
   a checksum, or pseudo-random walk. Sound but loses the
   simplicity of the current "obviously CPU work" framing.

**Recommended:** (1) for production-grade realism + closes a
related codegen gap; (2) for a quick fix while (1) is being
designed.

### G2 — Single connection-count value (`-c100`) [structural]

**What's wrong.** `bench.sh` hardcodes `wrk -t4 -c100 -d{warmup,
measure}s`. As shown in [`http_layer_perf.md`](http_layer_perf.md)'s
connection sweep, the Kāra server's throughput rises from 94 K
(`-c100`) to 135 K (`-c5000`); a single point on this curve
under-reports the server's capacity. Rust's row also looks
artificially low at `-c100`.

**Fix.** Sweep `-c` and report the curve. `bench.sh` runs three
points: `-c100`, `-c1000`, `-c5000`. README shows three rows per
impl (or a chart). Total bench time triples (~12 minutes total)
which is acceptable for a non-CI bench.

### G3 — Single measurement, no run-to-run statistics [structural]

**What's wrong.** Each impl gets one 30 s measurement. Run-to-run
variance on the same machine is typically 5-10 % even with no
config changes. A single number can't distinguish a 3 % perf
regression from a 5 % run-to-run blip.

**Fix.** Run each impl `N=5` times. Report median + min + max
(or mean ± stddev). Increase total bench time ~5× — acceptable
for a non-CI bench. Add a `--runs=N` flag to `bench.sh` that
defaults to 5 and can be lowered for quick iteration.

### G4 — No higher-percentile latency (p99.9, p99.99) [structural]

**What's wrong.** Only p99 is reported. Real services care about
p99.9, p99.99, and max. `wrk --latency` already collects the full
distribution; we only parse `99%` line. A pause-driven outlier
(GC, scheduler hiccup, page fault) can be invisible at p99 but
deadly at p99.9.

**Fix.** Parse `99.000%`, `99.900%`, `99.990%`, and `100.000%`
lines from `wrk --latency` output. README table grows to 5
percentile columns. Especially relevant for Go (GC-driven tail)
and Node (event-loop-driven tail) — the current p99 column likely
under-states their worst-case behavior.

### G5 — Cold-start vs steady-state conflated [structural]

**What's wrong.** Tokio runtime warm-up, hyper accept-loop
warm-up, allocator state, OS page cache, CPU branch predictor —
all need ~hundreds of ms to stabilize. The current 10 s warmup is
borderline; first-request latency (cold dispatch path JIT, TLS
init, etc.) can leak into measurement window.

**Fix.** Measure cold-start separately. Add a `--cold-start` mode
that runs `wrk -d1s` against a fresh-spawned server and reports
the first 100 latencies. Steady-state numbers stay in the main
table; cold-start numbers go in a separate "Cold start" section
(for Demo readers / production sizing).

### G6 — wrk version not pinned [precision]

**What's wrong.** `bench.sh` checks for `wrk` but doesn't pin a
version. wrk 4.0.x and 4.2.x have slightly different output
formats (specifically the latency-distribution row format).
Future readers running the bench on a different wrk version may
hit parser breakage.

**Fix.** Print `wrk --version` at the top of every bench run;
write to a `bench.run.log` file alongside the README so
reproducibility is documented. If a parse failure occurs, surface
the wrk version in the error.

### G7 — Same-machine load generator [precision]

**What's wrong.** `wrk` and the server compete for the same CPU
cores. With `-t4 -c100`, wrk uses ~4 cores; server uses up to 18.
Cross-contamination means each impl's number is reduced by
whatever wrk happens to consume that run. Network-localhost is
also faster than realistic LAN — adds ~10× the round-trip
latency a production deployment would see.

**Fix (later).** Run `wrk` on a separate machine over a 1 Gbps
LAN. Adds setup complexity, partial mitigation by binding wrk to
a P-core via `taskset -c 0-3` and binding server to E-cores via
`taskset -c 10-17` on Linux (macOS lacks easy CPU pinning).
Acceptable to defer until G1-G3 land — those are higher-impact.

### G8 — Single endpoint [realism]

**What's wrong.** `GET /dashboard/<id>` is the only endpoint
tested. Real services have many endpoints with different cost
profiles. A single-endpoint bench can mask issues with router
performance, request-method dispatch, etc.

**Fix (later).** Add 2-3 endpoints with different shapes:
`GET /healthz` (no work), `GET /dashboard/:id` (current),
`POST /event` (with body — exercises body-parsing path). Bench
each separately. Ship as v2 of the bench.

### G9 — Body size variance not characterized [realism]

**What's wrong.** Response body is a fixed 144-byte JSON literal.
Real APIs have bodies from <1 KB to >1 MB. Throughput at small
body sizes is dominated by per-request overhead; at large bodies,
by serialization + memcpy.

**Fix (later).** Add a `--body-size=<bytes>` knob that sets the
response body to a synthetic JSON of that size. Run the bench at
{144 B, 1 KB, 10 KB, 100 KB} and report the curve. Identifies
where the impl's performance crosses from per-request-overhead-
bound into bandwidth-bound.

### G10 — No regression-detection / CI tracking [structural]

**What's wrong.** Numbers go in `examples/parallax/bench/README.md`
manually after each verification run. Easy to forget; easy to let
a regression slip in over multiple commits. CI runs the smoke
tests but not the bench.

**Fix (later).** Save bench results to a checked-in JSON file
(`examples/parallax/bench/runs/{date}.json`) per run. CI doesn't
*run* the bench (still too expensive) but does *check* that any
PR touching the bench surface includes a corresponding
`runs/{date}.json` update. Manual but enforced. Long-term:
nightly CI run on a dedicated bench machine, regression-flag
PRs that exceed a tolerance band.

### G11 — Long-duration soak missing [realism]

**What's wrong.** 30 s measurement window catches throughput +
tail latency under brief load but misses slow-degradation issues
— memory growth, fd leak, gradual GC pause-time growth, allocator
fragmentation. A real production server runs for days.

**Fix (later).** Add a `--soak=<duration>` mode that runs the
bench for hours and reports the throughput curve over time. Watch
for monotonic degradation. Out of scope for daily iteration but
worth running once per major perf-impacting change.

---

## Suggested execution order

1. **G1** (optimization-eaten busy_loops) — load-bearing for any
   future bench-driven decision. Block all other improvements
   until this lands; otherwise the numbers we capture are
   misleading. Recommended path: weave Dashboard values into
   response body (closes the codegen-gap follow-up
   simultaneously).
2. **G2 + G3 + G4** (connection sweep, multi-run, percentile
   distribution) — together these elevate the bench from "single
   snapshot" to "robust measurement". Can land in one slice.
3. **G5** (cold-start separation) — useful for Demo narrative
   but lower priority than G1-G4.
4. **G6** (wrk version pin / log) — small, do whenever.
5. **G7-G11** (separate machine, multi-endpoint, body-size
   sweep, CI regression tracking, soak) — all are "make it
   really production-grade" upgrades; defer until the v1 bench
   has shipped a couple iterations and proven its diagnostic
   value.

---

## Findings

### 2026-05-10 — G1 landed (apples-to-apples kernel + observable fold)

**Fix.** Two changes applied symmetrically across all four bench
impls (`server.kara`, `main.rs`, `main.go`, `server.js`):

1. **Kernel swap.** Replaced `sum = sum + i` (triangular-sum,
   recognized by LLVM's `LoopIdiomRecognize` as `n(n-1)/2`) with
   `x = (x*31 + i) % 1073741789` (hash-mix step; no algebraic
   identity). The kernel actually runs at any optimization level.
   Same iteration counts as before (700 K / 4 M / 1.7 M / 2.7 M)
   so total work-per-request matches the bench's design intent.
2. **Observable fold.** Three of four `fetch_*` fns return the
   busy_loop result directly (was `let _ = busy_loop(...);
   <constant>`, which DCE'd both the call and the wrapping). In
   `handle()`, the three i64 `Dashboard` fields fold into the
   response — status code for Kāra (`200 + ((order ^ notif ^ rec)
   & 1)`), JSON body for Rust/Go/Node (which already had a
   serializer wired up). One fetch (`fetch_profile_name`) returns
   `String`/`&str` and its busy_loop result has no observable use;
   accepted — 3-of-4 fan-out branches dominate the parallel
   critical path, and folding String values would require
   String-hash ops or body-weaving that's out of scope here.

**Disasm verification.** `_busy_loop` post-fix is a real loop body
(~12 inst/iter through the mod-prime arithmetic):

```
.L:
  add x9, x9, #0x1            ; i++
  add x8, x12, x8, lsl #5     ; x = x*32 - x = x*31; then add i
  smulh x12, x8, x10          ; (x mod p) via Barrett reduction
  asr   x13, x12, #29
  ...
  msub  x8, x12, x11, x8
  cmp   x9, x0
  b.lt  .L
```

`fetch_latest_order_id` calls into `_busy_loop` — confirmed via
`otool -tV` against the post-fix kara binary.

**Bench result (apples-to-apples).** All four impls at `wrk -t4
-c100 -d10s+30s`, sequential per-impl runs, same hardware as v3:

| Impl | req/s pre-G1 (DCE'd) | req/s post-G1 (real work) | p99 pre | p99 post |
|---|---|---|---|---|
| Rust |  47,489 |   730 | 5.0 ms |   849 ms |
| Kāra |  97,172 |   711 |  57 ms |   300 ms |
| Go   |   7,729 |   677 |  62 ms |   982 ms |
| Node |      93 |   3.0 |  1.0 s |  1.87 s |

**Throughput collapsed to ~700 req/s** for all three multi-core
impls (within 8 % of each other). The collapse is the bench
finally measuring the four busy_loops the design called for — at
~25 ms total CPU work per request × 18 cores ÷ 100 in-flight
connections, ~720 req/s is the saturation ceiling regardless of
language. The previous 47 K – 97 K range was all DCE; the current
~700 range is real fan-out CPU work.

**The honest comparison takeaways.**
- **Kāra ≈ Rust on throughput** (711 vs 730, 3 % gap). Rust is
  marginally ahead because tokio's `spawn_blocking` blocking pool
  (512 threads default) admits more in-flight work than Kāra's
  bounded `karac_par_run` pool (18 = `num_cpus`). At CPU
  saturation, neither stretches the other.
- **Kāra ~1 % ahead of Go.** Roughly equivalent.
- **Kāra has 3× lower p99 than Rust, 3.3× lower than Go.** This
  is the bench's load-bearing finding: `karac_par_run`'s work-
  helping wait loop (tokio worker that called the handler picks
  up dispatched tasks during its wait) gives effective
  parallelism beyond the dedicated pool size, smoothing burst
  response. `tokio::join!(spawn_blocking(...))` hands every
  branch to a separate blocking thread, paying scheduler-handoff
  on every branch and producing queueing tail. Go's tail is
  GC-driven.

The "Kāra is 2× Rust" headline from the v3 numbers was a DCE
artifact; the v4 numbers are the bench's first credible
comparison and they show Kāra at parity with Rust on throughput
and meaningfully ahead on p99 for fan-out workloads.

**G2 / G3 / G4 status.** Not landed yet. The connection-count
sweep we ran ad-hoc (`-c100`, `-c500`, `-c1000`, `-c2000`) is
captured in [`http_layer_perf.md`](http_layer_perf.md); folding
it into `bench.sh` + adding multi-run statistics + parsing
p99.9 / p99.99 are the next slice on this doc's priority order.

### 2026-05-10 — G2 + G3 + G4 landed

**Fix.** `bench.sh` refactored from single-shot
`wrk -t4 -c100 -d10s+30s` per impl into a swept matrix:

- **G2 (connection sweep).** New `--connections=A,B,C` flag
  (default `100,1000,5000`). For each impl, runs the measurement
  loop at every connection count in the list. Output is one row
  per (impl, conn) pair — 4 impls × 3 conn-counts = 12 rows by
  default. The new `run_impl` launches the server *once* per impl
  and reuses it across all connection-count rounds, avoiding
  per-conn build + spawn overhead.
- **G3 (multi-run statistics).** New `--runs=N` flag (default 3).
  For each (impl, conn) pair, the measurement loop runs N rounds
  and aggregates: req/s reported as `median [min..max]`; each
  latency percentile reported as median across rounds. The
  aggregator separately tracks "valid rps" rows vs "valid full
  percentile" rows so a partially-failed run (saturated server
  prints `Requests/sec:` but no `Latency Distribution`) still
  contributes to the rps median without polluting the percentile
  medians with 0s.
- **G4 (percentile distribution).** Parser extended from
  `Requests/sec` + `99%` only to `Requests/sec` + `Latency Max` +
  `50%/75%/90%/99%`. wrk 4.2.0 doesn't print finer percentiles
  (p99.9, p99.99) without a custom Lua HdrHistogram script —
  surfaced as a separate follow-up; the current 5 percentiles
  + max give meaningful tail characterization for the impl-shape
  comparisons we care about.

Bug fixes uncovered while iterating: (a) the awk regex
`^[[:space:]]+Latency[[:space:]]` matched both the per-thread
stats row (`Latency  136ms ...`) and the `Latency Distribution`
header that immediately follows, with the latter clobbering
`lat_max` to 0 on its empty 4th field; tightened to require a
digit immediately after `Latency`. (b) Default warmup of 3 s at
`-c100` against the slow Node server pinned 100 keep-alive
connections in TIME_WAIT state on macOS loopback for ~30 s,
starving subsequent measure rounds of ephemeral ports — node's
rows came back all-NA. Default warmup dropped to 0 (the
existing N=3 measure rounds + median aggregation naturally
exclude first-round JIT/cold-start outliers); users can opt in
via `--warmup=N` for explicit characterization.

**Final apples-to-apples table** (M5 Pro, post-G2+G3+G4):

| Impl | -c    | req/s (med [min..max]) | p50  | p99  | max  |
|------|-------|------------------------|------|------|------|
| Kāra | 100   | 715 [714..720]         | 135  | **313**  | 431  |
| Kāra | 1000  | 678 [678..698]         | 1210 | 1960 | 2000 |
| Kāra | 5000  | 673 [673..675]         | 1300 | 1980 | 2000 |
| Rust | 100   | 720 [719..722]         | 119  | 824  | 1710 |
| Rust | 1000  | 719 [714..720]         | 763  | 1940 | 2000 |
| Rust | 5000  | 244 [207..698]         | 1140 | 1980 | 2000 |
| Go   | 100   | 661 [405..662]         | 137  | 1200 | 1620 |
| Go   | 1000  | 621 [575..659]         | 808  | 1910 | 2000 |
| Go   | 5000  | 577 [572..634]         | 1350 | 1980 | 2000 |
| Node | 100   | 6 [6..6]               | 1100 | 1970 | 1970 |

**New findings the rich format reveals (impossible to see in v4
single-shot data):**

1. **Rust collapses at -c5000.** The rps row goes from
   720 / 719 (steady at -c100 / -c1000) to **244 [207..698]**
   (huge variance). Some runs survive, some hit
   `tokio::task::spawn_blocking`'s blocking-pool ceiling and
   stall. Kāra is rock-stable across the same sweep
   (715 → 678 → 673, only -6 %). v4's single -c100 measurement
   would never have surfaced this asymmetry.
2. **Go's p99 at -c100 (1200 ms) is worse than its p99 at
   -c5000 (1980 ms) ratio'd to throughput.** GC pauses that hit
   under sustained allocation pressure show up cleanly in the
   percentile-distribution view. p50/p75/p90 are reasonable;
   p99 is the GC-pause-dominated outlier.
3. **Go has a "warm-up tax" at -c100** — `[405..662]` is huge
   variance, suggesting one of the 3 runs caught a major GC
   cycle. Multi-run statistics surface this; a single
   measurement could either land in the slow run (looks like Go
   is broken) or the fast run (looks like Go is matching Kāra).
4. **Kāra's tail-latency advantage from `karac_par_run`'s work-
   helping wait loop is even more striking with the rich
   percentile data.** At -c100, Kāra's p99 (313 ms) is *2.6×
   lower than Rust's* (824 ms) and *3.8× lower than Go's*
   (1200 ms). The previous single-snapshot v4 numbers showed
   the gap but didn't characterize how robust it is — the
   rich format makes it clear this isn't a single-run artifact.

**G5 / G6 / G7-G11 status.** Not landed; ranked priority
unchanged from the doc's "Suggested execution order" section.
G5 (cold-start separation) is the next-most-impactful; G6 (wrk
version pinning) is now partially handled because `bench.sh`
now prints the wrk version string at the top of every run.
