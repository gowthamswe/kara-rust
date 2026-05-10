# Parallax three-language benchmark

Side-by-side `GET /dashboard/<user_id>` throughput across **Kāra**,
**Rust**, **Go**, and **Node.js** — the recordable artifact for Demo 1
([`docs/demo_ideas.md § Demo 1: Parallax`](../../../docs/demo_ideas.md)).

Each impl serves the same canonical fan-out + join workload: four
provider "fetches" per request, each carrying `reads(R_i)` on a
disjoint resource, joined into a `Dashboard` aggregate. The Kāra impl
gets that fan-out from the compiler — straight-line sequential code,
the auto-par codegen runs the four reads concurrently. The other three
write the fan-out by hand (Rust `tokio::join!`, Go goroutines + WaitGroup,
Node `Promise.all`) and serve as the reference perf cohort.

## What this measures

**Throughput (req/s)** and **p99 latency** under sustained `wrk` load
on a single machine. Each impl is built and run in turn; the bench
captures `Requests/sec` and the 99th-percentile latency from `wrk
--latency` output.

The provider "fetches" are CPU-bound busy loops sized to roughly
approximate **2 / 5 / 8 / 12 ms** of latency on a modern x86-64 core
(`FETCH_PROFILE_WORK = 700K`, `FETCH_ORDERS_WORK = 4M`,
`FETCH_NOTIFS_WORK = 1.7M`, `FETCH_RECOMMEND_WORK = 2.7M` iterations
respectively). Total work per request: ≈ 27 ms sequential / ≈ 12 ms
fully parallel (waiting on the slowest fetch).

The asymmetry is deliberate (F5): it surfaces the "join waits on the
slowest provider" property in trace narration. Symmetric work would
look uniform across impls and hide the auto-par story's punch line.

