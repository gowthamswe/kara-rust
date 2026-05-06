# v62 — Interpreter performance and binary size

Two observations from the kata-katas/leetcode/1-two-sum bench (2026-05-04, M1, hyperfine `--warmup 3 --runs 10 --shell=none`, release karac with `--features llvm`):

| Run | Mean ± σ |
|---|---|
| rust brute_force | 1.3 ± 0.1 ms |
| rust hash_map | 1.2 ± 0.1 ms |
| **kara brute_force (codegen)** | **1.6 ± 0.3 ms** |
| **kara hash_map (codegen)** | **2.0 ± 0.3 ms** |
| py brute_force | 17.7 ± 0.3 ms |
| py hash_map | 13.3 ± 0.2 ms |
| kara brute_force (interp) | 672.3 ± 11.4 ms |
| kara hash_map (interp) | 21.9 ± 0.3 ms |

Binary sizes (release): `kara_brute_force` = 33 KB, `kara_hash_map` = **1.4 MB** · `rust brute_force` = 466 KB, `rust hash_map` = 468 KB.

Two problems are analyzed below. **Problem 1** — why the interpreter is ~38× slower than CPython, and what that implies for the REPL / test runner. **Problem 2** — what "Map runtime statically linked" means, and whether the 42× size jump from Map is something to fix.

---

## Problem 1 — Tree-walk interpreter is ~38× slower than CPython

**Why.** CPython is "interpreted" but isn't a tree walker. It compiles source to bytecode (a flat opcode array), then runs that on a stack VM written in heavily tuned C — computed gotos for dispatch, inline caches for attribute lookup, small-int caching, etc. Decades of perf tuning live in the eval loop.

Kāra's interpreter walks the AST node by node. Every binary op, variable lookup, range check, and loop step does an enum-match dispatch and often a small allocation, then recurses into Rust `eval()` calls. Per-iteration cost works out to **3.4 µs (Kāra) vs 89 ns (CPython)** in this bench — ~38×. That gap is the cost of "AST-walking dispatcher" vs "bytecode loop in C." It's not Rust-vs-C; a tree-walk in C would land in the same place.

**What it affects.**
- **Bench numbers** — already worked around: codegen-Kāra ≈ Rust at small N. The interpreter's slowness is not a v1 blocker.
- **REPL feel** — REPL latency is dominated by parse + typecheck + dispatch, not iteration count. Defining a struct, calling a fn with small args, running a 100-element loop — all sub-millisecond on the tree-walk. The 38× shows up only on tight loops over 100K+ items, which is uncommon in REPL use.
- **`karac test`** — runs every `test_*` function via the interpreter today. Same calculus as REPL: fine for unit tests doing small amounts of work; bad for property tests / fuzz-style loops. Not a current pain point but a future ceiling.

### Options

- **A. Stay tree-walk forever.** Simplest. Accept the ceiling on REPL/test loop-heavy workloads. Tell users "drop to a build for any benchmark."
- **B. Bytecode interpreter.** Add a lowering from AST to a flat opcode array, write a stack-VM eval loop in idiomatic Rust. Expect ~10× speedup over tree-walk; still well short of native. Big code-surface addition (a fourth execution mode after typecheck → effects → ownership → eval) for a middling perf win.
- **C. JIT REPL / test cells via the existing LLVM backend.** Reuse `karac build`'s codegen path but emit an in-memory module instead of a file. Inkwell + LLVM ORC supports this directly. Each REPL cell becomes an LLVM compile + execute. Cost: the LLVM compile is itself a few hundred ms cold, so trivial cells get *slower* than tree-walk; a hybrid trigger needed.
- **D. Hybrid: tree-walk by default, route tight loops through codegen.** Detect loops with high trip count or annotation (`@hot`, `@bench`) and JIT just those. Most cells stay on tree-walk for fast startup. Adds a partial-evaluation seam.

### Recommendation

**Locked 2026-05-05: Option C (always JIT) with C-LLJIT as the JIT engine.** Every function lazy-compiled via LLJIT on first call; tree-walk interpreter retained only for development/debug tooling, not for runtime REPL/test execution. Skip B (bytecode VM) permanently. Skip C-MCJIT as a shipping vehicle (deprecated foundation conflicts with the "design Kāra upfront" principle). Skip D (hybrid) for v1. C-Cranelift evaluation deferred to v2 — Kāra is designed around LLVM (codegen, optimizer assumptions, "Kāra ≈ Rust" perf goal); single LLVM stack across AOT and JIT preserves architectural coherence. Cranelift's compile-speed advantage matters most for languages where REPL/JIT *is* the primary execution mode; Kāra's primary path is `karac build` to native, with REPL/test as production-grade tooling but not the headline use case. If real-world v1 feedback shows LLJIT cold-start is unacceptable, Cranelift becomes a v2 enhancement axis with empirical user data to justify the dual-backend cost.

The lock has two dimensions:

**1. Execution model: C (always JIT, no hybrid).** Every function gets JIT-compiled on first invocation. No hot-cell heuristic, no dispatcher, no tree-walk routing in the runtime hot path. Pure predictability: every cell pays the same ~100 ms compile-then-run cost on first invocation, fast on subsequent calls. Trivial REPL cells (`let x = 1+1`) cost ~100 ms instead of <1 ms — but the cost is **expected and uniform**, never a mystery slowdown. Honest framing for users: "REPL has built-in compile latency by design."

