# Kāra Demo Products

Post-Phase 10 showcase ideas. The compiler is self-hosting, LLVM backend is
live, auto-concurrency runtime works, WASM and GPU targets compile. The question
is: what do you build *with* it to prove the language to an outside audience?

Each demo is selected to showcase a differentiating capability that other
languages cannot replicate without significantly more programmer effort. The
audience is: other language designers, systems programmers, AI researchers, and
early adopters.

---

## Contents

- [Demo planning](#demo-planning) — five pillars + practical filters
- [Tier 1 — Must-Build](#tier-1--must-build-core-story-highest-impact) — Parallax, Mend, Slipstream
- [Tier 2 — High-Value](#tier-2--high-value-compelling-story-focused-audience) — Cartographer, Husk, Weave, Chronicle
- [Tier 3 — Domain-Specific](#tier-3--domain-specific-strong-for-specific-audiences) — Relay, Forge, Iris
- [Launch Sequence](#launch-sequence)
- [Reusable Scaffolding](#reusable-scaffolding)

---

## Demo planning

A strong Kāra demo makes one or more of these five pillars tangible. Each pairs the user-facing win with the compiler/tooling moment that delivers it:

1. **Effects-as-signature** — A public function's signature tells reviewers what real-world resources it touches. *Show:* `karac query effects` on the codebase; public effect diffs between revisions.

2. **Provider injection for tests** — Tests replace `Clock`, `Env`, `RandomSource`, databases, etc. without dependency-injection boilerplate. *Show:* deterministic tests run against injected providers; identical results every run.

3. **Auto-concurrency from effects** — The compiler says "these operations are independent; I ran them together; here is why." *Show:* independent resource reads parallelized with no annotations; the concurrency report explains the decision.

4. **Profiles as target constraints** — A `deterministic` or `embedded` profile rejects the wrong code mechanically. *Show:* the profile failing a build with a clear effect-based error.

5. **AI-readable diagnostics** — AI agents consume structured compiler output and produce safe, scoped edits. *Show:* `karac build --output=json` feeding an LLM that fixes its own Kāra code.

A grand-tour demo built on a small service or pipeline (`Clock`, `Env`, `FileSystem`, `Network`, plus a user-defined database resource) can cover all five. Most demos below focus on one or two pillars; the suite together should cover them.

On top of the pillar lens, every demo should also pass these practical filters:

- Hard to replicate in another language without significantly more programmer effort.
- Achievable by a small team (1–3 people, 1–3 months).
- Visible, concrete output — not just a benchmark number.
- Tells a complete story in under 5 minutes.

---

## Tier 1 — Must-Build (core story, highest impact)

### Demo 1: Parallax — Auto-Concurrency API Gateway

**Primary capability:** Auto-concurrency without `async fn` or function coloring.

**What it is:** A real HTTP API server that aggregates data from multiple
upstream services — the canonical "fan-out and join" pattern that every
backend language handles differently.

```kara
// The entire concurrency story in one function.
// No goroutines. No async fn. No .then() chains. No colored functions.
pub fn get_dashboard(user_id: UserId) -> Dashboard
    with reads(UserDB, OrderDB, NotifDB, RecommendDB)
{
    let profile       = fetch_profile(user_id);       // reads(UserDB)
    let orders        = fetch_recent_orders(user_id);  // reads(OrderDB)
    let notifications = fetch_notifications(user_id);  // reads(NotifDB)
    let recommended   = fetch_recommendations(user_id);// reads(RecommendDB)

    // Four reads on four different resources — zero data dependencies.
    // Compiler runs all four concurrently. No annotation needed.

    build_dashboard(profile, orders, notifications, recommended)
}
```

**What the demo shows:**
1. The Kāra code above, as-written — clean, sequential-looking.
2. `karac build --concurrency-report` output: shows the four calls in a
   `parallel_group`, explains *why* they are parallel ("no data dependencies;
   no effect conflicts: different resources, all reads").
3. Side-by-side benchmark: the same server in Go with explicit goroutines and
   channels, the same server in Node.js with `Promise.all`. Kāra matches their
   throughput with simpler code.
4. One deliberate change: make two functions write to the same resource. Show
   the compiler's concurrency report flip from `parallel_group` to `sequential`
   with a clear explanation.

**Why it's compelling:** Every developer has written `Promise.all` or
`goroutine + channel` boilerplate for this. Kāra gets the same result from
plain sequential code — the compiler does the work. The concurrency report makes
the decision *visible and auditable*, which no other language offers.

**Effort:** Medium. HTTP server layer needed (FFI or a thin Kāra wrapper).
Benchmark harness is straightforward.

#### Slice E — Three-language benchmark harness (design drafted 2026-05-09)

Demo 1's recordable artifact: side-by-side `GET /dashboard/:user_id` benchmark across Kāra, Rust, Go, and Node. Predecessor: HTTP handler ABI trampoline (`docs/implementation_checklist/phase-7-codegen.md` § "HTTP handler ABI trampoline"). Execution scaffolding in `docs/implementation_checklist/wip-list1.md` (`[BLOCKED on trampoline ship]`). Slices A (`ab611d3`), B (`3c3d87b`), C (`f5c7b31`), D (`502250a`), F (`91768f2`) all shipped 2026-05-09 — Slice E is the final piece that makes Demo 1 recordable.

**Settled design forks (2026-05-09).**

- **F1 — Recording artifact.** Markdown throughput table at `examples/parallax/bench/README.md` + checked-in `bench.sh` reproducer. Engineers believe code, not videos; a rerunnable script is the highest-credibility artifact. Asciinema cast and video walkthrough are post-v1 polish — additive, not gating.
- **F2 — Load distribution.** `wrk` Lua script generates random uniform user IDs in `1..1000`. Provides realistic-looking traffic in the recording without affecting throughput numbers (providers use `sleep_ms(n)`, not real lookups, so there's nothing to cache).
- **F3 — Win condition.** Measure first; frame post-hoc based on actual numbers. No defensive bar pre-baked. If results disappoint, that's a diagnostic signal for follow-up perf investigation, not a story to pre-rationalize.
- **F4 — Cross-language fairness controls.** Tokio worker count for Kāra and Rust matches `GOMAXPROCS` (= CPU count); Go uses default `GOMAXPROCS`; Node runs single-process (faithful to the language's default deployment reality); same hardware, sequential `wrk` runs (10s warmup + 30s measurement per impl). README footnotes Node's single-process choice and notes that cluster mode would scale ~Nx at the cost of process orchestration.
- **F5 — Provider delays.** Asymmetric: 2/5/8/12 ms per provider. Total sequential ~27ms; total parallel ~12ms (waiting on the slowest). Asymmetry surfaces the "join waits on slowest provider" property in the trace narration.
- **Rust added to comparison cohort 2026-05-09.** Extends the original Go+Node staging entry to a 4-way comparison. Rust (tokio + hyper + `tokio::join!()`) is the natural perf ceiling and uses the same runtime stack as Kāra, so the Kāra-vs-Rust gap measures Kāra's value-type ABI + trampoline overhead vs raw Rust. Single 4-way table per F3 — let the data tell the story rather than pre-splitting cross-paradigm vs implementation-overhead framings.
- **Demo location.** `examples/parallax/bench/{kara,rust,go,node}/` with top-level `bench.sh` and `README.md`. Same compiler repo (matches the existing `examples/parallax_lite/` precedent). Reproduction is opt-in (not part of `cargo test`); `bench.sh` graceful-degrades when language toolchains are missing (skip-with-stderr-notice pattern from `tests/memory_sanitizer.rs::KARAC_SKIP_ASAN_TESTS`).

**Out of scope (deferred to follow-ups).**

- TLS, HTTP/2, WebSockets — Phase 11.
- Real database FFI (Postgres / MySQL / Redis) — Phase 11 long-tail. Demo uses `sleep_ms(n)` providers + in-memory state.
- Cluster-mode Node — footnoted in README; not implemented in v1 bench.
- Asciinema cast / video walkthrough — post-v1 polish if shareability benefits warrant it.
- Multi-user load patterns (Zipf, sticky-session) — random uniform is enough to exercise per-request fan-out; richer load shapes are a follow-up if perf investigation calls for it.
- Splitting Parallax bench into a standalone repo (`parallax-bench/`) for shareable cloning — premature; same-repo until a real reason to split surfaces.

---

### Demo 2: Mend — AI Writes Kāra

**Primary capability:** AI-first tooling — structured compiler output as a
machine-readable feedback loop.

**What it is:** A live demonstration where an LLM (Claude or GPT-4) writes,
diagnoses, and fixes Kāra code autonomously using only `karac build --output=json`.
No human in the loop after the initial prompt.

**The loop:**

```
User prompt → LLM writes Kāra code
→ karac build --output=json
→ LLM reads structured errors + fix diffs
→ LLM applies diffs mechanically
→ repeat until clean build
```

**What the demo shows (live, in a terminal):**
1. User types: *"write a function that reads users from a database and sends
   them a welcome email concurrently."*
2. LLM writes reasonable but imperfect Kāra: wrong effect annotations, one
   missing ownership annotation.
3. `karac build --output=json` returns structured diagnostics including
   `"inferred_effects": ["reads(UserDB)", "sends(EmailService)"]` and a
   machine-applicable diff.
4. LLM applies the diff verbatim — no re-reasoning, no guessing.
5. Clean build in 2-3 iterations.

**The before/after is the story:** Show the same task in Python. The LLM
writes code that *looks* correct but has a subtle concurrency bug (shared
mutable state, race condition). Python's tools cannot catch it statically.
Kāra's compiler catches it before the code ever runs.

**Why it's compelling:** This is the thesis of the language — "a language
designed to be written by AI." The demo makes it literal. The structured
output isn't a nice-to-have; it's the mechanism. No other language has an
effect system + ownership checker + machine-applicable fix diffs in a single
JSON envelope.

**Effort:** Small to Medium. The demo framework is mostly a shell script that
calls `karac` and pipes output to an LLM API. The hard part is picking
examples that show the interesting cases without being contrived.

---

### Demo 3: Slipstream — Interactive Wind Tunnel

**Primary capability:** Auto-concurrency of sequential code; layout blocks for
cache-efficient SoA access; same code runs on CPU and GPU.

**What it is:** A real-time 2D wind tunnel simulator. Air flows left to right
around a NACA wing cross-section; the user rotates the wing and watches
aerodynamic stall develop live. Runs the Lattice Boltzmann Method (LBM D2Q9)
— the standard algorithm for interactive computational fluid dynamics, with
reference implementations in every major language. The physics is solved, the
algorithm is 40 years old. The demo is about what Kāra does to it.

```kara
// LBM node — 9 velocity distribution functions (D2Q9 model)
struct LbmNode {
    f0: f32, f1: f32, f2: f32, f3: f32, f4: f32,
    f5: f32, f6: f32, f7: f32, f8: f32,
    flags: u8,   // solid=airfoil boundary, fluid=open
}

// SoA layout: all f0 values contiguous, then all f1, ..., then all f8.
// The streaming step walks one velocity component across all nodes —
// SoA turns that into a sequential memory scan; LLVM auto-vectorizes it.
layout grid: Vec[LbmNode] {
    group distributions { f0, f1, f2, f3, f4, f5, f6, f7, f8 }
    group geometry      { flags }
}

// Collision + streaming — pure per-node, no cross-node writes in collision.
// Written as a plain sequential function. The compiler sees that
// lbm_step and advect_tracers read the same resource (FluidGrid) with no
// writes between them — reads/reads never conflict — and runs them in a
// parallel_group automatically. No par {} written anywhere.
fn simulate_tick(world: mut ref World) with writes(FluidGrid) {
    let new_grid    = lbm_step(world.grid, world.viscosity);   // reads(FluidGrid)
    let new_tracers = advect_tracers(world.tracers, world.grid); // reads(FluidGrid)
    world.apply(new_grid, new_tracers);
}

// GPU path: identical kernel, dispatched to the GPU.
// Same source — no rewrite, no port.
#[gpu]
fn lbm_step_gpu(grid: ref Vec[LbmNode], viscosity: f32) -> Vec[LbmNode]
    with reads(FluidGrid), allocates(GpuBuffer)
{ ... }
```

**What the demo shows:**
1. A window: tracer particles stream left to right around a wing. A turbulent
   wake forms behind the trailing edge. Flow is smooth and continuous.
2. Drag a slider to rotate the wing angle of attack — at around 15° the flow
   separates and the wake explodes into chaotic swirling. Stall. Drag the
   slider back — flow reattaches.
3. Press `C`/`G` to switch between CPU and GPU paths. FPS changes; the
   fluid behavior is identical. The on-screen overlay shows:
   `Path: CPU | FPS: 60 | Layout: SoA | Cache miss: 0.4%`
4. Press `L` to toggle layout off (AoS). The cache miss counter jumps — and
   on a large enough grid the framerate visibly drops. Press `L` again to
   recover.
5. `karac build --perf-report` shown in a terminal beside the window: the
   `simulate_tick` body appears as a `parallel_group` containing `lbm_step`
   and `advect_tracers`, with the compiler's reasoning: *"reads(FluidGrid) ×
   reads(FluidGrid) — no conflict; no data dependency; parallelized."*

**Why it's compelling:** One sentence explains it to anyone: *"This is what a
$10M wind tunnel does — in real time on your laptop."* The angle-of-attack
slider makes it a tool, not a screensaver: you ask a question (will this wing
stall?) and the simulator answers it. Engineers and non-engineers both
understand it immediately. The C/G/L toggles deliver the same experience as
the original particle demo — abstract concepts made visible through numbers
that move on screen — but grounded in something real rather than decorative.

The Kāra story lands without needing explanation: point at `simulate_tick` and
ask "where is the threading code?" There isn't any. The compiler found it.

**Effort:** Medium. The LBM algorithm is well-understood with clean reference
implementations (Dan Schroeder's JavaScript version is ~300 lines). The CPU
demo is unblocked after Phase 11 completes (auto-concurrency codegen lands in
Phase 8 floor, but the full stdlib + FFI for SDL2 rendering need the long-tail
in Phase 11). The GPU path requires Phase 10 but can be added later without
changing the Kāra source — only the dispatch call changes.

---

## Tier 2 — High-Value (compelling story, focused audience)

### Demo 4: Cartographer — Effect Graph Visualizer

**Primary capability:** `karac query` as a developer tool; effect types as
architecture documentation.

**What it is:** A web-based visualization of a program's effect graph. Feed
it any Kāra project; it renders an interactive graph showing every function,
its effects, and which functions can run concurrently vs must serialize.

**What the demo shows:**
1. Load a medium-sized Kāra project (say, a 500-line HTTP service).
2. The visualizer shows: green nodes (pure functions), blue nodes (reads),
   orange nodes (writes), edges (call graph), parallel bands (auto-concurrency
   groups).
3. Click a function: see its declared effects, inferred effects, and which
   callers are blocked from parallelizing because of it.
4. Edit the code live (Monaco editor embedded): change an effect annotation,
   watch the graph update.

**Why it's compelling:** This makes the effect system *visible*. Most
programmers can't see the concurrency structure of their program. Kāra + this
tool makes it a first-class architectural artifact, not a runtime observation.
The graph is also the best possible explanation of what the effect system does
for new users.

**Effort:** Medium. `karac query effects` and `karac query concurrency` provide
all the data. The visualizer is mostly a frontend (D3.js or similar). Compile
Kāra to WASM so the tool runs in-browser without a server.

---

### Demo 5: Husk — Minimal Kernel

**Primary capability:** `kernel` profile — no heap, no panics, no std, inline
assembly, MMIO, interrupt handlers.

**What it is:** A tiny OS kernel that boots on QEMU (RISC-V or ARM). Keyboard
input, VGA text output, a trivial shell with three commands. Roughly 1,000
lines of Kāra.

```kara
// This is real systems code — no std, no heap, hardware I/O via effects.
#[interrupt(TIMER)]
fn timer_isr() with writes(Hardware) {
    TICKS.fetch_add(1, Release);
    clear_timer_interrupt();
}

pub fn kernel_main() with writes(Hardware, Stdout) {
    init_uart();
    install_interrupt_vectors();
    enable_interrupts();

    loop {
        let line = read_line();    // reads(Hardware) — UART
        shell_dispatch(line);
    }
}
```

**What the demo shows:**
1. QEMU boots. Text appears on screen. You type commands.
2. `karac build --output=json` for the kernel — show the effect graph:
   every function's hardware access is explicit. The ISR has `writes(Hardware)`,
   the memory allocator has `allocates(Heap)` suppressed (forbidden by profile).
3. Deliberately try to use `Vec` (heap-allocated) in kernel code — show the
   compile error: "allocates(Heap) is forbidden by the `kernel` profile."
4. Show `karac query stack kernel_main` — static stack depth analysis, no
   surprises at runtime.

**Why it's compelling:** "Can it boot?" is the most credible systems language
question. A working kernel demo answers it definitively. It also showcases
the `kernel` profile as a practical, useful feature — not a theoretical one.

**Effort:** Large. Requires completing the `#[repr]`, `#[interrupt]`, inline
asm, and volatile memory access gaps from the hardware analysis (v8). This
demo is the validation test for those features.

---

### Demo 6: Weave — Data Pipeline with Verifiable Invariants

**Primary capability:** Refinement types + contracts + effect types together.

**What it is:** An ETL pipeline that transforms data from raw CSV through
validation, enrichment, and aggregation. Every stage's preconditions and
postconditions are declared and checked; every stage's data access is tracked
by the effect system.

```kara
// Refinement types: the type system enforces that invalid data can't
// reach downstream stages.
type ValidEmail    = String where self.contains("@") && self.len() < 255
type PositivePrice = f64   where self > 0.0
type NonEmpty[T]   = Vec[T] where self.len() > 0

fn parse_row(raw: String) -> Result[ValidatedRow, ParseError]
    // No effects — pure transformation
    ensures(result) match result {
        Ok(row) => row.email is ValidEmail && row.price is PositivePrice,
        Err(_)  => true,
    }

fn enrich_prices(rows: NonEmpty[ValidatedRow]) -> NonEmpty[EnrichedRow]
    with reads(CurrencyDB)    // needs live exchange rates
    requires rows.all(|r| r.price is PositivePrice)
    ensures(result) result.len() == rows.len()  // no rows dropped

fn aggregate(rows: NonEmpty[EnrichedRow]) -> Summary
    // Pure — no I/O, no side effects
```

**What the demo shows:**
1. Run the pipeline on a dataset with intentionally bad rows. Show parse errors
   reported cleanly with row numbers.
2. `karac build --output=json`: effect annotations tell you exactly what
   external resources each stage touches. The pipeline's data flow is auditable
   from the signatures alone.
3. Show what happens if you skip the parse stage and pass raw strings to
   `enrich_prices` — compile error: `String` is not `ValidEmail`.
4. Show what happens if you pass an empty `Vec` to `aggregate` — compile error
   at the call site: `Vec[EnrichedRow]` is not `NonEmpty[EnrichedRow]`.

**Why it's compelling:** This is the "correctness by construction" story.
Compare it to a Python pipeline where bad data causes a runtime exception 10
steps later. In Kāra, the shape of bad data is a compile error. For data
engineers who have debugged midnight pipeline failures, this is immediately
valuable.

**Effort:** Small to Medium. Purely a Kāra application — no FFI, no GPU, no
special tooling. The complexity is in selecting good examples. Bonus: use this
pipeline to process real public data (census data, stock prices) so the demo
has realistic inputs.

---

### Demo 7: Chronicle — Compiler Written in Itself

**Primary capability:** Self-hosting — Phase 10 is complete; the compiler is
the demo.

**What it is:** Show `karac query effects src/typechecker.kara.check_function`.
The output is the type checker's effect signature: what resources it reads (the
AST, the symbol table), what it writes (the diagnostic list, the type table).
Then show the concurrency report for the full compilation pipeline — which
phases run in parallel, which must serialize.

**What the demo shows:**
1. `karac query effects src/` — generate an effect report for the entire
   compiler. Every module's effect footprint, the full resource dependency graph.
2. `karac build --concurrency-report src/main.kara` — show which compiler
   phases the *Kāra compiler itself* parallelizes when compiling a large program.
3. `karac query concurrency src/compiler.kara.compile` — show the
   parallelization decisions for the compilation pipeline (lexer → parser → type
   checker can be pipelined; type checker → effect checker → ownership checker
   are dependent).
4. Run `diff` between the Rust-based compiler and the Kāra-based compiler on
   their output for a test program. Identical.

**Why it's compelling:** Self-hosting is the canonical "we're serious" signal
in language design. But more than that — using Kāra's own tooling to explain
Kāra's own architecture is the meta-level demonstration of the AI-first thesis.
The compiler's architecture is expressed in its effect signatures.

**Effort:** Small (Phase 10 already delivers this). The work is curating the
right query examples and building a good presentation layer.

---

## Tier 3 — Domain-Specific (strong for specific audiences)

### Demo 8: Relay — High-Performance Network Proxy

**Audience:** Infrastructure engineers, performance-focused backend developers.

**What it is:** A Layer 7 HTTP proxy / load balancer. Accepts connections,
routes requests, collects metrics. Targets 500K+ requests/second on a single
machine.

**Why Kāra:** The auto-concurrency runtime (Phase 6 v1.1 — network event loop)
routes `sends(Network)` and `receives(Network)` effects to epoll/kqueue
automatically. No `async fn`. No explicit task management. The proxy is
written as if it's sequential; the compiler manages the event loop.

**Differentiator vs Go:** Go's goroutines handle the concurrency, but the
programmer still has to think about goroutine lifecycle. In Kāra, the effect
system drives routing — you never write goroutine-management code.

**Effort:** Medium. Builds on the HTTP server from Demo 1. The interesting
part is the benchmark setup and comparison (wrk, hey, or equivalent).

---

### Demo 9: Forge — Embedded Sensor Logger

**Audience:** Embedded systems engineers, hardware developers.

**What it is:** A firmware image for a microcontroller (STM32 or RP2040).
Reads temperature/humidity from an I2C sensor, logs to SD card via SPI, and
streams over UART. Runs under the `embedded` profile — no heap, no panics,
no std.

**Why Kāra:** The `embedded` profile enforces the constraints at compile time.
No stray `Vec.new()` slipping in. The hardware effects (`reads(Hardware)`,
`writes(Hardware)`) make the I/O map explicit. Interrupt handlers use the
`#[interrupt]` attribute (from the v8 hardware gaps — this demo validates
that feature).

**Differentiator vs Rust embedded:** Rust embedded is excellent but requires
understanding lifetimes in depth. Kāra's ownership inference handles most of
it, surfacing only the edge cases that require annotation. The effect system
is cleaner than Rust's embedded-hal trait hierarchy for expressing hardware
access.

**Effort:** Large. Requires the hardware-level gaps from v8 to be resolved
(volatile access, `#[repr]`, `#[interrupt]`, `no_std` boundary). This demo
is the integration test for the embedded story.

---

### Demo 10: Iris — Browser Image Editor

**Audience:** Web developers, WASM users.

**What it is:** An in-browser image editor with non-trivial filters (Gaussian
blur, sharpening, histogram equalization, edge detection). Written entirely in
Kāra, compiled to WASM, runs in browser with no server.

**Why Kāra:** The WASM target (Phase 10) compiles the same Kāra code that
runs natively on the command line. Show the filter code once — then show it
running as a native binary (fast) and as a WASM module in Firefox (also fast).
No rewrite, no port, no language boundary.

**Differentiator vs Rust + wasm-pack:** Kāra is simpler to learn and has
better error messages. The layout blocks + SIMD story (when available) means
the image kernels are naturally cache-efficient. Bonus: the effect annotations
on image processing functions (`reads(ImageBuffer)`, `writes(ImageBuffer)`)
make the data flow visible.

**Effort:** Medium. WASM backend is Phase 10. Image processing algorithms are
straightforward. The interesting engineering is the browser integration (JS
glue, canvas rendering) and keeping the native / WASM comparison honest.

---

## Launch Sequence

Build in this order:

| Phase | Demo | Why now |
|---|---|---|
| Day 1 (pre-launch) | **Demo 2: Mend** | Cheapest to build. Makes the AI-first thesis real. Sets the tone. |
| Month 1 | **Demo 1: Parallax** | Broadest appeal. Every backend engineer relates to fan-out + join. |
| Month 2 | **Demo 4: Cartographer** | Teaches the effect system visually. Reduces onboarding friction for new users. |
| Month 3 | **Demo 6: Weave** | Correctness story for data engineers. Complements the concurrency story. |
| Month 4 | **Demo 7: Chronicle** | Self-hosting milestone. Marks Kāra as "a real language." |
| Month 3 (CPU) | **Demo 3: Slipstream** | Visually striking, instantly explainable. CPU demo unblocked after Phase 11 (long-tail stdlib + FFI). GPU path added later without changing Kāra source. |
| When hardware gaps land | **Demo 5: Husk** | Systems credibility. Requires v8 features. |

**Parallax** and **Mend** together are the minimum viable showcase — they
cover the two core theses (auto-concurrency, AI-first) with achievable effort.
Everything else layers on top.

---

## Reusable Scaffolding

Build these once; they serve multiple demos:

| Scaffold | Used by |
|---|---|
| HTTP server / client layer (thin FFI over hyper or similar) | Demo 1, 3, 8 |
| `karac --output=json` visualization harness | Demo 2, 4, 7 |
| WASM → browser integration boilerplate | Demo 4, 10 |
| Benchmark harness (wrk, criterion, perf) | Demo 1, 3, 8 |
| QEMU boot scaffolding | Demo 5, 9 |