> **Sleep substitute (deviation from the design's F5 lock).** F5
> originally specified `sleep_ms(n)` providers (real I/O simulation,
> no CPU burn). Kāra's stdlib has no `sleep_ms` in v1 (Phase 11
> long-tail). To keep the four impls apples-to-apples, **all four
> use CPU-bound busy loops** instead of sleeps. The shape of the
> benchmark — fan-out + join over four independent operations — is
> preserved, but the implication for measured throughput is
> different: with sleeps, throughput is driven by the event-loop
> scheduler; with busy loops, throughput is driven by core count
> and worker contention. The **relative ordering of impls** still
> reflects each runtime's fan-out efficiency on multi-core hardware
> (which is the demo's intended story); the **absolute numbers**
> are CPU-bound and won't match a real I/O-bound API server.

## How to reproduce

### Toolchain prerequisites

Each impl needs its language toolchain installed; `bench.sh`
graceful-degrades when one is missing (`skip: <lang> not installed`
to stderr, the bench continues with the rest).

| Impl  | Required toolchain | Tested with |
|-------|--------------------|-------------|
| kara  | `cargo` + this repo's `karac` build (auto-built by `bench.sh`) | rustc 1.x  |
| rust  | `cargo` (any stable) | rustc 1.x  |
| go    | `go`               | go 1.21+   |
| node  | `node`             | Node 18+   |
| wrk   | `wrk`              | wrk 4.x    |

### Run

```sh
# default — all four impls, 10s warmup + 30s measurement per impl
sh examples/parallax/bench/bench.sh

# dry-run (no servers spawned, no wrk; checked into CI via
# tests/parallax_bench.rs::test_bench_script_dry_run)
sh examples/parallax/bench/bench.sh --dry-run

# subset (kara + rust only)
sh examples/parallax/bench/bench.sh --impls=k,r

# tweak window
sh examples/parallax/bench/bench.sh --warmup=5 --measure=15
```

`bench.sh` builds each impl on the fly (Kāra via `karac build`, Rust
via `cargo build --release`, Go via `go build`, Node served directly
from `server.js`), launches it, awaits the conventional
`BOUND_PORT=<n>` stdout line, runs `wrk -t4 -c100 -dWARMUP+MEASURE`,
parses `Requests/sec` + `99% <lat>`, and kills the server.

The bench is **not** part of `cargo test`. CI runs only the smoke
tests in [`tests/parallax_bench.rs`](../../../tests/parallax_bench.rs):
a single-request Kāra-server smoke and a `bench.sh --dry-run`
syntactic gate. Throughput numbers are the bench's artifact, not a
regression gate.

## Throughput results

**Measured on 2026-05-10** (post-codegen-IR-opts fix; see History
below), Apple M5 Pro (10P + 8E cores, 18 logical CPUs), 64 GB RAM,
macOS 26.4.1, `wrk 4.2.0`. `bench.sh` defaults (`-t4 -c100`, 10s
warmup + 30s measurement, sequential per-impl runs).

| Impl   | req/s     | p99 latency | Notes                       |
|--------|-----------|-------------|-----------------------------|
| Kāra   | 97,172.48 |  56.83 ms   | auto-par fan-out, `default<O2>` mid-end passes; busy_loops elided by DCE — see ⚠ below |
| Rust   | 47,489.12 |   4.98 ms   | tokio + hyper + `tokio::join!`; busy_loops also DCE'd by `rustc` release codegen |
| Go     |  7,728.67 |  61.87 ms   | `net/http` + goroutines + `sync.WaitGroup`, default `GOMAXPROCS` |
| Node   |     92.71 |   1.01 s    | `http` + `Promise.all`, single-process per F4 |

> **⚠ The bench is currently optimization-eaten and not apples-to-
> apples.** Both Kāra and Rust release codegen identified the
> `busy_loop` body as the triangular-number sum (`Σi = n(n-1)/2`)
> and replaced the loop with the closed-form arithmetic. Rust then
> dead-code-eliminated the `let _ = busy_loop(...)` calls outright;
> Kāra's `default<O2>` pass pipeline does the same. Neither impl is
> actually doing the four 2/5/8/12 ms loops the design intended.
> The Kāra row's 97 K req/s is the **trampoline + HTTP-path
> ceiling** (no-op-handler probe gave 108 K) and the Rust row's
> 47 K reflects per-request hyper/tokio framing on Rust's side
> being heavier than Kāra's hand-rolled trampoline at the
> "everything optimizes away" floor — *not* a real fan-out
> comparison. Re-instating apples-to-apples requires an
> optimization-barrier in both impls (`black_box` in Rust + a Kāra
> equivalent extern, or weaving `Dashboard` values into the
> response body). Tracked at
> [`docs/investigations/parallax_perf.md § Recommended next step
> (separate slice)`](../../../docs/investigations/parallax_perf.md).

**How to read this for now.** The Kāra row's 97 K is *headline-
shape correct* — Kāra's compiler now produces optimized native
code for tight integer kernels at parity with Rust's release
codegen. The Kāra-vs-Rust *throughput* numbers in this snapshot
should not be taken as evidence that Kāra is faster than Rust on
real workloads — it's evidence that both impls' optimizers ate the
benchmark's intended work, and Kāra's per-request framing is
lighter than hyper/tokio's. Go's row is unchanged from pre-fix
(Go's release codegen has done DCE all along; the busy_loops never
ran in Go either); Node's row is unchanged because V8 is
conservative on this idiom + single-process serialization
dominates regardless.

## History

**v1 — first verification run (`4f7b72d`, 2026-05-09).** Kāra at
1,089.99 req/s / 438.18 ms p99, four-language table populated.
First end-to-end measurement of the Kāra HTTP stack under sustained
load.

**v2 — `karac_par_run` worker-pool fix (`3953a14`, 2026-05-09).**
Profiling diagnosed that 60 % of the process's CPU was being spent
in `mach_vm_protect` setting up pthread stack guard pages —
`karac_par_run` was creating fresh OS threads on every fan-out
call. Replaced with a long-lived worker pool: thread churn -94 %,
p99 -46 % (438 → 238 ms), CPU efficiency 9× better. Throughput
essentially unchanged because the bench was wrk-connection-bound,
not pool-capacity-bound at that point.

**v3 — codegen `default<O2>` pass pipeline (this commit,
2026-05-10).** Probe sweep revealed that `karac_par_run`, the
trampoline, frame-tracking, and HTTP layer were all *not* the
bottleneck — the no-op-handler ceiling is 108 K req/s. The
remaining gap was that karac never ran any LLVM mid-end
optimization passes on its IR (`OptimizationLevel::Default` was set
on the target machine but no `PassManager` was wired up), so
`busy_loop` shipped with locals in stack slots and inner loops at
~12 cycles/iter. Wiring `module.run_passes("default<O2>", …)` lets
LLVM's `mem2reg` + `loop-idiom` + `instcombine` reduce the loop
body to the Gauss-sum closed form and DCE the dropped results.
Throughput jumped 92× (1,054 → 97,172). See ⚠ above for why this
makes the bench less, not more, comparable until an optimization-
barrier is added.

Full investigation log + per-step disassembly + reasoning lives at
[`docs/investigations/parallax_perf.md`](../../../docs/investigations/parallax_perf.md).

## Fairness controls (F4)

Cross-language benchmarks are easy to slant; these are the controls
the design lock specifies:

- **Hardware:** all four impls run on the same machine, sequentially
  (one impl active at a time). Background load is the same for all.

- **Worker counts:** Kāra and Rust default to tokio's multi-thread
  runtime, which uses `num_cpus` workers — same as Go's default
  `GOMAXPROCS = num_cpus`. Node runs single-process. **No tuning
  knobs are pre-set;** every impl gets the runtime's natural default.

- **Single-process Node footnote:** Node's single-process default is
  faithful to the language's typical deployment reality. Node clusters
  scale roughly linearly with worker count via `cluster.fork()` at the
  cost of process orchestration; cluster-mode Node would multiply the
  number below by ~`num_cpus` but is **not** v1 of this bench. Reader
  takeaway: the Node row is honest about Node's single-process default,
  not a strawman.

- **wrk window:** `wrk -t4 -c100 -d10s` warmup (discarded) + `-d30s`
  measurement (recorded). Same window for every impl.

- **Same wire shape:** every impl returns a JSON body for `GET
  /dashboard/<id>`. Kāra returns a fixed JSON literal (see Source
  comparison below for the v1 codegen-gap workaround); the others
  serialize the dashboard struct via their language's standard JSON
  encoder. Body bytes differ in size by < ~30 bytes across impls —
  not a load-bearing throughput factor.

- **Path randomization (F2):** `wrk` URL is hard-coded to
  `/dashboard/1` in v1 of `bench.sh`. The original F2 plan called for
  a Lua script generating uniform IDs in `1..1000`; deferred for now
  because the busy-loop-based fan-out is `user_id`-invariant — there's
  no provider state to cache, so the fixed-ID and random-ID throughput
  numbers should be indistinguishable. If a future iteration adds
  per-user state, the Lua randomizer is a one-line addition to
  `run_wrk()`.

## Source comparison

Four impls, four idioms for the same problem.

- **[`kara/server.kara`](kara/server.kara)** — fan-out is implicit.
  `get_dashboard` is straight-line sequential code; the four
  `let p = fetch_X()` bindings carry disjoint `reads(R_i)` effects;
  the auto-par analyzer groups them into one `parallel_group` and
  the codegen lowers to `karac_par_run` over four worker threads.
  No `async`, no `await`, no `par {}`, no `Promise.all`. Run
  `karac build --concurrency-report kara/server.kara` to see the
  decision.

  **v1 limitation: response body is a fixed JSON literal.** Two
  pre-existing codegen gaps (the auto-par's `refs_in_expr` lacks an
  `InterpolatedStringLit` arm; f-string accumulators are
  unconditionally scope-exit-freed even when returned) gate weaving
  the dashboard's data into the response body. The four parallelized
  busy-loop fetches still run on every request — they're the
  benchmark surface — but their results don't ride back into the
  wire. Both gaps are filed for follow-up; see the in-source comments
  for the failure trace and the workaround rationale.

- **[`rust/src/main.rs`](rust/src/main.rs)** — `tokio` + `hyper` +
  `tokio::join!`. `get_dashboard` `await`s a `tokio::join!` of four
  `spawn_blocking` tasks. The natural perf ceiling for the cohort
  since Kāra's runtime sits on the same tokio multi-thread runtime;
  the Kāra-vs-Rust gap measures Kāra's value-type ABI + handler
  trampoline overhead vs raw Rust.

- **[`go/main.go`](go/main.go)** — `net/http` + goroutines +
  `sync.WaitGroup`. `getDashboard` spawns four `go func() { ... }`
  goroutines, each writes its result into a captured local, the
  `WaitGroup.Wait()` joins.

- **[`node/server.js`](node/server.js)** — Node `http` stdlib (no
  Express dep) + `Promise.all`. `getDashboard` `await`s
  `Promise.all([fetch_X(), ...])`. Single-process; CPU-bound busy
  loops resolve serially on the event loop thread. F4 footnote
  applies.

## Out of scope (deferred to follow-ups)

Per the design lock at [`docs/demo_ideas.md § Slice E`](../../../docs/demo_ideas.md):

- TLS, HTTP/2, WebSockets — Phase 11.
- Real database FFI (Postgres / MySQL / Redis) — Phase 11. Demo uses
  `sleep_ms(n)`-substitute providers (busy loops; see footnote above).
- Cluster-mode Node — footnoted; not implemented.
- Asciinema cast / video walkthrough — post-v1 polish.
- Multi-user load patterns (Zipf, sticky-session) — `--lua` randomizer
  if a future perf investigation calls for it.
- Splitting Parallax bench into a standalone repo — premature.

## See also

- [`docs/demo_ideas.md § Demo 1: Parallax`](../../../docs/demo_ideas.md) —
  the demo's design storyboard + Slice E settled-design-fork record
  (F1–F5 + Rust addition).
- [`examples/parallax/`](../) — the multi-file source-of-truth Parallax
  workload (provider impls, traits, resources). The bench's Kāra impl
  is a single-file restatement so `karac build` works without multi-file
  project mode codegen (parked as wip-list2 Theme 4).
- [`tests/parallax_bench.rs`](../../../tests/parallax_bench.rs) — the
  two CI tests that gate the bench harness (smoke + dry-run).