D (hybrid tree-walk + JIT dispatch) was considered and rejected for v1. The argument: a known/expected 5 ms vs 100 ms gap is acceptable; an unexpected 2 s vs 20 s gap forces investigation across code, data, language, library, network, and dependencies — orders of magnitude more user pain than a flat predictable cost. Hybrid remains purely-additive optimization for v1.5/v2 once real-world data shows whether the predictable cost is acceptable. No experimental opt-in hybrid in v1 either — same trap as MCJIT-as-shipping-vehicle (ship a half-feature, end up stuck supporting it; split user-test surface; reintroduce the heuristic-design effort we're saving).

**2. JIT engine: C-LLJIT.** Resolved among the three sub-paths from the original option C:

- ~~**C-MCJIT** (ship today, replace later)~~ — **rejected as shipping vehicle.** Retained as a sanity-check prototype only; rough LLJIT cold-start lower bound (LLJIT ≤ MCJIT due to lazy compilation). If MCJIT numbers come back wildly out of bounds (e.g., >2 s per compile), flag for investigation before committing engineering effort to orc2 integration — but the response is "diagnose and fix," not "switch backends."
- **C-LLJIT (drop to llvm-sys for the JIT subset)** — **selected.** Keep inkwell for codegen, call `llvm-sys::orc2` directly for JIT. ~30-function C API surface; the hard parts are ORC's lifetime/threading invariants. Effort estimate 2–4 weeks initial; time stretch acceptable per user direction (final-product perf > dev time).
- **C-Cranelift (separate JIT backend)** — **deferred to v2 evaluation.** Not a v1 fallback path. If real-world v1 feedback shows LLJIT cold-start is unacceptable (or memory footprint, or some other axis), Cranelift becomes a v2 enhancement decision with empirical user data to justify the dual-backend cost.

Skip B (bytecode VM) permanently. A bytecode VM that is 10× faster than tree-walk is still ~5× slower than codegen — it's the worst kind of intermediate, complex enough to maintain but not fast enough to solve the perf complaint.

**Sequencing.**
- (a) Sanity-check C-MCJIT prototype in a branch — ~3 days. Validates rough LLVM JIT viability on representative Kāra modules. Wildly-out-of-bounds numbers flag investigation before committing to orc2; otherwise proceed.
- (b) Wrap `llvm-sys::orc2` and integrate C-LLJIT as v1 JIT engine. Measure published cold-start number from real workloads. ~2–4 weeks initial; time-stretch budget per question #5 if needed.
- (c) Ship v1 with documented cold-start. Evaluate Cranelift only as a v2 enhancement if real-world feedback shows the cost is unacceptable.

**Open questions.**
- ✅ Inkwell's JIT API surface — answered: stable MCJIT only, ORC v1 behind experimental flag, no ORC v2 / LLJIT. Load-bearing for the C-LLJIT decision (forces drop to llvm-sys).
- ❌ LLJIT cold-start cost on first cell — **sharpened 2026-05-05.** Now load-bearing for v1 UX (every cell pays this under C-only) and now also the *number we publish to users* (Y plan: ship LLJIT with documented latency, no Cranelift fallback). Measurement plan: run cold/warm timings on three representative module shapes (small ~10 fns, medium ~50 fns, large ~200 fns) on M1 and Linux x86. Distinguish three components: LLVM init (paid once per process), per-module load cost (first time module touched), per-function lazy compile (first call to each fn). Targets: LLVM init <50 ms; per-function lazy compile <100 ms median. MCJIT sanity-check prototype gives a rough upper bound; real LLJIT measurement comes after orc2 integration and is what gets documented.
- ⊘ Hot-cell detection heuristic — **dissolved 2026-05-05** by the C-only lock. No heuristic in v1; every function JIT'd. Prior exploration retained for reference if hybrid resurfaces post-v1: H2 (static AST-shape: loops + recursion → JIT) → H6 (inverted default — JIT unless trivial whitelist matches) → C-only (no heuristic). The progression was driven by predictability concerns: H2 had multiplicative-cost false negatives (10 s mystery slowdowns); H6 reduced false-negative cost via lazy-compile asymmetry; C-only eliminates the heuristic entirely for flat known cost. OSR-based runtime tier-up (H4) reserved as the P2 "JIT v2" axis if real users find the flat ~100 ms cost unacceptable.
- ⊘ Hybrid dispatcher placement — **dissolved 2026-05-05** by the C-only lock. Single execution path (lazy LLJIT), no dispatcher needed. Prior exploration retained for reference if hybrid resurfaces post-v1: lazy classifier function attached to Fn nodes, called at function-entry, with cached jit_target/interp_target flag. Interp ↔ JIT call-boundary marshalling cost was flagged as the empirical risk; with C-only that boundary doesn't exist within the runtime hot path.
- ❌ `llvm-sys::orc2` wrapping effort — **sharpened 2026-05-05.** Milestones: W1 = skeleton wrapper compiles and runs a hello-world IR module via LLJIT; W2 = lifetime/ownership story stable, multiple modules co-existing without leaks; W3–4 = full integration, all current `tests/codegen.rs` E2E tests pass through JIT execution; W5 = error handling + threading edge cases. **Tripwire: if W3 hasn't landed by W6, halt and re-evaluate** (likely revisits Cranelift-as-v2-now question). "Wrapping done" criterion = full codegen E2E suite passes via JIT path with Level 2 DWARF preserved (per #7 lock).
- ⊘ Cranelift-fallback trigger threshold — **dissolved 2026-05-05** by the LLJIT-only-for-v1 commit. No mid-v1 backend switch; Cranelift evaluation deferred to v2 with empirical user feedback as the trigger. Architectural rationale: Kāra is designed around LLVM (codegen, optimizer assumptions, "Kāra ≈ Rust" perf goal); single LLVM stack across AOT and JIT preserves coherence. Cranelift's compile-speed advantage applies most to languages where REPL/JIT *is* the primary execution mode — Kāra's headline is `karac build`. v2 evaluation question becomes "is Cranelift worth the dual-backend cost given real user data?", not a pre-defined threshold.
- ✅ Diagnostic story when a JIT'd cell crashes — **resolved 2026-05-05: Level 2 floor for v1, Level 3 staged as P1.** Level 2 = every LLVM IR instruction tagged with source span via `DIBuilder`; runtime panic handler walks stack and resolves machine addresses → source line:col via emitted DWARF; user sees `panic at file:line:col in fn_name`. ~3–6 weeks. Level 3 = rustc-style snippets with caret + AST-node-type-aware messages ("this *index* operation panicked"); builds on the same DWARF plus diagnostic-formatter integration; ~3–4 additional weeks staged post-v1. Bonus from emitting DWARF: LLJIT/ORC's GDB JIT interface gives gdb/lldb users symbolic backtraces for free.

---

## Problem 2 — Static linking of the runtime, and the 42× binary-size jump from `Map`

**Why.** Both Kāra and Rust statically link their collection runtimes — neither expects a `libkarac_runtime.so` on the target machine. The difference is dead-code-elimination granularity:

- **Rust.** Generics monomorphize per concrete type, then LTO + DCE strip everything not transitively reachable from `main`. `HashMap<i64, usize>` ships only the `i64`/`usize` instantiation, only the methods you called (`insert`, `get`), only the helpers those reach. Adding `HashMap` to a binary that already pulls in `println!` (panic handler, allocator, formatter) costs **2 KB**.
- **Kāra (today).** `libkarac_runtime.a` is shipped as a few large object files. Linker pulls in object-file granularity: any reference to a `Map` symbol drags the entire object containing it, including unused methods, unused key/value type combinations, and supporting helpers. **1.4 MB** for a binary that uses one `Map[i64, i64]`.

**What it affects.**
- **Runtime perf** — nothing. Hot Map operations run at native speed once loaded. This is purely a code-size issue.
- **Cold start** — measurable on serverless / Lambda (every megabyte adds maybe 5–20 ms of disk read + page-in). Irrelevant on dev machines.
- **Distribution** — relevant for shipping CLIs as binaries (Homebrew-style), embedded targets where flash is limited, and any download-size-sensitive context.
- **Headline framing** — "Kāra produces 1.4 MB binaries for one Map" is a thing reviewers will notice when comparing to Rust's 2 KB delta. It tells against the "Kāra ≈ Rust" framing once codegen is solid.

### Options

- **A. Accept current cost.** YAGNI until shipping context surfaces. Risk: the number gets quoted in reviews against Kāra ("ships 1.4 MB hello-worlds with maps").
- **B. Enable LTO + DCE across user code + runtime.** Configuration change in the build pipeline — pass `-Wl,--gc-sections` (or the macOS equivalent) and `-fuse-ld=lld` with cross-archive LTO. Probably the single biggest win for least effort. Catches: (a) runtime functions called only via runtime-internal paths must be marked `#[used]` or kept by retain-list; (b) ASAN builds may regress if LTO interacts badly with sanitizer instrumentation.
- **C. Split runtime into per-feature archives.** `libkarac_core.a`, `libkarac_map.a`, `libkarac_vec.a`, etc. Linker only pulls in archives the user actually references. Smaller archives = finer-grained DCE. More moving parts in the build system; redundant once B is in place if LTO is aggressive enough.
- **D. Generic-instantiation pruning at compile time.** Codegen-side — only emit the type instantiations the user code actually uses, instead of pre-emitting all combinations the runtime supports. This is what Rust's monomorphization gives for free. Requires the compiler to understand which Map type combinations are reachable from main, then pass that set to the runtime. Larger refactor.
- **E. Dynamic linking (`libkarac_runtime.so`).** Rejected — trades binary size for the deployment-headache of a versioned runtime dependency on the target machine. Static-linking is the right default; don't change it.

### Recommendation

**Locked 2026-05-06.** Final shape:
- **Phase 1 + 2 (P0, v1):** cheap wins (strip + panic=abort, then LTO/DCE). Ship in v1.
- **Phase 3 (folded into Phase 4):** runtime split reshaped — under the monomorphized-collections lock, "split runtime archive at per-feature granularity" disappears. Replaced by "decide what minimal residual archive remains for non-monomorphizable primitives," done as part of Phase 4.
- **Phase 4 (P0 design property, P1 implementation):** Kāra collections monomorphize (locked direction); runtime restructured as monomorphizable source rather than pre-built C archive. Empirical justification + sequencing in the Phase 4 section below. Indirection microbench 2026-05-06 confirmed 25% erasure tax (75% of total Karac-vs-std gap).
- **E (permanent omission):** dynamic linking off the table.

Cheap wins first; structural changes (Phase 4) take the time they need but the direction is locked.

Investigation reordered the sequence. Cheap wins first; structural changes only if the cheap wins leave a visible gap.

**Phase 1 — `strip -x` + `panic = "abort"` (one-line + Cargo profile, one PR).**

- Add `strip -x <output>` after the `cc` invocation in `link_executable`. Verified: 1.4 MB → 1.18 MB (-18%). Free.
- Set `panic = "abort"` in the runtime crate's `[profile.release]`. Drops `__eh_frame` + `__unwind_info` (~114 KB). Ship unconditionally for v1 — `catch_panic` (v60 item 26) is currently P1 deferred, not v1, so no need to pre-build a build-profile gate now. When `catch_panic` lands later, the gate (kernel/embedded → abort, default → unwind) lands with it as part of that feature's PR. Don't pre-design for a hypothetical.

Estimated combined effect: 1.4 MB → ~900–1000 KB. About halfway to Rust's 468 KB number, with no architectural change.

**Phase 2 — `-Wl,-dead_strip` (macOS) / `-Wl,--gc-sections` + runtime crate `lto = "thin"` (Linux).**

- macOS: `-Wl,-dead_strip` is mostly redundant given `.subsections_via_symbols`, but explicit doesn't hurt.
- Linux: requires runtime crate to emit function-sections (`RUSTFLAGS=-C link-arg=-ffunction-sections -C link-arg=-fdata-sections` per-target, or `lto = "thin"` in the runtime crate's release profile to get cross-crate inlining + DCE).
- Adds an LTO step at link time. Compile time goes up modestly.

Estimated effect on top of Phase 1: another 100–300 KB on Linux (where Phase 1 alone benefits less). Marginal on macOS because subsections-via-symbols is already doing function-level DCE.

**Phase 3 — reshaped 2026-05-06 by Phase 4 lock.** Was: split pre-built runtime archive at per-feature granularity (`karac-runtime-core` + `karac-runtime-map`). Under the monomorphized-collections lock, this becomes: **decide what minimal residual archive remains** for primitives that genuinely can't be monomorphized (panic infra, allocator interface, FFI bridges, OS bindings). Implicit in the runtime restructure done as part of Phase 4. No standalone Phase 3 PR — folded into Phase 4 implementation.

**Phase 4 — locked 2026-05-06: collections monomorphize.** Kāra collection types (`Map[K,V]`, `Set[T]`, `Vec[T]`, future `BTreeMap[K,V]`, etc.) emit one specialized implementation per concrete type tuple, like Rust's `std::collections::HashMap<K,V>`. Today's type-erased C-runtime model — function-pointer dispatch on hash/eq, byte-blob storage, dynamic-size memcpy — is replaced. `libkarac_runtime.a` shrinks to non-monomorphizable primitives.

This is a **P0 codegen design property** (locked direction), with **P1 implementation timing** (post-v1 work). Locking direction now constrains future runtime additions: no new collection types ship on the erased model.

**Empirical justification (microbench 2026-05-06, M1, hyperfine `--warmup 3 --runs 10`):**

| Config | Mean ± σ | Δ vs A |
|---|---|---|
| **A.** `std::HashMap<i64,i64>` (Rust headline algorithm) | 420.8 ± 23.3 ms | — |
| **B.** Karac algorithm, monomorphic Rust (no fn pointers, no byte blobs) | 449.4 ± 10.9 ms | **+6.8%** |
| **C.** Karac runtime via FFI (today's erased model) | 554.3 ± 12.6 ms | **+31.7%** |

Workload: two-sum-style insert+get loop, N=1M elements, 10 iterations, all configs verified to compute identical hits. Calibration check: microbench's 1.32× of std HashMap matches v62 doc's N=5000 number (1.33× = kara 2.8 ms / rust 2.1 ms) — the algorithm-dominated regime is faithfully captured. Bench source: [`bench/indirection_cost/`](../bench/indirection_cost/) (standalone Cargo project, not in main workspace; rebuild with `cargo build --release` and rerun via `hyperfine` against the three release binaries).

**Decomposition of the 31.7% Karac-vs-std gap:**
- **6.8% = algorithm choice tax** (Karac's open-addressing + FNV-1a vs std's Robin Hood + AHash). Independent of erasure; survives monomorphization.
- **24.9% = erasure tax** (function-pointer indirection on hash/eq + dynamic-size memcpy + void* API). **Fully eliminable by monomorphization.**

**Why lock b despite the moderate (~25%) erasure-tax delta:**
1. **75% of the Karac-vs-Rust hash_map gap is the erasure tax.** Monomorphization moves us from 1.32× → ~1.07× of std HashMap. "Near-parity" framing instead of "1.3× competitive."
2. **Cheapest moment to lock direction.** Runtime is 726 LOC across 2 files. Every future collection (Set, BTreeMap, Vec specializations) added on the erased model deepens the entrenchment and grows the eventual switch cost.
3. **Trait-bounds-at-codegen enforcement** is needed for the language to mature regardless of this decision; under the b lock it becomes a **foundational P0 prerequisite** (must enforce `K: Hash + Eq` at monomorphization for `Map[K,V]` to work right). See investigation findings below.
4. **"Kāra ≈ Rust perf" headline** matches "near-parity" much better than "1.3× competitive."
5. **4–8 weeks of focused work** is acceptable cost-of-correctness for a P0 design property — locked per user direction "if the only trade off is 4 to 8 weeks of work and it gets me that parity, i would be fine with that."

**What gets locked:**

- **Direction (P0):** all Kāra collections monomorphize. No type-erased runtime collections in v1+. No mixed model — a single ABI convention for collections across the language.
- **Implementation model:** runtime collection source becomes monomorphizable Kāra/Rust source compiled per user crate, like `std::collections::HashMap` in Rust. Existing codegen infrastructure (`generic_fns`, `generated_monos`, `mangle_mono_name`, `type_subst` — see investigation findings) extends naturally to runtime collections.
- **Implementation timing (P1):** post-v1. Sequencing: (1) close trait-bounds-at-codegen enforcement gap (foundational, useful regardless), (2) restructure runtime as monomorphizable source rather than precompiled archive, (3) extend codegen to invoke runtime-collection methods through `compile_generic_call`.
- **Pre-implementation gate (passed):** indirection microbench confirmed 25% erasure tax and 75%-of-total-gap attribution — direction lock has empirical backing.
- **What `libkarac_runtime.a` becomes:** primitives that genuinely can't be monomorphized — panic infra, allocator interface, FFI bridges, OS bindings. Order of magnitude smaller than today's archive.

**Skip permanently:** "hybrid mono + erased" approaches (some collections monomorphize, others stay erased). Mixed-model trap = two ABI conventions, doubled test surface, no clear rule for which to use.

A → C residual 25% gap above is the **answer to** the "is monomorphization worth it" question, not a problem to solve separately. The remaining 6.8% (B vs A) is "Karac chose open-addressing + FNV-1a vs Robin-Hood + AHash" — separate algorithm-design question for post-v1; not load-bearing for v1.

**Mark E (dynamic linking) as permanent omission**, in the same style as v60 item 32 (specialization). Rationale: static linking is the right default — predictable, no install dance, no dlopen at startup. Dynamic linking trades binary size for distribution headache (versioned runtime dependency on the target machine, ABI stability burden, package-manager friction); the binary-size wins available from Phases 1–4 are sufficient without changing the deployment model. Locked 2026-05-06.

**Binary-size targets.** Cumulative through phases:
- Today: 1.4 MB
- After Phase 1 (v1): ~900 KB (-35%)
- After Phase 2 (v1): ~700 KB on Linux, ~850 KB on macOS (-50% / -40%)
- After Phase 4 (post-v1): per-instantiation scaling — Map[i64,i64]-only programs closer to Rust's 468 KB; programs with N concrete (K,V) collection pairs scale linearly with N. Per-pair Map machinery ≈ 5–10 KB inlined.

**Perf targets (added 2026-05-06 with Phase 4 lock).** Map operations:
- Today (erased runtime): 1.32× of std::HashMap on i64-key workloads (microbench-validated)
- After Phase 4: ~1.07× of std::HashMap (residual is Karac's open-addressing + FNV-1a algorithm choice vs std's Robin Hood + AHash — separate post-v1 algorithm question if it matters)

If the goal is "Kāra hello-world ≈ Rust hello-world" *binary size*, Phase 1+2 is sufficient. If the goal is "Kāra ≈ Rust *perf* on Map-heavy workloads," Phase 4 is required.

**Open questions.**
- ✅ macOS vs Linux DCE flag differences — answered: macOS `-Wl,-dead_strip`, Linux `-Wl,--gc-sections`. Mach-O has subsections-via-symbols always-on, so DCE works automatically without `-ffunction-sections`. Linux ELF requires explicit `-ffunction-sections -fdata-sections` on the runtime build for granular DCE.
- ✅ Inkwell linker control — answered: link is via `cc` (`src/codegen.rs:121`), so passing linker flags is a one-line edit. No inkwell-specific LTO API needed.
- ✅ `panic = "abort"` interaction with future `catch_panic` (v60 item 26) — **resolved 2026-05-06.** Ship Phase 1's `panic = "abort"` unconditionally for v1. `catch_panic` is P1 deferred, not v1, so the build-profile gate (kernel/embedded → abort, default → unwind) becomes part of the `catch_panic` feature PR when that lands. Don't pre-build the gate.
- ✅ Runtime sweep for `#[used]` / `#[link_section]` / ctor-dtor symbols — **resolved 2026-05-06 as a concrete pre-flight task gating Phase 2.** Action: grep `runtime/src/` for `#[used]`, `#[link_section]`, `#[ctor]`, `#[dtor]`, `#[no_mangle]`, and `extern "C"` declarations before enabling LTO. Document any findings in the Phase 2 PR. The runtime is 726 LOC across 2 files — this is a 5-minute task, not a design question. Phase 2 PR's checklist: (1) sweep done, (2) any flagged symbols documented as keep-list, (3) LTO enabled, (4) E2E tests pass, (5) measured size delta reported.

**Phase 4 — resolved 2026-05-06: locked as P0 design property with P1 implementation.** See Phase 4 section above for full lock + empirical justification. Knock-on effects: Phase 3 reshapes (folded into Phase 4); trait-bounds-at-codegen lifts to P0 prerequisite (see new open-question entry below).

**Trait-bounds-at-codegen enforcement — flagged 2026-05-06 as P0 prerequisite for Phase 4.** Currently *parsed and validated* but **not enforced** at monomorphization time (`src/typechecker.rs:4581–4601`): concrete `T` is not checked to satisfy declared trait bounds when a generic is instantiated. Must close before monomorphized collections can enforce `K: Hash + Eq` for `Map[K,V]`. Useful regardless of Phase 4 lock. Sequencing: this is step 1 of the Phase 4 implementation work — independent and shippable on its own as a language-correctness improvement.

---

## Investigation findings (2026-05-04)

Concrete data gathered while drafting this doc — load-bearing for the recommendations above.

### Link pipeline (`src/codegen.rs:121`)

- `karac build` produces an LLVM object via `inkwell`'s `TargetMachine`, then invokes `cc <obj> -o <bin> <libkarac_runtime.a>` to link. `cc` is the system compiler driver (clang on macOS, gcc on Linux).
- Linker flags pass through trivially via `-Wl,...`. Adding flags to the link step is a one-line edit.
- Runtime archive resolution order: `KARAC_RUNTIME` env var → installed `<bin>/../lib/libkarac_runtime.a` → dev fallback `target/release/libkarac_runtime.a`.

### Runtime crate (`runtime/`)

- `crate-type = ["staticlib"]`, two source files: `lib.rs` (258 LOC) + `map.rs` (468 LOC) = **726 LOC total**.
- Built archive `target/release/libkarac_runtime.a` is **17.6 MB** with **1,337 external symbols** (the bulk is transitive Rust libstd: panic, allocator, formatting machinery, unwinding).
- Whole runtime is small enough that per-feature splitting (option C in problem 2 above) is a config-only change, not a refactor — there's nothing to redesign, just split `runtime/src/map.rs` into its own crate when needed.

### macOS DCE is already happening at function granularity

Mach-O linker has `.subsections_via_symbols` always enabled — function-level dead stripping is automatic on macOS. The 17.6 MB archive → 1.4 MB binary jump for `kara_hash_map` is the result of `cc`'s default link already doing per-function DCE. `-ffunction-sections` / `-fdata-sections` are no-ops on macOS.

This means the LTO+DCE recommendation differs by platform:
- **macOS**: function-level DCE is the default. Easy wins are `strip -x` and `-Wl,-dead_strip` (the latter mostly redundant given subsections-via-symbols).
- **Linux ELF**: needs the runtime crate compiled with `-ffunction-sections -fdata-sections` (`RUSTFLAGS=-Clink-arg=...` or per-target rustc config), then `-Wl,--gc-sections` on the link. Runtime crate may also need `lto = "thin"` in `[profile.release]` for cross-crate inlining.

(Ref: [Linker garbage collection — MaskRay](https://maskray.me/blog/2021-02-28-linker-garbage-collection))

### Strip is a free 18% win

```
$ ls -la kara_hash_map                  # before: 1,433,224 bytes
$ cp kara_hash_map /tmp/x && strip -x /tmp/x
$ ls -la /tmp/x                         # after:  1,183,192 bytes  (still runs, prints -20)
```

Strip drops 250 KB by trimming the `__LINKEDIT` segment (symbol tables, dyld info). One-line add to the cc invocation in `link_executable` (`src/codegen.rs:~121`). Should land regardless of bigger DCE strategy.

### Section breakdown of the 1.4 MB hash_map binary (macOS, `size -m`)

| Segment / Section | Bytes | % | Notes |
|---|---|---|---|
| `__TEXT.__text` (code) | 591 KB | 41 % | Executable code — Map machinery + libstd |
| `__LINKEDIT` | 557 KB | 39 % | Symbol tables, dyld info — `strip -x` drops most of this |
| `__TEXT.__eh_frame` + `__unwind_info` | 114 KB | 8 % | Unwinding tables — drop with `panic = "abort"` in runtime profile |
| `__TEXT.__const` | 100 KB | 7 % | Read-only data |
| Other | ~70 KB | 5 % | |

Compare `kara_brute_force`: 16 KB total. Doesn't reference the runtime → linker drops everything.

This points the optimization sequence: `strip -x` (cheap) → `panic = "abort"` for the runtime crate (cheap, but check `catch_panic` interaction first — see v60 item 26 if it lands) → cross-archive LTO (medium) → per-feature runtime split (medium).

### Inkwell does NOT expose ORC v2 / LLJIT

Load-bearing finding for problem 1 option C above.

- **Stable inkwell** exposes only the `execution_engine` module (MCJIT-flavored). Methods: `create_jit_execution_engine`, `create_mcjit_execution_engine_with_memory_manager`. Whole-module compile, no incremental JIT.
- **Inkwell `experimental` cargo feature** wraps **ORC v1** (`LLVMOrcJITStackRef`). v1 itself is upstream-deprecated.
- **ORC v2 / LLJIT (`LLVMOrcLLJITRef`, `LLVMOrcThreadSafeContextRef`, `LLVMOrcResourceTrackerRef`)** is **not exposed** by inkwell at any feature level.
- MCJIT is feature-frozen; LLVM upstream guidance: "MCJIT clients should use LLJIT". No formal removal date in 2024–2025 LLVM releases, but the deprecation direction is unambiguous.

(Refs: [inkwell ExecutionEngine](https://thedan64.github.io/inkwell/inkwell/execution_engine/struct.ExecutionEngine.html) · [LLVM ORCv2 docs](https://llvm.org/docs/ORCv2.html) · [Apache Arrow Gandiva MCJIT→LLJIT migration](https://github.com/apache/arrow/issues/37848))

This means option C in problem 1 has three sub-paths, none free:
- **C-MCJIT (today, deprecated foundation):** use inkwell's `create_jit_execution_engine`. Ships today. Builds on a feature-frozen LLVM API; will need migration when MCJIT is finally removed.
- **C-LLJIT (modern, drop to llvm-sys):** bypass inkwell for the JIT subset, call `llvm-sys::orc2` directly. Keeps inkwell for codegen. Real Rust unsafe-FFI surface to maintain — not enormous (the LLJIT C API is ~30 functions) but non-trivial.
- **C-Cranelift (modern, separate backend):** Cranelift's JIT focus gives ~10× faster compile than LLVM at ~14% worse code quality. Run-time perf is fine for REPL/test cells. Tradeoff: maintaining a second backend (LLVM AOT + Cranelift JIT). [Cranelift design — wasmtime](https://github.com/bytecodealliance/wasmtime/blob/main/cranelift/docs/compare-llvm.md)

### Optimization level bump (O2 → O3) is not the bottleneck

`create_target_machine` in `src/codegen.rs:170-188` configures
`OptimizationLevel::Default` (= LLVM `-O2`) and CPU `"generic"` with no target
features. Tested 2026-05-04: bumping to `OptimizationLevel::Aggressive`
(= `-O3`) and re-running all four bench workloads produced no measurable
improvement — every number sat within noise of the O2 baseline:

| Workload | O2 (baseline) | O3 | Rust | Gap O2→O3 |
|---|---|---|---|---|
| hash_map | 2.8 ms | 2.7 ms | 1.9 ms | 1.4× → 1.45× |
| coin_change | 8.7 ms | 8.4 ms | 5.1 ms | 1.7× → 1.64× |
| brute_force | 97.5 ms | 98.9 ms | 32.9 ms | 3.0× → 3.0× |
| sieve | 11.1 ms | 11.2 ms | 2.7 ms | 4.3× → 4.14× |

Why the bump did nothing: O2 → O3 mostly unlocks loop autovectorization,
function-cloning, and more aggressive inlining. None of those passes can
fire on Kāra's IR today because **bounds checks on every indexed access
break the prerequisite analyses** (the autovectorizer can't vectorize a
loop with a side-exit on every iteration). The optimizer is doing what it
can at O2 — there's no extra room at O3 because the IR doesn't expose any.

This is actually informative — it confirms the gap is *structural*, not a
matter of "we forgot to crank the optimizer." Bounds-check elision is the
load-bearing change to close the brute_force / sieve gaps. Until that
lands, opt-level changes are meaningless. Reverting the change.

CPU `"generic"` separately disables ISA-specific instructions (NEON on M1,
AVX on x86). Switching to `"native"` would help, but matches Rust's default
behavior, so it's a tie there. Worth exposing as a `--target-cpu=native`
flag rather than a default change.

### ORC v2 cold-start latency — still empirical

No concrete cold-start benchmarks in publicly indexed sources. Inferences:
- LLVM ORC supports concurrent + lazy compilation by design — first-function execution can begin before the rest of the module is compiled.
- Cranelift's ~10× faster compile gives a rough upper bound on what "fast JIT" costs vs LLVM. If LLVM cold compile of a ~50-line REPL cell is, say, 80 ms, Cranelift would be ~8 ms.
- The honest answer is that this needs prototyping to size — paper benchmarks generalize poorly across JIT engines and module shapes.

---

## Bench & framing follow-ups

- **Codegen correctness is P0 over both.** ✅ resolved 2026-05-04. Root cause was a missing Slice arm in the codegen method dispatcher; `Slice.len()` silently returned the catch-all's i64 0 instead of reading the slice's length field, making any `for i in 0..nums.len()` loop a no-op. Fixed by adding `len` / `is_empty` handlers in `src/codegen.rs`; 4 regression tests in `tests/codegen.rs` (search `test_e2e_slice_len_after_array_coercion`). Performance and size optimizations remain blocked on codegen correctness in general, but this specific blocker is gone. The wider follow-up — converting the dispatcher's silent-0 catch-all to a compile-time error so other latent method-dispatch bugs surface — is tracked in [`implementation_checklist/wip-list1.md`](../docs/implementation_checklist/wip-list1.md).
- **Bench script change.** `bench/bench.sh` currently invokes `karac run` (interpreter) — which is what produced the misleading 480× Rust-vs-Kāra ratio. Wire in a `build_kara` step (analogous to `build_rust`) so the script benchmarks the compiled binaries. Until the parent-file codegen bug is fixed, only the bench-workload variants compile correctly, so the wiring is straightforward.
- **README snapshot is stale.** § Benchmarks shows the 663 ms tree-walk number as headline. Once codegen-Kāra is in the script and the parent-file bug is fixed, the README should lead with the codegen numbers and demote the interpreter row to "what the interpreter costs" context.
- **N=200 is too small.** README already calls this out. The hash-map vs brute-force algorithmic crossover is invisible in Rust at N=200 because the algorithm is below process-startup floor. Bump to N=5000 once codegen-Kāra is the headline — that's where the algorithmic story shows up.

## Proposed actionable decisions

All items below are pending decision unless marked otherwise.

### Interpreter execution path

1. **REPL/test execution path** — **locked 2026-05-05.** Option C (always JIT) with C-LLJIT as the JIT engine; LLVM is the single backend across AOT and JIT. Every function lazy-compiled via LLJIT on first call; tree-walk retained only as development/debug tool. Skip B (bytecode VM), C-MCJIT-as-shipping-vehicle, D (hybrid in any form, including experimental opt-in), and Cranelift-as-v1-fallback permanently. Hybrid optimization (any heuristic + dispatch scheme, including OSR tier-up) and Cranelift evaluation both deferred as purely-additive future work for v2 if real-world data shows the predictable ~100 ms LLJIT cold-start is unacceptable. Sequencing: (a) C-MCJIT sanity-check prototype (~3 days), (b) wrap `llvm-sys::orc2` and integrate C-LLJIT as v1 JIT engine, (c) ship v1 with documented cold-start. JIT-crash diagnostics resolved (Level 2 v1, Level 3 P1 — see open questions). Remaining empirical open questions (LLJIT cold-start measurement, orc2 wrapping effort) gate the integration work.

### Binary size

2. **Phase 1 (cheap wins) — locked 2026-05-06, P0 (v1):** add `strip -x` to the link step (verified: -18%), set `panic = "abort"` unconditionally in the runtime `[profile.release]` (-114 KB unwind tables). No `catch_panic` build-profile gate now — that lands with `catch_panic` itself (P1 deferred). Combined estimated effect: 1.4 MB → ~900 KB. Single PR, no architectural change.
3. **Phase 2 (LTO/DCE) — locked 2026-05-06, P0 (v1):** add `-Wl,-dead_strip` (macOS) / `-Wl,--gc-sections` (Linux) + `lto = "thin"` on the runtime crate's release profile. Pre-flight sweep for `#[used]` / `#[link_section]` / `#[ctor]` / `#[dtor]` / `#[no_mangle]` / `extern "C"` symbols in `runtime/src/` is part of the Phase 2 PR's checklist (5-minute grep, document any findings as the keep-list). Estimated effect on top of Phase 1: ~700 KB Linux, ~850 KB macOS.
4. **Phase 3 (runtime split) — folded into Phase 4 by 2026-05-06 lock.** Pre-built archive split disappears under the monomorphized-collections lock; replaced by "decide what minimal residual archive remains for non-monomorphizable primitives," done as part of Phase 4's runtime restructure.
5. **Phase 4 (collections monomorphize) — locked 2026-05-06, P0 design property + P1 implementation.** All Kāra collection types (Map, Set, Vec, future BTreeMap, etc.) emit one specialized impl per concrete type tuple. Runtime restructured from pre-built C archive to monomorphizable source. Empirical justification: indirection microbench 2026-05-06 confirmed 24.9% erasure tax on i64 hash_map (75% of total Karac-vs-std gap). Sequencing of implementation: (a) close trait-bounds-at-codegen enforcement gap [P0 prerequisite, also useful standalone], (b) restructure runtime as monomorphizable source, (c) extend codegen to invoke runtime collections via `compile_generic_call`. Estimated effort 4–8 weeks focused. No mixed mono/erased collections — single ABI convention.
6. **Trait-bounds-at-codegen enforcement — locked 2026-05-06, P0 prerequisite for #5.** Currently parsed and validated (typechecker.rs:4581–4601) but not enforced at monomorphization time. Concrete `T` must be checked to satisfy declared bounds at instantiation. Useful as a standalone language-correctness improvement; foundational for Phase 4. Step (a) of the Phase 4 sequencing.
7. **E (dynamic linking) — locked 2026-05-06, permanent omission.** Rationale captured in deferred.md alongside specialization (v60 item 32) when canonical migration runs.

### Bench / README

> **Items moved to [`implementation_checklist/wip-list1.md`](../docs/implementation_checklist/wip-list1.md) for execution** (originally #8 / #9 / #10): bench script `build_kara` wiring; README §Benchmarks rewrite; N=5000 bump across all bench files. Pure execution work — no design fork. Removed from this brainstorm 2026-05-06 to keep the locked-decisions section focused on design choices.

### Codegen perf levers

Bench data (added 2026-05-04, context for the items below): kara coin_change (N=50K, K=20) 8.7 ± 0.4 ms vs rust 5.1 ± 0.3 ms — **1.7× gap**, slice indexed-access workload. kara sieve (N=10K, K=200) 11.1 ± 0.2 ms vs rust 2.6 ± 0.1 ms — **4.3× gap**, bool-array tight-loop workload (likely Rust bounds-check elision + vectorization). Combined with the two-sum data, Kāra codegen is **1.4×–4.3× of Rust** depending on how much LLVM can optimize Rust's code.

8. **Bounds-check elision — locked 2026-05-06.** Three sub-paths considered: **(a) LLVM-friendly emission** (bounds checks shaped via `llvm.assume` + cold-attribute panic blocks + SCEV/GVN-friendly idioms; ~2–4 weeks, user-invisible, the rustc model); **(b) Karac-side BCE pass** (pattern-match `for i in 0..xs.len()` and similar, mark indexing unchecked pre-codegen; ~4–8 weeks); **(c) `unsafe { xs.get_unchecked(i) }` escape hatch** (~1 day, user-visible, complements (a)/(b)).

    **Lock: (a) + (c) as P0 v1; (b) as P2 contingent.** Foundational rationale (not "one perf lever among many"): the O2→O3 investigation finding (in the "Optimization level bump" section above) shows LLVM autovec is currently *dark* on Kāra IR because bounds checks at every indexed access create a side-exit per iteration. **Without BCE, every loop-perf optimization in LLVM is a no-op** — that's why O3 = O2 = 0% delta today. (a) addresses the documented prerequisite failure by emitting bounds checks in a form SCEV/GVN can prove redundant from loop induction variables. All three bench workloads (sieve 4.3×, brute_force 3.0×, coin_change 1.7×) sit squarely in (a)'s strength zone — stride-1 / step-based induction over slice bounds, exactly the case rustc's BCE handles. (c) is a 1-day escape hatch using a well-known Rust pattern that fits Kāra's existing `unsafe_lint.rs` story; ships with v1 so any residual case (a) misses has user-side recourse without API churn. (b) lacks empirical motivation — bench-suite workloads should all close under (a) alone — and is deferred to P2 with trigger condition: post-v1 user data shows real-world patterns (a) misses. Combined v1 cost ~3–5 weeks; parallelizes with LLJIT integration (#1) and Phase 4 monomorphization (#5) since each touches different compiler subsystems.
9. **`--target-cpu=native` flag for `karac build` — locked 2026-05-06, P1 (post-v1), gated on #8 landing.** Optional flag flipping the LLVM target machine from `"generic"` to host CPU. Unlocks NEON/AVX/etc. for autovectorization. Default stays generic so binaries remain portable. Pre-blocked by #8: per the O2→O3 finding, vectorization currently can't fire on Kāra IR — `--target-cpu=native` provides zero benefit until BCE lets autovec engage. Cheap addition once #8 lands. P1 not P0 since it's a post-BCE follow-up that can ship in v1.1 / patch release; not load-bearing for the v1 perf headline.

> **Items moved to [`implementation_checklist/wip-list1.md`](../docs/implementation_checklist/wip-list1.md) for execution** (low-hanging fixes, no design discussion needed): the dispatcher silent-`0` catch-all → hard error follow-up; the repeat-literal `[v; N]` zero-init fast path. Both are mechanical codegen cleanups removed from this brainstorm so reviewing the remaining decisions doesn't get distracted by the no-brainers.
