# 64. Backend-First Positioning and the v1 Concurrency Ceiling

**Status:** Open. Draft 2026-05-06.

**Trigger:** Positioning shift — Kāra v1 is backend-first; REPL / data-science / comptime / AR are *add-ons that compose*, not co-equal v1 personas. This invalidates the framing under which **G7** (the v1 blocking-I/O ceiling) was resolved in `brainstorming/archive/v1.md` and codified into the current roadmap as the Phase 6.2 / 6.3 split. Phase 6.3 (network event loop, ~100K-connection target) currently lives in v1.1; the question is whether backend-first positioning can ship credibly without it.

The line that triggered this re-open: *"a language that can't break 10K concurrent connections gets excluded from that proving ground"* — `v1.md` G7's own framing of the problem it then deferred.

This brainstorm decides:
- What "backend-first v1" actually means as an adoption story.
- Whether Phase 6.3 (and how much of the surrounding stdlib floor) moves into v1.
- The right concurrency target — 10K (status quo), 100K (current 6.3), 1M+ (Go/Tokio/Loom-class), or something in between.
- The P0 / P1 carve so the work fits inside v1 without scope-blowing the rest of the roadmap.

Per stored priority-tier definitions, **v1 = P0 + P1**. Anything labeled P2 here is post-v1.

---

## Problem 1. What "backend-first" actually means

"Backend-first" is not one feature — it's a coherent positioning bundle. Before deciding what v1 needs, it's worth being concrete about what programs Kāra v1 must be able to host without users hitting a wall and switching to Go / Rust / Elixir.

**The canonical backend workloads, ranked by how aggressively they exercise the runtime:**

| Workload | Connection model | What it stresses |
|---|---|---|
| HTTP/1.1 request-response API | Short-lived, ~10ms-1s per connection | Throughput (RPS), low jitter, GC/RC pauses |
| HTTP/2 multiplexed API | Long-lived, multiplexed streams | Stream framing, flow control, suspend/resume |
| WebSocket / SSE / long-poll | Long-lived, mostly idle | **Concurrent idle connections** (the headline ceiling) |
| API gateway / BFF | Fan-out + join | Auto-concurrency story — Kāra's unique angle |
| gRPC service | Long-lived multiplexed | TLS, protobuf, HTTP/2 |
| Real-time messaging / chat | Long-lived idle | Idle connection ceiling, push semantics |
| Streaming pipeline / ETL | Long-running compute | Throughput, memory, backpressure |
| Database driver / ORM | Connection pool | Connection pooling primitives |
| Sidecar / service mesh | Many short connections | Connection setup cost, TLS handshake |

**Three structural concerns the bundle imposes:**

1. **Concurrency ceiling.** The "10K connections" headline. This is the most visible bar — every Hacker News comparison post leads with it.
2. **Latency floor (P99 / tail).** Backend systems are judged on tail, not mean. RC fallback decisions, allocator behavior, scheduler fairness all show up here.
3. **Operational story.** TLS, observability, structured logging, panic recovery, graceful shutdown, healthchecks. Less glamorous, but a language without these is "research project" not "backend stack."

**Three things "backend-first" does *not* mean:**
- It does not mean Kāra needs an opinionated framework like Spring or Phoenix at v1. Floor primitives + stdlib HTTP are enough.
- It does not mean Kāra needs to win benchmarks against Go / Tokio at v1. Being competitive (within ~2x on apples-to-apples) is enough; being structurally capable is the bar.
- It does not mean abandoning the other personas. REPL / data-science / comptime compose on top of a backend-grade runtime — they don't conflict with it. The concurrency runtime, async I/O, and stdlib floor that backend-first demands are *also* what serious data-engineering REPL workloads need.

---

## Problem 2. Decomposing the 10K ceiling

The "10K" number is a memorable headline; the *actual* mechanism is two separate ceilings stacked on top of each other. Conflating them led to G7's "phased acceptance" being more conservative than it had to be.

