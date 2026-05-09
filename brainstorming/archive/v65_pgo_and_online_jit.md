# 65 — Profile-guided optimization and the online-JIT feasibility question

**Status:** Open. Draft 2026-05-06.

**Trigger:** While editing v63 (LLM↔compiler query channel) it became clear that real PGO has *no* canonical home in the Kāra docs. `roadmap.md:752` carries a "PGO stubs" line, but that is just `llvm.expect` emitted from static effect analysis — it borrows the LLVM intrinsic name without doing any of the things PGO actually does (instrumentation, profile collection, recompile loop). v63 § Problem 7 floats "Likely P2" but v63 is a brainstorm doc with no canonical force. So: real PGO is currently **undocumented** as a feature. It needs a brainstorm that resolves into `deferred.md` + `implementation_checklist.md`.

While we are at it: PGO and online JIT sit on a **single axis** — both close the AOT-can't-see-runtime gap, just at different times relative to the deploy boundary. v63 made the explicit claim that the query channel is *complementary* to PGO; this doc takes that seriously and asks whether Kāra can credibly support not just offline PGO but a *narrow* online-JIT story that augments AOT without becoming HotSpot.

This brainstorm decides:
- What real PGO (instrumented + sample-based) requires from the Kāra implementation, and where it lands on the priority axis.
- Whether any online / in-process JIT story is **feasible** within Kāra's design constraints — and which slice (continuous-PGO recompile, runtime monomorphization, speculative tiering) is worth committing to.
- The minimum architectural commitments v1 needs so neither feature is a breaking change later.

Per stored tier definitions: **v1 = P0 + P1.** P2 = important post-v1 features that *will* ship. P3 = post-v1 libraries / frameworks that may or may not.

Framing claim: PGO belongs in **P2** (canonical, post-v1, will be built). The online-JIT question is mostly **P3**, with one specific slice — *continuous PGO with background recompile and hot-swap* — that may be promotable to P2 if the backend-first positioning (v64) demands warehouse-grade adaptive performance.

---

## Problem 1 — Status quo audit

Before designing forward, what does Kāra have today that PGO or JIT could ride on?

- **Codegen exists** (`src/codegen.rs`, ~11K lines via inkwell/LLVM 18). Gated on `--features llvm`. Phase 7 in the roadmap; in active development.
- **No runtime support library beyond `libkarac_runtime.a`** (allocators, RC, panic handlers). No counter library, no profile-dump hooks, no JIT shim.
- **Interpreter is tree-walking** (`src/interpreter.rs`), used for `karac run` and `karac test`. Not a tier-up target for codegen.
- **CLI has `karac build`, `karac run`, `karac test`** but no `--profile-generate` / `--profile-use`, no `karac jit`, no `karac bench`. (`karac query` exists with non-PGO query kinds.)
- **Source identity is `SpanKey` (byte offset + length).** v63 § Problem 4.A makes the case for path-based DefId + structural hash as a P0 architectural commit. PGO needs the same primitive — AutoFDO is *built* on structural-hash keying for source-drift resilience (LLVM `.profdata`).
- **Effects and ownership are statically declared and verified.** This is load-bearing for the JIT discussion: any in-process compilation must produce code whose effect signature matches what the typechecker already verified, or the soundness story breaks.
- **Monomorphization-first generics.** Every `Vec[T]` / `fn sort[T: Ord]` instantiation is a separate compiled body. AOT covers everything statically reachable. The *only* monomorphization gap is when `T` enters via a dynamic boundary (deserialization, FFI, dynamic plugin load) — narrow but real, and the most defensible JIT use case if any.
- **No ABI / linkage story for hot-swap.** Functions are statically linked; there is no `RTLD_LAZY` / shared-object / function-pointer-table indirection that would let the runtime swap an AOT body for a JIT'd specialization. This is the load-bearing missing piece for any online story.

**Takeaway:** PGO can be built on what exists today (LLVM already does the hard parts; we just need codegen flag plumbing + runtime counter library + CLI wiring). Online JIT is a much bigger commitment — it would need an indirection layer that does not currently exist *and* a JIT runtime in-process *and* a stable ABI between AOT-compiled and JIT-compiled code.

---

## Problem 2 — Real PGO: the offline loop

PGO has two distinct flavors, each with a different cost surface.

### 2.A Instrumented PGO

Standard `gcc -fprofile-generate` / `clang -fprofile-instr-generate` flow:

1. Build instrumented binary: codegen inserts atomic counter increments at every basic-block edge (or every function entry, depending on granularity).
2. Run the binary against a representative workload. Counters dump to `default.profraw` on exit.
3. `llvm-profdata merge default.profraw -o default.profdata`.
4. Re-build with `--profile-use=default.profdata`. Codegen consumes the profile and feeds it to LLVM's PGO passes (block layout, inlining heuristics, branch hints, function ordering, register allocation priorities).

**What Kāra needs:**
- A new codegen mode controlled by a flag — sketch: `karac build --profile-generate=DIR` — that turns on `LLVMAddInstrumentation` (or the inkwell equivalent on the `PassBuilder`). LLVM's `InstrProfiling` pass does the actual counter insertion.
- A counter runtime in `libkarac_runtime`: atomic u64 counters, a `__llvm_profile_write_file` analog, signal-safe dump on exit. LLVM's `compiler-rt/lib/profile` is the reference implementation; we can either link it directly or write a minimal Rust port.
- `karac build --profile-use=PATH` to feed `.profdata` back into `PassBuilder`.
- Profile lifecycle policy: where does `default.profdata` live? Same as build artifacts (`target/profile/`)? Committed to the repo? My recommendation: `target/profile/` by default, with a `--profile-out` flag for users who want to commit a "blessed" profile.

**Open question:** does instrumented PGO interact with the auto-concurrency-group fork decisions? Today those are static heuristics with no runtime feedback. PGO gives us frequency data per basic block — useful for the v63 query channel ("I would fork at N=64; raise/lower?") but does not require a separate mechanism.

### 2.B Sample-based PGO (AutoFDO)

Different cost model: no instrumented build, no separate workload run. Instead:

1. Ship the regular release binary to production.
2. Sample the running binary via `perf record -e br_inst_retired:near_taken -j any,u`.
3. Convert with `create_llvm_prof --binary=BIN --profile=perf.data --out=auto.prof` (an out-of-tree tool, originally Google).
4. Re-build with `--profile-use=auto.prof`.

**What Kāra needs on top of 2.A:**
- DWARF-quality debug info that survives optimization. LLVM produces this; we just have to make sure inkwell's `DIBuilder` usage in `src/codegen.rs` preserves enough source-level detail for `create_llvm_prof` to map samples back to basic blocks.
- The `create_llvm_prof` tool itself is external (we link to it, not bundle it).
- Stable function names across rebuilds — i.e., the v63 P0 stable-identity work. AutoFDO is *much* more sensitive to source drift than instrumented PGO, because the sampled binary and the rebuilt binary may not be byte-identical.

**Position:** instrumented PGO is the v1.x deliverable; AutoFDO is a v2 add-on once warehouse users actually need it. Sample-based PGO gives ~5–10% over instrumented in published numbers (Linux kernel 2024 mainline), but the cost ladder is steeper.

### 2.C Post-link rewriting (BOLT, Propeller)

Operates on the *final* binary, not in IR:
- **BOLT** rewrites the binary in place using collected profile.
- **Propeller** does relink-time block layout (between codegen and link) using profile data.

Both are LLVM-adjacent but separate tools. Out of v1 scope; we plan around them not against them. Propeller is more interesting long-term because it integrates into the linker rather than being a separate post-pass.

### 2.D Profile representation

Reuse LLVM `.profdata`. Custom format = no benefit, lots of work, breaks tool interop. The `.profdata` format is structural-hash-keyed, which is exactly what we want for source-drift resilience.

For *display* and *commit-to-repo* use cases, we may want a text-format dump (`llvm-profdata show --text` or AutoFDO text format) — but the canonical artifact is binary `.profdata`.

---

## Problem 3 — Online JIT: framing the spectrum

"Online JIT" is not one thing. There are at least five points on the AOT↔JIT axis, with very different feasibility profiles for Kāra:

| Point | What runs at runtime | Adaptation latency | Cost surface |
|---|---|---|---|
| **3.0** Pure AOT (status quo) | Compiled native | None | None beyond codegen itself |
| **3.1** Static PGO (Problem 2) | Compiled native | One deploy cycle | Codegen flag + counter runtime |
| **3.2** Continuous PGO + hot-swap | Compiled native + counters in prod | Minutes (background recompile) | Counter runtime + dynamic linkage + recompile orchestration |
| **3.3** AOT + runtime monomorphization | AOT binary + on-demand specializer for unseen `T` | Microseconds (first call) | In-process JIT (LLVM ORC or Cranelift) + IR shipped alongside binary |
| **3.4** AOT + speculative tiering w/ deopt | AOT + recompiled hot paths with assumptions, OSR fallback | Sub-second | Frame metadata + deopt points + JIT compiler in-process |
| **3.5** Full bytecode-first JIT (HotSpot/V8) | Interp/baseline → tier-up → optimizing JIT | Sub-second | The whole JVM stack |

3.0 is where we are. 3.1 is Problem 2. The interesting feasibility question is which subset of 3.2–3.5 is *defensible* for Kāra given the language design.

### 3.5 is wrong for Kāra

Bytecode-first JIT is a different language design. It implies:
- Source ships as IR or bytecode, not native.
- Cold-start penalty is the norm.
- The optimizing compiler runs in-process for *every* program, not just adaptive workloads.
- Effect/ownership checking is moved (partially) to JIT time.

This contradicts the explicit AOT-first stance and is the wrong shape for backend services, embedded targets, and `karac build` artifacts that get distributed as native binaries. **3.5 is rejected on design-philosophy grounds, not feasibility.** P3 at most; realistically not on the roadmap at all.

### 3.4 is heavy and conflicts with Kāra's invariants

Speculative tiering (HotSpot-class) requires:
- Deoptimization points in IR — the ability to reconstruct an interpreter frame from an optimized native frame at any safepoint.
- Type-feedback profiling at runtime to discover speculation candidates.
- On-stack replacement (OSR) for tier-up of long-running loops.
- An invariant-violation handler: when "this `match` arm never taken" turns out wrong, bail out cleanly.

The Kāra-specific friction:
- **Effects.** A speculative inline that crosses an effect boundary changes the function's effect set. Re-checking at JIT time is possible but doubles the verification surface.
- **Ownership.** Move/borrow analysis is a property of the AOT-checked source; speculative reordering must preserve it. Re-running ownership analysis on JIT'd code is feasible but complicates the soundness argument.
- **Frame layout.** Stack frames for OSR need a stable on-disk schema; today there is none.

**My read:** 3.4 is *technically* feasible but the reward-to-complexity ratio is poor. Backend services and embedded targets do not need HotSpot-class adaptation; the cases where it pays off (long-running JVM-style monoliths with very high throughput) are precisely the cases where continuous PGO (3.2) gets most of the win at a fraction of the cost. **P3.**

### 3.3 is narrow but defensible

> **Review note (added 2026-05-09 during walkthrough — ClangJIT post-mortem).** The use-case-overlap argument for 3.3 is *stronger* in Kāra than in C++. C++ has `std::variant`, virtual dispatch, `dlopen`-plugin patterns, and external codegen frameworks (TVM, Halide) — the "narrow gap" ClangJIT filled was real but small in the C++ ecosystem, and that smallness was a contributing factor to ClangJIT not surviving upstream review. Kāra has fewer escape hatches; the gap is wider here. Argues for 3.3 more strongly than the current draft states.
>
> **Bitcode-embedding policy needs a pick, not a both-and.** The current draft mentions both (a) `#[jit_template]` author opt-in and (b) compiler-derived "generic crosses a dynamic boundary." These are different policies with different binary-size and ergonomics tradeoffs. Pick one before promotion: (a) predictable but requires per-library decisions; (b) automatic but binary size becomes an emergent property; (c) all generics — untenable. Suggest (a) for v1 ship of 3.3, with (b) as a v1.x refinement once usage patterns surface.

Runtime monomorphization is the most Kāra-shaped JIT story:

- Kāra is monomorphization-first. AOT generates one body per `Vec[T]` instantiation it can see.
- The *only* gap: a `T` that arrives via a dynamic boundary — JSON/MsgPack deserialization into a generic container, FFI handing back an opaque type, dynamically-loaded plugins instantiating templates declared in the host.
- For these cases, today the program has two options: monomorphize-everything-needed at AOT time (impossible if `T` is genuinely runtime-discovered) or fall back to a dyn-trait-style boxed representation.
- A runtime monomorphization JIT compiles the missing instantiation on first use. Subsequent calls hit a code cache.