**Ceiling A — memory.** OS threads cost stack space. Linux pthread default is 8 MB; 10K threads = 80 GB of *virtual* address space (most unmapped, but address-space-bound matters on 32-bit and constrained embedded; on 64-bit servers it's mostly accounting). Reduce per-thread stack to 256 KB and 10K threads = 2.5 GB virtual, fully workable; push to 100K threads = 25 GB virtual, still fine on a 64-bit box with plausible RSS.

**Ceiling B — scheduler.** Linux can schedule O(100K) threads but the cost of context switches, TLB flushes, and runqueue contention rises non-linearly. A box that runs 10K threads at full utilization is fine; 100K threads with 1% active is fine; 100K threads with 50% active is a meltdown. The *idle* keep-alive case is where event loops decisively win: 1M idle WebSocket connections = 1M file descriptors + a tiny per-connection state struct, vs 1M parked threads.

**The actual cliff for backend-first:**

- For **request-response APIs at moderate scale** (sub-10K concurrent in-flight requests at a time), the v1 thread-per-blocked-task model is genuinely fine. Most APIs live here.
- For **idle-keep-alive systems** (WebSocket servers, long-poll, real-time messaging), the cliff is sharp — even at "10K connections" most are idle, and thread-per-connection is the wrong shape entirely. Event loop wins by 10-100×.
- For **fan-out gateways** (the demo Parallax-lite is shaped like, the demo Parallax is shaped like) — limited by upstream count × in-flight requests. Usually fine on threads.

The "10K excludes you from backends" framing was right — *but the exclusion is from the idle-keep-alive subset*, not from backends generally. This matters because it changes which v1 deliverables are load-bearing.

**Implication.** Stack-size tuning (Ceiling A) is cheap and helps the request-response case, but does **not** fix idle-keep-alive. Only an event loop fixes idle-keep-alive. So if backend-first means "we want WebSocket / SSE / real-time servers as a v1 use case," option (b) below is insufficient — only (c) or stronger crosses the threshold.

---

## Problem 3. Where Kāra's roadmap actually stands today

Pulling current state from `docs/roadmap.md`:

**Phase 6: Auto-Concurrency Runtime — header reads "COMPLETE (6.1 + 6.2; 6.3 deferred to v1.1)".**

| Sub-phase | Status | Mechanism | Concurrency ceiling |
|---|---|---|---|
| 6.1 Concurrency Analysis | ✅ done | Effect-based dependency graph, parallelization decisions | n/a (analysis only) |
| 6.2 v1 Runtime (Blocking I/O) | ✅ done | OS threads via `std.thread.scope`, work-stealing pool | ~10K blocked tasks |
| 6.3 v1.1 Runtime (Network Event Loop) | ❌ deferred | epoll/kqueue + state-machine transform for network-boundary functions | ~100K idle connections |
| Full hybrid (state-machine for arbitrary code) | post-v1 | state-machine transform of any `suspends` function | ~1M+ tasks |

**Stdlib reality from `roadmap.md`:**

- **Phase 8 (floor stdlib):** `std.json`, `std.time`, `std.path`, `std.error`, `std.mem`, `std.bytes`, `std.cmp`, `std.hash` — plus full method sets for `Vec` / `Map` / `Set` / `String`, operator traits, I/O wrappers, providers.
- **Phase 11 (long-tail stdlib):** `std.regex`, `std.http`, `std.process`, security primitives, numerical primitives.

So the canonical placement today is: **HTTP server is post-v1 (Phase 11)**, **network event loop is v1.1 (Phase 6.3)**, **TLS / protobuf / WebSocket are not on any explicit list and would naturally land in Phase 11 or community libraries**.

**The G7 framing in `v1.md` resolved at "phased — accept 10K at v1, lift to 1M+ in v1.1".** The current roadmap softened the v1.1 target from 1M+ to 100K (Phase 6.3 done-when says "100K concurrent idle connections"). That softening wasn't explicitly re-litigated; it appears to be incidental drift. Worth flagging.

**One more relevant fact:** Phase 6.3 explicitly includes *"State machine transform: For network-boundary functions only (limited scope)"* — meaning the truly hard part of async (suspending arbitrary code) is partially in scope for 6.3, just bounded to functions that touch sockets. That's a non-trivial implementation undertaking but well-bounded.

---

## Problem 4. Prior-art concurrency targets

Establishing the credibility bar by looking at where comparable backend-grade languages ship:

| Language / runtime | Mechanism | Practical ceiling | Notes |
|---|---|---|---|
| Go | M:N goroutines, growable stacks, integrated netpoller | ~1M goroutines / process | Default 8 KB initial stack, grows to need |
| Erlang / BEAM | M:N processes, per-process heap | 1M+ processes | Telecom-grade — WhatsApp ran 2M connections / box on FreeBSD |
| Java Loom (virtual threads) | M:N on JVM, continuation-based | Millions | Production since JDK 21 (2023) |
| Tokio (Rust) | State-machine transform + work-stealing executor | 1M+ tasks | Async/await ecosystem |
| .NET (Task) | State-machine transform + thread-pool | ~100K-1M tasks | |
| Crystal | M:N green threads on libevent | ~100K-1M | Closest ergonomic analogue to Kāra |
| Node.js | Single-threaded event loop | ~10K-100K connections / process | Process-per-core scale-out |
| Python asyncio | Event loop, no preemption | ~10K-50K | The lower end of "credible" |
| Vert.x / Akka | Event loop / actor | ~1M actors | |

**The credibility floor for "this is a backend language" sits around 100K concurrent connections.** Anything below that gets tagged as "for moderate-scale apps" — which is fine for a niche language but not a position-defining one.

**The credibility ceiling is roughly 1M.** Above that is Erlang-class telecom territory and isn't required for the proving-ground.

So the right target window for Kāra v1, if backend-first is the positioning, is **somewhere in [100K, 1M] concurrent network connections per process**. The current Phase 6.3 target of 100K sits at the low end of credible; raising to 250K-1M is achievable with the same fundamental architecture (event loop + state-machine for network functions) but requires more polish on the implementation.

---

## Problem 5. Five paths — pick one

The actual decision surface. Lettered per the standard convention.

### (a) Keep current plan unchanged

**Shape.** v1 ships with the Phase 6.2 model (10K ceiling). Phase 6.3 stays in v1.1. HTTP server stays in Phase 11. Backend positioning is a stated *direction*, but the v1 shipping reality is "good for moderate-scale APIs, not for idle-keep-alive."

**Pros.**
- Zero new engineering scope. Roadmap already reflects this.
- v1 ships sooner.
- Language surface stable across phases — no premature commitments.

**Cons.**
- The "10K ceiling" benchmark posts get written at v1 launch and become the canonical reference. Hard to undo.
- Backend-first positioning is contradicted by shipping reality. Marketing claim fails technical scrutiny on day one.
- Forces every early adopter who *was* going to write a backend to wait for v1.1 — and v1.1 is downstream of stabilization, so realistically 6-12+ months later.

**When this is correct.** If "backend-first" is interpreted as a *medium-term* positioning that v1 can foreshadow but not deliver — i.e., we're willing to launch v1 as "promising backend language, full backend story in v1.1." This is honest but loses momentum.

### (b) Stack-size mitigation only

**Shape.** Phase 6.2 ships as-is, but with smaller default thread stack (256 KB instead of 8 MB) plus a tunable knob. Ceiling moves from ~10K to ~50-100K threads.

**Pros.**
- Cheap. A few days of runtime work + benchmarking + a tuning knob.
- Moves the headline number out of "embarrassing" and into "moderate."

**Cons.**
- Does **not** solve idle-keep-alive (Problem 2 cliff). 100K idle threads = 100K context switches when a small fraction become active.
- Scheduler overhead at 100K threads is real; running benchmarks at that scale will look bad.
- Trades one bad number ("10K") for a slightly less bad number ("50K under load, 100K idle if you tune") that still puts Kāra below the credibility floor.

**When this is correct.** As a *complement* to (a) or as a stop-gap before (c) lands — never as the headline answer. If shipped alongside (a), the v1 story improves marginally.

### (c) Promote Phase 6.3 to v1, target 100K-250K

**Shape.** Network event loop ships in v1. The Phase 6.3 target (100K idle connections) is the v1.0 baseline; minor polish work pushes it toward 250K. State-machine transform stays scoped to network-boundary functions only (no full-async-everywhere). Phase 6.4-or-equivalent (full hybrid) stays post-v1.

**Pros.**
- Crosses the credibility floor at v1 launch.
- Language surface unchanged from current design — `suspends` effect, scheduler verbs, structured concurrency lifetimes already specified. The work is runtime engineering against a stable spec.
- Solves idle-keep-alive — WebSocket / SSE / real-time messaging become first-class v1 use cases.
- Aligns the runtime with the auto-concurrency story (sends/receives(Network) routes through the event loop, the differentiator gets to do its job at scale).

**Cons.**
- Substantial runtime engineering. Ballpark: a few months of sustained work on a well-understood pattern, even with LLM-assisted velocity.
- State-machine transform — even bounded to network-boundary functions — touches codegen, debugger contract (already partly specified in roadmap Phase 6.1), unwinding semantics, FFI boundary, and panic-during-suspend behavior. Each touchpoint is a real design problem.
- Pushes v1 launch later. The opportunity cost is shipping the rest of v1 (compiled stdlib floor, Phase 8/9/10) on a longer timeline.
- 100K is below where Go / Tokio / Loom sit. Comparison posts will say "credible but not best-in-class" — better than (a)'s outcome but not a flex.

**When this is correct.** This is the *right* answer if backend-first positioning is real and v1 launch is willing to absorb the runtime-engineering scope.

### (d) Aggressive: 6.3 promoted, target 1M+

**Shape.** Same as (c) but with explicit 1M+ target. Pushes implementation polish — efficient timer wheel, optimized fd-set primitives, careful task-state representation, Linux io_uring path for file I/O alongside epoll for sockets.

**Pros.**
- Best-in-class headline. "Kāra ships with Go/Tokio/Loom-class concurrency at v1" is a *position-defining* claim, not just a credibility claim.
- Defensible against benchmark scrutiny from day one.
- Compounds with auto-concurrency — at 1M tasks, the auto-parallelization machinery becomes meaningful in a way it isn't at 10K-100K.

**Cons.**
- Engineering cost roughly 2× of (c) — most of the polish work is the last 5× of scaling, not the first 10×.
- io_uring is Linux-specific and recent — portable abstraction is its own project. macOS / Windows / BSD parity adds significant scope.
- Risk of overshoot: chasing 1M targets reveals second-order issues (memory fragmentation under churn, GC/RC pause distribution, allocator behavior at scale) that aren't visible at 100K. Can blow the timeline.
- Benchmark-driven engineering can detune the design — some of the optimizations Tokio uses are genuinely complicated and bake into the language's correctness story.

**When this is correct.** If backend-first is *the* core positioning and Kāra is willing to spend the engineering for a defensible flagship number.

### (e) Extreme: full hybrid (state-machine transform of arbitrary code) in v1

**Shape.** State-machine transform applied to every `suspends` function, not just network-boundary. Kāra becomes Tokio-shaped at v1.

**Pros.**
- Most flexible runtime — any user-written suspending function composes naturally.
- Cleaner conceptual story (no "but only for network functions" caveat).

**Cons.**
- Substantially more codegen work. State-machine transform of arbitrary control flow (across try/defer/errdefer, RC drops, panic unwinding, generics, FFI) is a multi-quarter effort by itself.
- Language-surface implications: RAII-across-yield rules become load-bearing for *every* `suspends` function, not a narrow subset. Currently a "warning" in 6.3; would need to be a hard rule.
- Almost certainly delays v1 by 6-12 months on its own.
- Does not buy meaningfully more headline concurrency than (d) for the dominant backend workloads.

**When this is correct.** Probably never for v1. The cost/benefit is wrong; (d) covers the headline cases at much lower cost. (e) is a v2 story.

---

## Problem 6. What else does backend-first imply for v1 stdlib?

A 1M-connection event loop with no HTTP server in stdlib is not a backend story. Concurrency runtime is necessary but not sufficient — backend-first positioning forces re-evaluation of the Phase 8 vs Phase 11 split.

**Currently in Phase 8 (floor stdlib, ships with v1):**
- `std.json`
- `std.time`
- `std.path`
- `std.error`, `std.mem`, `std.bytes`, `std.cmp`, `std.hash`
- Full collection method sets, operator traits, I/O wrappers, providers

**Currently in Phase 11 (long-tail, post-v1):**
- `std.http`
- `std.regex`
- `std.process`
- Security primitives (TLS, crypto)
- Numerical primitives

**For backend-first v1, the candidates to pull from Phase 11 into v1:**

| Module | Lift to v1? | Why |
|---|---|---|
| `std.http` (HTTP/1.1 server + client) | **P0** | The single most important "is this a backend language" demo. Without it, the event loop has nothing to drive. |
| TLS (`std.tls` or `std.crypto`) | **P0** | HTTPS is non-negotiable for backend-first credibility. Likely vendored rustls / native bindings. |
| `std.process` | P1 | Subprocess spawning. Useful but not flagship. |
| `std.regex` | P1 | Common but workaroundable via string ops. |
| WebSocket | P0 | If idle-keep-alive is the headline use case, WebSocket is the canonical example. |
| HTTP/2 | P1 | Required for gRPC; nice-to-have if HTTP/1.1 ships solid. |
| HTTP/3 | P2 | Post-v1. Even Go is still rolling this out. |
| protobuf | P1 | gRPC-adjacent; can ship as community lib. |
| gRPC | P1 / P2 | Depends on HTTP/2 + protobuf — if both land, gRPC follows. |
| `std.tracing` (OpenTelemetry-shape) | P1 | Operational story matters but can ship in v1.x. |
| Database drivers (Postgres, etc.) | P2 / community | Not stdlib material in any modern language. |

**Implication.** Backend-first v1 means at minimum lifting `std.http` (HTTP/1.1 server + client + WebSocket) and TLS into v1 from Phase 11. That's another non-trivial scope chunk on top of the runtime work.

**The good news:** much of this is mechanical work that LLMs accelerate well — HTTP/1.1 parsing, TLS via rustls binding, WebSocket framing are *well-documented protocols with reference implementations*. Not language-design work, just execution. The leverage is high.

---

## Problem 7. P0 / P1 carve under "backend-first v1"

Recommended decomposition assuming option (c) or (d) is chosen:

### P0 — must ship for v1 to be backend-credible

1. **Network event loop runtime** — epoll/kqueue/IOCP wrapper, integrated with the existing work-stealing scheduler. (Promotes Phase 6.3 to v1.)
2. **Effect-routed task parking** — `sends/receives(Network)` triggers park-on-event-loop instead of block-on-OS-thread.
3. **State-machine transform for network-boundary functions** — codegen path for functions with `sends/receives(Network)` in their inferred set. Bounded scope; arbitrary `suspends` functions stay thread-blocking.
4. **`std.http` (HTTP/1.1)** — server + client, basic enough to write a real backend. Connection lifecycle, keep-alive, chunked transfer, Host routing.
5. **TLS** — vendored rustls or platform-native, sufficient for HTTPS server + client.
6. **WebSocket** — RFC 6455 framing, handshake, ping/pong, close. Built on `std.http`.
7. **Concurrency target: ≥100K idle connections per process** — measurable, benchmarkable, in a flagship demo.
8. **RAII-across-yield enforcement** — currently spec'd as a warning in Phase 6.3; for backend-first v1 it should be a hard error to prevent silent footguns.
9. **A flagship backend demo** — Parallax (the planned auto-concurrency API gateway) but executed on the v1 runtime, not Parallax-lite. Demonstrates concurrency credibility under realistic load.

### P1 — additive, ship in v1 if time permits, otherwise v1.1

10. **HTTP/2** — multiplexed streams, flow control. Prerequisite for gRPC.
11. **`std.process`** — subprocess spawning + wait + I/O.
12. **`std.regex`** — common backend need.
13. **protobuf** — wire format + codegen (could lean on comptime).
14. **`std.tracing`** — OpenTelemetry-shape spans + propagation.
15. **File-system event loop** — io_uring on Linux, sticky kqueue elsewhere. Improves disk-I/O-heavy backends.
16. **Concurrency target: ≥1M idle connections** — pushes from credible to flagship.
17. **Connection-pool primitives** — `Pool[T]` with `acquire/release`, bounded waiters, health checks.

### P2 — post-v1, deliberately deferred

18. **Full-hybrid state-machine transform** (option (e)).
19. **gRPC** — depends on HTTP/2 + protobuf landing first.
20. **HTTP/3 / QUIC.**
21. **Database drivers** — community territory in every modern language.
22. **Microservice mesh primitives.**
23. **Custom executors / pluggable schedulers.**

---

## Problem 8. Risk and second-order effects

**Risk: scope blow-up.** P0 above is ~6-9 months of focused runtime+stdlib work even with LLM acceleration. Layering this on top of the rest of v1 (Phase 8 stdlib floor, Phase 9 semantic lock, Phase 10 cross-compilation) plausibly pushes v1 back by 4-6 months. *Mitigation:* aggressively cut P0 to the smallest credible set; defer everything that isn't load-bearing for the launch demo.

**Risk: language-surface churn.** State-machine transform for network functions surfaces design questions: panic-during-suspend, debugger contract for parked tasks, FFI calls inside suspending code, RC-drop ordering across yield points. Each is its own design problem. *Mitigation:* most of these have spec sketches in design.md (debugger contract, RC dataflow, panic unwinding) — surface them, audit for completeness, don't invent new design under deadline pressure.

**Risk: benchmark trap.** Once Kāra has a public concurrency number, every release becomes a benchmark-regression watch. Detuning happens. *Mitigation:* establish a CI benchmark gate on the flagship demo (Parallax-class) early; treat regression as a release blocker.

**Risk: LLM-assisted hubris.** "LLMs make this fast" is true for mechanical work (HTTP parsing, TLS binding) but false for novel correctness work (state-machine transform interacting with effect system + ownership + RC). *Mitigation:* treat correctness-critical pieces (transform, RAII-across-yield, panic-during-suspend) as non-LLM-accelerated; budget human-review time at full multiplier.

**Second-order positive: data-science and REPL benefit.** A serious data-engineering REPL hitting Kafka, S3, or Postgres drivers wants the *exact same* event loop + connection-pool primitives the backend story needs. Backend-first investment compounds into the secondary persona.

**Second-order positive: comptime.** Compile-time HTTP route tables, comptime protobuf codegen, comptime SQL parsing — all of these align well with `std.http` + protobuf landing in v1 stdlib. The comptime investment from v60 item 31 finds its first flagship use case here.

**Second-order risk: stability lock.** Phase 9 is "semantic lock" — once stdlib APIs stabilize, breaking changes are expensive. Pulling `std.http` into v1 means locking the HTTP API surface earlier than planned. *Mitigation:* audit `std.http` API explicitly against Go's `net/http` and Rust's `hyper` for known footguns before lock; consider a `v1` / `v2` namespace pattern (`std.http.v1`) if that's needed for evolution.

---

## Problem 9. Open questions

- ⊘ **What's the right v1 target — 100K (option c), 250K, or 1M+ (option d)?** This is the single biggest open question. 100K crosses the credibility floor; 1M is a flag-planting number. The cost difference is substantial — order-of-magnitude in scaling polish work.

- ⊘ **Does `std.http` ship in v1 floor stdlib, or in a "v1 platform" tier between Phase 8 floor and Phase 11 long-tail?** The roadmap currently has a binary split (floor / long-tail). Backend-first might motivate a third tier — "v1 backend platform" — that holds `std.http`, TLS, WebSocket, `std.tracing` together as a coherent backend bundle separate from both the floor and the long-tail.

- ⊘ **TLS provider story — vendored rustls, vendored OpenSSL, or platform-native (Schannel/SecureTransport/OpenSSL)?** Each has trade-offs (binary size, security update lifecycle, FFI surface). v37's "WASM bundle" positioning may push toward a single vendored cross-platform option.

- ⊘ **Is the Parallax demo (full version, with four upstreams + full provider story) the right v1 flagship, or something narrower?** Parallax is currently scoped as a v1 demo; but its cost-model-tuning angle is entangled with research questions that may not block v1 launch.

- ⊘ **State-machine transform — full design audit or incremental?** Phase 6.3 currently lists this as a single bullet ("State machine transform: For network-boundary functions only — limited scope"). Backend-first v1 needs this fleshed out as a full design subsection in design.md before implementation starts.

- ⊘ **Does "backend-first" change the marketing-tier ordering of personas in design.md's preamble / README?** Currently positioning is multi-modal. If backend-first wins, the lede shifts.

- ⊘ **What does `karac new --backend` look like as a project template?** If backend-first is positioning, the CLI should reflect it — `karac new` defaulting to a HTTP server skeleton, `--lib` / `--cli` / `--data` as alternates.

- ⊘ **Database driver question.** Stdlib in Go has `database/sql`; Rust has `sqlx` (community). Where does Kāra land? If `database/sql`-class lands in v1, that's another P1 chunk. If community-only, that needs to be a stated and defended choice.

- ⊘ **Backpressure primitives.** Backend systems rely on backpressure (semaphores, bounded channels, rate limits). Currently the providers story (design.md § Provider-Rooted Resources) handles per-provider concurrency limits at the *deployment* layer. Are stdlib backpressure primitives needed in user code at v1?

---

## Cross-references

- `brainstorming/archive/v1.md § G7` — the original "v1 blocking I/O — 10K concurrency ceiling" framing. Resolved as phased-acceptance there; this brainstorm re-litigates under backend-first positioning.
- `docs/roadmap.md § Phase 6` — current Phase 6.1 / 6.2 / 6.3 split.
- `docs/roadmap.md § Phase 8` (floor stdlib) and § Phase 11 (long-tail stdlib) — the boundary that "backend-first" reshapes.
- `docs/design.md § Runtime`, § Concurrency Semantics, § Execution Effects (`blocks` / `suspends`) — the language-surface contracts the runtime work targets. **No changes proposed** under option (c)/(d) — runtime engineering only.
- `docs/design.md § AI-First Compiler Interface > Debugger Contract` — already specifies metadata for parked tasks and `par`-block debug support; aligns with state-machine transform requirements.
- `brainstorming/archive/v37.md` — AR / WASM positioning. Compatible with backend-first; AR is a deployment target, backend-first is a workload class.
- `brainstorming/archive/v62_interpreter_perf_and_binary_size.md` — REPL + binary-size analysis. The REPL persona is now downgraded to "add-on"; v62's perf concerns still apply, but on a longer timeline.
- `brainstorming/63_llm_compiler_query_channel.md` — separate axis. Backend-first positioning has no direct interaction.

---

## Resolution path

This brainstorm decides:

1. **Positioning lock.** Backend-first is the v1 lead persona. design.md preamble + README updated to reflect ordering.
2. **Option lock.** Pick one of (a)/(b)/(c)/(d)/(e). Working assumption pending discussion: **(c) with explicit growth path to (d) before v1 launch**.
3. **Phase reorganization.** roadmap.md restructured: Phase 6.3 promoted into v1; new "Phase 8.5" or "Phase 9 prerequisite" tier holds `std.http` + TLS + WebSocket + `std.tracing`; Phase 11 retains regex / process / numerical / security long-tail.
4. **Open question close-out.** Each ⊘ above resolves to ✅ or ❌ before this doc archives.
5. **Implementation tracking.** P0 entries from Problem 7 land in `implementation_checklist/`; P1 entries land in `deferred.md` (per the P1-needs-checklist-entry convention) with corresponding `[→ P1]` lines in the checklist.
6. **design.md audit.** State-machine transform for network-boundary functions gets a full subsection; RAII-across-yield gets promoted from "warning" to "compile error" semantics; panic-during-suspend gets explicit specification.
7. **Archive.** This file moves to `brainstorming/archive/v64_backend_first_v1_concurrency.md`.

**Until resolved:** v1 scope decisions involving runtime, stdlib, or roadmap phase boundaries should reference this brainstorm. Significant downstream work (e.g., starting Phase 6.3 implementation) blocks on this decision.