**Why this is uniquely defensible for Kāra:**
- The unit of JIT is well-defined: one generic instantiation. Not a hot loop, not an inlining decision — a whole function body for a specific `T`.
- Effects, ownership, and trait bounds are already checked at AOT time on the *generic* body; the JIT's job is purely codegen-substitution. No fresh verification.
- The IR for the generic body can be shipped alongside the binary in a dedicated section (LLVM bitcode embedded in `.kara_jit_template`). Binary-size cost is bounded and opt-in.
- The fallback is well-defined: if JIT is unavailable (embedded target, security policy disallows mmap+exec), the call site errors at the dynamic boundary, not silently.

**Cost surface:**
- Embed LLVM ORC2 (via inkwell's `LLJIT` bindings) or Cranelift in `libkarac_runtime`. Cranelift is smaller and faster-compiling; LLVM ORC2 reuses our existing pipeline. Recommendation: Cranelift for runtime specializer, accept ~10% slower JIT'd code than AOT, gain ~30× compile speed and ~10× smaller runtime footprint.
- A `jit_template` annotation (or compiler-derived from "this generic crosses a dynamic boundary") to mark which generic bodies need their IR shipped.
- Code cache management — bounded LRU; mmap+exec; on iOS/locked-down platforms, fall back to AOT-only with a build-time error.
- Stable-identity for cache keying (the v63 P0 work).

**Position:** **P2.** Worth designing carefully; ships post-v1; warehouse and plugin-host workloads are the natural users.

### 3.2 is the most pragmatic online story

Continuous PGO with background recompile is closer to "operations" than "compiler engineering":

1. Production binary collects PGO counters live (low-overhead instrumentation, AutoFDO-style sampling, or hardware perf counters).
2. Counter snapshots ship to a build farm (or a sidecar) periodically.
3. Background compile produces a `v2.so` with updated profile.
4. The running process `dlopen`s the new shared object and indirectly redirects function pointers to the new bodies. Old bodies stay live until in-flight calls drain.

Mechanically: this is *exactly* PGO (Problem 2) plus a hot-reload story. No deopt, no OSR, no fresh verification — the v2 binary is built by the same AOT pipeline with updated profile data.

**What it adds beyond 2.A:**
- Function pointer indirection or PLT/GOT-equivalent for hot-swappable functions. This is a one-time ABI commitment; without it, hot-swap is impossible.
- Drain protocol: when can old code be unmapped? Probably "after all threads have crossed a quiescence point," which RCU-style.
- Orchestrator: who triggers the rebuild? A daemon, a Kubernetes sidecar, a `karac` subcommand?

**Why this is the interesting hybrid:**
- It captures the "online adaptation" story without committing to in-process JIT.
- Latency is minutes, not microseconds — but for warehouse-scale services, minutes is fine.
- Soundness story is identical to AOT — the v2 binary went through the same checker as v1.
- Kāra's effects/ownership invariants survive trivially; nothing about the language design has to bend.

**Position:** the *hot-swap* part is **P2** if the backend-first positioning (v64) demands it; otherwise **P3**. The *continuous PGO* part (collect counters, ship them, rebuild) is just operations on top of Problem 2 and ships whenever PGO ships. The expensive piece is the function-pointer indirection ABI commitment, which has to happen at v1 codegen freeze or it becomes a breaking change.

---

## Problem 4 — Architectural commitments needed at v1

Even though most of this ships post-v1, three things must be decided **at v1 codegen freeze** or else later additions become breaking changes:

### 4.A Stable identity (shared with v63 P0)

Path-based DefId + structural hash. Already load-bearing for v63's query channel. PGO needs it for `.profdata` keying that survives source drift. Runtime monomorphization (3.3) needs it for code-cache keying. Continuous PGO (3.2) needs it for symbol mapping across rebuilds.

**Conclusion:** v63 § Problem 5 § P0 item 1 is *also* the P0 for this doc. Single architectural commitment, two motivating consumers.

### 4.B Function-call ABI: indirection or direct?

Today (assumed) every internal call is a direct call (`call @foo`). For hot-swap (3.2) to be possible later, callers must either:
- Always call through a thunk / function pointer table.
- Or be re-emitted when a callee is hot-swapped (less general; works for whole-program swap but not partial).

**The cheap commitment:** ship v1 with a build flag (off by default) that emits an indirection layer for `extern`-public functions. Internal calls stay direct. Hot-swap then targets module boundaries, not arbitrary function bodies. This is the same shape as ELF lazy binding via PLT/GOT.

**Why "decide at v1 freeze":** if we ship v1 without any indirection plumbing, retrofitting it for 3.2 means recompiling every binary. The AOT artifact format effectively becomes versioned at that point.

**Conclusion:** add a Phase 7 codegen feature flag (`--enable-hot-swap`) that turns on PLT-style indirection for module-public symbols. Default off in v1; turning it on is non-breaking. Without this commitment, 3.2 is locked out post-v1.

### 4.C JIT-template metadata format

If 3.3 (runtime monomorphization) is going to ship post-v1, the AOT compiler must be able to *embed bitcode for generic templates that need runtime instantiation* into the binary. This requires:
- A new section in the artifact (`.kara_jit_template`).
- A manifest mapping `(generic_def_id, type_arg_pattern)` → bitcode offset.
- A v1-stable schema for the manifest, even if no code reads it in v1.

The cheapest commitment: define the section name and an empty-but-versioned manifest in v1. Producers and consumers both stub out; v2 fills them in.

**Conclusion:** define `.kara_jit_template` and a one-byte version manifest in v1; leave the actual emission and consumption for P2.

> **Review note (added 2026-05-09 — IR ABI stability gap).** The "empty-but-versioned manifest in v1" punts the harder question: when the Kāra runtime that JITs a template differs in version from the Kāra compiler that emitted it, does the embedded bitcode still parse and lower correctly? **This is the operational kill that ended ClangJIT** — embedded LLVM IR is not stable across LLVM major versions, so binaries with embedded bitcode broke under runtime upgrades. v65 needs a tentative pick before P0 implementation:
>
> - **(a) Pin runtime + AOT-compiler to the same Kāra version.** Practical short-term; means binary is no longer redistributable across Kāra releases. Probably the v1 stance.
> - **(b) Stable bitcode format separate from LLVM IR.** Cranelift CLIF is more stable than LLVM IR but not perfectly stable. Still need versioned readers.
> - **(c) Re-emit a portable, stable Kāra-side IR (KIR / typed AST snapshot).** Highest cost but solves version skew permanently. Long-term right answer if 3.3 ships broadly.
>
> The version manifest in v1 should be the lever that eventually picks between these, not just a forward-compat tag.

---

## Problem 5 — P0 / P1 / P2 / P3 carve

| Item | Tier | Rationale |
|---|---|---|
| **PGO (instrumented + AutoFDO)** | P2 | Builds on v1 codegen; complementary to v63 query channel; warehouse users will need it. |
| **Continuous PGO collection (counters live)** | P2 | Strict superset of instrumented PGO at deploy time; same machinery. |
| **Hot-swap with shared-object reload (3.2)** | P2 if v64 backend-first wants it; else P3 | Needs ABI commitment at v1 freeze (4.B); the *runtime* piece is post-v1. |
| **Runtime monomorphization JIT (3.3)** | P2 | Most Kāra-shaped JIT story; narrow scope; uniquely useful for plugin-host and dynamic-deserialization cases. |
| **Speculative tiering with deopt (3.4)** | P3 | High complexity, conflicts with effects/ownership invariants, marginal ROI. Document as "considered, declined." |
| **Full bytecode-first JIT (3.5)** | rejected | Wrong shape for AOT-first systems language. Document as "out of scope by design." |
| **Stable DefId + structural hash** | **P0** | Shared with v63; load-bearing for everything downstream. |
| **`--enable-hot-swap` codegen flag (off by default)** | **P0** | Must be present at v1 freeze or 3.2 becomes a breaking change. |
| **`.kara_jit_template` section + empty manifest** | **P0** | Same logic for 3.3. |

The three P0 items are the *architectural commit*. They are cheap individually (DefId is already load-bearing for v63; the hot-swap flag is a codegen pass that emits PLT-style indirection; the JIT-template section is an empty bytecode region with a version byte). Together they lock in the shape of v1 such that PGO and the narrow online-JIT slices are all non-breaking additions.

---

## Problem 6 — Interaction with v63 (the LLM↔compiler query channel)

v63 frames the query channel as **complementary** to PGO: PGO answers distribution-shaped questions, the channel answers intent-shaped questions. This doc takes that seriously.

Concrete interactions:

- **Stable identity is shared.** v63's P0 item 1 is the same primitive as this doc's P0 item 1. Build it once; both consumers benefit.
- **Branch-hint queries become richer with PGO.** v63 § Problem 4.F item 4 ("This `match` arm appears unlikely; confirm?") is a static-analysis guess today. With PGO data, the same query becomes "I observed this arm in 0.3% of sampled invocations; treat as cold? (PGO confirmed)" — much higher signal.
- **Specialization queries map to runtime monomorphization.** v63 § Problem 4.F item 2 (generic specialization) currently surfaces statically: "you monomorphized 14 times for these `T`s." With 3.3 in the picture, the same query has a runtime mode: "production saw `T = i64` from deserialization 200K times; bake an AOT specialization to skip the JIT step?" The author resolves it once, pays no JIT cost going forward.
- **Verification of author claims.** v63 § Open Questions raises this: "if the author writes `#[likely]` and it's wrong, what catches it?" PGO is the natural answer. Author claim → PGO data → mismatch becomes a query: "you annotated `#[likely]` here, but observed frequency is 8%; revise?"

**Implication:** the two systems should share a CLI subcommand surface. `karac query queries --with-profile=PATH` and `karac build --profile-use=PATH` are two consumers of the same identity-and-profile substrate.

---

## Problem 7 — Cost ladder (rough)

Eyeball numbers, not commitments:

| Item | Engineering weeks (rough) | Risk |
|---|---|---|
| P0: stable DefId + structural hash | 2–3 (already costed in v63) | Medium — touches resolver |
| P0: `--enable-hot-swap` codegen flag | 1 | Low |
| P0: `.kara_jit_template` empty section | 0.5 | Low |
| P2: instrumented PGO end-to-end | 4–6 | Medium — counter runtime is non-trivial |
| P2: AutoFDO support | 3–4 | Low — mostly external tooling |
| P2: runtime monomorphization JIT (Cranelift-based) | 8–12 | High — new in-process compiler |
| P2: continuous PGO + hot-swap | 6–8 | Medium-High — drain protocol is subtle |
| P3: speculative tiering w/ deopt | 16–24+ | Very high — interacts with effects/ownership/frame layout |

Total P0 is ~3–4 weeks (most of it shared with v63). P2 in aggregate is 20–30 weeks of post-v1 work, sequenceable.

---

## Open questions

- ❌ **Cranelift vs LLVM ORC2 for the runtime JIT (3.3).** Cranelift is smaller, faster-compiling, JIT-tuned; ORC2 reuses our existing LLVM pipeline and matches AOT codegen exactly. Cranelift is the right choice if JIT'd code performance can be ~10% off AOT; ORC2 if it must match. Tentative: Cranelift.
- ❌ **Counter runtime: link `compiler-rt/lib/profile` or write a Rust port?** Linking compiler-rt is fastest; a Rust port is more controllable and avoids a system dependency. Tentative: link compiler-rt for v1.x, port to Rust if it becomes a maintenance burden.
- ❌ **Profile format as artifact: target/ or repo-committed?** `target/profile/` is the obvious default. Some users will want to commit a "blessed" profile from a known-good workload run for reproducible builds. Tentative: `target/` default, `--profile-out=PATH` for opt-in commit.
- ❌ **`--enable-hot-swap` cost on AOT performance.** PLT-style indirection adds an extra load+jump per public-symbol call. Estimate <1% on most workloads but needs benchmarking. If cost is real, may need to make it per-symbol opt-in rather than module-wide.
- ❌ **AutoFDO sample collection on macOS / Windows.** `perf` is Linux-only. Equivalent on macOS is Instruments + Time Profiler with sample export; Windows is xperf. Tentative: Linux-first, document the gap, accept that warehouse users are mostly Linux anyway.
- ❌ **Drain protocol for hot-swap (3.2).** RCU-style quiescence is the standard; we may need explicit safepoints in long-running loops to bound drain time. Tentative: tie to the existing `suspends` effect verb (loops that already have suspend points are drain-safe; loops that don't get a compile warning).
- ❌ **Does runtime monomorphization (3.3) need a new effect?** A function that triggers JIT compilation now `allocates` (code memory) and `panics` (if the JIT itself fails). Tentative: yes, model JIT-triggering call sites as `allocates(jit_code)` + `panics(jit_failure)` so the type system reflects the runtime cost.
- ❌ **Embedded targets (no-mmap, no-exec).** `karac build --target=embedded` should hard-error on `.kara_jit_template` emission and on `--enable-hot-swap`. Tentative: yes, gated on profile (per existing `isr` profile precedent). **Review note 2026-05-09:** the W^X-enforcement audience is *broader* than just embedded — server hardening, browser-deployed WASM (which categorically lacks `mmap(PROT_EXEC)`), iOS, Android, gVisor sandboxes, FIPS deployments. v65 should explicitly state that 3.3's audience is "Linux + macOS + Windows servers without strict W^X" — real but smaller than the current framing implies. WASM targets in particular need `karac build --target=wasm-*` to hard-error on JIT-template features the same way embedded does.
- ❌ **Interaction with REPL.** The REPL already does a kind of just-in-time compilation (interpreter today; possible LLJIT post-v1 per `archive/v62`). If 3.3 lands, the REPL might naturally consume the same JIT runtime. Worth validating that the two stories share infrastructure.
- ❌ **Verifier-backed JIT outputs (Alive2-class).** Should JIT'd specializations be equivalence-checked against a reference interpretation before being installed? Out of scope for v1.x but worth a position in the design doc.
- ⊘ **Whether to ship full bytecode-first JIT (3.5).** Resolved 2026-05-06: no. Wrong shape for AOT-first systems language. Document as out-of-scope-by-design.
- ⊘ **Whether to ship speculative tiering (3.4).** Resolved 2026-05-06: no for v1, P3 at most. Cost-to-reward ratio is poor; conflicts with effects/ownership invariants. Continuous PGO (3.2) captures most of the win.

---

## Cross-references

- **brainstorming/63_llm_compiler_query_channel.md** — query channel; shares the P0 stable-identity work; PGO and queries are complementary signals (intent vs distribution).
- **brainstorming/64_backend_first_v1_concurrency.md** — backend-first positioning; whether it demands warehouse-grade adaptive perf is the deciding factor for promoting 3.2 (continuous PGO + hot-swap) from P3 to P2.
- **brainstorming/archive/v62** — interpreter performance / lazy LLJIT; the REPL's JIT story may share infrastructure with this doc's 3.3 (runtime monomorphization).
- **`docs/roadmap.md:752`** — the existing "PGO stubs" line; rename to "Static branch hints from effect analysis" to avoid the false signal that v1 ships any PGO.
- **`docs/implementation_checklist/phase-11-stdlib-longtail.md:10`** — same.
- **`docs/deferred.md`** — destination for P2 entries (PGO, continuous PGO, runtime monomorphization JIT); P3 entry (speculative tiering); rejected note (full bytecode JIT).
- **`docs/design.md`** — needs a short "Specification Layers" addition classifying PGO data and JIT'd code as reported-but-unstable, same band as v63 § Problem 4.E.
- **`src/codegen.rs`** — PGO instrumentation pass entry point; hot-swap indirection emission.
- **`runtime/`** — counter runtime library (new); JIT runtime library (P2, new).

---

## Resolution path

This brainstorm resolves into:
- **`docs/deferred.md`** — three new entries: "Profile-Guided Optimization (instrumented + AutoFDO)", "Continuous PGO with hot-swap", "Runtime monomorphization JIT". Each labeled P2. One short entry — "Speculative tiering with deopt" — labeled P3 with rationale for declining it from v1. One "Out of scope by design" note for full bytecode JIT.
- **`docs/implementation_checklist/`** — P1 lines for the three P0 items (stable DefId is shared with v63; `--enable-hot-swap` flag stub; `.kara_jit_template` empty section). P2 entries for each post-v1 item per the standing P1-needs-checklist-entry rule (extended to P2 entries for parity).
- **`docs/design.md`** — extend Specification Layers (§113–146) to classify PGO output and JIT'd code as reported-but-unstable behavior, alongside inferred effects.
- **`docs/roadmap.md:752`** — rename "PGO stubs" to "Static branch hints from effect analysis" to remove the false signal that v1 ships PGO.

Then this brainstorm doc is archived.
