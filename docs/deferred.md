# Deferred — Post-MVP Features

Detailed design specifications for features deferred from MVP. Each entry has a committed design shape — syntax, semantics, and non-breaking guarantees — so the rest of the language can be designed without conflicting assumptions.

## Priority Tier Definitions

The project uses four tiers to express both *when* a feature ships and *how committed* the project is to building it:

| Tier | When it ships | Commitment | Lives in |
|------|---------------|------------|----------|
| **P0** | v1, day-one (MVP) | Committed; the minimum-viable surface | [design.md § Deferred Items](design.md#deferred-items) |
| **P1** | v1, after MVP (any v1 patch release) | Committed; non-breaking additions whose absence would weaken v1 | This file, § P1 |
| **P2** | Post-v1 | Committed — *will* be built by the language author or the community; the work is just post-v1 | This file, § P2 |
| **P3** | Post-v1 (or never) | Open — may or may not be built; libraries / frameworks built on top of the language, not language features | This file, § P3 |

**Crucial distinctions:**

- **P0 vs P1.** Both are v1. P0 ships first; P1 ships within v1 but later. "Deferred to P1" does **not** mean "post-v1."
- **P1 vs P2.** P1 ships in v1; P2 ships post-v1. Moving an item between these is a meaningful commitment about *when* it ships.
- **P2 vs P3.** Both are post-v1. P2 is **committed language work** ("will happen, just later"). P3 is library / framework work where the question of *who builds it* (and whether) is open.
- P2 entries use **promotion gates** when the mechanism is genuinely uncertain — explicit conditions under which the design would solidify (e.g., "promote to P1 when Phase 9 prover handles X reliably and a corpus of N projects exists"). The gates exist so P2 doesn't become indefinitely deferred.
- P1 entries also have a corresponding `[→ P1]` line in [`implementation_checklist/`](implementation_checklist/) for v1 tracking. P2 / P3 entries don't (they're not v1 work) — the deferred.md entry is the tracking surface.

---

## P1 — Decided, Non-Breaking

### Fine-Grained Conditional Compilation (`#[cfg]`)

**Decision:** Defer `#[cfg]` annotations. Platform directories are the v1 mechanism for conditional compilation. `#[cfg]` may be added later if real programs show a need for per-item granularity.

**Why deferred:** Platform directories (e.g., `poller_linux.kara`, `poller_macos.kara`) cover the common case — entire files of platform-specific code. Every case where `#[cfg]` would be needed on a single function or struct field can be handled by extracting the platform-specific part into a function in a platform file and calling it from shared code. Slightly more verbose but keeps conditional logic in one place rather than scattered through the codebase.

**Why non-breaking:** Purely additive. Existing platform directories are unaffected. `#[cfg]` would be a new annotation on items that currently have no annotation.

**Design shape:**

```kara
// If added, Rust-style cfg on individual items
#[cfg(os = "linux")]
fn create_poller() -> Poller { epoll_create() }

#[cfg(os = "macos")]
fn create_poller() -> Poller { kqueue_create() }

// Could also support feature flags and CPU features
#[cfg(target_feature = "avx512")]
fn fast_multiply(a: Vector[f32, 8], b: Vector[f32, 8]) -> Vector[f32, 8] { ... }
```

---

### Production Contract Checking (`#[checked]`)

**Decision:** Defer `#[checked]` from MVP. At MVP, contracts (`requires`/`ensures`/`invariant`) are stripped in release builds; production-time validation uses explicit `if` checks with `Result` returns.

**Why deferred:** `#[checked]` creates a third category between "debug assertion" and "real validation logic." The `Result`-based failure mode is too restrictive (only works on functions returning `Result`). The `panics` failure mode is simpler but adds complexity to the effect system. For v1, the guidance is clear: contracts for development-time verification, explicit validation for production-time checks. If experience shows users want a shorthand for "keep this contract in release," add `#[checked]` later.

**Why non-breaking:** Purely additive. Existing contracts remain stripped in release. `#[checked]` would be a new attribute on contracts that currently have no attribute.

**Design shape:**

```kara
fn transfer(amount: i64, from: Account, to: Account) -> Result[Receipt, Error]
    #[checked] requires amount > 0
    #[checked] requires from.balance >= amount
    ensures(result) match result {
        Ok(r) => r.amount == amount,
        Err(_) => true,
    }
{ ... }
```

`#[checked]` contracts use `panics` semantics — a violated contract panics and adds `panics` to the function's effect set.

---

### Tail-Call Optimization (`#[tailrec]`)

**Decision:** Both verification (compile error if a recursive call is not in tail position) and codegen (LLVM `musttail`) for `#[tailrec]` are deferred to post-MVP. Keeping both halves together avoids a false promise (verifying tail position without emitting `musttail`).

**Why deferred:**

1. Promotes a backend optimization (`musttail`) to a language contract — couples language semantics to LLVM's codegen guarantees.
2. The primary use case is deeply recursive functional patterns; users who need guaranteed non-overflow can hand-write loops for the ~90% case.
3. No concrete use case beyond deep recursion has emerged that can't be solved by iteration.
4. Verification without codegen gives the programmer a false promise — both should ship together.

**Why non-breaking:** Adding `#[tailrec]` later is purely additive — a new attribute on existing function syntax.

**Design shape:**

- `#[tailrec]` on a function is a compile error if any recursive call to that function is not in tail position.
- The compiler emits LLVM `musttail` on each recursive call, guaranteeing loop-equivalent stack usage.
- Functions without `#[tailrec]` receive no TCO guarantee; LLVM may still optimize opportunistically.
- Mutual tail recursion is not covered — direct self-calls only.
- GPU and `embedded` profiles forbid recursion entirely; `#[tailrec]` is not valid in those profiles.

---

### Opt-in Release-Mode Contract Checks

**Decision:** Allow individual contracts to survive release builds via a `#[checked]` annotation, plus a build-level flag for blanket control.

**Why deferred:** Static discharge (contracts provable via refinement types are eliminated at compile time) and debug-mode checking cover the immediate needs. No user demand yet for release-mode contract checks.

**Why non-breaking:** Purely additive. Default behavior (contracts stripped in release) is unchanged.

**Design shape:**

```kara
fn transfer(amount: i64, balance: i64)
    #[checked] requires amount > 0          // survives release builds
    #[checked] requires amount <= balance   // survives release builds
    requires some_expensive_validation(amount) // debug-only (default)
{ ... }
```

Build-level override:
- `karac build --contracts=none` — strip all (current default)
- `karac build --contracts=checked` — keep only `#[checked]` contracts in release
- `karac build --contracts=all` — keep all contracts in release (safety-critical domains)

---

### Hot Reloading

**Decision:** Shared library reloading is the recommended approach. Deferred to Phase 10+; requires stable ABI and state serialization design.

**Why deferred:** Depends on stable compiled output (Phase 7+). The ABI and state serialization design cannot be finalized until the LLVM backend is working and the runtime memory layout is stable.

**Why non-breaking:** Purely additive runtime feature.

---

### Self-Hosting (Phase 12)

**Decision:** The Kāra compiler should eventually be written in Kāra.

**Why deferred:** Requires a mature, working compiler (through Phase 11 — full v1 stdlib, both floor and long-tail) before the language is expressive enough to implement its own compiler. Logically follows all other phases.

**Why non-breaking:** Implementation concern, not a language change.

---

### Additional Compilation Targets (Phase 10)

**Decision:** Extend codegen beyond the initial LLVM target — WASM, GPU (SPIR-V/WGSL), and embedded targets.

**Why deferred:** Each target has unique constraints (no heap for embedded, no recursion for GPU, etc.) that are better addressed after the core compiler pipeline is stable.

**Why non-breaking:** Purely additive backend targets.

---

### Spec-First Programming

**Decision:** Depends on working pre/post conditions (contracts, gradual verification Level 3) before it is meaningful. Deferred to Phase 12+.

**Why non-breaking:** Purely additive tooling/workflow feature.

---

### `#[no_std]` / Bare Metal Support

**Decision:** Current design is bare-metal-compatible. No design changes needed. To add later: (1) `#[no_std]` module attribute, (2) compiler enforces no `allocates(Heap)` effect, no `shared struct`, no Vec/Map/String construction, (3) `ref String` for string literals works on bare metal (static data, no heap). Effect system provides better foundation than Rust's blanket `#![no_std]` — can use fine-grained `#[no_effect(allocates(Heap))]`.

**Why non-breaking:** Purely additive. Existing programs unaffected.

---

### List Comprehensions

**Decision:** Python-inspired syntax sugar: `[expr for item in collection if condition]`. Desugars to iterator chain (`.filter().map().collect()`).

**Why deferred:** Evaluate after Phase 8 iterators exist — iterators alone may be sufficient. Purely a parser desugaring feature.

**Why non-breaking:** New syntax that doesn't conflict with existing expressions.

---

### Generators (`yield`)

**Decision:** Defer generators to post-MVP. `yield` keyword is reserved. Manual `Iterator` implementations cover v1.

**Why deferred:** The deferred items table says "Add generators when manual `Iterator` boilerplate becomes friction." Manual `Iterator` impls work correctly without generators. No v1 feature depends on generators. The design is settled: `yield` is pure iteration, orthogonal to `suspends`.

**Why non-breaking:** `yield` is already a reserved keyword. Purely additive desugaring to `Iterator` implementations.

**Design shape:** See design.md § Deferred Items — generators entry.

---

### Effect Variable Bounds (`with E: no writes(R)`)

**Decision:** Upper-bound constraints on effect variables are deferred to post-MVP. Unbounded `with E` is sufficient for the MVP.

**Why deferred:** Bounds require a more complex checker (effect set subsumption, not just propagation) and add surface area to error messages. The feature is opt-in per function. No existing or planned MVP code requires bounded effect variables.

**Why non-breaking:** Existing `with E` declarations have no bounds — equivalent to `with E: any`. Adding bounds is opt-in.

**Design shape:** See design.md § Effect Variable Bounds for full spec (exclusion bounds `no writes(R)`, inclusion bounds `only reads(AuditLog)`, checked at monomorphization).

---

### Field-Level Effect Granularity for `par struct` (v1.5)

**Decision:** Per-field synthetic effect resources (`writes(Elevator.stops)` vs. `reads(Elevator.floor)`) for `par struct` types are deferred to v1.5. In v1, all `mut` field accesses on a `par struct` attribute to a single `writes(T_resource)` effect for the containing type.

**Why deferred:** The current effect system tracks resources at binding granularity, not field granularity. Extending it to field-level requires a non-trivial rework of how synthetic resources are generated and unified — out of scope for the v1 effect checker.

**Why non-breaking:** The v1 conservative model is always safe; per-field granularity is a precision improvement that reduces unnecessary serialization in `par {}` regions. Existing effect signatures remain valid; no new keyword or syntax is required.

**v1 mitigation:** Split logically independent subsystems into separate `par struct` types, each with its own effect resource. This is the structurally correct long-term design anyway — the v1.5 per-field optimization makes the merged form competitive without requiring the split.

**Design shape:** See design.md § `par struct` — Field-level effect granularity for the full rationale and mitigation pattern.

---

### Complexity Budgets (G43) and Static Stack Depth Analysis

**Decision:** Defer both heap complexity budgets and static stack depth analysis to post-MVP. The heap case is already covered by the effect system (`allocates(Heap)` detection). Stack depth analysis requires transitive call graph computation and is primarily useful for embedded builds.

**Why deferred:** Effect system handles heap allocation tracking. Stack budget analysis is embedded-only. Both depend on profiles (G46) which are a Phase 6-7 concern.

**Why non-breaking:** Purely additive. Opt-in annotations (`#[max_stack(N)]`).

**Design shape:** See design.md § Static Stack Depth Analysis for `#[max_stack(N)]` spec.

---

### REPL (`karac repl`)

**Decision:** Defer the interactive REPL to post-MVP. `karac run` for executing `.kara` files and `karac check` for type-checking are the critical CLI tools.

**Why deferred:** A REPL requires significant additional infrastructure (incremental compilation, state persistence, expression-vs-statement disambiguation) that is orthogonal to the compiler pipeline. No test or example depends on REPL availability.

**Why non-breaking:** Purely additive CLI feature.

---

### `#[must_use]` Enforcement

**Decision:** Defer `#[must_use]` enforcement (warnings on unused values of annotated types or unused function return values) to post-MVP.

**Why deferred:** This is a lint/diagnostic feature, not a semantic one — it produces warnings, not errors. No program behavior changes with or without `#[must_use]`. Purely additive.

**Why non-breaking:** New warning on existing code. `let _ =` suppresses.

---

### `stable` Modifier on Effect Groups

**Decision:** Defer the `stable` annotation on effect groups (`stable effect group Name = ...;`) to post-MVP. Only meaningful for library authors publishing packages with semver guarantees.

**Why deferred:** Until the package manager and registry exist (Phase 7-8), there is no semver boundary to enforce. Effect groups work without the `stable` modifier. Purely additive.

**Why non-breaking:** Existing effect groups are unaffected. New opt-in annotation.

---

### Atomic Operations and Memory Ordering (Phase 10 — partial)

**Decision:** Split. The primitive subset — `Atomic[T]` with `load`/`store` and the full `Ordering` enum — ships in v1 (Phase 11, embedded primitives) to close the ISR-to-main signaling gap for embedded targets. The advanced operations (`swap`, `compare_exchange`, `fetch_add`, `fetch_and`, `fetch_or`) and hardware fences (`fence`/`compiler_fence`) are deferred to Phase 10 (P1) alongside the embedded target work.

**v1 scope:** `Atomic[bool]`, `Atomic[u8]`, `Atomic[u16]`, `Atomic[u32]`, `Atomic[u64]`, `Atomic[usize]` with `new`, `load(ord)`, `store(val, ord)`. Full `Ordering` enum. This covers the canonical ISR pattern: `store(true, Release)` in the ISR, `load(Acquire)` in the main loop.

**Why the split:** The ISR example in the `#[interrupt]` ABI section already uses `Atomic[T]` — deferring all of atomics left embedded v1 with no safe ISR-to-main signaling mechanism. `critical_section.acquire()` (interrupt disable) is a sledgehammer that blocks other ISR priorities. `load`/`store` map to single LLVM intrinsics and are trivial to implement; the advanced RMW operations require more careful memory model reasoning and can wait.

**Why non-breaking:** Purely additive. Advanced operations add new methods to the existing `Atomic[T]` type.

**Design shape:**

Kāra provides `Atomic[T]` as a `shared struct` (reference semantics, RC-backed) for lock-free inter-context communication — primarily ISR-to-main signaling in embedded programs and spinlocks/barriers in kernel code.

`T` must be `Copy` (all atomic types are integer, bool, or pointer — this is always satisfied). `Atomic[T]` is a `shared struct`, so multiple contexts hold shared references to the same instance without exclusive ownership.

```kara
let flag: Atomic[bool] = Atomic.new(false);

// In interrupt handler:
flag.store(true, Release);

// In main loop:
while not flag.load(Acquire) { /* spin */ }
```

#### Methods

| Method | Signature |
|---|---|
| `new` | `fn new(val: T) -> Atomic[T]` |
| `load` | `fn load(ord: Ordering) -> T` |
| `store` | `fn store(val: T, ord: Ordering)` |
| `swap` | `fn swap(val: T, ord: Ordering) -> T` |
| `compare_exchange` | `fn compare_exchange(old: T, new: T, success: Ordering, failure: Ordering) -> Result[T, T]` |
| `fetch_add` | `fn fetch_add(val: T, ord: Ordering) -> T` (numeric `T` only) |
| `fetch_and` | `fn fetch_and(val: T, ord: Ordering) -> T` (integer `T` only) |
| `fetch_or` | `fn fetch_or(val: T, ord: Ordering) -> T` (integer `T` only) |

`compare_exchange` takes `old` and `new` by value; both are `Copy`, so no ownership issue arises.

#### Memory Ordering

```kara
enum Ordering {
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}
```

Semantics match the C11/LLVM memory model exactly.

#### Fences

```kara
// Safe — compiler-only reordering barrier; no hardware instruction emitted
compiler_fence(Ordering);

// Unsafe — emits a hardware barrier instruction (dmb, mfence, etc.)
unsafe { fence(Ordering); }
```

`fence` is `unsafe` because a misplaced hardware barrier can cause incorrect behavior in concurrent/interrupt-driven code, not just a performance regression. `compiler_fence` is safe — it only constrains compiler instruction scheduling.

```kara
// Memory fence for DMA completion:
unsafe {
    dma_buffer[0] = payload;
    fence(Release);         // all writes visible before DMA sees the buffer
    dma_start_reg.write(1); // trigger DMA
}
```

#### Effect Model

`Atomic[T]` operations themselves are **effect-free synchronization primitives**. `load`, `store`, `swap`, `compare_exchange`, `fetch_add`, `fetch_and`, and `fetch_or` contribute nothing to a function's inferred or declared effect set. They are memory-ordering primitives at the codegen layer, not resource accesses at the language layer.

A function's effect set comes from ordinary reads and writes on the resources the atomic is *synchronizing access to*, not from the atomic operations themselves. In the canonical signal-flag pattern, the flag synchronizes *when* it is safe to touch the data; the code that actually touches the data is what produces `reads(SensorData)` or `writes(SensorData)`:

```kara
// Flag guards access to SensorData; sensor_buf holds the actual data.
fn signal_ready(flag: ref Atomic[bool], reading: SensorReading)
    with writes(SensorData)
{
    sensor_buf.write(reading);   // produces writes(SensorData)
    flag.store(true, Release);   // synchronization only — no effect contribution
}

fn wait_and_read(flag: ref Atomic[bool]) -> SensorReading
    with reads(SensorData)
{
    while not flag.load(Acquire) {}  // no effect — pure sync
    sensor_buf.read()             // produces reads(SensorData)
}
```

**Why effect-free.** Atomic operations are linearizable by construction — any interleaving of concurrent atomic ops on the same atomic is a valid execution. Tracking atomic ops as `writes(...)` would force auto-concurrency to serialize them, discarding the entire point of lock-free concurrent access. Tracking them as `reads(...)` would misrepresent `fetch_add`/`store` as pure. The only consistent choice is to leave atomics outside the effect system's resource-access model: hardware atomicity ensures memory safety at the language level, and the programmer's ordering reasoning happens one layer lower via the memory-ordering arguments.

**Effect-free is not conflict-free.** If two private functions both update the same `Counter` via `fetch_add`, both infer empty effect sets and auto-concurrency is free to run them in parallel. This is *correct* — `fetch_add` is linearizable, so any interleaving produces a well-defined result (one of the valid orderings). Programmers who need a specific ordering between two atomic updates must use memory orderings (`Release` / `Acquire` happens-before) or a higher-level synchronization mechanism (`Mutex[T]` with `lock` blocks, channels), neither of which is expressed as an effect-system atom.

**Memory orderings are not effects.** `Ordering.Relaxed`, `Acquire`, `Release`, `AcqRel`, and `SeqCst` are codegen attributes that constrain hardware memory barriers, not effect-system atoms. They do not appear in `karac explain` effect-inference output, do not participate in effect subsumption, and are not rejected by an effect-boundary check. A future `karac explain --memory-model` view may surface them for reasoning about data-race freedom; that is deferred to Phase 5+ (P0 — tool feature, not part of the effect verb vocabulary).

**RC integration.** An `Atomic[T]` field inside a `shared struct` is covered by the struct's Phase 2 `Rc → Arc` promotion decision uniformly — when the enclosing struct is promoted, all of its fields (atomic or otherwise) move with it. Atomic operations work identically under both tiers because the atomicity guarantee is a property of the hardware instructions, not the RC wrapper. There is no separate "atomic field promotion" analysis; the existing Phase 2 algorithm in Feature 4 Part 4 handles it. A `shared struct` that uses *only* `Atomic[T]` fields for cross-task mutation does not require `Mutex[T]` — the `Sync`-forbidden rule (which mandates `Mutex[T]` for shared mutable field access) applies to non-atomic `mut` fields only. Atomic fields are themselves the synchronization.

**`lock` blocks and effect inference.** The `lock node { ... }` syntax in Part 5: Shared Types is an **effect-transparent** code-generation construct: effects inside the lock block contribute to the enclosing function's inferred or declared effect set exactly as if they were written outside the block. `lock` does not scope, mask, or rename effects. Its role is purely runtime synchronization — the effect system and the mutex layer are complementary, with effects providing coarse compile-time serialization (per-resource) and `Mutex[T]` providing precise runtime serialization for intra-resource granularity that effects alone cannot express. Auto-concurrency sees the body's effects and serializes accordingly; the `Mutex` exists for fine-grained partitioning *within* what the effect system sees as a single resource.

**Summary.** For any code that uses atomics or locks, the effect system tracks *what* is touched (via the resources reached by reads/writes to non-atomic state) and the mutex/atomic layer tracks *when* it is safe to touch it. Programmers who want "which resources does this function access" read the function's declared or inferred effect set. Programmers who want "in what order will these operations observe each other" read the memory-ordering arguments and the `Mutex[T]` structure. The two layers do not overlap, and the language commits to never collapsing them into a single notion.

#### Embedded Profile

Atomics and fences are available in the `embedded` profile. The `embedded` profile bans scheduler-level concurrency (`spawn`, task queues, channels requiring a runtime). ISR-to-main communication via `Atomic[T]` is the *correct* embedded pattern and is explicitly permitted. Single-core multi-context (ISR + main) is not "concurrency" in the scheduler sense.

---

### `Send + Sync` Enforcement on `with_provider` Concrete Provider Type — Superseded by Structural Cross-Task-Safe Set (item 48)

**Status (as of 2026-05-02):** Superseded. The original entry deferred enforcement until "auto-trait infrastructure ships with the concurrency work that introduces `spawn`." Per v60 item 48 (decided 2026-05-02 alongside item 61's "no auto-traits" stance), Kāra does **not** ship auto-trait infrastructure — instead, cross-task safety is enforced via the **structural cross-task-safe set** specced at `design.md § Structured Concurrency Lifetime Guarantees`. The v1 enforcement mechanism is therefore the same one that ships when `spawn` lands in Phase 6; there is no separate "auto-trait phase" to wait on.

**Updated forcing function.** When Phase 6 lands, the `with_provider` typechecker check becomes a direct application of the cross-task-safe-set predicate to the concrete provider type. The diagnostic is `E_NOT_CROSS_TASK` with the type-path-through-tree shape (e.g., `Provider → field 'cache' → Rc[Cache]`) and the type-swap fix-it (`Rc[Cache]` → `Arc[Cache]`).

**Updated design shape.** The signature is no longer `... P: R.Provider + Send + Sync ...` — instead it is `... P: R.Provider ...` and the cross-task-safety check runs on the concrete `P` at every call site, exactly as on every other spawn-boundary crossing. No trait-bound is named in the signature; the structural rule fires at the call site against the resolved concrete type.

**Why non-breaking:** Programs that satisfy the documented contract today (don't pass `shared struct` / `Rc[T]` / `OnceCell[T]` / raw pointers as providers) remain valid when the check turns on. No surface syntax changes; the original signature with explicit `Send + Sync` bounds was a forward-commitment placeholder, and replacing it with the structural rule is a renaming-only adjustment.

**Why this entry stays in deferred.md** (rather than being removed): historical context for readers who encounter the original `Send + Sync` wording in older commits or reviewers' mental models. The deferred-then-superseded note documents the design evolution explicitly.

---

### `Concurrent` Auto-Trait

**Phase:** 10 (P1). Ships alongside or after `par struct` / `par enum` are in use at scale.

**Decision:** Introduce a `Concurrent` auto-trait that propagates concurrent-safety structurally through the type graph, enabling generic bounds of the form `[T: Concurrent]`. Deferred until `par struct` has real usage — the motivating patterns (generic parallel algorithms over concurrent-safe types) only become necessary once the single-type API (`par struct` explicitly) becomes limiting.

**What it enables.** Today, generic code over concurrent-safe types must take `par struct` explicitly (e.g., a `broadcast` function takes a concrete `par struct Counter` rather than a generic `T`). This is correct and sufficient for the common case — the explicit type is honest about intent. The `Concurrent` bound unlocks the rare case: a library function that needs to be generic over any concurrent-safe type.

```kara
fn broadcast[T: Concurrent](value: T, n_tasks: i64) {
    par { for _ in 0..n_tasks { process(value.clone()) } }
}
```

**Derivation rules:**
- `par struct` and `par enum` types are `Concurrent` by definition.
- A plain `struct` whose every field is `Concurrent` derives `Concurrent` structurally (similar to Rust's `Send`/`Sync` auto-trait propagation, but simpler because the starting point is a declared keyword rather than fully inferred field types).
- `shared struct` and `shared enum` are explicitly **not** `Concurrent` — they are RC, single-task only.

**Why simpler than Rust's `Send`/`Sync`:** Rust's auto-traits must infer concurrency-safety from raw field types because there is no declaration-site marker — `Arc<T>` is a generic wrapper, and any `T` can go inside. In Kāra, `par struct` is a definition-site keyword: the intent is declared rather than inferred. The auto-trait derivation starts from an explicit commitment and propagates structurally, rather than reverse-engineering the commitment from field types alone.

**Why deferred:** The three concrete rules in `design.md § Part 5b` (`par struct` crosses boundaries freely; plain owned moves into one task need no bound; `shared struct` cannot cross) handle the overwhelming majority of real parallel code without any generic bound machinery. Distribute-style parallel algorithms (the common case — each task gets a different allocation) need no bound at all. Broadcast-style (same allocation, N tasks) can take `par struct` explicitly for v1. The generic escape hatch is real but rare in Kāra's concurrency model.

**Forcing function:** When library authors begin writing generic concurrent utilities and the explicit-`par-struct` API becomes genuinely limiting. The anchor is clean: `par struct` definition site is exactly where structural auto-trait derivation starts, and the `Send + Sync` auto-trait infrastructure (above) lands in the same concurrency work phase.

**Cross-reference:** `design.md § Deferred Items` table (Phase 10 row); `design.md § Part 5b` (par struct specification); `Send + Sync` enforcement on `with_provider` (above) — same auto-trait infrastructure phase.

---

### RC Flavor User Control (`#[prefer_rc]` / `#[rc_only]`)

**Decision (updated 2026-05-08):** `#[prefer_rc]` is the **resolution surface for compiler queries channel P1.1** (RC fallback at use site) — graduated from a standalone deferred item into the queries channel catalogue per brainstorm v63. `#[rc_only]` remains deferred as a separate sharper-than-`#[prefer_rc]` assertion form.

**Context.** The compiler's RC fallback (Feature 4 Part 4) inserts tentative `Rc` at re-use-after-consume / closure-capture / container-store sites, then Phase 2 promotes to `Arc` wherever a live range crosses a parallel region (spawn, par block, task boundary). The conservative default is `Arc` when the promotion decision is ambiguous (`design.md:5271`). Users can inspect the chosen representation via `karac explain` but cannot currently direct it.

**What v51 resolved.** The "I want `Arc` because this value is concurrent" case is now handled by declaring `par struct` / `par enum` — the type's concurrent intent is stated at definition, not via attribute. The remaining open question is the `Rc`-preference direction: "I know this `shared struct` is single-threaded; keep it `Rc` to avoid atomic overhead."

**What v63 resolved.** The "should I add a knob for this?" question is resolved into "this is exactly what the compiler queries channel is for." Each `RcFallbackNote` decision site emits a structured query enumerating `{ keep_rc | prefer_rc | no_rc }` with the compiler's rationale per option; `#[prefer_rc]` is the resolution annotation that pins the answer. The forcing function shifts from "real production profiles" to "P1.1 of the queries channel landing in [Phase 8 stdlib floor](implementation_checklist/phase-8-stdlib-floor.md)" — the channel infrastructure (P0) ships with v1, P1.1 is the first user-visible query, `#[prefer_rc]` ships as its resolution surface.

**Intended design shape.** Two affordances, not one:
- `#[prefer_rc]` on a function or module — a *hint* that the Phase 2 pass respects in ambiguous cases but overrides when it has proof Arc is required. Fails safe. **Ships with P1.1 of the queries channel.**
- `#[rc_only]` on a function or module — a *safety-checked assertion*: the compiler errors if it determines `Arc` is actually required. Same shape as `#[no_rc]` but one step milder ("cheap RC or error" rather than "no RC at all"). Conflict with `#[no_rc]` is a compile error. **Remains deferred** — it is not a query resolution surface (queries hint, do not assert), so it sits outside the channel and waits for separate motivation.

Granularity: per-function and per-module. Per-binding is too granular (RC sites are compiler-inserted, not named in source). Per-type is too coarse (would change behavior wherever the type is instantiated, not just in the hot-path function). A project-wide default (e.g., `prefer_rc = true` in the manifest) is plausible for embedded / kernel profiles where atomics are never wanted.

**Diagnostic improvement (non-deferred).** The RC-insertion note (already emitted per `design.md:5175`) should name the chosen flavor: *"note: inserted `Rc[T]` at line 37 (not promoted; value does not cross a parallel region)"* or *"note: promoted to `Arc[T]` at line 37 (crosses `spawn` at line 42)"*. This is independent of the knob question and is a small one-line diagnostic change with high visibility payoff.

**Cross-reference:** `design.md § Compiler Queries` (the channel that resolves `#[prefer_rc]`); `design.md § Feature 4 Part 4` (RC insertion and Phase 2 algorithm); `design.md:5271` (conservative Arc default); `design.md:124` (`karac explain` representation field); `Concurrent` auto-trait (above).

---

### `TreeSet[T]` — Sorted Set Collection

**Decision:** Add `TreeSet[T]` as the sorted counterpart to `Set[T]`, mirroring the `TreeMap[K, V]` / `Map[K, V]` relationship. Deferred to post-MVP.

**Why deferred:** `Set[T]` covers the common case. `TreeMap` is in v1 because sorted key-value lookup is a distinct use case from sorted unique values. `TreeSet` is straightforward to add once `TreeMap` is stable — it's essentially `TreeMap[T, ()]` with a set-oriented API.

**Why non-breaking:** Purely additive. New collection type.

**Design shape:** API mirrors `Set[T]` (insert, remove, contains, len, is_empty, union, intersection, difference) with `T: Ord` bound instead of `T: Hash + Eq`. Iteration yields elements in sorted order.

---

### Panic Recovery (`catch_panic`) and `process.exit()` Interaction

**Status:** Resolved 2026-05-02 (v60 item 26). `catch_panic[T]` is specced at design.md § Catching Panics; the `process.exit()` interaction is locked in per the original decision below.

**Decision:** `process.exit()` propagates through `catch_panic` unconditionally. The runtime tags the in-flight unwind as an exit rather than a recoverable panic; the catch frame inspects the tag and re-raises rather than producing an `Err(PanicInfo)`. Implementation lands with the unwinding substrate in Phase 7 (LLVM backend).

**Alternative if semantic clarity becomes important:** `exits` as a separate built-in effect resource (alongside `panics`) is a valid non-breaking addition. Private functions get it inferred automatically; public functions gain `with exits` in their signature, which is additive (Kāra's semver rules treat adding an effect as a minor change, and `exits` requires no provider injection so callers need no changes). If the distinction between "unexpected termination" (`panics`) and "intentional exit" (`exits`) proves useful in practice — e.g., for linting, for tooling that wants to find all exit points, or if `catch_panic` recovery makes the two effects mechanically different — carve `exits` out then. For v1, `panics` covers both.

---

### Shape-Parameter Arithmetic (`[A + B]`, `[N * 2]`)

**Decision:** Arithmetic over shape parameters — concat (`[A + B]`), reshape-by-factor (`[N * 2]`), split-along-dim — is deferred to v1.5. Requires a type-level const-evaluator.

**Why deferred:** Shape unification (same parameter appearing in multiple positions) ships in v1 and covers the common tensor-ops cases. Arithmetic requires const-evaluation infrastructure that is better designed once comptime lands. Until then, the affected operations (concat, reshape-by-factor, split-along-dim) return partially-dynamic shapes.

**Why non-breaking:** Purely additive. Existing shape parameters remain unchanged; arithmetic extends the grammar in type-parameter position.

**Design shape:** See design.md § Tensor Shapes — shape-param arithmetic inline note.

---

### Trait Aliases — Expansion (`trait Numeric = Copy + Add + Sub + ...;`)

**Decision:** Trait alias *grammar* lands at v1; *expansion* lands at P1. The parser accepts `trait NAME [GENERIC_PARAMS] = TRAIT_BOUND { + TRAIT_BOUND } [WHERE_CLAUSE];` and stores the AST. At v1, encountering a use of a declared alias produces `error[E_TRAIT_ALIAS_NOT_IMPLEMENTED_YET]: trait alias 'NAME' is recognized but not yet expanded; the implementation lands in P1 — write the bound list explicitly for now`. The diagnostic includes the alias's source span so users can copy the bound list verbatim into the use site as a workaround.

**Why deferred (expansion only — grammar lands at v1):** Substituting an alias's bound list at every use site, computing transitive flattened bound sets across nested aliases, propagating generic arguments, enforcing the alias's `where` clause, rejecting impl-of-alias, and detecting cycles is *implementation* work that touches the trait resolver, the bound-checking machinery, and the diagnostic surface. Adopting the syntax now reserves the form so the post-v1 expansion lands without parser-grammar churn or source breaks; the v1 stub diagnostic gives users a copy-paste fix-it without committing the resolver to the larger change.

**Why non-breaking:** Purely additive. The grammar lands at v1; the v1 use-site stub diagnostic is replaceable by the real expansion when P1 ships. Code that uses the v1 workaround (writing the bound list explicitly) keeps compiling under P1; the alias declaration itself becomes load-bearing rather than ignored.

**Design shape:** See design.md § Trait Aliases for the full surface — declaration form, use-site forms (bound, `where`-clause predicate, `dyn` type), composition, generics, visibility, `where`-clause-on-alias, and the "cannot be implemented" rule. The P1 implementation slice covers:

1. **Resolver — alias-reference recording.** When the resolver hits a trait reference at a use site, it consults the trait registry. If the resolved name is a trait alias, the resolver emits an `AliasReference { alias: AliasId, generic_args: Vec<TypeArg> }` placeholder rather than a concrete `TraitRef`.
2. **Bound expansion.** A new pass between resolution and bound-checking walks every `AliasReference`, looks up the alias's stored bound list, substitutes the alias's generic parameters with the call-site's arguments, and produces a `Vec<TraitRef>` (the flattened expansion). Nested aliases recurse; the SCC check from below catches cycles before the recursion explodes.
3. **Cycle detection.** At alias-registration time (just after resolver), the compiler builds a directed graph of "alias `A` mentions alias `B`" edges and runs Tarjan's SCC. Any non-trivial SCC is `error[E_TRAIT_ALIAS_CYCLE]` listing the cycle path. Self-edges (a trait alias whose body mentions itself) are the same error.
4. **Where-clause-on-alias propagation.** When the alias body carries a `where` clause, the alias's expansion at a use site is gated on the `where` clause's predicates. Concretely: the use site `[U: OrderedFloat[i64]]` first verifies `i64: Numeric + Bounded` (the alias's `where`) and then enforces `U: Ord` (the body). Failure on the `where` clause produces a focused diagnostic that names the alias *and* the failing predicate, distinguishing it from ordinary bound-resolution failures.
5. **Impl-rejection.** When the typechecker encounters an `impl AliasName for T { ... }`, it produces `error[E_IMPL_TRAIT_ALIAS]: cannot implement trait alias 'AliasName'; implement each component trait separately`. The diagnostic enumerates the alias's expansion (e.g., `Copy`, `Add`, `Sub`, `Mul`, `Div`) so the programmer sees exactly which impls are required.
6. **`dyn` handling.** `dyn AliasName` is accepted as a `dyn` trait object iff the alias's expansion contains exactly one trait that produces a vtable (auto traits like `Send`, `Sync`, marker traits with no methods are vtable-free; one method-bearing trait is the canonical case). `dyn AliasName` where the expansion has zero or two-plus method-bearing traits is rejected with `error[E_DYN_REQUIRES_SINGLE_METHOD_TRAIT]` listing the alias's expansion. (This rule mirrors Rust's `dyn` object-safety constraints; the alias does not loosen them.)
7. **`where`-clause use-site.** `where T: AliasName` desugars to the alias's expanded bound list. Generic arguments propagate normally.
8. **Effect-bound rejection.** Trait alias bodies cannot list effect predicates (`trait Foo = Iterator + writes(Db);` is rejected with `error[E_EFFECT_IN_TRAIT_ALIAS]: effect predicates do not belong in a trait alias body; use an effect group declaration`). Effect groups (§ Effect Groups and Composition) are the parallel mechanism for naming effect-set unions.
9. **Diagnostic shape — surfacing the alias name in errors.** When a use-site bound fails because of the *alias's* expansion (not the underlying trait directly), the diagnostic should name the alias *and* the offending component: `the trait \`Add\` is not implemented for \`T\` (required by the trait alias \`Numeric\` at <span>)`. The alias declaration's source span is reachable through the typechecker's symbol table.
10. **Re-exports across packages.** A `pub trait Foo = ...;` is re-exportable through `pub import` exactly like a regular trait. The alias's expansion is computed at the consumer's compile time using the consumer's view of every component trait — there is no compile-time materialisation that crosses package boundaries (the alias is fully expanded at each use site).

Test coverage (P1 implementation slice): grammar (already in v1, but the P1 slice adds use-site tests); positive expansion at every position (bound, `where`-clause, `dyn`); nested alias chains expand correctly with generic-arg propagation; impl-of-alias rejection with enumeration of components; cycle detection on direct and indirect cycles; `where`-clause-on-alias gating; effect-bound rejection diagnostic; `dyn` accepted on single-method-trait expansions, rejected on zero / multi-method-trait expansions; cross-package re-exports compile and resolve correctly. Phase target: P1 — slated for the post-v1 trait-resolver-extension work.

**Why non-breaking:** Purely additive (P1 column). The grammar already lands at v1; users who write trait aliases get the v1 stub diagnostic until P1 ships; the stub diagnostic's recommended workaround (write the bound list explicitly) keeps working under P1.

---

### Type Alias `impl Trait` (TAIT) — Witness Inference and Opaque Surface

**Decision:** TAIT *declaration grammar* (`type X = impl Trait;`) lands at v1; the *witness-inference and opaque-surface machinery* that makes a TAIT semantically distinct from a `dyn Trait` lands at P1. At v1, a TAIT use site is treated identically to a `dyn Trait` of the same trait bound for typechecking purposes — calls go through the declared trait surface, the witness type is not inferred from defining-use bodies, no same-concrete-type enforcement across defining functions, no opaque cross-package surface. At v1, encountering a TAIT use site that depends on the witness type (e.g., calling a method that the trait does not declare but the witness type does) produces `error[E_TAIT_NOT_IMPLEMENTED_YET]: TAIT 'NAME' is recognized but the witness-inference pipeline lands in P1; cast through the trait surface for now`.

**Why deferred (semantics only — declaration grammar lands at v1):** The full TAIT machinery requires four interlocking pieces that do not need to ship at v1:

1. **Defining-use inference.** Walk every function in the defining package whose return type names the TAIT, infer the concrete return type from the body, and pin it as the alias's witness. Multiple defining-use sites must produce the same witness — otherwise `error[E_TAIT_CONCRETE_MISMATCH]` naming both sites and both witnesses.
2. **Same-concrete-type enforcement.** Run after typecheck of every function in the defining package; aggregate the candidate witnesses and reject any disagreement. The check is package-boundary-relative because the witness is a package-private fact.
3. **Opaque cross-package surface.** Cross-package consumers see the trait bound, never the witness — even though the witness is computable inside the defining package. The compiler's symbol export marks TAIT names as opaque to downstream consumers; the downstream consumer's type-checker treats the alias as if it were `impl Trait` (an existential, not a name for a concrete type).
4. **Generic TAITs** (`type Iter[T] = impl Iterator[Item = T];`). The witness is parametric over the alias's type parameters; the `Iter[i32]` and `Iter[String]` instantiations have independent witnesses; same-concrete-type enforcement is per-instantiation.

The four pieces are tightly coupled — shipping any subset alone produces either a hole in the encapsulation guarantee (cross-package consumers seeing the witness) or a hole in soundness (witness inference without the same-concrete-type check could allow inconsistent dispatch). v1 ships the declaration grammar so users can write `type X = impl Trait` against the eventual surface today; P1 lands all four pieces together.

**Why non-breaking:** Purely additive (P1 column). The declaration grammar already lands at v1; v1 use sites that depend on witness-type behavior produce the named stub diagnostic with a workaround pointer; the v1 workaround (cast through the trait surface, or use `dyn Trait` instead of TAIT) keeps compiling under P1.

**Design shape:** Full surface in design.md § `impl Trait` (Existential Types) > Type alias `impl Trait` (TAIT). The P1 implementation slice covers:

1. **Resolver — TAIT-reference recording.** Every type-position reference to a TAIT name is recorded as `TaitReference { tait: TaitId, generic_args: Vec<TypeArg> }`. The resolver does not yet substitute the witness — it just marks the reference for the witness-inference pass.
2. **Witness-inference pass.** A new typechecker pass (between ordinary typechecking and effect checking) walks every function in the defining package whose return type contains a `TaitReference`. For each defining-use site, infer the return-expression's concrete type. Aggregate the candidate witnesses per `(TaitId, generic_args)`; if all agree, pin the witness; if not, emit `E_TAIT_CONCRETE_MISMATCH` naming both sites and both witnesses. The witness is stored in the package's TAIT-witness table.
3. **Use-site resolution.** Inside the defining package, after the witness-inference pass has run, every TAIT reference is resolved to its witness type for the purpose of inherent-method resolution (the witness's methods are reachable through the alias name *only* when the use site is in the same package). Method calls on a TAIT value through methods *not* on the trait require resolution through the witness. Use sites in other packages always go through the trait surface.
4. **Cross-package opacity.** When the compiler's symbol export pass writes the package's metadata, TAIT names export their *trait bound* (and any generic params), not their witness. A consumer's resolver and typechecker reading this metadata see the alias as a fresh `impl Trait` existential. No witness leakage.
5. **Generic TAIT instantiation.** `type Iter[T] = impl Iterator[Item = T];` — the witness inference runs per `(TaitId, generic_args)` tuple; `Iter[i32]` and `Iter[String]` are independent witnesses. The same-concrete-type check is per-instantiation.
6. **Capture-set rule.** The TAIT's capture set is determined by the type alias's own type parameters. There is no implicit capture of the *defining function's* generic parameters or borrow regions — those are not in scope at the alias's declaration site. The capture rule is the same as the v1 stub's behavior; only the witness-inference machinery is new at P1.
7. **Diagnostic shape — surfacing the alias name in errors.** When a use site fails because the inferred witness does not satisfy a bound the use site requires (e.g., the witness lacks a method the consumer is calling), the diagnostic should name the alias *and* the inferred witness: `the inferred witness type \`SomeIter[i32]\` for TAIT \`Iter\` does not implement \`ExactSizeIterator\`` etc. The alias declaration's source span is reachable through the typechecker's symbol table.
8. **Re-exports.** A `pub type Foo = impl Trait;` re-exported via `pub import` exposes only the trait bound to the re-exporter's downstream consumers. The witness remains private to the original defining package.

Test coverage (P1 implementation slice): grammar (already in v1, but P1 adds witness-inference tests); positive — two defining-use sites returning the same concrete type compile cleanly with the alias; method calls *through the trait* work in any package; method calls *through the witness* work only in the defining package. Negative — two defining-use sites returning different concrete types produce `E_TAIT_CONCRETE_MISMATCH`; cross-package consumer trying to use a non-trait method on a TAIT value gets the trait-surface-only diagnostic; generic TAIT with two instantiations producing inconsistent witnesses (each instantiation independently checked); re-exported TAIT remains opaque to the second-hop consumer. Phase target: P1 — slated for the post-v1 type-system extension work, scheduled alongside the trait-alias expansion entry above.

**Cross-reference.** This entry is the TAIT-specific complement to the broader `impl Trait` implementation entry in implementation_checklist/ (which covers argument-position, return-position, and RPITIT at v1). Until P1, TAIT use sites that depend on the witness type produce `E_TAIT_NOT_IMPLEMENTED_YET`; argument-position / return-position / RPITIT continue to ship at v1 unchanged.

---

### Try Blocks — `?`-Retargeting and Error-Type Unification

**Decision:** Try-block *grammar* (`try { ... }`) lands at v1; the *typechecker pipeline* that gives the construct its semantics — `?`-retargeting from the function to the block, error-type unification across the block's `?` sites with From-chain coercion, type inference of the block's `T` and `E`, integration with `defer`/`errdefer` scoping — lands at P1. At v1, every `try { ... }` use site produces `error[E_TRY_BLOCK_NOT_IMPLEMENTED_YET]: try block syntax is recognized but the typechecker pipeline lands in P1 — extract the body into a helper function returning Result for now`. The diagnostic includes a span pointing at the block and a help line naming the workaround (extract a helper function).

**Why deferred (semantics only — grammar lands at v1):** The typechecker work touches three machineries that interact in non-trivial ways: (a) the `?`-target stack, which currently has only one frame (the enclosing function's return type); try blocks add per-block frames that nest. (b) The error-type unification pass that runs at function-return time needs a per-block variant. (c) The From-chain coercion that already runs at `?` sites needs a per-block error-type target rather than the function's return type. Each piece is small individually; the integration testing surface is large enough that shipping at v1 is unmotivated when the workaround (extract a helper function returning `Result[T, E]`) is mechanical.

**Why non-breaking:** Purely additive (P1 column). The grammar lands at v1 (the parser accepts `try { ... }`, the AST captures it); use sites at v1 produce the named stub diagnostic with a workaround pointer. The v1 workaround (extract a helper function) keeps compiling under P1; users who write the workaround today can later replace the helper with an inline `try` block at any time without changing semantics.

**Design shape:** Full surface in design.md § Error Handling > Try Blocks (`try { ... }`). The P1 implementation slice covers:

1. **`?`-target stack.** Currently the typechecker's `?`-resolution looks up the enclosing function's return type. Add a stack of `try`-block return targets; `?` resolves to the innermost frame. The function-return frame is the bottom of the stack and remains the fallback.
2. **Per-block error-type unification.** Each `try` block has its own `E` metavariable, unified across all `?` sites inside the block (using the same algorithm the typechecker already uses for the function-level case). The From-chain coercion machinery from § Error Handling > **`?` desugaring and effect tracking** applies inside try blocks the same way it applies at the function boundary; the From conversion's effects flow to the enclosing function's inferred set, not to the block's value.
3. **Block-level `T`/`E` inference.** The block's `T` is the type of its tail expression (or `()` for an empty / no-tail block); the block's `E` is the unified error type. Both flow into the enclosing context's type inference normally — an annotated binding `let r: Result[Foo, MyError] = try { ... };` constrains `T = Foo` and `E = MyError`; an unannotated binding solves both via downstream uses.
4. **Empty try block.** `try { }` has type `Result[(), E]` where `E` is a metavariable that downstream context must solve; if no context is available, the existing "cannot infer type" diagnostic fires.
5. **Diverging tail.** A `try` block whose tail diverges (`panic!`, `return`, `loop { }` with no break) has type `Result[Never, E]` — the block diverges, but its type is still well-formed because `Never` coerces to any `T`. This is consistent with the LUB rule (`Never` is the bottom).
6. **`defer` / `errdefer` integration.** A `try` block introduces its own cleanup scope, exactly as ordinary blocks do. `defer` declared inside a `try` fires on the block's exit (whether tail or `?`-short-circuit). `errdefer` fires on the `Err` exit path only. The function-level `defer`/`errdefer` chain is unaffected — try blocks nest cleanly with the existing scope rules.
7. **Effect interaction.** `try { ... }` itself contributes no effects to the enclosing function — it is a control-flow construct, not an operation. Effects from the block's body (operations, From conversions at `?` sites) flow to the enclosing function's inferred set as they would in any block.
8. **Diagnostic shape — `?`-target ambiguity.** When a `?` site's error-type does not unify against the enclosing `try` block's `E` (and no `From` conversion exists), the diagnostic must name *which* return target the `?` is resolving to (the innermost `try` block, or the enclosing function) so users can see whether they need to fix the `From` impl, fix the block's expected error type, or restructure the nesting. The existing "no `From` impl" diagnostic gets a context line: `(propagating to the try block at <span>)`.
9. **Closure-boundary rule.** A `?` inside a closure body never targets a `try` block in the *enclosing* lexical scope — closures are a control-flow boundary the same way they are for `break label` (per § Loops > Labeled blocks closure-boundary rule). A `?` inside a closure resolves to the closure's own return type, which must itself be `Result[_, _]` or `Option[_]`.

Test coverage (P1 implementation slice): grammar (already in v1, but P1 adds use-site tests); positive — `try { lex(s)? }` evaluates to `Ok(...)` on success; the same with `Err` on a failing `?`; nested try blocks short-circuit to the innermost one; From-chain conversion across try-block `?` sites works the same as across function `?` sites; empty try block infers correctly given binding context; `defer` inside a `try` block fires on the block's exit; `errdefer` inside a `try` block fires on `Err` exit only; tail expression of `Never` type still produces a well-formed `Result[Never, E]`. Negative — `?` inside a closure body inside a `try` block does not target the outer `try` (closure-boundary rule); `?` site whose error type does not unify with the block's `E` produces the diagnostic with the named try-block context line; an unannotated empty try block in a context without downstream constraints produces the standard "cannot infer type" diagnostic.

Phase target: P1 — slated for the post-v1 typechecker-extension work, scheduled alongside the trait-alias expansion (item 40) and TAIT witness-inference (item 41) entries above. The three P1 entries form a coherent batch; all three reserve grammar at v1 and ship semantics at P1.

---

### Workspace Scaffolding (`karac init --workspace`)

**Decision:** Defer workspace-aware scaffolding to the package-manager CR. For v1, `karac init` only scaffolds single-package projects. Users who want a workspace hand-write the root `kara.toml` with `[workspace] members = [...]` and then run `karac init <name>` inside each member subdirectory.

**Why deferred:** The `[workspace]` manifest key parses silently in v1 (per `design.md § Package System`) but has no runtime behavior — the resolver and builder do not yet honor it. Scaffolding a workspace root that `karac build` wouldn't multi-build would teach users the wrong mental model. Workspace scaffolding belongs to the same deliverable that implements the workspace resolver and multi-package build, not to `karac init`.

**Why non-breaking:** Purely additive. Adding `--workspace` to `karac init` at a later date is a new flag on an existing subcommand; v1-scaffolded projects remain valid workspace members once added.

**Design shape:**

```bash
karac init --workspace myrepo           # writes myrepo/kara.toml with [workspace] members = []
cd myrepo
karac init mylib --lib                  # auto-adds "mylib" to root's members array
karac init mycli                        # auto-adds "mycli" to root's members array
```

Key behaviors to settle during the package-manager CR:
- Detecting that the CWD (or an ancestor) is a workspace root and auto-registering new members.
- Whether `--workspace` on an already-initialized project is an error or a promotion (converting a single-package `kara.toml` into a root).
- Interaction with `--force` when a root already exists.

---

### `karac test --coverage` (LLVM-instrumented coverage)

**Decision:** Defer LLVM-backed coverage support for `karac test` — originally CR-24 follow-up slice 4 — to a standalone P1 CR. `karac test` continues to ship interpreter-only; the `--coverage` flag is not accepted and produces an unknown-flag error. The JSONL schema does not yet reserve `coverage` or `coverage_delta` events.

**Why deferred:** the spec requires LLVM instrumentation (`-fprofile-instr-generate`, `-fcoverage-mapping` equivalents) on the codegen path, but `karac test` today routes entirely through the interpreter. Shipping coverage requires bringing the codegen path up to parity for tests:

1. Synthesize a per-package test-runner binary entry point so `karac test` has something to compile and link.
2. Provide codegen implementations of the test prelude builtins (`assert`, `assert_eq`, `assert_ne`) that today exist only in the interpreter.
3. Thread instrumentation flags through inkwell IR generation and `cc` link (the link side already supports extra flags via `link_executable_with_sanitizer`'s pattern).
4. Post-process `.profraw` → `.profdata` → `lcov` via `llvm-profdata` / `llvm-cov` (analogous to how the runtime path is resolved in `src/codegen.rs`).
5. Emit a `coverage` JSONL event summarizing aggregate line / branch / function coverage AND a `coverage_delta` JSONL event reporting changed-but-uncovered code against a git ref; write `dist/coverage/lcov.info` for tooling consumption (Codecov, Coveralls, GitHub Actions).
6. `--coverage --min=N` for CI gating — exits non-zero if aggregate coverage falls below the threshold.
7. `--coverage --since REV` for delta-oriented reporting — emits the `coverage_delta` event computed against the named git revision.

**Reporting surfaces — primary vs secondary.** The two surfaces serve different consumers:

- **Delta-oriented (primary for PR review and LLM-loop consumers).** The `coverage_delta` JSONL event reports what the active change set did and did not cover, against a git ref supplied via `--since REV` (composes with the test-runner `--since` selector — the same revision serves both flags). Two delta signals: (a) changed functions with no direct test (a test function whose body syntactically calls the function), (b) changed branches not covered by any executed test. The forward direction — "which tests reach this function transitively" — lives in [`karac query affected-by`](#karac-query-affected-by--call-graph-reach-query) (P1, separate entry); coverage focuses on the reverse direction (uncovered code under change). This is the surface a PR reviewer or LLM TDD client should anchor on.

- **Aggregate (secondary, retained for CI gating).** The `coverage` JSONL event reports total-program line / branch / function coverage; `--coverage --min=N` gates CI on a project-wide threshold. Aggregate has its place — historical tracking, compliance reporting, threshold-based gates — but it is not the headline metric for change review. Global percentage alone hides the case where coverage stays high while a new untested branch lands.

**Stale-snapshot reporting** is *not* part of this entry — `karac test --clean-snapshots` ([design.md § Snapshot tests](design.md)) already reports orphaned snapshot files. A future composition could surface stale-snapshot count alongside coverage delta in the same `cycle_complete` summary (see the `karac tdd` Watch Driver entry), but the data source remains the existing `--clean-snapshots` walk, not the LLM coverage instrumentation.

**Why non-breaking:** purely additive. `--coverage`, `--since`, and `--min` are new flags on `karac test`; default behavior is unchanged. `dist/coverage/lcov.info` is a new artifact path under the existing `dist/` convention. Both `coverage` and `coverage_delta` JSONL events slot into the existing schema discriminator (existing consumers ignore unknown event kinds).

**Why P1 (not P0):** non-blocking for v1 ship. CI integration is a tooling concern, not a language correctness concern, and the manual workflow (build under `--features llvm`, run under `cargo-llvm-cov` or equivalent on the Rust compiler itself) provides a temporary substitute for compiler-internals coverage. End-user Kāra projects accept the gap until this lands.

**Interpreter-path coverage:** explicit non-goal. Folded into this CR only if real demand surfaces — otherwise compiled-binary instrumentation is the single supported path.

**Sequencing:** a separate CR scoped to coverage. Sub-commits (a) codegen `assert` builtins, (b) test-binary entry synthesis, (c) instrumentation flags through codegen, (d) `llvm-cov` post-processing + lcov + aggregate `coverage` event, (e) `--min=N` aggregate-threshold gating, (f) `--since REV` delta computation + `coverage_delta` event. Not blocked by `with_provider` runtime work (CR-24 follow-up slice 3) — the two are independent.

---

### Structured Diagnostics and Error Class Enum (`karac explain --format=json`)

**Decision:** Defer machine-parseable diagnostic output — `karac explain --format=json` with a finite error-class enum, typed `expected` / `got` fields, and ranked candidate patches — from MVP. At MVP, `karac explain` ships human-readable prose only; the class enum is not frozen until the catalogue of diagnostics has matured.

**Shape when delivered:** each JSON record carries `class` (enum from a published catalogue — `TYPE_MISMATCH`, `EFFECT_UNDECLARED`, `OWNERSHIP_MOVE_AFTER_USE`, target-incompatibility classes, etc.), `span` (byte offsets + file), typed `expected` / `got` where applicable (effect sets, type names, generic bounds), and `fixes: [{ description, edits: [{ span, replacement }] }]` for machine-applicable candidate patches. Enum values live in the reported-behavior tier (unstable across releases, stable within a release) per the Specification Layers policy — the same policy already governs `karac explain` prose.

**Target-incompatibility errors** are one class in the enumeration, not a standalone diagnostic category — file-suffix conditional compilation mismatches, target-feature-gated intrinsics, and cross-target effect violations all land under a shared `TARGET_INCOMPATIBLE` family.

**Why deferred:** enumerating error classes before the diagnostic surface has stabilized locks in a shape that may not match how the diagnostics actually land. Diagnostics keep being written as features ship — waiting until the catalogue is ~20+ entries deep gives enough signal to finalize the enum and the patch-edit shape without retrofitting. The human-readable output format continues to be reported-behavior in the interim.

**Why non-breaking:** purely additive CLI flag. Default `karac explain` behavior unchanged; JSON output is opt-in.

---

### Signature-from-Call-Site Stub Diagnostic

When the resolver encounters an unresolved-identifier call inside a `_test.kara` file (the classic TDD opener — a test that calls a function that doesn't exist yet), enrich the existing `unresolved identifier` diagnostic with a `"suggested_stub"` machine-applicable diff that defines the function in the sibling production file with a best-effort inferred signature. The stub follows the `karac test --init` compiling-skeleton convention (see the `karac tdd` Watch Driver entry below): parameter types inferred from the call's argument expressions, return type inferred from any `assert_eq` / `==` comparison the call participates in, body is `todo()`. The diff slots into the existing `hints[].diff` shape (see [design.md § AI-First Compiler Interface](design.md)) — no new protocol.

**Why this matters for the LLM-driven TDD loop.** The classic red-green opener is a test that fails to compile because the function under test is unwritten. Today the LLM consumes one parse round-trip to learn the unresolved name, then writes the stub itself with whatever type guesses it makes from the call site. With this diagnostic the LLM begins each cycle from the first parse with the stub already proposed — fewer round-trips, fewer guesses, and the proposal is grounded in argument types the *compiler* sees rather than types the LLM infers from textual context.

**Diagnostic shape** (extending the existing `hints[].diff`):

```json
{
  "id": "d1",
  "severity": "error", "primary": true,
  "code": "E0100", "category": "resolve",
  "concept": "resolve/unresolved-identifier",
  "file": "src/math_test.kara", "line": 3, "column": 15,
  "message": "undefined name 'add'",
  "hints": [{
    "description": "stub `add` in src/math.kara with inferred signature",
    "diff": {
      "file": "src/math.kara",
      "line": <end-of-file>,
      "old": "",
      "new": "fn add(arg0: i32, arg1: i32) -> i32 {\n    todo()\n}\n"
    }
  }]
}
```

**Inference scope** — left to implementation time. Two layers are plausible:

1. **Resolver-time best-effort.** Cheap, local-only inference: literal arguments (`add(2, 3)` → `i32, i32`), explicit-typed bindings (`let x: u64 = ...; add(x)` → `u64`), and obvious comparison context (`assert_eq(call(...), 5)` → return type `i32`). Falls back to `_` placeholders for argument expressions whose types depend on typechecking. Ships first.
2. **Post-typecheck refinement.** Typechecker continues past unresolved-call errors (synthesizing a placeholder signature for the missing function) and infers argument and return types where context permits. Higher-quality stubs but a bigger pipeline change. Optional second milestone.

The implementation chooses the layer based on what real LLM-loop usage shows: if the resolver-time best-effort produces enough quality to drive most cycles, the post-typecheck layer can be deferred or skipped. The diagnostic shape is identical at both layers — the difference is how many `_` placeholders the LLM has to fill in.

**Body convention.** The stub body is `todo()` per the same compiling-skeleton policy as `karac test --init` (see *`karac tdd` Watch Driver* above, "Test scaffolding" subsection — the parameter-type-default table). For `ref T` / `mut ref T` parameters, no synthetic `_owned` binding is generated at the function signature site — that's a test-body concern, not a function-signature concern.

**Activation gate.** This diagnostic enrichment fires *only* when the unresolved-call site is inside a `_test.kara` file. Production files emit the plain `unresolved identifier` diagnostic without the stub hint — for production code, the failure usually means a typo or a missing import, not a function the user is about to write. Limiting to test files matches the classic TDD red-green workflow without polluting non-TDD diagnostics.

**Why non-breaking:** the `hints` field already exists; adding entries to it is additive. Existing JSONL consumers see new hint records under the existing schema. The plain-text human-readable diagnostic format may surface the suggestion as an additional hint line; humans who don't want the suggestion can ignore it. No new flag, no new event type.

**Distinct from `karac test --init` scaffolding.** The `karac test --init` subsection in the `karac tdd` Watch Driver entry below scaffolds *tests* for existing functions; this entry scaffolds *functions* for tests that don't resolve. Both feed the LLM TDD loop in different directions: `karac test --init` is "I have a function, give me a test"; this is "I have a test, give me the function." Both reuse the compiling-skeleton convention; both emit machine-applicable diffs.

---

### `karac tdd` Watch Driver — Unified TDD Cycle Loop

A `karac tdd` subcommand that orchestrates the existing build, diagnostic, and test surfaces into a tight red-green-refactor loop suitable for LLM-driven test-first development. Watches the project filesystem; on change, re-runs the affected pipeline and emits a unified JSONL event stream covering build phases, diagnostics, test execution, and a per-cycle summary.

Sketch of the cycle envelope (final shape TBD; specifics emerge from prerequisite items):

```bash
karac tdd --watch --output=jsonl
karac tdd src/foo.kara::function_name --output=jsonl
```

```json
{"type":"cycle_start","changed":["src/foo.kara","src/foo_test.kara"]}
{"type":"phase_start","phase":"parse"}
{"type":"diagnostic","phase":"parse","id":"d1","primary":true}
{"type":"test_fail","test":"foo::test_empty_input","left":"...","right":"..."}
{"type":"cycle_complete","status":"red","next_best_action":"fix_primary_diagnostic"}
```

**Prerequisite work** (each is its own committed item):

- Stable `karac test` JSONL contract ([design.md § Test Runner Output Format](design.md))
- `karac build --output=jsonl` streaming mode ([design.md § AI-First Compiler Interface](design.md))
- Unified envelope shape across build and test JSONL streams
- Targeted test selection (`--failed`, `--related FILE`, `--since REV`)
- Cycle-summary status taxonomy distinguishing compile-fail / no-tests-discovered / tests-failed / tests-passed / tests-skipped-resource-unavailable
- `karac test --init <module::function>` scaffolding for stub creation
- `karac explain --format=json` for structured diagnostic patches (already P0 deferred — see *Structured Diagnostics and Error Class Enum* above)

**Why deferred:** This is integrating tooling, not a language feature. It composes pieces specified elsewhere in the design plus the prerequisites above. The watch loop itself is a thin shim over file watching (`notify`-style crates handle portability) and successive `karac build` / `karac test` invocations. The non-trivial parts — the cycle envelope, `next_best_action` triage policy, affected-tests selection — depend on the prerequisites above being battle-tested. Specifying the cycle event schema before its prerequisites land would over-spec; specifying it after gives schema choices grounded in real client integrations.

**Why non-breaking:** New subcommand. Existing `karac build` / `karac test` invocations are unchanged. The watch driver wraps them as a sub-process or in-process call; both are unaffected by the wrapper's existence. Cycle events are additive on top of the existing per-tool envelopes.

**Capstone framing.** The reviewer who proposed this also proposed the prerequisite items above. Together they constitute "build out the LLM-driven TDD surface end-to-end." Each prerequisite stands on its own merit and is decided independently; this entry exists so the prerequisites have an integrating destination to aim at, and so the watch driver is not forgotten as a coherent productized loop after the prerequisites land.

**Cycle-summary status taxonomy.** TDD starts in red, so the `cycle_complete` event needs a status field richer than green/red. The five distinct end-states an LLM client must act on differently:

| Status                        | When                                                                                                | LLM action                                                                              |
|-------------------------------|-----------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------|
| `compile_error`               | Build phase emitted at least one error diagnostic; test phase did not run                           | Surface the primary diagnostic; route to fixing it before any test inference            |
| `no_tests_discovered`         | Build succeeded but no `_test.kara` files or no `fn test_*` matched the active scope                | Distinct from `tests_passed` because nothing was verified — write a test, do not assume green |
| `tests_failed`                | Build succeeded; at least one `test_fail` event                                                     | Standard red state; primary loop driver — fix the failing assertion                      |
| `tests_passed`                | Build succeeded; at least one test ran; every event was `test_pass` (or permitted `test_skip` not under `--all`) | Green; refactor or extend coverage                                                       |
| `tests_skipped_unavailable`   | Build succeeded; at least one `test_skip` with reason `unsatisfied_requires` (and not running under `--all`) | Provider not configured or external service unavailable — surface which resource, not silently treat as green |

Precedence when multiple conditions apply within a single cycle: `compile_error` > `tests_failed` > `tests_skipped_unavailable` > `tests_passed` > `no_tests_discovered`. Under `--all` mode, any skip becomes a fail and the status collapses to `tests_failed`. Under permitted-skip mode, a mix of `test_pass` and `test_skip` resolves to `tests_skipped_unavailable` — the skipped resource is information the loop surfaces rather than silently loses, and an LLM that wants green must address the missing provider.

The taxonomy locks the *set* of statuses so future test-runner reasons can extend `test_skip` without expanding the cycle-status vocabulary. Adding new statuses is a major-version decision; adding new `test_skip` reasons (per design.md test-runner forward-compat rules) routes through `tests_skipped_unavailable` until evidence justifies a new top-level cycle status.

**Test-selection flags.** Substring filtering (`karac test <substring>` per design.md § Filtering) is useful but crude — LLM loops and watch clients need precise selection. The flag set:

| Flag                       | Semantics                                                                                          | Dependency                                                                                       |
|----------------------------|----------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------|
| `--failed`                 | Re-run only test IDs that emitted `test_fail` in the previous run.                                 | None — pure runner book-keeping. Persists last-run state in a cache file (`.kara/test-state.json` or similar). Ships standalone with the watch driver; no compiler analysis required. |
| `--related <FILE>`         | Run tests whose transitive call graph reaches code in `<FILE>`.                                    | Thin wrapper over [`karac query affected-by`](#karac-query-affected-by--call-graph-reach-query) (P1, separate entry). |
| `--since <REV>`            | Run tests affected by changes since git ref `<REV>` (e.g., `--since HEAD`, `--since main`).        | Composes [`karac query affected-by`](#karac-query-affected-by--call-graph-reach-query) over the files surfaced by `git diff <REV>...HEAD`. |
| `--module <path>`          | Run tests in the named module path (e.g., `--module db.connection`).                               | None — discovery already groups tests by module path; flag is a literal-prefix filter on the fully-qualified ID. |
| `--exact <full::test::id>` | Run exactly the named test (e.g., `--exact db.connection::test_reconnect`).                        | None — equality filter on the fully-qualified ID. Distinct from substring (which can ambiguously match multiple). |

The existing substring filter remains — it is the casual default. The flags above are additive and orthogonal: `karac test --failed --module db` runs the previous-failure set intersected with the `db` module. Combinations resolve as set intersections. `--all` overrides selection (runs everything regardless of selectors), preserving its existing "fail-on-skip" semantics.

`--related` and `--since` block on the affected-by query landing — without it, both flags would need ad-hoc heuristics that miss real reach edges (closure captures, trait-object dispatch, generic monomorphizations). The watch driver lands these flags only after the query is in place.

The `.kara/test-state.json` cache for `--failed` is per-project, gitignored, regenerated each run; corruption resets to "no previous state" (treats `--failed` as `--all` with a stderr note). The cache schema is internal — not part of the JSONL contract — so it can evolve freely.

**Test scaffolding (`karac test --init`).** Generates a compiling test skeleton for a named function, removing the boilerplate "open the test file, write a `fn test_*`, plumb default arguments, add `#[with_provider]` for any effects" sequence from the LLM loop.

```bash
karac test --init src/db/user.kara::create_user
karac test --init db.user::create_user        # module-path form
```

Both forms are accepted; module path is resolved to its source file via the standard module walk, and the file path resolves to its module path the same way `karac build` does. The function must be defined in the current project — scaffolding tests for dependency code is rejected.

Target file resolution:
- Sibling test file path is the source path with `.kara` replaced by `_test.kara` (e.g., `src/db/user.kara` → `src/db/user_test.kara`).
- If the file exists, append the new test function at the end (after the last item).
- If it does not exist, create it with no extra preamble — `_test.kara` files inherit private sibling access and auto-injected `assert` / `assert_eq` per design.md § Testing.

Test function name:
- Default: `test_<fn_name>` (e.g., `test_create_user`).
- On collision (a `fn test_create_user` already exists in the file): append a numeric suffix incrementing from `_2` until unique, and emit a stderr note naming the chosen name.

Generated body — the compiling-skeleton policy:

| Parameter type                       | Default value generated                                       |
|--------------------------------------|---------------------------------------------------------------|
| Numeric primitives (`i32`, `u64`, `f32`, etc.) | `0`, `0u64`, `0.0` matching the target type            |
| `bool`                               | `false`                                                       |
| `String`                             | `String.from("")`                                             |
| `Option[T]`                          | `None`                                                        |
| `Result[T, E]`                       | `Ok(<default for T>)` — `Err` would force the caller to construct an `E` value |
| `Vec[T]` / `Map[K, V]` / `Set[T]`    | `Vec.new()` / `Map.new()` / `Set.new()`                       |
| `ref T` / `mut ref T`                | Synthesize `let <param>_owned: T = <default>;` above the call, pass `ref <param>_owned` (or `mut ref ...`) |
| Refinement types (`Positive[i32]`, etc.) | `todo()` — defaults may not satisfy the refinement predicate |
| User-defined struct / enum           | `todo()` — the compiler cannot know which constructor to pick |
| Generic type parameter (e.g., `T`)   | `todo()` — no concrete type chosen at scaffold time           |

The skeleton compiles whenever every parameter is in the "concrete default" rows of the table; otherwise it compiles after the user replaces `todo()` calls with values. The scaffold's *intent* is "ready to run with a green build the moment defaults work, with `todo()` markers showing exactly what to fill." For functions whose return type is `Result[T, E]` or whose body has fallible refinement returns, the assertion line is `assert!(/* TODO */ true);` — a literal placeholder that compiles but is meaningless until the user writes a real assertion.

Effect-aware scaffolding: if the function under test declares effects (e.g., `with reads(Db)`), the generated test includes a `#[with_provider(Db, /* TODO */ todo())]` line above the test function. The provider value is `todo()` — the user supplies a fake. This makes the effect surface visible at the scaffolding site without forcing the scaffolder to know which fakes are available.

Errors:
- Function not found: `E0xxx` "function `<name>` not found in module `<path>`".
- Function is private to a sibling that is not the source file's sibling test: rejected (the test file would need access the language doesn't grant).
- Function is in a dependency: rejected as above.
- Source file is itself a `_test.kara` file: rejected ("cannot scaffold tests for test code").

The exit code is `0` on success (file written or appended); non-zero on any of the above errors. On stdout, the command emits one JSONL `init` event with the chosen test name, target file path, and any `todo()` markers placed, so an LLM client can read what to fill next without re-parsing the file.

---

### Signature Catalog (`karac catalog`)

**Decision:** Defer a tooling subcommand — `karac catalog` — that indexes the public API surface (fully qualified name, kind, generic parameters with bounds, parameter modes and types, return type, declared effect row, refinement constraints, source span) and emits JSONL for downstream consumers (LLM agents, IDE plugins, documentation generators).

**Shape when delivered:** public surface only — private functions have inferred, reported-tier effect rows that are not stable enough to index. One entry per exported item (`fn`, `struct`, `trait`, `impl`, `const`, type alias). Queryable by any field component: "find all public fns that take a `Path` and produce `writes(Fs)`," "find all traits with a `Display` bound in their supertrait set," etc.

**Why deferred:** pure tooling, blocks on no language decisions. Natural fit once the language surface is stable and real consumers (LLM agents, IDE tooling) materialize. Overlaps with **Structured Diagnostics and Error Class Enum** above — both are JSONL-emitting tooling that benefits from a shared schema vocabulary; build them in concert when their respective consumers are real.

**Why non-breaking:** new `karac` subcommand; no language-surface impact.

---

### `karac query affected-by` — Call-Graph Reach Query

Extension to the existing query API ([design.md § AI-First Compiler Interface § 2](design.md)) that exposes the compiler's call graph as a queryable surface, alongside the shipped `karac query effects` / `ownership` / `concurrency` subcommands. Inputs: a file path with optional line range, or a fully-qualified function path. Outputs: the transitive callers and callees that the call graph already computes for effect inference, plus the test functions that reach the input through that graph.

**Invocation:**

```bash
karac query affected-by src/sort.kara                    # all functions affected by changes to file
karac query affected-by src/sort.kara:42-58              # affected by changes to specific line range
karac query affected-by math::sort                       # affected by changes to a specific function
karac query affected-by math::sort --tests-only          # only test functions reaching this
karac query affected-by math::sort --direction=callees   # transitive callees only (not callers)
```

**Output format (JSONL):**

```json
{"type":"affected_by","input":"math::sort","callers":[{"fn":"app::main","file":"src/main.kara","line":12}],"callees":[{"fn":"std::cmp::min","file":"std/cmp.kara","line":34}],"tests":[{"fn":"math_test::test_sort_preserves_length","file":"src/math_test.kara","line":3}]}
```

Schema:
- `input`: the function or file the query was issued against (echoed for client correlation).
- `callers`: array of `{fn, file, line}` for every function that transitively calls into the input. Direct callers first, then their callers, etc. — partial topological order.
- `callees`: array of `{fn, file, line}` for every function the input transitively calls.
- `tests`: array of test functions (those defined in `_test.kara` files matching the `fn test_*` discovery rule plus those marked `#[test]`) that reach the input through the call graph. Subset of `callers` filtered to test functions, surfaced separately because the test-selection consumers (`--related`, `--since`) want this view directly.

**Call-graph construction subtleties** (well-understood engineering, not research):

- **Trait-object dispatch (`t.method()` on `dyn Trait`).** The graph includes every impl of `Trait` known at query time as a possible callee. Conservative — false positives (impls the runtime never reaches) are acceptable for affected-by; false negatives (real reaches missed) would break the test-selection use case.
- **Generic monomorphization.** A generic function `fn f[T](x: T)` instantiated with multiple concrete `T` values may have different call graphs per instantiation. The query summarizes across all instantiations the compiler sees in the project — the union of every monomorph's reach. A future flag could parameterize by `T` if a concrete use case emerges.
- **Closure captures and escape.** When a closure escapes its creation site (stored, returned, passed to a function that calls it later), the call site of its body is the *escape consumer*, not the closure-creation site. The graph traces escape paths so callers of the consumer are correctly attributed as transitive callers of the closure body.
- **FFI / `extern` boundaries.** The graph does not cross `extern` boundaries — `extern fn`s are leaf nodes. Their declared effects propagate, but their internal call graph is opaque (no body to analyze).
- **Recursion / SCCs.** Strongly-connected components in the call graph are treated as a single unit for the affected-by closure — every function in an SCC affects every other function in the SCC.

**Why P1:** structural prerequisite for three already-committed P1 features:

- [`karac tdd` Watch Driver](#karac-tdd-watch-driver--unified-tdd-cycle-loop) — uses affected-by to scope cycles to changed code.
- Test-selection flags `--related <FILE>` and `--since <REV>` (in the `karac tdd` entry's flag taxonomy) — both block on this query landing; without it they need ad-hoc heuristics that miss real reach edges.
- `coverage_delta` event in [`karac test --coverage`](#karac-test---coverage-llvm-instrumented-coverage) — uses affected-by to compute the "tests covering changed function" delta signal.

Without this query, those three features either ship with reduced functionality or wait. Shipping them as designed requires the affected-by data.

**Why non-breaking:** new query subcommand under the existing `karac query` umbrella. No existing query behavior changes. JSONL output uses the standard `"type"` discriminator (matching the unified envelope per F6); existing JSONL consumers ignore unknown event types.

**Implementation cost.** Moderate. The data exists already — effect inference computes the call graph, including the trait-dispatch / generics / closure handling above. The work is plumbing it into a query interface, defining the JSONL output, and exposing the existing graph traversals as a public surface. No research questions; well-understood engineering.

---

### Doctests as `#[example]` Blocks on `pub` Items

Compiler-extracted runnable examples on `pub`-item documentation. Following the well-trodden pattern from Rust (`cargo test --doc`), Python (`doctest`), Haskell (`doctest`), and OCaml (`mdx`) — examples in or attached to docstrings that the compiler extracts and runs as tests under `karac test`, with assertion failures reported through the existing test-runner JSONL envelope.

**Kāra-specific value-add.** Beyond the standard documentation-drift defense (an example that fails to compile or run blocks CI; the API can't evolve away from its examples without breaking the build), Kāra's effect / contract / refinement system means examples cover the drift gap from two directions: an LLM-written example that violates the function's `ensures` clause becomes a compile error rather than runtime evidence; a succeeding example gives the contract executable verification. The shape that LLMs reach for first when documenting an API becomes a first-class verification artifact.

**Mechanics (committed):**

1. **Discovery rule.** `karac test` extends its current `_test.kara`-only walk to also visit regular source files looking for example items / blocks attached to `pub` items. Examples on non-`pub` items are a parse-level diagnostic — examples are public-API artifacts. Examples on items in `_test.kara` files are also rejected (test files have their own testing surface; examples belong to the documented API).
2. **Test-prelude injection.** Examples receive the same prelude as `_test.kara` files (`assert`, `assert_eq`, `Arbitrary` if applicable). Imports inside the example body resolve through the example's enclosing module — examples have access to the public surface of the module they live in plus any `import`s the file already brings in.
3. **Doc rendering interaction.** `karac doc` renders example bodies as code blocks alongside the docstring prose — same source, two views. The renderer reuses `pulldown-cmark` for markdown-flavored examples, or emits attribute-shaped examples as `<pre><code class="language-kara">` blocks under a "Examples" heading.
4. **Effect inference.** Examples are normal compiled functions; effects propagate through standard inference. An example calling `pub fn read_file()` with `reads(FileSystem)` inherits `reads(FileSystem)` on its synthetic test-fn signature. If the project profile permits `reads(FileSystem)` for tests, the example runs; otherwise the example is rejected at compile time, surfacing the same effect-mismatch diagnostic that any other effect-violating function would.
5. **Compilation cost / when to run.** `karac build` does NOT compile or run examples (preserves the fast-build property — examples don't gate plain compilation). `karac test` discovers and runs examples alongside `_test.kara` tests. A `karac test --no-examples` flag lets developers iterate on test-file changes without re-running every example.
6. **MVP scoping for effectful examples.** Pure examples (`assert_eq(abs(-5), 5)`) ship in the MVP. Effectful examples that require providers (`#[with_provider]`-style setup in a `_test.kara` file) need a syntax for declaring providers within the example block — defer this to a follow-up once the pure-example surface lands and real demand for effectful examples surfaces.
7. **Failure reporting.** A failing example emits a `test_fail` JSONL event with `test: <module_path>::<item_name>::example` (or `::example_<n>` if multiple examples are attached to one item). The failure event includes the example body in the diagnostic so the reader sees exactly what was being asserted, even without source access.
8. **Discovery-error handling.** An example that doesn't compile is a hard error under `karac test`, the same way a `_test.kara` file that doesn't compile is. No silent skip — broken examples are broken docs.

**Syntax candidates (pick deferred to implementation prototyping):**

The three plausible shapes each have honest tradeoffs. Implementation prototypes each on a representative slice of the stdlib (~10 `pub` items with varied effect surfaces) and picks the winner against artifact, not speculation.

**(i) `#[example] fn _ex() { ... }` — explicit function with attribute.**

```kara
/// Computes the absolute value.
pub fn abs(x: i32) -> i32 { if x < 0 { -x } else { x } }

#[example]
fn abs_handles_negatives() {
    assert_eq(abs(-5), 5);
    assert_eq(abs(0), 0);
}
```

*Pros:* fits Kāra's existing attribute culture (`#[test]`, `#[property]`, `#[snapshot]`, `#[derive(...)]`); reuses existing AST infrastructure (the example IS a function); explicit naming gives precise test IDs; effect declaration via the standard `with` clause works transparently.

*Cons:* visually separated from the docstring it documents; readers must scan past the `pub fn` to find the example; verbose for one-line assertions.

**(ii) Rust-style fenced code blocks in `///`.**

```kara
/// Computes the absolute value.
///
/// ```
/// assert_eq(abs(-5), 5);
/// assert_eq(abs(0), 0);
/// ```
pub fn abs(x: i32) -> i32 { if x < 0 { -x } else { x } }
```

*Pros:* visually collocated with the docs they verify (the prose-with-example flow that's the whole point of doctests); most natural form for LLM-generated docs (markdown is the lingua franca); `karac doc` rendering is trivially natural (the code block is already markdown); concise.

*Cons:* requires parser support for extracting fenced code blocks from doc comments as test bodies; effect declaration is awkward (where does `with reads(FileSystem)` go on a fenced block?); test ID is positional within the docstring rather than named.

**(iii) `#[example(of = path)]` as a separate top-level item.**

```kara
/// Computes the absolute value.
pub fn abs(x: i32) -> i32 { if x < 0 { -x } else { x } }

#[example(of = abs)]
fn abs_handles_negatives() {
    assert_eq(abs(-5), 5);
    assert_eq(abs(0), 0);
}
```

*Pros:* most flexible (multiple examples per item, examples in different files, examples organized by topic rather than co-located with the item); explicit cross-reference makes the relationship machine-readable.

*Cons:* most verbose; loses the prose-with-example flow entirely; requires a path-resolution pass to validate `of = abs` references a real `pub` item; falls back to (i) if the cross-reference is degenerate (one-to-one with the documented item).

**Implementation guidance.** Prototype (i) and (ii) on a representative slice of the stdlib. Measure: example density per item, ergonomics for one-liners vs. multi-statement examples, integration with effect declaration when the example calls effectful code, doc-rendering quality, parser complexity. Pick the winner. (iii) is a fallback considered only if the primary candidates have a structural problem we don't currently see.

**Why P1:** well-trodden pattern (no research uncertainty), existing infrastructure leverage is clean (test prelude, doc rendering, effect inference all already exist), real LLM-loop value (LLM-written examples auto-verified, examples-as-contracts story is genuinely distinctive to Kāra), independent of the `karac tdd` capstone and its sub-features so the work can land on its own timeline.

**Why non-breaking:** new attribute syntax (or doc-comment convention, depending on which candidate wins); existing `pub` items without examples are unaffected. `karac build` behavior is unchanged (build doesn't run examples); `karac test` gains additional discovery scope but pre-existing tests still run identically. New JSONL `test_pass` / `test_fail` events for examples slot into the existing schema discriminator.

**Phase placement.** Phase 5.1 (Tooling) — `karac test` discovery extension + `karac doc` rendering interaction.

---

### Structured Runtime Traces Keyed to Source Spans

**Decision:** Defer tooling for structured runtime trace output — events annotated with the source span of the emitting site, suitable for debugging effect conflicts, ownership timing, scheduler placement, and other properties that manifest only when the program runs. Complementary to compile-time `karac explain`.

**Why deferred:** depends on mature codegen and runtime (Phase 8+). The source-span side is cheap — every AST node already carries a `Span`. The runtime side requires stable instrumentation hooks that cannot be pinned until the codegen path and scheduler are real enough to instrument. Deciding the output format now risks a mismatch with the emission points once they exist.

**Ecosystem compatibility — open.** Candidate formats include an OpenTelemetry-compatible emitter, a `tokio-trace`-style layered subscriber, or a Kāra-specific JSONL format shared with `karac test`. Pick when the instrumentation hooks land; the trade-off is familiarity vs. schema control.

**Why non-breaking:** opt-in runtime feature; off by default.

---

### `std.einsum` — Einstein Summation

**Decision:** Defer `einsum` to Phase 11+ stdlib. Ship as a string-notation function once the core numerical stdlib (`Tensor`, shape types, `std.linalg`) is stable.

**Why deferred:** Pure stdlib addition — no language changes required. Holding it lets the broader numerical surface stabilize so `einsum` fits cleanly alongside matmul, reduce, and broadcast methods.

**Why non-breaking:** New stdlib function. No existing API affected.

**Design shape:**

```kara
use std.einsum.einsum;

let c   = einsum("ij,jk->ik", a, b);        // matmul
let tr  = einsum("ii->", a);                // trace
let out = einsum("i,j->ij", u, v);          // outer product
let bat = einsum("bij,bjk->bik", a, b);     // batched matmul

// Return type: Tensor[T, [?]] — shape derived at runtime from the einsum string.
// Typed einsum with compile-time shape checking is deferred to P2.
```

The string parser validates index consistency (each index appears at most twice per operand on the left, exactly once in the output) at runtime and returns an error on malformed strings.

---

### `std.linalg` — Linear Algebra Suite

**Decision:** Defer `std.linalg` to Phase 11+. Minimum surface: SVD, eigendecomposition, QR factorization, Cholesky, least-squares (`lstsq`), matrix norm, inverse, determinant, and rank. Dispatch through LAPACK (linked at build time) or a pure-Kāra Cooley-Tukey fallback.

**Why deferred:** Requires stable `Tensor` stdlib (Phase 11) and LLVM backend (Phase 7) for LAPACK linkage. No language decisions are blocking.

**Why non-breaking:** New stdlib module.

**Design shape:**

```kara
use std.linalg;

let (u, s, vt) = linalg.svd(a);
let (vals, vecs) = linalg.eig(a);           // square matrix only
let (q, r) = linalg.qr(a);
let l = linalg.cholesky(a);                 // positive-definite — panics otherwise
let x = linalg.lstsq(a, b);
let n = linalg.norm(a, ord: linalg.Norm.Fro);  // Norm.L1, Norm.L2, Norm.Inf also available
let inv = linalg.inv(a);
let d   = linalg.det(a);
let r   = linalg.matrix_rank(a);
```

All functions require `T: Float` (`f32` or `f64`). Output shapes follow standard linear algebra conventions and return partially-dynamic shapes until shape arithmetic (v1.5) allows full static expression.

---

### `std.fft` — Fourier Transforms

**Decision:** Defer `std.fft` to Phase 11+. Minimum surface: 1D FFT/IFFT, N-D FFT, real FFT (`rfft`), and frequency helper (`fftfreq`). Dispatch through FFTW (linked at build time) or a pure Cooley-Tukey fallback.

**Why deferred:** Pure stdlib work — no language decisions blocking. Requires LLVM backend (Phase 7) for FFTW linkage.

**Why non-breaking:** New stdlib module.

**Design shape:**

```kara
use std.fft;

let spectrum  = fft.fft(signal);                        // Tensor[Complex[f64], [N]]
let recovered = fft.ifft(spectrum);
let rspec     = fft.rfft(signal);                       // Tensor[Complex[f64], [N/2 + 1]]
let freqs     = fft.fftfreq(n: 1024, d: 1.0 / rate);   // Tensor[f64, [1024]]
let spec2d    = fft.fftn(image);                        // Tensor[Complex[f64], [H, W]]
```

`Complex[T]` is a stdlib struct with `real` and `imag` fields and the standard arithmetic traits. It is not a new numeric primitive.

---

### `std.random` — Distribution Extensions

**Decision:** Defer statistical distribution sampling beyond basic uniform random to Phase 11+. Minimum surface: normal (Gaussian), uniform (continuous), binomial, Poisson, and exponential.

**Why deferred:** Basic uniform sampling ships with `std.random` in Phase 8. Distribution extensions are a follow-on slice with no language dependencies.

**Why non-breaking:** Additive to the existing `std.random` module.

**Design shape:**

```kara
use std.random.{Rng, distributions};

let mut rng = Rng.from_seed(42);

let x = rng.sample(distributions.Normal(mean: 0.0, std: 1.0));
let y = rng.sample(distributions.Uniform(lo: 0.0, hi: 1.0));
let n = rng.sample(distributions.Binomial(n: 10, p: 0.3));     // u64
let k = rng.sample(distributions.Poisson(lambda: 2.5));        // u64
let e = rng.sample(distributions.Exponential(rate: 1.5));

let arr: Tensor[f64, [100, 100]] = Tensor.from_fn(|_, _| rng.sample(distributions.Normal(0.0, 1.0)));
```

---

### `Tensor.where` — Conditional Element Selection

**Decision:** Defer element-wise conditional selection to Phase 11+ as a stdlib function.

**Why deferred:** Pure stdlib addition — depends only on boolean tensor support being in place (ships with the `Tensor` type itself via element-wise comparison operators).

**Why non-breaking:** New stdlib function.

**Design shape:**

```kara
// Free function: Tensor.where(condition, if_true, if_false)
let result  = Tensor.where(mask, x, y);      // shapes of mask/x/y must match exactly
let clipped = Tensor.where(arr > 0.0, arr, 0.0);  // scalar broadcasts as with other operators

// Method alias
let result = mask.select(x, y);
```

Shapes must match exactly — no implicit tensor-tensor broadcasting (consistent with Kāra's broadcasting design). Scalar arguments broadcast as with other scalar-tensor operators.

---

### Boolean and Fancy Indexing

**Decision:** Defer boolean mask indexing and index-array indexing to Phase 11+. Result shape is always partially dynamic — boolean mask result count is data-dependent; index-array result shape depends on the index array's shape.

**Why deferred:** v1 `Tensor` handles scalar-index access (`t[i, j, k]`) only. Boolean and fancy indexing require additional `Index` trait impls — a pure stdlib extension, but dependent on the Phase 11 `Tensor` implementation being in place.

**Why non-breaking:** New `Index` trait impls for new argument types. Existing `t[i, j, k]` form is unaffected.

**Design shape:**

```kara
let arr: Tensor[f64, [10, 5]] = ...;

// Boolean indexing — result row count = number of true entries in mask
let mask: Tensor[bool, [10]] = arr[:, 0] > 0.0;
let filtered = arr[mask];           // Tensor[f64, [?, 5]]

// Fancy indexing — index with an array of integer indices
let idx: Tensor[usize, [3]] = Tensor.from([1, 4, 7]);
let rows = arr[idx];                // Tensor[f64, [3, 5]]
```

Both forms return owned tensors (not views) — the gathered elements may be non-contiguous in the source buffer and must be materialized into a fresh allocation.

---

### `Tensor.meshgrid` — Coordinate Grid Generation

**Decision:** Defer `meshgrid` to Phase 11+ as a stdlib convenience.

**Why deferred:** Pure stdlib — no language changes. Low priority relative to `std.linalg`, `std.fft`, and `std.einsum`.

**Why non-breaking:** New stdlib function.

**Design shape:**

```kara
use std.tensor.meshgrid;

let x = Tensor.from([0.0, 1.0, 2.0]);   // Tensor[f64, [3]]
let y = Tensor.from([0.0, 1.0]);        // Tensor[f64, [2]]

let (xx, yy) = meshgrid(x, y);
// xx: Tensor[f64, [2, 3]] — x values broadcast over rows
// yy: Tensor[f64, [2, 3]] — y values broadcast over columns
```

Returns broadcast-expanded (strided) views by default. `.compact()` materializes into contiguous memory when needed.

---

### Tensor Element-Wise Math and Clamp

**Decision:** Defer the full suite of element-wise unary math functions — and the `clip` clamp utility — to Phase 11+ as `Tensor` methods and free functions in `std.math`.

**Why deferred:** These require the `Tensor` stdlib (Phase 11) and LLVM auto-vectorization (Phase 7) to perform well. No language decisions are blocking.

**Why non-breaking:** New methods/functions. No existing API affected.

**Design shape:**

Transcendental and rounding functions dispatch through `std.math` and vectorize via LLVM:

```kara
// Element-wise — return Tensor of same shape
arr.exp()       // e^x per element
arr.log()       // natural log; log2(), log10() also available
arr.sqrt()
arr.abs()
arr.sign()      // -1.0, 0.0, or 1.0
arr.floor()
arr.ceil()
arr.round()
arr.sin()  arr.cos()  arr.tan()
arr.sinh() arr.cosh() arr.tanh()
arr.asin() arr.acos() arr.atan()
Tensor.atan2(y, x)   // element-wise two-argument arctangent

// Clamp — the most common value-bounding operation
arr.clip(lo: 0.0, hi: 1.0)          // element-wise clamp; lo/hi are scalars
arr.clip_lo(0.0)                     // lower bound only (ReLU idiom)
arr.clip_hi(1.0)                     // upper bound only
```

All functions require `T: Float`. The `clip` family operates analogously to scalar broadcasting: `lo` and `hi` are `T`, not `Tensor[T, Shape]`.

---

### Tensor Construction Functions

**Decision:** Defer explicit construction helpers to Phase 11+. These are required for almost every numerical program and must ship alongside the core `Tensor` type, but the exact API is pinned here to avoid ad-hoc decisions during implementation.

**Why deferred:** Pure stdlib work — no language decisions blocking. Pinning the API shape now ensures the interpreter and codegen don't grow incompatible ad-hoc constructors.

**Why non-breaking:** New functions on `Tensor`.

**Design shape:**

```kara
// Filled
Tensor.zeros[T: Numeric](shape: Shape) -> Tensor[T, Shape]
Tensor.ones[T: Numeric](shape: Shape) -> Tensor[T, Shape]
Tensor.full[T](shape: Shape, value: T) -> Tensor[T, Shape]

// Range — 1D only
Tensor.arange(stop: f64) -> Tensor[f64, [?]]
Tensor.arange(start: f64, stop: f64, step: f64 = 1.0) -> Tensor[f64, [?]]
Tensor.linspace(start: f64, stop: f64, n: usize) -> Tensor[f64, [?]]

// Identity / diagonal
Tensor.eye[T: Numeric](n: usize) -> Tensor[T, [?, ?]]      // n×n identity
Tensor.diag(v: Tensor[T, [?]]) -> Tensor[T, [?, ?]]        // 1D → diagonal matrix
Tensor.diag(m: Tensor[T, [?, ?]]) -> Tensor[T, [?]]        // matrix → main diagonal

// Element-wise construction
Tensor.from_fn[T](shape: Shape, f: Fn(usize...) -> T) -> Tensor[T, Shape]

// From nested Vec / array literals
Tensor.from[T](data: Vec[T]) -> Tensor[T, [?]]             // 1D
Tensor.from_nested[T](data: Vec[Vec[T]]) -> Tensor[T, [?, ?]]
```

Static-shape overloads (where `Shape` is fully static) are resolved at compile time; dynamic overloads return `Tensor[T, [?...]]`.

---

### Scan Operations (`cumsum`, `cumprod`)

**Decision:** Defer prefix-scan operations to Phase 11+.

**Why deferred:** Pure stdlib work. Like axis reductions, the fully-typed axis-indexed versions require shape arithmetic (v1.5) — the output has the same shape as the input (no dimension removal), so they can ship in Phase 11 with fully static output types even before v1.5.

**Why non-breaking:** New methods on `Tensor`.

**Design shape:**

Unlike axis reductions, scans preserve the input shape, so no shape arithmetic is required:

```kara
let t: Tensor[f64, [3, 4]] = ...;

// Global (flatten then scan)
t.cumsum() -> Tensor[f64, [12]]
t.cumprod() -> Tensor[f64, [12]]

// Axis-indexed — output shape identical to input (no remove_dim needed)
t.cumsum[1]() -> Tensor[f64, [3, 4]]   // running sum along columns
t.cumprod[0]() -> Tensor[f64, [3, 4]]  // running product along rows
```

Axis-indexed scans can therefore ship in Phase 8, unlike axis-indexed reductions which require shape arithmetic to express the reduced dimension.

---

### Shape-Manipulating Operations (`concat`, `stack`, `reshape`, `squeeze`, `expand_dims`)

**Decision:** Ship in Phase 11 with partially-dynamic output shapes. v1.5 shape arithmetic will provide fully-typed versions where the output shape is statically known.

**Why deferred from v1:** Depends on stable `Tensor` stdlib. Output shapes require shape arithmetic for full static typing; partially-dynamic shapes are acceptable for Phase 11.

**Why not held for v1.5 (unlike axis reductions):** These are too fundamental to hold — without them, users cannot assemble tensors from parts or change layout. The dynamic return shapes are safe to ship; callers that need the precise output shape can call `.shape()` at runtime or wait for v1.5.

**Why non-breaking:** The output type changes from `Tensor[T, [?...]]` in Phase 11 to a more specific static shape in v1.5, which is additive — code accepting the dynamic type continues to work with the more specific type.

**Design shape:**

```kara
// Concatenate along an existing axis
Tensor.concat(tensors: Slice[Tensor[T, [?, ...]]], axis: usize) -> Tensor[T, [?, ...]]
// v1.5: concat[const AXIS: usize] -> Tensor[T, concat_dim(S, AXIS)]

// Stack along a new axis (tensors must have identical shape)
Tensor.stack(tensors: Slice[Tensor[T, S]], axis: usize) -> Tensor[T, [?, ...]]
// v1.5: stack[const AXIS: usize] -> Tensor[T, insert_dim(S, AXIS, N)]

// Reshape — total element count must match; panics at runtime if not
t.reshape(new_shape: Slice[usize]) -> Tensor[T, [?, ...]]
// v1.5: reshape[...NewS](t: Tensor[T, S]) -> Tensor[T, NewS] where prod(S) == prod(NewS)

t.flatten() -> Tensor[T, [?]]        // reshape to 1D

// Add / remove size-1 dimensions
t.expand_dims(axis: usize) -> Tensor[T, [?, ...]]
t.squeeze(axis: usize) -> Tensor[T, [?, ...]]   // panics if dim != 1
t.squeeze_all() -> Tensor[T, [?, ...]]          // removes all size-1 dims
```

---

### Set-Like Operations (`unique`, `searchsorted`)

**Decision:** Defer to Phase 11+ as stdlib functions on 1D tensors.

**Why deferred:** Pure stdlib work. `unique` output length is data-dependent (always `[?]`); `searchsorted` output shape matches the index array shape.

**Why non-breaking:** New stdlib functions.

**Design shape:**

```kara
// unique — deduplicated sorted values
let (vals, counts, inverse) = t.unique();
// vals:    Tensor[T, [?]] — sorted unique values
// counts:  Tensor[usize, [?]] — frequency of each unique value (optional)
// inverse: Tensor[usize, [?]] — index into vals that reconstructs t

// searchsorted — binary search in a sorted array
let idx = sorted.searchsorted(values, side: SearchSide.Left);
// idx: Tensor[usize, same shape as values]
// SearchSide.Left: first valid insertion point; SearchSide.Right: last
```

Both require `T: Ord`. `unique` always returns owned tensors.

---

### NaN and Inf Handling

**Decision:** Defer NaN/Inf predicates and NaN-ignoring reductions to Phase 8+.

**Why deferred:** Requires stable `Tensor` stdlib. NaN handling is a floating-point concern; the predicates are element-wise (no shape change) and the NaN-ignoring reductions follow the same shape rules as their non-NaN counterparts.

**Why non-breaking:** New methods and functions. No existing API affected.

**Design shape:**

```kara
// Predicates — element-wise, same shape as input
arr.is_nan()    -> Tensor[bool, S]
arr.is_inf()    -> Tensor[bool, S]
arr.is_finite() -> Tensor[bool, S]

// NaN-ignoring global reductions (treat NaN as absent, not as error)
arr.nansum()    -> T
arr.nanmean()   -> T
arr.nanmin()    -> T
arr.nanmax()    -> T
arr.nan_count() -> usize    // number of NaN elements

// Replace NaN with a fill value
arr.fill_nan(value: T) -> Tensor[T, S]
```

Axis-indexed NaN-ignoring reductions (`nansum[AXIS]()`) follow the same v1.5 deferral as axis reductions generally — they require shape arithmetic for the return type.

**Floating-point special values.** `f32` and `f64` gain associated constants:

```kara
f64.NAN      // Not-a-Number
f64.INF      // positive infinity
f64.NEG_INF  // negative infinity
```

These are value-level constants, not types. `Column[T]` uses bitmap nullability for missing data (distinct from NaN). Using NaN as a missing-value sentinel in a `Tensor` is discouraged — use `Column[T]` if nullability is semantic.

---

### `.npy` / `.npz` Array File I/O

**Decision:** Defer NumPy array file format support to Phase 8+ as `std.io.npy`.

**Why deferred:** The ML ecosystem uses `.npy`/`.npz` ubiquitously for saving and loading tensors (model weights, datasets, intermediate results). Arrow covers the data-engineering stack (Parquet, IPC), but the ML checkpoint workflow runs on `.npy`. Without this, users who load a pre-trained weight file must shell out to Python. No language changes required.

**Why non-breaking:** New stdlib module.

**Design shape:**

```kara
use std.io.npy;

// Single-array .npy
let arr: Tensor[f64, [?, ?]] = npy.load("weights.npy")?;   // shape inferred at runtime
npy.save("output.npy", arr)?;

// Multi-array .npz archive
let archive = npy.load_npz("checkpoint.npz")?;
let w1 = archive.get[f64]("layer1.weight")?;   // Tensor[f64, [?...]]
let b1 = archive.get[f64]("layer1.bias")?;

let mut builder = npy.NpzBuilder.new();
builder.insert("weights", w1);
builder.insert("bias", b1);
builder.save("checkpoint.npz")?;
```

Supported dtypes: `f32`, `f64`, `i32`, `i64`, `u8`, `bool`. Complex dtypes (`complex64`, `complex128`) are supported once the `Complex[T]` stdlib type is defined (see `deferred.md § Complex[T]`). All other dtypes surface as `IoError.UnsupportedDtype`. Fortran-order (column-major) arrays are loaded and converted to C-order via `.compact()` automatically. Effect annotation: `reads(Fs)` for load, `writes(Fs)` for save.

---

### `Complex[T]` — Complex Number Type

**Decision:** Defer `Complex[T]` to Phase 11+ as the canonical stdlib complex number struct. Must be the single shared definition — two libraries defining incompatible `Complex` types cannot interop (FFT output feeding a filter, `Tensor[Complex[f64], S]` crossing a module boundary, etc.).

**Why deferred:** Pure stdlib work — no language changes required. Validated alongside `std.fft` and `std.linalg`, which are its primary consumers.

**Why non-breaking:** New stdlib type. No existing API affected.

**Design shape:**

```kara
struct Complex[T: Float] {
    real: T,
    imag: T,
}

impl Complex[T] {
    fn new(real: T, imag: T) -> Complex[T]
    fn from_polar(r: T, theta: T) -> Complex[T]   // r * e^(i*theta)
    fn imag_unit() -> Complex[T]                   // 0 + 1i

    fn abs(ref self) -> T             // magnitude: sqrt(real² + imag²)
    fn arg(ref self) -> T             // phase angle in radians
    fn conj(ref self) -> Complex[T]   // conjugate: real - imag*i
    fn norm_sq(ref self) -> T         // real² + imag²  (avoids sqrt)
}

impl Add[Complex[T]] for Complex[T] { ... }
impl Sub[Complex[T]] for Complex[T] { ... }
impl Mul[Complex[T]] for Complex[T] { ... }   // (a+bi)(c+di) = (ac-bd) + (ad+bc)i
impl Div[Complex[T]] for Complex[T] { ... }   // multiply by conjugate / norm_sq
impl Neg for Complex[T] { ... }
impl PartialEq for Complex[T] { ... }
impl Display for Complex[T] { ... }   // "3+2i", "3-2i", "2i", "3"
impl Debug for Complex[T] { ... }
```

**Memory layout:** interleaved `[real0, imag0, real1, imag1, ...]` — matches FFTW's convention and C99's `_Complex` ABI, enabling zero-copy handoff to FFTW or CUDA complex kernels. `Tensor[Complex[f64], Shape]` is the canonical type for FFT output and complex-valued signal processing.

**No complex literal syntax in v1.** Users write `Complex.new(3.0, 2.0)` or `Complex.from_polar(r, theta)`. A `2.0i` suffix is deferred — it requires careful lexer work to avoid ambiguity with the integer suffixes `i8`, `i16`, `i32`, `i64`.

---

### std.crypto

Constant-time cryptographic primitives. Cryptography is one of the few domains where a wrong stdlib choice causes real-world security incidents — algorithm agility, side-channel-safe implementations, and a narrow default-secure API surface matter more than flexibility.

**Phase:** 10+ (P1). Blocked on FFI stabilization (Phase 9) so the implementation can delegate to a vetted C library (libsodium or similar) for the primitives themselves, rather than implementing raw cryptographic algorithms in Kāra.

**Why P1 (not P2):** Cryptography is not speculative — every networked application needs it, and getting the API shape wrong at the stdlib level is a long-term security liability. Committing the API shape now (even before Phase 10 implementation) prevents community libraries from proliferating incompatible interfaces that become impossible to consolidate.

**Algorithm choices (committed):**

| Purpose | Algorithm | Rationale |
|---|---|---|
| Authenticated encryption | ChaCha20-Poly1305 | Misuse-resistant; no padding oracles; fast in software; safe without hardware AES |
| Key exchange | X25519 | Widely deployed; constant-time Curve25519 DH |
| Signatures | Ed25519 | Deterministic; fast; small keys; no nonce reuse risk (unlike ECDSA) |
| Password hashing | Argon2id | Memory-hard; 2019 PHC winner; tunable time/memory cost |
| General hashing | BLAKE3 | Fast; parallel; keyed and extendable modes; not SHA-2 (which requires HMAC wrapping) |

**No algorithm agility in the default API.** `std.crypto.seal(key, plaintext)` takes a `ChaCha20Poly1305Key` — not a `dyn CipherKey`. Negotiating algorithms is the responsibility of protocol libraries (`std.tls` if it ever ships), not the primitive layer. Algorithm agility at the primitive level is where most cryptographic accidents originate.

**Effect annotations:**

```kara
// Key generation touches the OS entropy source
fn generate_key() -> ChaCha20Poly1305Key
    with reads(EntropySource), allocates(Heap)

// Seal / open are allocation-free for fixed-size output
fn seal(key: ref ChaCha20Poly1305Key, nonce: Nonce, plaintext: ref [u8]) -> Vec[u8]
    with allocates(Heap)

fn open(key: ref ChaCha20Poly1305Key, nonce: Nonce, ciphertext: ref [u8]) -> Result[Vec[u8], AuthError]
    with allocates(Heap)

// Hash is allocation-free if result is written to caller-provided buffer
fn hash(data: ref [u8]) -> [u8; 32]   // BLAKE3, stack-allocated output
```

`EntropySource` is a stdlib-declared resource representing OS-level entropy (`/dev/urandom`, `getrandom(2)`, `BCryptGenRandom`). Functions that read entropy must declare `reads(EntropySource)` — this makes entropy consumption visible at API boundaries and allows embedded/deterministic-testing profiles to forbid it via `no_effects`.

**Nonce handling:** nonces are explicit parameters (not hidden state) so callers must manage them. `std.crypto` provides a `NonceCounter` helper (increment-and-return, single-threaded) and a `RandomNonce` generator (reads entropy per call). This makes nonce reuse a visible programming decision, not a silent default.

---

### CircularBuffer[T]

A fixed-capacity ring buffer with O(1) push/pop at both ends and no heap reallocation after construction. The standard workhorse for audio DSP, networking packet queues, sensor data pipelines, and any producer/consumer pattern with bounded memory requirements.

**Phase:** 10 (P1). Does not block v1 ship — `Vec[T]` suffices for most use cases — but the absence of a ring buffer in the stdlib forces every audio and networking library to ship its own, leading to incompatible types at API boundaries.

**Why P1 (not P2):** The design is fully settled (classic ring buffer, no open questions), and the demand is deterministic — any audio, DSP, or real-time networking library will need this. The only reason it is not P0 is that the Phase 8 stdlib scope is already large; it is non-breaking and can land in v1 after MVP.

**Design shape:**

```kara
struct CircularBuffer[T] {
    // capacity fixed at construction; no reallocation
}

impl[T] CircularBuffer[T] {
    fn new(capacity: i64) -> CircularBuffer[T]
        with allocates(Heap)

    fn push_back(mut ref self, value: T) -> Result[Unit, Full]
    fn push_front(mut ref self, value: T) -> Result[Unit, Full]
    fn pop_back(mut ref self) -> Option[T]
    fn pop_front(mut ref self) -> Option[T]
    fn peek_back(ref self) -> Option[ref T]
    fn peek_front(ref self) -> Option[ref T]

    fn len(ref self) -> i64
    fn capacity(ref self) -> i64
    fn is_empty(ref self) -> bool
    fn is_full(ref self) -> bool
    fn clear(mut ref self)

    // Contiguous read window (for DMA / zero-copy I/O)
    // Returns one or two slices depending on wrap state
    fn as_slices(ref self) -> ([ref T], [ref T])
}
```

**No allocation after construction.** Every method after `new` is allocation-free — callers can include `push_back` / `pop_front` in functions that omit `allocates`, providing the same real-time guarantee as stack allocation with the flexibility of a queue.

**Overwrite mode (library extension, not v1 stdlib).** A non-erroring `push_overwrite` that evicts the oldest element is a common variant (audio capture ring buffers almost always want this). It is intentionally omitted from the core API to keep the default behavior explicit — `push_back` returning `Err(Full)` forces the caller to handle backpressure.

---

## P2 — Important Post-v1 Language Features

Important features deferred from v1; the language author or the community will build them post-v1. Each entry has a committed design or design shape; for items where the mechanism is genuinely uncertain, the entry names the conditions under which the design would solidify (the *promotion gates*) so the entry doesn't become indefinitely deferred. Distinct from P3, where the may-or-may-not question is open.

### Layout-Capability Bound (Type-System-Enforced Layout Requirements)

A type-system mechanism that lets a function require a specific physical layout (e.g., SoA) without making layout part of the type signature. Today, layout is a codegen specialization at the binding site — `Vec[Entity]` with SoA and `Vec[Entity]` without are the same type ([design.md § Layout Blocks](design.md#feature-1-explicit-data-layout)). This preserves "changing layout is not an API break," but leaves a gap: a GPU kernel that *requires* SoA can be passed AoS data, with the failure mode being a runtime perf cliff or wrong results. The current spec acknowledges this gap explicitly and routes layout requirements to documentation.

A future mechanism — a bound or attribute like `where Vec[T]: SoaLayout`, an `#[expects_layout(soa)]` attribute, or a structural trait derived from a binding's applied layout block — would let a function declare its layout requirement without forcing the requirement into every caller's type signature. The exact shape is open: too many plausible mechanisms, none chosen, all hypothetical until real GPU users hit the gap in practice.

**Why deferred:** GPU codegen is Phase 10. Designing a mechanism without real GPU code risks the wrong shape — the right design depends on what kernels users actually write, what diagnostics they want at the `gpu.dispatch` boundary, and whether a `karac explain` suggestion ("this kernel expects SoA but received AoS — try `layout entities { group ... }`") is enough or whether type-level enforcement is needed. Revisit when Phase 10 ships and a measurable corpus of `#[gpu]` code exists.

**Why non-breaking:** Any layout-capability bound added later is opt-in and additive. Existing function signatures (`fn process(data: Vec[Entity])`) continue to accept any layout. New signatures that opt in (`fn process(data: Vec[Entity] where Vec[Entity]: SoaLayout)`) gain compile-time enforcement; callers that previously passed AoS to such a kernel were already buggy at runtime — surfacing the bug at compile time is a behavior improvement, not a regression. No semantic change to existing code.

---

### Resource-Modeling Friction Lints

Compile-time advisory lints that flag suspicious resource-modeling patterns in user code: dense `independent A, B;` declarations across related resources (over-fragmentation hint), `resource` declarations that exist only to force ordering between independent operations (phantom-resource hint), and other heuristics for "you may be modeling resources too coarsely or too finely." Triggered through `karac build --perf-report`, not in normal compilation — the language-health-metric framing in [design.md § Auto-Concurrency via Effect Analysis](design.md#feature-5-auto-concurrency-via-effect-analysis) governs.

**Why deferred:** Without enough real-world Kāra programs, speccing specific lints is guesswork — patterns that look like misuse in one domain are legitimate in another. Revisit once production codebases reveal which patterns reliably indicate modeling errors.

**Why non-breaking:** Lints are warning-level, suppressible with `#[allow(...)]`, and do not change auto-concurrency decisions, conflict analysis, or runtime behavior. Existing programs continue to compile and run identically; users see new advisory diagnostics they may opt to act on.

---

### Promote Passing Test Assertions into Contracts (`karac test --suggest-contracts`)

A `karac test` mode that, for each *passing* assertion in the test corpus, emits structured suggestions mapping the assertion expression to candidate `requires` / `ensures` clauses on the function under test. The natural inverse of derivation chains: an LLM authors a test, the compiler proposes a contract, the next build either statically discharges the contract (free) or surfaces it as a runtime check (declared cost). This is the only place in the design where test artifacts feed back into the declarative surface of the language.

Concrete example: `test_sort_preserves_length` asserting `assert_eq(sort(v).len(), v.len())` becomes a candidate `ensures(result) result.len() == v.len()` clause on `sort`. The compiler would emit:

```json
{
  "type": "suggest_contract",
  "function": "sort",
  "function_file": "src/sort.kara",
  "function_line": 14,
  "kind": "ensures",
  "predicate": "result.len() == v.len()",
  "evidence_test": "math_test::test_sort_preserves_length",
  "static_discharge": "likely",   // or "unlikely" / "uncertain"
  "derivation": [...]
}
```

**Why deferred (P2, not P1):**

The translation/inference quality is genuinely uncertain. Three open questions block a confident v1 commitment:

1. **Specific-vs-universal classification.** Most assertions check specific cases (`assert_eq(add(2, 3), 5)`) — not contract candidates. Some express universal claims (`assert_eq(sort(v).len(), v.len())`) — good candidates. Distinguishing these requires identifying which assertion variables are bound to function arguments vs. literal test inputs. Heuristic at best; without a Kāra test corpus, we can't calibrate the heuristic.
2. **Pre-condition inference is much harder than post-condition.** A test that happens to pass non-empty `v` to `find_min(v)` doesn't logically *require* non-empty input — the test just doesn't exercise the empty case. Inferring `requires` from "tests that happen to pass" is unsound; the spec should focus on `ensures` first.
3. **Static-discharge integration is the unique compiler value-add.** LLM clients can analyze passing tests for contract candidates from source today via prompting — but only the compiler can tell whether a candidate would be statically dischargeable (free) or would add runtime check cost on every call. That value-add depends on Phase 9 contract prover maturity.

**Promotion gates** (P2 → P1 — when to revisit):

- Phase 9 contract prover handles `len` / equality / arithmetic refinements reliably (the static-discharge story works for the common assertion shapes the tool would surface).
- A corpus of ≥10 real Kāra projects with substantive test suites exists, providing calibration data for the specific-vs-universal classification heuristic.
- A prototype implementation, run against the corpus, shows an honest acceptance rate for proposed contracts — a number the prototype itself reveals, not pre-committed.

If those gates are met, this becomes a P1 v1.x feature. If they aren't, the entry stays P2 with the gates documenting what's missing.

**Additive to the LLM TDD loop.** This is *additive* to the `karac tdd` Watch Driver capstone (above), not a prerequisite. The watch driver — together with envelope unification, the cycle-summary status taxonomy, test-selection flags, `karac test --init` scaffolding, and the signature-from-call-site stub diagnostic — ships fine without contract suggestion. The value here is the *next layer*: once the loop is humming and contracts are mature, suggestions feed test artifacts back into the declarative surface. The capstone is never blocked on this entry.

**Why non-breaking when shipped:** new `karac test --suggest-contracts` flag (default off); new JSONL `suggest_contract` event slots into the existing schema discriminator; existing consumers ignore unknown event types. No language-surface change — suggestions are advisory output, not enforced code modifications. The user (or LLM client) decides whether to accept any given suggestion.

---

### Auto-Derived `Arbitrary` and `Shrink` Honoring Refinements and Invariants

Extend `#[derive(Arbitrary)]` to automatically produce property-test generators *and* invariant-respecting shrinkers for types carrying refinement predicates or `invariant` blocks. Today, `#[derive(Arbitrary)]` generates fields independently — types with non-trivial constraints must hand-write `Arbitrary` and `Shrink` (per design.md § Property tests). For LLM-driven property testing, this is the largest grunt-work tax in the test surface; the proposal is to remove it by letting one piece of source — the refinement predicate or invariant — do triple duty: type rule, contract, and test-input generator.

**Two-strategy generator:**

1. **Direct constructor** when the predicate is *structural* — the compiler recognizes a fixed catalogue of patterns it can satisfy by construction without rejection. Examples: `x > N` / `x >= N` / `x < N` / `N <= x < M` (numeric ranges → generate within the satisfying interval), `x.len() > 0` (`Vec` / `String` → generate at least one element), `s.is_ascii()` (`String` → generate from ASCII alphabet), conjunctions of recognized patterns. Output: a generator that produces only valid values, no rejection cycle.
2. **Rejection filter** when the predicate is non-structural (`is_prime(x)`, arbitrary user functions). Output: generate the underlying type, evaluate the predicate, retry on failure. Configurable bailout — abort after N rejections with a structured diagnostic (`refinement_unsatisfiable` or similar) rather than hanging indefinitely.

**Invariant-respecting shrinker.** When a property test fails on `xs: Vec[PositiveI32]`, the shrinker walks toward smaller-but-still-valid inputs. Shrinking `[5, 3, 2]` to `[5, 3, 0]` violates `PositiveI32`'s refinement — the shrinker must reject that step. For refinement types on single fields this is straightforward (filter shrink candidates through the predicate). For struct-level `invariant` blocks involving multiple fields conspiring (e.g., `start <= end`), the shrinker must either co-shrink the conspiring fields or reject shrink steps that break the invariant. Either approach works; the right choice depends on shrinking quality.

**Why deferred (P2 with promotion gates, not P1):**

The reviewer flagged this entry's open questions explicitly: *"Worth a separate design pass before committing, because the rejection-vs-construction split has real implications for shrinking quality and test runtime."* Three substantive open questions:

1. **Predicate-pattern catalogue.** Which patterns should the structural-constructor recognize? Too narrow → most refinements fall back to rejection (slow). Too broad → the compiler ships a sprawling pattern matcher that's hard to maintain. The right catalogue is empirical, calibrated against real refinement usage in real Kāra programs.
2. **Invariant-aware shrinking is research territory.** No widely-deployed PBT framework (QuickCheck, Hypothesis, proptest, jqwik) has solved invariant-respecting shrinking generically. The naive "filter shrink steps through the invariant" approach can produce poor shrinking quality (the shrinker gets stuck in local minima where every step violates the invariant). Constraint-solving alternatives are more general but expensive.
3. **Bailout-default calibration.** What's the right default rejection bailout? Too low → false-negative test failures ("no inputs found"). Too high → tests hang on impossible refinements. The right default is empirically calibrated, not theoretically derivable.

**Promotion gates** (P2 → P1 — when to revisit):

- Phase 9 refinement type system and `invariant` blocks are mature (the substrate this feature derives from is stable enough to commit to).
- A prototype implementation of structural-pattern recognition exists, with a *measured* catalogue size that handles the common cases reflected in real Kāra programs.
- An invariant-respecting shrinker prototype shows acceptable shrinking quality on a benchmark suite — the prototype itself defines the threshold, since "acceptable shrinking" depends on the corpus. Either the rejection-filter approach is empirically good enough, or a constraint-solving approach has demonstrably better quality at acceptable runtime cost.
- A corpus of ≥10 real Kāra projects with substantive refinement-typed property tests exists, providing calibration data for the bailout default and the structural-pattern catalogue.

If those gates are met, this becomes a P1 v1.x feature. Until then, hand-written `Arbitrary` and `Shrink` impls remain the supported path for types with non-trivial constraints — annoying but tractable.

**Additive, not blocking.** Like the contract-suggestion entry above, this is *additive* to the LLM TDD loop. Property tests with refinement-typed inputs can be written today by hand-implementing `Arbitrary` / `Shrink`; the `karac tdd` Watch Driver capstone, its sub-features (envelope unification, cycle-summary status, test-selection flags, scaffolding, signature stub), and the contract-suggestion entry all ship fine without auto-derived `Arbitrary` / `Shrink`. The value here is removing a specific grunt-work tax once the substrate is mature.

**Why non-breaking when shipped:** extension to existing `#[derive(Arbitrary)]` and a new `#[derive(Shrink)]` (or expansion of the existing derive) — purely additive macro behavior. Existing hand-written `Arbitrary` impls are unaffected (they're hand-written, not derived). New refinement-typed types that opt into the derive get the auto-generation; types that don't keep using hand-written impls.

---

### Gradual Verification (Level 3-4)

SMT solver integration for proving contracts at compile time. May never be built — the cost/benefit ratio depends on how far the effect system and refinement types take the language without formal verification.

---

### Typed `einsum` with Compile-Time Index Checking

Named index dimensions checked at compile time, eliminating string parsing and all runtime shape errors from contraction expressions.

```kara
// Hypothetical syntax — named index dims as const-generic symbols
let c  = einsum[i j, j k -> i k](a, b);   // K-dim mismatch caught at compile time
let tr = einsum[i i ->](a);               // diagonal constraint enforced statically
```

Requires either a proc-macro equivalent or new generic symbol kinds in the type system. The string-notation `einsum` (P1 above) covers the practical use case; typed einsum is an ergonomics and safety improvement. Revisit once comptime is stable and the numerical stdlib has real-world usage data.

---

### Units of Measure

F#-style dimensional analysis at the type level: `Meters`, `Seconds`, `Newtons`, etc. as phantom type parameters, with compiler-enforced dimensional correctness (`Meters / Seconds` is `MetersPerSecond`; `Meters + Seconds` is a type error).

```kara
// Hypothetical syntax — not committed
type Meters   = f64 tagged Meters
type Seconds  = f64 tagged Seconds
type MetersPerSecond = f64 tagged Div[Meters, Seconds]

let distance: Meters  = 10.0<m>
let time: Seconds     = 2.0<s>
let speed             = distance / time   // : MetersPerSecond, inferred
let wrong             = distance + time   // compile error: Meters ≠ Seconds
```

**Status: explicitly deferred (not absent).** Units-of-measure checking is a well-understood, high-value feature for scientific computing and embedded control systems (NASA Mars Climate Orbiter, medical device dosing errors, avionics unit bugs are canonical examples of what static dimensional analysis prevents). It is deferred — not rejected — because:

1. **Type system prerequisite.** F#-style units require phantom type parameters or a dedicated dimension-kinded parameter that participates in type inference. Kāra v1's generic system does not yet have dimension-kinded parameters. Adding them is a significant type-system extension, not a library concern.
2. **Syntax is unsettled.** `10.0<m>`, `10.0[m]`, `10.0 m`, and `@meters(10.0)` are all plausible; the right choice depends on how the literal suffix system and generics interact.
3. **Not a post-v1 breaking change.** Unit types introduced later do not need to affect existing code — a `Meters` tagged type can be introduced as a new stdlib type without breaking any programs that use plain `f64`.

**Revisit trigger:** comtime stabilizes (Phase 10+) and at least one scientific-computing library author files a concrete use case with a proposed syntax.

---

### Bidirectional Compiler Hints

Compiler suggests code changes to the AI; AI suggests optimization strategies to the compiler. Waiting for real AI-assisted development usage to reveal whether this is valuable.

---

### Machine-Verifiable Intent Annotations

Programmer states intent in a machine-checkable form beyond contracts. Depends on a verification system that doesn't exist yet. Waiting for real AI usage patterns.

---

### Flow-Sensitive Refinement Narrowing (Restricted)

Within the `then` branch of `if x > 0 { ... }`, automatically narrow `x` to the matching refinement type (e.g., `Positive`) without requiring `Positive.try_from(x)?`. Restricted to: immutable local bindings only (not `mut`, not a parameter, not a closure capture, not reassigned), syntactic predicate match against a refinement type's constraint, single-function scope. No closure interaction, no cross-scope reasoning, no mutable rebinding. This avoids the complexity of general flow-sensitive narrowing while covering the common case of simple numeric predicates after a guard.

---

### Homogeneous Varargs

Variable-length parameter lists where every argument has the same type: `fn sum(nums: ...i64) -> i64`, called as `sum(1, 2, 3)` or `sum()`. Inside the function, `nums` is received as either a slice (`ref [i64]`, zero-allocation) or an owned `Vec[i64]` — design choice. Distinct from *Heterogeneous Varargs* (below), which allows each argument to have a different type tracked at the type level.

**Motivating use cases:** builder-style APIs (`query.where_in("id", 1, 2, 3)`), N-ary constructors (when combined with the `Call` trait, enabling `Set(1, 2, 3)`), and generic helpers that accept "any number of Ts" without forcing callers to wrap arguments in `[...]` or `Vec.from([...])`.

**Not needed for:** `println`/`format`-style functions. Kāra's f-strings (`f"hello {x} {y}"`) already cover that use case more ergonomically than varargs would — one argument, first-class interpolation, no runtime format-string parsing.

**Design questions to settle:**

1. **Received type.** Slice (`ref [T]`) is zero-allocation but read-only; `Vec[T]` is owned but forces heap allocation on every call (and would contribute `allocates(Heap)` to the caller's inferred effects even for three-element calls). Slice is probably the right default, with opt-in `Vec` via a trailing `.collect()` inside the body.
2. **Position restriction.** Almost certainly last-parameter-only; anywhere-in-the-signature varargs creates genuine ambiguity with default-valued parameters (which Kāra has — `docs/design.md:2576`).
3. **Zero-arg calls.** `sum()` — allowed (empty slice) or compile error? Allowed is simpler and matches Go/Java.
4. **Interaction with default parameter values.** A signature like `fn f(x: i64 = 0, ...rest: i64)` needs clear rules for which positional args go where.

**Why deferred:** Kāra's f-strings and array-literal coercion absorb most practical varargs pressure. The remaining use cases (builder APIs, N-ary constructors) are nice-to-haves. Revisit once concrete examples from real Kāra code accumulate — if the pattern keeps appearing with `[...]`-wrapped args, that's the signal.

**Why non-breaking:** Purely additive. New `...T` parameter-declaration syntax; existing parameter declarations unchanged. Call sites `f(1, 2, 3)` remain well-defined against fixed-arity signatures.

---

### Heterogeneous Varargs / Variadic Generics

Type-level variable-length parameter lists where each argument can have a different type, tracked statically. Syntax sketch: `fn row[Ts...](values: Ts...) -> Row[Ts...]`. This is the type-system-heavy cousin of *Homogeneous Varargs* — much more powerful but requires comptime/const-generics infrastructure (Phase 7-8).

**Motivating use cases:**

- Lifting the `collect_all` 8-branch arity cap in the stdlib.
- Generic `zip`, `map_all`, and similar N-ary combinators across collections of different element types — no more fixed-arity overload explosion.
- Typed heterogeneous tuples for ORM-style row types (`Row[String, i64, bool]`).
- Multi-arg `Call` sugar — `Set(1, 2, 3)` via a variadic `impl Call[Ts..., Set[T]]` where all `Ts` unify to a common bound.

**Why deferred:** No committed design. Const generics ship in v1 (see `design.md` § Type Inference > *Const generic parameters*), so the compile-time-value half of the type system is already settled; the remaining unknown is how comptime / type reflection shape the generic-list machinery once user code can synthesize types. Variadic generics is genuinely hard — every mainstream language that has it (C++ parameter packs, Scala HList, Haskell type-level lists) ended up with a heavyweight design. Kāra should have a clear picture of its comptime model before committing to a shape here.

**Promotion gate (P2 → P1 / scheduled).** Do **not** design now. Promote out of P2 only when *use cases beyond fixed-arity `collect_all` materialize* in real Kāra code. The named criterion is deliberately narrow because the existing fixed-arity pattern (an 8-branch overload set covering tuples up to length 8, like the stdlib's `collect_all`) is *already* enough for >95% of "N collections at once" needs in practice — promoting on the strength of that pattern alone would be a heavyweight design move serving a problem the workaround already solves. The gate fires when *all three* of the following are observable in committed Kāra code (stdlib or external crates with broad usage), not just hypothetical:
1. **Recurring user code that hits the arity cap.** Concrete `collect_all_9`, `zip_10`, ad-hoc tuple-of-9 patterns showing up across multiple unrelated projects — not one specialised library.
2. **Non-tuple shapes.** Use cases that genuinely need *type-level* heterogeneity beyond what a fixed-arity overload set can express: ORM row-type families, typed message schemas, builder APIs whose argument types depend on prior arguments. If everything reduces to "N parallel iterators of homogeneous element type each," homogeneous varargs (the entry above) is the better promotion.
3. **A workable interaction story with the comptime model.** Heterogeneous varargs and `comptime fn` (item 31) overlap at the type-synthesis layer; promotion presupposes the comptime substrates have shipped and the design can express variadic generics *as* a comptime/type-reflection pattern rather than a parallel mechanism. Promoting before comptime ships risks committing to a shape that comptime later subsumes or contradicts.

If only criterion (1) fires and (2)/(3) do not, the right answer is to extend the fixed-arity overload set (e.g., raise `collect_all`'s cap from 8 to 12) — not to ship variadic generics. If (2) fires under (3) without (1), document the use cases and revisit at the next edition gate; isolated demand is not enough to justify the design cost.

**Why non-breaking:** Purely additive. New type-level syntax on generic parameter lists; existing generics (`[T]`, `[T: Ord]`, `[T, U]`) unchanged.

**Cross-reference:** **User-Defined Callable Types (`Call` trait)** — below. `Call` + heterogeneous varargs is what unlocks Python-style `Vec(1, 2, 3)`; `Call` without varargs only delivers single-argument sugar.

---

### Call-Site Spread

Expand an existing collection into positional arguments at a call site. Sketch (syntax TBD): `let xs = [1, 2, 3]; f(...xs)` where `f: fn(i64, i64, i64) -> T` or `f: fn(...nums: i64) -> T`. Dual of varargs on the caller side; orthogonal to both varargs flavors above — works with fixed-arity signatures, homogeneous varargs, or (eventually) heterogeneous varargs.

**Design questions to settle:**

1. **Syntax choice.** `...xs` (JS/TS), `*xs` (Python — collides with dereference in Rust-family), `xs: _*` (Scala). Must not collide with Kāra's existing `..` and `..=` range syntax or the parameter-declaration form from *Homogeneous Varargs*. Leading `...` on an expression is probably safe.
2. **Arity checking.** Reject at compile time when the collection's length doesn't match the target arity (possible for `Array[T, N]` with statically-known length; impossible for `Vec[T]` / slices). Runtime check otherwise.
3. **Position.** Trailing only, or anywhere in the argument list? Trailing is simpler; anywhere enables `f(a, ...middle, z)`.
4. **Mixed with named/default args.** Interaction with Kāra's default parameters needs explicit rules.

**Why deferred:** Niche ergonomic convenience. Workarounds exist today (`f(xs[0], xs[1], xs[2])` for known arity, or redesigning `f` to accept a slice). Not blocked on varargs — can ship independently if the need arises.

**Why non-breaking:** Purely additive. New expression-position syntax (`...expr`); no existing parse rule uses a prefix `...` at the expression level.

---

### User-Defined Callable Types (`Call` trait / `apply`)

Allow user types to be invoked with parens-call syntax by implementing a callable trait, paralleling Scala `apply`, Kotlin `invoke`, Python `__call__`, and Swift `callAsFunction`. The natural shape:

```kara
trait Call[Args, Output] {
    fn call(self: ref Self, args: Args) -> Output with _
}
```

Call-site sugar: `t(x, y)` desugars to `t.call((x, y))` whenever `t: impl Call[(A, B), _]`. Closures already implement this family implicitly; the feature would simply unseal it for user types and unify the closure-vs-user-callable distinction into one trait.

**Motivating use cases:** memoized functions, interpolation tables, parser combinators, validators, and DSLs that want a function-like surface without naming the type at every call site. A secondary (smaller) payoff is sugar for conversions like `Set(words)` in place of `Set.from(words)` — but only for single-argument cases without variadic generics.

**Key design decisions to settle before implementation:**

1. **Tuple-struct construction interaction.** `Point(1.0, 2.0)` is direct tuple-struct init today. Either auto-derive `impl Call` for every tuple struct (clean unification, non-breaking pre-1.0) or keep tuple-struct init as a parser-level-precedence rule that runs before `Call` dispatch. Auto-derivation is cleaner but formally makes tuple structs a special case of the callable mechanism.
2. **Enum-variant construction.** Probably *not* subsumed — variants carry a discriminant that's semantically distinct from arbitrary callable dispatch. `Some(x)` stays variant construction.
3. **Relation to `From` / `.from`.** Not subsumed. `From` carries conversion-specific semantics (reversible via `Into`, used by `?` for error widening). `Call` is more general; they coexist.
4. **Orphan rules.** Whether third-party modules can `impl Call[X] for StdType` needs an explicit rule; default should be "no" — otherwise any type in the ecosystem becomes arbitrarily callable by downstream code.
5. **Diagnostic quality.** Non-callable types hit with parens-call need a specific error naming the fix: "type `T` is not callable; implement `Call[Args, _]` or use an associated function such as `T.new(...)`."
6. **Effect integration.** Clean — `fn call(...) with _` plus per-impl effect variable. No new machinery needed.

**Why deferred to P2:** The construction-sugar ergonomic payoff is crippled without heterogeneous varargs — `Vec(1, 2, 3)` requires a 3-arity `Call` impl distinct from the 2-arity and 4-arity ones, which scales poorly. The unification-of-closures-and-user-callables payoff is real but modest in a systems-oriented language where the callable-object pattern is rarer than in DSL-heavy or scientific-computing languages. Better to revisit once **Heterogeneous Varargs / Variadic Generics** (above) has a committed design — `Call` plus heterogeneous varargs together is what makes this genuinely useful; `Call` alone delivers at most single-argument sugar.

**Why non-breaking:** Purely additive. Existing code using explicit associated-function names (`T.from(x)`, `T.new()`, `T.with_capacity(n)`) is unaffected. Opt-in per type via trait impl.

**Cross-reference:** **Heterogeneous Varargs / Variadic Generics** (above). Should be decided together or in sequence — `Call` without varargs is strictly less valuable.

---

### Formal Specification as Primary Artifact

The spec becomes a formally verifiable document (not just prose). Only meaningful if pre/post conditions (Level 3) land; effect annotations are a lightweight precursor. Revisit if Level 3 ships.

---

### Struct Literal Type Prefix in Check Mode

Whether the struct literal prefix (`WordCount { total: 42, unique: 30 }`) should remain required in every position (status quo) or become elidable to `{ total: 42, unique: 30 }` when a unique target struct type is known from context — return type of the enclosing function, `let x: T = ...`, argument position, or a nested struct-literal field value.

**Current lean:** ~55/45 toward elidable-in-check-mode, weakly held. Consistency is the deciding factor: Kāra already infers generic type arguments (`Vec.filled(5, 0) → Vec[i64]`), integer literal types (`let x: u8 = 42`), and closure parameter types from check-mode context (grammar accepts, typechecker errors if unresolved). Requiring the struct-literal prefix is the only redundant annotation Kāra currently mandates in a check-mode position. The "semantics-Rust, syntax-mainstream" tiebreaker also favors elision — C# target-typed `new()`, Swift `.init(...)` on typed target, Java record target-typing all elide.

**Strongest counter-argument:** local readability in long functions — a reader shouldn't have to trace outward to the return signature, let-binding, or call site to identify the type of a brace-literal. Real but not a consistency argument. An unexplored alternative (`.{ ... }` à la Zig as a distinct "infer-from-context" syntax) is rejected — second syntax for a minor ergonomic gain.

**Why non-breaking:** Purely additive. Existing code with explicit prefixes continues to work under either resolution. Elision is opt-in at the construction site.

**Re-evaluation criterion:** Revisit once enough real Kāra code exists to count how often the prefix is genuinely redundant vs. load-bearing for local readability. Heuristic: if >80% of struct literals sit next to a target annotation within ~3 lines, favor elision; if long functions with deeply nested literals are common and the prefix materially helps reading, keep status quo.

**Backstop:** Must decide before `docs/book/src/` introduces struct literals to external readers in any tutorial (ch1/ch2 of the getting-started chain). Syntax shown there becomes muscle memory and is costly to change afterward.


---

### Lint on Explicit `ref T` for Copy Primitives

Whether the compiler should emit a non-fatal diagnostic when a programmer writes `ref T` in a parameter list for a small Copy primitive (`i*`, `u*`, `f*`, `bool`, `char`) where bare `T` (owned) would carry the same information in less machine code. The pessimization: `ref i64` is an 8-byte pointer with one indirection; owned `i64` is the 8-byte value itself — same argument size, one fewer load. Under 2A all modes are declared explicitly, so the question is whether the compiler flags a declared `ref` on Copy primitives as likely-unintentional.

**Current lean:** (a) silent at source level — no lint. Rely on (i) `karac explain` to surface inferred-vs-written modes on demand, and (ii) standard backend optimizer passes (argument promotion, inlining + SROA) to narrow the observable perf gap between `ref` and `own` on small Copy types at the machine-code level.

**Guiding principle:** parameter modes are part of a function's semantic signature, not optimizer hints. They govern what the callee can observe and do, participate in trait coherence, and are visible to external callers. The compiler must not silently rewrite them. Performance recovery belongs in the backend, where `ref i64` can be lowered to a register-held value without changing the source-level contract. Frontend lint/rewrite conflates two concerns that Kāra keeps separate.

**Why not a lint (c):** competes with `karac explain` for the same user-facing teaching role. Every viable threshold rule has problems — R4 (primitives only) fires where the pessimization is most obvious and misses tuples-of-primitives where it's most confusing; R1/R2 couple the lint to ABI heuristics. A lint framework plus attribute/suppression syntax are larger spec commitments than this single lint justifies.

**Why not auto-rewrite:** breaks trait conformance (impl signatures must match their trait), violates the "declared modes are the public contract" principle on which `docs/design.md:114` stability depends, discards non-perf reasons to write `ref` (documentation signal, signature uniformity across Copy/non-Copy instantiations, future-proofing), and creates source-to-codegen mismatches that confuse performance profiling.

**Why non-breaking:** adding a lint later is additive. Removing would be too. Either direction is safe from a compatibility standpoint.

**Re-evaluation triggers (all required):**

1. A corpus scan of real Kāra code shows a non-trivial number of explicit `ref <primitive>` parameters (heuristic: ≥5 instances across examples/tests/ecosystem after Phase 9-10).
2. `karac explain` has shipped and empirically failed to close the teaching gap for the patterns found above.
3. A general lint framework exists for reasons independent of this specific lint (i.e., there are ≥2 other lints pending that would justify the framework cost).

If any trigger is absent, skip — the lint is dead weight against `explain` + backend passes.

**Why P3 (not P2):** the lint addresses a narrow pattern that the language design already discourages at the teaching level (the idiomatic spelling for Copy primitives is bare `T`, not `ref T`). Its teaching value is duplicated by `karac explain`. Its perf value is recoverable in the backend. The cost of the infrastructure it would require (lint framework, attribute syntax) is disproportionate to a single warning.


---

### Stdlib Scope for Non-Primitive Resources

Whether the Kāra stdlib should ship opinionated traits for common non-primitive resource categories (SQL connections, HTTP clients, KV caches, message queues, etc.) or leave all non-primitive categories to the ecosystem. Built-in *primitive* resources (`FileSystem`, `Clock`, `Env`, `Network`, `Stdin/Stdout/Stderr`, `RandomSource`, `Heap`, `Hardware`, `GpuBuffer`, per `docs/design.md:3336-3353`) are hardwired by the compiler/stdlib and not in question — they're the language-level set with compiler-known verbs. Everything else (databases, caches, HTTP clients, queues, vendor APIs) currently requires user- or library-written traits.

**Current lean:** (a) thin stdlib — ship only primitive resources. Non-primitive categories are ecosystem-defined. Rust's model. Matches `docs/design.md:2347`'s posture of a minimum-viable-I/O stdlib surface.

**Rejected alternatives:**

- **(b) Opinionated stdlib** — ship `std.sql.Connection`, `std.http.Client`, etc. — premature. Kāra has no ecosystem yet; choosing the 3–5 categories and their trait shapes without real-world usage data is pure speculation. A bad `std.sql.Connection` is harder to fix than no `std.sql.Connection`. Go's `database/sql` is often cited as a success, but the ecosystem that validated its shape existed first; Kāra does not have the corresponding corpus.
- **(c) Marker traits only** — ship empty marker traits (`std.resource.Sql`, `std.resource.Http`) for category-level tooling. Unclear what problem this solves. The effect system already treats every resource as independent; parallelism analysis doesn't need categories. Thin value proposition and risk of cargo-culting.
- **(d) Drop the trait bound on `effect resource` entirely** — breaks the `with_provider` injection model (`docs/design.md:3909-3982`), breaks the test-substitution story, requires significant spec rewrite. Not viable.

**Why non-breaking later:** Adding stdlib traits is additive. Existing user-written traits continue to work. Libraries that want to implement the new stdlib trait do so voluntarily. The only compatibility risk is name collision (a user's `my_app.sql.Connection` won't collide with `std.sql.Connection` because they're in different modules), which is manageable.

**Re-evaluation triggers (both required):**

1. A package manager / registry exists and two or more independent libraries have shipped in *at least one* category (SQL, HTTP, cache, queue).
2. The shapes of those libraries' core traits have converged enough that a stdlib trait would codify consensus rather than impose opinion. Heuristic: at least two independent libraries share ≥70% of method signatures on the "connection" or "client" primitive.

If either condition is absent, skip — the stdlib trait would be a bet on a shape that hasn't been tested.

**Why P3 (not P2):** entirely speculative at this phase. No ecosystem, no empirical shape data, no urgent forcing function. The question is aspirational — what posture Kāra takes once the ecosystem starts forming. Until then, any stdlib commitment is pure assumption.

**Pre-resolution task (small, not a deferred item):** when the book chapters introducing the effect system are drafted — particularly wherever `DatabaseProvider` appears (currently `examples/phase0/dashboard.kara`) — add a one-line note clarifying that `DatabaseProvider` is an illustrative *user-written* trait, not a stdlib import. Removes the ambiguity. Do when the book sections are written; don't churn unwritten prose now.


---

### Language-Integrated Query (SQL DSL) and ORM

Whether Kāra grows a language-level query mechanism — either an embedded query syntax (LINQ / F# query expressions / sqlx-style compile-time-checked SQL strings) or a struct-to-table ORM framework (derive-macro-driven mapping, Diesel / SQLAlchemy / ActiveRecord shape).

**Distinct from** "Stdlib Scope for Non-Primitive Resources" (above), which covers the *runtime driver* question (`std.sql.Connection`). This entry is about language-level query integration on top of whatever driver exists. The two axes are orthogonal: the driver question is ecosystem-shape; the query-integration question is whether Kāra spends language-design budget on a SQL-specific surface.

**Current lean:** no language-level query DSL or ORM in v1. Users write plain function calls against whatever driver ships (user-space first, stdlib eventually per the entry above). Compile-time-checked SQL strings, if they appear, start as a library using f-string interpolation + a user-written `sql!(...)` macro once macros exist — not a language feature.

**Why deferred (not rejected):**

1. **Contracts and refinement types are load-bearing for the interesting version of this.** A SQL DSL whose distinguishing value over plain strings is *type-checked column access, row schemas, and query-composition safety* needs refinement types and compile-time row-shape tracking to land first. Shipping a DSL before those would force early commitments (how does "column exists" check at compile time? how are join row types represented?) without the primitives that make the answers clean.
2. **Comptime / heterogeneous varargs interact.** Typed row shapes like `Row[String, i64, bool]` are already named as a motivating case for *Heterogeneous Varargs / Variadic Generics* (this file, above). A query DSL that returns strongly-typed rows depends on that feature's shape. Committing to DSL syntax before variadic generics is decided is a retrofit trap.
3. **ORM shape is an ecosystem question.** Go (`database/sql` → sqlx → sqlc → gorm), Rust (Diesel vs sqlx vs SeaORM), and Python (SQLAlchemy Core vs ORM vs Django ORM) all show the same pattern: the community explores several shapes before a consensus lifts. Kāra has no ecosystem yet; an ORM chosen now would be a bet on a shape that hasn't been tested.
4. **Effect system covers the correctness floor already.** `reads`/`writes` on a user-defined `Database` resource plus user-written `DatabaseProvider` trait already deliver the "this function touches the database" story. A DSL adds ergonomics and compile-time schema checking but not a new safety primitive.

**Why non-breaking later:** Purely additive — a new syntactic form for queries, or a new derive-macro for row structs. Existing plain-function-call driver usage continues to work. A library-level `sql!(...)` macro (once macros exist) is forward-compatible with any later language-integrated form.

**Re-evaluation triggers (any one of):**

1. Refinement types (`design.md § Refinement Types`) land and stabilize, AND ≥1 user-space query-builder or ORM library has shipped and its shape suggests language-level lift would deliver value the library can't.
2. Compile-time-checked SQL strings appear as a recurring request after the macro system ships, with a clear pattern of what the library version cannot express.
3. A concrete refinement-types + effect-system interaction emerges that would make Kāra's version genuinely distinctive (e.g., effect-tracked query composition, or refinement-typed WHERE clauses that prove index usage at compile time). If the version Kāra could ship is just "LINQ, again, in Kāra syntax," skip — the value-add doesn't justify the language budget.

**If none of the triggers fire:** query integration stays library-level indefinitely. Plain-function-call drivers + a community `sql!` macro are the permanent answer. That is a valid end state, not a failure mode.

**Cross-reference:** **Stdlib Scope for Non-Primitive Resources** (above) — driver question; **Heterogeneous Varargs / Variadic Generics** (above) — typed row shapes; `design.md § Refinement Types` — prerequisite for the distinguishing version of this feature.

---

### Compiler-Managed Transparent Threading on WASM

Kāra's ownership system proves data-race freedom at compile time. In principle, the compiler can use that property to automatically partition a WASM program across Web Workers + SharedArrayBuffer with **zero user annotation** — a `go { ... }` spawn transparently becomes a cross-worker boundary without any `--features wasm-threads` flag and without any worker/postMessage code in user space. Optional layering of WASM stack-switching gives fiber-weight tasks over a small worker pool.

**Current lean:** not in v1. Phase 10 ships sequential-by-default + `--features wasm-threads` opt-in — see `design.md § Concurrency Across Targets`. Transparent threading is a substantial research + engineering commitment that would stall Phase 10.

**Why deferred (not rejected):**

1. **Phase 10 needs to ship.** Getting a WASM backend working with a baseline concurrency story is prerequisite to learning what users actually need. Committing to transparent threading as the v1 story invites either missing the phase or shipping a half-working version that poisons the differentiator claim.
2. **The differentiator claim is real.** No other language has both (a) compile-time-proven data-race freedom and (b) a first-class browser story. If the transparent-threading lowering lands correctly, Kāra says something that Rust, Go, and JavaScript cannot. That is worth doing — *after* Phase 10 establishes the baseline.
3. **The WASM concurrency platform is still moving.** The W3C shared-everything-threads proposal may relax SAB's COOP/COEP requirement; WASM stack-switching is mid-landing in browsers. Designing against SAB today and redesigning against shared-everything-threads tomorrow is churn — a single re-evaluation after those proposals stabilize is cheaper than shipping twice.
4. **The language-level cost is already paid.** Source-level commitments already in place (`go`/channels target-agnostic, ownership transfer through channels specified once, data-race freedom as a language property) mean the transparent-threading lowering can land non-breaking at any future point.

**Why non-breaking later:** source commitments in `design.md § Concurrency Across Targets` guarantee that swapping the WASM lowering from sequential-default to transparent-multi-worker is additive. The source-level surface does not change; programs that use the opt-in `--features wasm-threads` flag keep working; programs that did not opt in gain throughput without code changes when the compiler's partitioning lands.

**Re-evaluation triggers (any one of):**

1. A real user-space workload demonstrates `--features wasm-threads` opt-in is insufficient — the COOP/COEP opt-in ceremony is a deployment blocker *and* ownership-proven data-race-freedom is load-bearing for the program's correctness.
2. WASM stack-switching ships in enough browsers with enough maturity that fiber-weight tasks over a small worker pool become implementable without an outsized engineering investment.
3. The W3C shared-everything-threads proposal (or successor) lands in shipping browsers, removing the COOP/COEP friction. Design against the stabilized shape.
4. Phase 12 (self-hosting) reveals the Kāra compiler itself would benefit from transparent threading on WASM, giving a first-party motivating workload.

**If none of the triggers fire:** `--features wasm-threads` opt-in stays the answer indefinitely. That is a valid end state — users who need shared-memory multithreading opt in and set their deployment headers; users who don't remain on the sequential default. Kāra does not lose language-quality points for not having transparent threading.

**Cross-reference:** `design.md § Concurrency Across Targets` — the v1 baseline; `docs/roadmap.md § Phase 10` — the phase that ships the baseline.

---

### `await` Keyword for Async APIs

A dedicated `await` expression form for yielding a `go`-task on an async operation. Today the effect system (`suspends`) plus channel-receive semantics plus the scheduler's yield-to-event-loop behavior cover the full use case on both WASM and native — see `design.md § Async Host APIs on WASM`. A Promise-returning host API looks like `let x = fetch(url)?;` with `suspends` inferred / declared; there is no `.await` and no `async` keyword.

**Current lean:** no `await` keyword in v1. The existing primitives are sufficient; adding a keyword introduces new surface without replacing anything.

**Why deferred (not rejected):**

1. **The effect + channel machinery covers the functional need.** A user can write UI and networking code today (assuming the Phase 10 scheduler contract) using channels and `suspends`. `await` would be ergonomic sugar, not a new capability.
2. **Keyword choice is a high-commitment decision.** Once `await` ships, its interaction with the effect system (does `await` require a specific effect on the expression? does it propagate something?), with ownership (does the awaited expression's ownership transfer survive the yield?), and with `#[target(...)]` gating needs to be nailed down. Committing to those answers before seeing real library shapes is a retrofit trap.
3. **Real UI code has not been written yet.** Kāra has no ecosystem. The "channels feel awkward for UI code" hypothesis the v36 Q6 discussion raised is untested. Shipping Phase 10 with channels-only gives users a chance to surface concrete pain points that an `await` keyword would address — or to confirm channels are fine and `await` is unnecessary.

**Why non-breaking later:** purely additive. Adding `await expr` as a new expression form does not invalidate existing channel-based code. A library that uses `channel.recv()` continues to work; an alternative library using `await` is strictly new code.

**Re-evaluation triggers (any one of):**

1. At least one user-space UI library ships on Kāra, and its authors report that the channel-based pattern is awkward enough to justify language-surface addition — with specific examples of code that would be materially cleaner with `await`.
2. The scheduler / Phase 10 lowering surfaces a case where the channel contract cannot express something a Promise-adapter needs (e.g., cancellation semantics, structured concurrency composition). If the primitives need to change anyway, re-evaluate whether `await` is part of the cleaner answer.
3. A third primitive concurrency style emerges in the Kāra ecosystem that doesn't fit channels or effects cleanly — a sign that the primitive set is incomplete and `await` (or something) should be added deliberately.

**If none of the triggers fire:** channels + effects remains the permanent answer. That is a valid end state — Kāra stays function-coloring-free as a defining property.

**Cross-reference:** `design.md § Async Host APIs on WASM` — the v1 mechanism; `design.md § What Kāra Is Not` — the "no `async fn`, no function coloring" stance.

---

### Package Manifest Artifact Format (`.karapack`)

A structured, tool-consumable descriptor for a `karac build` output that would complement or replace the per-file-naming convention in `design.md § Target Build Artifacts`. Fields would include: module set, public export list, embedded WIT (for WASM Component Model), declared effect requirements per export, and toolchain version.

**Current lean:** not in v1. `karac build` emits the flat per-file layout (`dist/<target>/<pkg>.{wasm,js,d.ts}` etc.) for every target. Downstream tooling consumes files by name and convention.

**Why deferred (not rejected):**

1. **No ecosystem pressure.** No bundler, loader, or deployment pipeline currently asks for a Kāra-specific manifest. Committing to a shape before the tools exist forces premature decisions.
2. **Per-file convention is sufficient for the known use cases.** Browser bundlers consume `.wasm` + sibling `.js` + `.d.ts` by file naming. Component Model hosts consume embedded-WIT `.wasm`. Neither needs a separate descriptor for v1 deployments.
3. **Embedded WIT already covers a large fraction of what a manifest would carry.** For Component Model targets, the WIT interface describes exports, effect-like capabilities (via interface types), and versioning. A manifest would layer additional Kāra-specific fields, but the value over plain embedded WIT is speculative.

**Why non-breaking later:** purely additive. A `--manifest` flag on `karac build` emits the `.karapack` file alongside existing outputs; the per-file layout continues to work. Tools that want the manifest opt in; tools that don't are unaffected.

**Re-evaluation triggers (any one of):**

1. A downstream tool (deploy platform, registry, bundler plugin) emerges with a concrete request for structured build metadata that cannot be derived from the per-file artifacts + embedded WIT.
2. Multi-module Kāra packages become common enough that a descriptor listing "which modules are in this build" is useful.
3. Effect declarations per export become a value-add for downstream security / auditing tools — a `.karapack` could carry the full effect signature of every public export in a form those tools can read without loading the `.wasm`.

**Cross-reference:** `design.md § Target Build Artifacts` — the current per-file contract.

---

### Non-ASCII Identifiers

The lexer's case-class rules are defined on ASCII alphabetic characters; identifiers containing non-ASCII characters are a parse error in the current spec. A future edition may extend classification to Unicode case via UAX #31 conformance. No committed design.

**Cross-reference:** design.md § Identifiers — inline deferral note.

---

### Higher-Kinded Polymorphism and Phantom Variance

Higher-kinded type parameters (abstracting over type constructors — the `* -> *` class) and explicit phantom variance markers are deferred with no committed design. The single-kinded type system plus monomorphized generics covers the MVP expressiveness range; higher-kinded abstraction is a research-grade extension if real Kāra code accumulates pressure for it.

**Cross-reference:** design.md § Generics — inline deferral note.

---

### Taint Tracking (`Untrusted[T]` / `Validated[T]`)

Type-level marker for data originating at an external trust boundary (HTTP body, env var, CLI arg, file contents) with a `.validate(Validator)` step that strips the marker before it reaches a sink (SQL driver, `Process.spawn`, path join, URL constructor, template engine). The lever: sinks require `Validated[T]` instead of `T` at their signature, and the compile error surfaces "this untrusted value was never validated" at every missed site.

**Current lean:** not in v1. The injection-bug class (SQL, shell, path traversal, SSRF, XSS, template) is real and worth addressing eventually, but a v1 shape for the marker + `Validator` trait + stdlib sink adoption carries too many under-designed pieces to commit to now.

**Why deferred (not rejected):**

1. **Sink-coverage gap.** For every stdlib sink that takes `Validated[T]`, there are ten that take `T`. Users routinely `.as_raw_untrusted()` to thread values through non-aware APIs — at which point the type-level guarantee dissolves. The value degrades gracefully but the *expectation* set by shipping it may not: users assume their code is safe because types compile.
2. **API-churn tax.** Every stdlib surface that accepts external input has to pick: does `std.fs.read(path: Path)` take `Validated[Path]` or `Path`? If `Path`, the marker is bypassed; if `Validated[Path]`, every caller with a plain `Path` needs to `.validate()`. The wrong pick is a daily friction; picking blind (before operational experience) is a coin flip.
3. **Validator composability is under-specified.** Is `MaxLen[10]` + `AsciiPrintable` one `Validator[String]` or two chained validators? Is a validator value-level (`.validate(NameValidator)`) or type-level (`.validate[NameValidator]()`)? Several right answers; settling them in v1 without real use cases invites retrofit.
4. **Scope creep risk.** A taint system done well involves flow-sensitive analysis (was this value *derived from* an untrusted value?), integration with the effect system (`reads(Network)` return types), and a mature validator library. v1 scope does not accommodate all of this — a partial system is worse than none if it creates false confidence.
5. **Reserving a prelude name without behavior is worse than absence.** `Untrusted` in the prelude that implements nothing tells users the language has an opinion it doesn't actually have. Kāra has namespaces (`user_crate::Untrusted` doesn't collide with `std.taint.Untrusted`), so squat-prevention is cosmetic rather than operational.

**Why non-breaking later:** purely additive. Introducing `std.taint.{Untrusted, Validated, Validator}` and migrating stdlib sinks to require `Validated[T]` in a minor version is source-compatible: existing call sites wrap inputs with `Untrusted.new(...)` + `.validate(...)`, and the signature-level contract becomes visible at call sites without invalidating typed code. Already-covered classes (memory safety, integer overflow, safe deserialization) remain unaffected.

**Re-evaluation triggers (any one of):**

1. Enough v1 stdlib surfaces (`std.http`, `std.process`, a SQL driver) ship and accumulate real-world usage that the sink set stabilizes — at which point "which surfaces require validation" becomes a concrete question rather than a speculative one.
2. A credible Kāra-shaped proposal for validator composability (free-standing `fn validate_name` vs. `Validator` trait, value-level vs. type-level dispatch, interaction with refinement types) emerges with worked examples.
3. A concrete injection-bug incident in Kāra user code demonstrates the class is not being caught by existing defenses (effect-system capability gating, parameterized resources at sink boundaries, explicit ownership transfer through parse-before-use boundaries).

**If none of the triggers fire:** injection prevention stays at the effect-system + capability-gating layer (`reads(Network)` declares external data entering a function; `sends(Db)` declares a database sink) plus convention (parse-before-use, typed query builders at the library layer). That is a valid end state — the OWASP injection class can be addressed by disciplined boundary parsing without a language-level marker.

**Open design questions to settle when this is built:**

1. **Effect-system integration.** `reads(Network)` / `reads(Env)` / `reads(FileSystem)` all produce externally-originating data. Should these functions' *return types* automatically be `Untrusted[T]`? Tentative answer: too coercive — `read_config_file` returns structured, parsed config, and by the time it returns the deserialization boundary has already produced structured data. Better: a convention that *deserialization-boundary* functions return `Untrusted[T]`, and stdlib deserializers (`json::parse`, form-decode, etc.) expose this at their API.
2. **Taint propagation — sanitizers vs. transforms vs. derivations.** Is `u.to_lowercase()` still `Untrusted[String]`? Yes — transformation preserves taint. `u.len()`: `Untrusted[usize]` or plain `usize`? Likely plain `usize` — length is not injectable content. The rule needs a coherent story: *sanitizers* strip taint (the `.validate(Validator)` step), *transforms* preserve taint (operations whose output semantically carries input content), *derivations* produce plain values (operations whose output is metadata about the input, not the input itself).
3. **Generic containers.** `Vec[Untrusted[String]]`: iterating yields `ref Untrusted[String]` by construction. `.sort()` is fine — it does not sink contents. `.join(",")` produces `Untrusted[String]` — concatenation of tainted strings is tainted. Rule: any op whose output semantically carries content from the input carries taint.
4. **Composition with `Secret[T]`.** `Secret[Untrusted[String]]` is a legal composition but stylistically confusing — one wrapper says "do not print," the other says "validate before sinking." In practice, secrets are usually produced by our own code (token mint, derive-from-master-key) rather than accepted from external boundaries, so `Secret[String]` alone suffices. When external tokens *are* accepted (`Bearer` headers from inbound HTTP), the intended flow is: `Untrusted[String]` → `.validate(BearerFormat)` → constructor-wraps in `Secret[String]`. The two stages are sequential, not nested.

**Cross-reference:** `design.md § Feature 2: Effect Types` — the capability primitive that already constrains *which* boundaries untrusted data can enter through; `design.md § Refinement Types` — the closest in-language mechanism for validated-at-boundary types without a separate marker layer; `design.md § Secret Type (Secret[T])` — the sibling wrapper with distinct semantics.

---

### Unstructured `spawn`

Task spawn where the task's live range outlasts the spawning function. Kāra's MVP concurrency model is strictly structured — `TaskGroup`, `par {}`, auto-concurrency — which covers accept-loops, fan-out, and implicit parallelism without an unstructured primitive. Unstructured spawn adds real complexity around task lifetime, panic propagation, and resource cleanup; deferring it until real-world usage demonstrates where structured concurrency is insufficient keeps the v1 surface narrow. No committed design.

**Cross-reference:** design.md § Structured Concurrency — inline deferral note.

---

### Constant-Time Integer Types (`CtU64`, `CtBool`)

Side-channel-resistant integer types with a restricted op set (no early-exit branches, no data-dependent timing) for cryptographic code that needs constant-time arithmetic beyond the constant-time *equality* already provided by `Secret[T]`. Typical members: `CtU8`, `CtU32`, `CtU64`, `CtI32`, `CtI64`, `CtBool`. Operations cover addition, subtraction, bitwise, conditional-move, and conditional-select — each op constant-time by construction.

**Current lean:** not in v1. `design.md § Secret Type` covers constant-time equality via `ConstantTimeEq`; constant-time *arithmetic* is additive and less load-bearing for common v1 use cases (session tokens, HMAC digests, CSRF tokens — compared, rarely arithmetic'd).

**Why non-breaking later:** new wrapper types in `std.secret.ct` (or similar). No existing `u64` op is invalidated; `CtU64` is a distinct type.

**Re-evaluation triggers (any one of):**

1. Kāra stdlib ships a cryptographic primitive (`crypto::chacha20`, `crypto::x25519`) that would benefit from language-level constant-time arithmetic rather than hand-rolled per primitive.
2. A v1+ Kāra crypto library emerges and its authors report hand-rolling constant-time arithmetic is error-prone enough to justify a language-level primitive.

**Cross-reference:** `design.md § Secret Type (Secret[T])` — the constant-time-equality primitive this builds on.

---

### Generalized `#[zeroize]` Attribute

A `#[zeroize]` attribute applicable to struct fields or whole types that are *not* wrapped in `Secret[T]` but should still have their backing bytes wiped on drop. Covers the case where the full `Secret[T]` wrapper is too heavy (e.g., a large existing struct with one sensitive field where rewrapping would require rethreading `.expose()` through many call sites) but zero-on-drop behavior is still wanted.

**Current lean:** not in v1. `Secret[T]` (which dispatches through the `Zeroize` trait in its own `Drop` impl) handles the common case. `#[zeroize]` is additive when the wrapper's ergonomics don't fit.

**Why non-breaking later:** new attribute on existing struct/field syntax. Absent `#[zeroize]`, current drop semantics hold.

**Re-evaluation triggers (any one of):**

1. Real Kāra code accumulates the "large struct, one sensitive field, cannot rewrap into `Secret[T]`" pattern often enough to justify an attribute shortcut.
2. `Secret[T]` usage surfaces specific composition limitations (e.g., trait bounds the wrapper introduces that block certain generic uses).

**Cross-reference:** `design.md § Secret Type (Secret[T])` — the v1 primary mechanism; `design.md § Feature 4 Part 8: Drop` — the drop infrastructure.

---

### Package Manifest Capability Declarations

A package manifest field declaring the transitive effect set a library's public API requires (e.g., `capabilities = ["reads(FileSystem)", "sends(Network)"]`). The package manager flags when a dependency adds a capability to its declared set in a minor version — effectively a semver-visible permissions change. Covers the supply-chain vector where a dependency silently gains a new effect (a previously-pure formatter begins reading `Env`, or a logger begins sending to `Network`).

**Current lean:** not in v1. The effect system makes capability-transitive-requirements visible *per function*; lifting that to the package manifest is tooling that builds on the language feature. `design.md § Effect Semver Rules` already covers the per-function semver classification this would aggregate.

**Why non-breaking later:** purely additive. Manifests without the field are unconstrained; manifests with the field gain the check. Compiler and package manager cooperate — the compiler verifies the manifest against inferred/declared effects; the package manager enforces the change-in-minor-version rule at dependency resolution.

**Re-evaluation triggers (any one of):**

1. Kāra ecosystem grows enough that dependency auditing becomes a real user concern.
2. A supply-chain incident (in Kāra or an adjacent ecosystem) surfaces a concrete gap between per-function effect declaration and package-manifest-level policy.

**Cross-reference:** `design.md § Feature 2: Effect Types` — the language foundation; `design.md § Effect Semver Rules` — the per-function treatment this lifts to packages.

---

### Effect Diff Tooling for Cross-Version `panics` Surfacing

A build-side tool that diffs a library's effect surface across two versions and flags any function that gained `panics` as a candidate for major-version bump (panics are observable, so a minor release adding them to a previously-panic-free function is in principle a semver break). The language already classifies "adding an effect" as breaking (`design.md § Effect Semver Rules`); the tooling surfaces *which* effect was added and highlights `panics` specifically because its security and reliability implications are different from (say) `writes(Cache)`.

**Current lean:** not in v1. The effect semver classification is already in the language; standalone diffing tooling is additive and more valuable once an ecosystem exists to diff against.

**Why non-breaking later:** entirely tooling — no language change required. Existing effect declarations feed directly into the diff.

**Re-evaluation triggers (any one of):**

1. Kāra package registry ships and dependency-version-upgrade audits become a user concern.
2. A Kāra library publishes a minor version that silently added `panics` and breaks downstream users, surfacing a concrete need for the tool.

**Cross-reference:** `design.md § Effect Semver Rules` — the classification this builds on.

---

### Machine-Applicable Replacement Metadata on Typechecker / Effectchecker Diagnostics

Whether typechecker `TypeError` and effectchecker `EffectError` should carry `suggestion` / `replacement` fields so their `did you mean`-style diagnostics flow through the same `karac fix` and IDE quick-fix infrastructure that resolver and ownership classes already use. Today neither phase has a `suggestion` field on its error struct; adding one is a per-struct expansion.

**Current lean:** not in v1. The infrastructure is in place — rounds 12.28–12.32 closed the `replacement` thread for resolver E0223 / E0225 / E0228 / E0229 plus ownership N0507, with `karac fix`, single-file JSON, and multi-file JSON / JSONL paths all wired through. Extending coverage to typechecker / effectchecker phases is per-class metadata work that lands opportunistically alongside the per-pass refactors that benefit. Most existing diagnostic surfaces in those phases carry sentence-prose suggestions, not single-token edits — which is why the v1 cutoff stops at resolver + ownership.

**What's needed:**

1. **Diagnostic-struct expansion** — add `pub suggestion: Option<String>` and `pub replacement: Option<Box<crate::resolver::TextEdit>>` to `TypeError` (`src/typechecker.rs`) and `EffectError` (`src/effectchecker.rs`). Propagate `None` defaults through every existing construction site (mechanical, multi-site).
2. **CLI rendering** — extend the typechecker / effectchecker JSON-rendering paths in `src/cli.rs` to emit the `replacement` payload (mirror the ownership pattern at `cli.rs:1411`).
3. **Per-class tagging** — pick high-value sites with mechanical fixes:
   - `TypeErrorKind::UndefinedField` — when the field is misspelled, `suggest_similar` against the struct's known fields produces a single-token replacement.
   - `TypeErrorKind::UndefinedVariant` — same shape against enum variants.
   - `EffectErrorKind::UnknownEffectVerb` — fuzz-match against the eight built-in verbs.
4. **`karac fix` dispatcher** — already runs the full pipeline (round 12.32), so newly-tagged classes are picked up automatically.

**Why non-breaking:** purely additive. New fields default to `None` for untagged classes; new metadata flows through the same JSON envelope pattern; no existing diagnostic class changes shape.

**Why P2 (not P1):** the resolver + ownership coverage shipped in v1 already covers the common-case quick-fixes a v1 user hits (typo'd identifier, typo'd type, typo'd module / import, unused-mut perf note). Typechecker / effectchecker tagging adds polish for less-common cases that an IDE could surface but a CLI user is rarely blocked on. Dispatcher work is done; remaining tagging is per-pass busywork that has no v1 deadline pressure.

**Re-evaluation triggers (any one of):**

1. An IDE / LSP integration ships and the absence of typechecker / effectchecker quick-fixes becomes a user-visible gap.
2. A standalone typechecker or effectchecker refactor lands that naturally adds `suggestion` infrastructure as a side-effect.
3. A corpus scan of real Kāra programs shows a non-trivial fraction of typechecker / effectchecker diagnostics where a mechanical fix exists — i.e., the polish would matter at scale.

**Cross-reference:** `implementation_checklist/ § rounds 12.28–12.32` — the resolver / ownership rollout this builds on.

---

### Effect-Row Verbosity Audit

Whether Kāra forces `with ...` declarations in places where the user would reasonably expect implicit propagation — e.g., inside a generic bound that already restricts what effects a type parameter's impls can carry, across trait-method boundaries that inherit the trait's ceiling, or on closures passed to effect-polymorphic adaptors.

**How to resolve:** pick 3–5 representative programs from `design_studies/` and `examples/`, count every `with ...` clause, and ask whether removing it would (a) produce a useful diagnostic at a reasonable distance (same fn body) or (b) hide a real cost from the call site. If every `with` earns its presence, close as "Kāra is already where it should be." If one or more feel like pure ceremony, open a focused design item to relax that case.

**Why deferred:** the audit itself is bounded (~30–60 min of careful reading), but it produces a useful decision only once representative programs exist. Current `design_studies/` and `examples/` are spec-illustration sized, not application sized. Revisit once Phase 4+ example programs accumulate.

**Why non-breaking:** if the audit surfaces a simplification, the change would relax a current requirement (fewer declarations required in some position) — purely additive in the backward-compatible direction.

---

### Comptime — AST→AST `comptime fn`

**Status:** Shape decided 2026-05-02 (v60 item 31) — AST→AST `comptime fn`, not value-level `const fn`. Full surface specced below; implementation deferred to post-v1. The earlier "Comptime Shape — AST→AST vs Value-Level `const fn`" deferred entry that catalogued the two options is resolved by this decision.

**Why AST→AST.** Kāra's metaprogramming surface must cover three jobs that mainstream languages typically split across separate mechanisms: value-level compile-time computation (Rust's `const fn`), derive macros (Rust's proc-macro crates), and code generation (Rust's `build.rs` + `proc-macro2`). Splitting these would force three separate sub-languages — a value subset, a procedural macro DSL, and ad-hoc build scripts. AST→AST `comptime fn` collapses all three into one mechanism, written in Kāra itself with the same type system, the same diagnostics, and the same effect surface. The cost is a larger upfront spec; the benefit is one language surface for everything that runs at compile time, which matches Kāra's stance on LLM-written code (single surface = simpler synthesis target) and on full-feature-up-front design.

#### Surface forms

Comptime introduces three syntactic forms, all gated by the `comptime` keyword (already reserved in v1 — see [`syntax.md § 1.1 Keywords`](syntax.md) and [`design.md § Reserved-for-Future-Use Keywords`](design.md#reserved-for-future-use-keywords)):

```kara
// 1. Function declaration — body runs at compile time when called from a comptime context.
comptime fn build_lookup_table(size: i64) -> Array[i64, size] { ... }

// 2. Block expression — forces compile-time evaluation of the inner expression/block.
let table = comptime { build_lookup_table(1024) };

// 3. Parameter prefix — argument must be a comptime-known value.
comptime fn matrix[const ROWS: i64, const COLS: i64](
    comptime kind: MatrixKind,
    init: Fn(i64, i64) -> f64,
) -> Matrix[ROWS, COLS] { ... }
```

The three forms compose. A `comptime fn` may call ordinary `fn`s — but only those whose effects are subset-restricted to the comptime-permitted set (see *Effects* below). An ordinary `fn` may call a `comptime fn` only inside a `comptime { ... }` block or by binding the result to a `static` / `const generic argument` / `default parameter value` — the boundary is explicit, never implicit.

**Definition-time validation of metavariable specifiers** (v60 item 63 — Tier "Future-proofing" pin committed at v1, behavior lands when comptime ships). Every `comptime fn` parameter must carry a type annotation at the declaration site — no anonymous parameter form, no inferred-from-call-site shape. The rule is already implicit in Kāra's broader function-parameter rules (per [`design.md § Trait method parameter names — required`](../design.md#trait-method-parameter-names--required), every fn parameter requires a name and type; comptime fn participates in the same rule). The rejection diagnostic is `error[E_MISSING_FRAGMENT_SPECIFIER]: comptime fn parameter '<name>' must declare a fragment specifier — annotate with a typed AST shape (`Expr`, `Stmt`, `Type`, etc.) at the declaration` and fires at the comptime fn's definition site, never at a call site. This is the load-bearing answer to the Rust pre-1.55 footgun where macro definitions could omit fragment specifiers on metavariables and surface mysterious matching failures at every call. Kāra forbids the omission outright at the declaration site so the bug's evidence is local to the macro definition, not scattered across calls. Companion principle pin in design.md `§ Reserved Fragment-Specifier Identifier Namespace > Forward-commitment — definition-time fragment-specifier validation`.

#### Types as first-class values

At comptime, types are values of the built-in pseudotype `Type`. This is the central enabling fact for AST→AST work: a `comptime fn` can take a `Type` as a parameter, inspect its structure, and emit code parameterized by it.

```kara
comptime fn print_fields(comptime T: Type) {
    for field in T.fields() {
        compiler.print(f"  {field.name}: {field.ty.name()}");
    }
}

comptime { print_fields(User) };   // prints User's fields at build time
```

`Type` values are first-class only at comptime — they cannot appear in runtime expressions. A runtime function may not take a parameter of type `Type` (it would be a value-level reference to a compile-time-only value). The boundary is enforced by the typechecker: a `Type` value flowing into a non-comptime context is a compile error with diagnostic `error[E_TYPE_VALUE_AT_RUNTIME]`.

The `Type` pseudotype's reflection surface — `fields()`, `variants()`, `methods()`, `name()`, `size_of()`, `align_of()`, `is_struct()`, `is_enum()`, `is_union()`, `is_generic()`, `generic_args()`, `attributes()` — is fixed by the language; user code reads it but cannot extend it.

#### Reflection API

The reflection API exposes the program tree to comptime code as ordinary Kāra values. The full surface is rooted at the `compiler` module (a comptime-only prelude module — see *Comptime stdlib surface* below):

| API | Returns | Description |
|---|---|---|
| `T.fields() -> Slice[Field]` | per struct | iterable list of `Field { name, ty, vis, attributes }` |
| `T.variants() -> Slice[Variant]` | per enum | each variant exposes its fields |
| `T.methods() -> Slice[Method]` | per type | methods declared on `T` (inherent + visible trait impls) |
| `T.attributes() -> Slice[Attribute]` | per item | the `#[...]` attributes attached at the declaration site |
| `T.name() -> String` | per type | canonical fully-qualified name |
| `T.size_of() -> usize` | per sized type | runtime size in bytes |
| `T.align_of() -> usize` | per sized type | runtime alignment |
| `compiler.current_module() -> Module` | global | the module the calling site lives in |
| `compiler.callsite_location() -> SourceLocation` | global | file/line/column of the comptime invocation |
| `compiler.diagnostic(severity, span, message)` | global effect | emit a build-time diagnostic at a chosen span |

The reflection API is read-only on the existing program tree. Code generation goes through the AST builder API.

#### AST builder API

A `comptime fn` emits code by constructing AST values and either returning them (when the function appears in declaration position) or invoking compiler-provided emit operations. The AST node types — `Expr`, `Stmt`, `Item`, `Pattern`, `Type`, etc. — are stdlib-defined enums with one variant per AST shape; their definitions are part of the comptime stdlib surface.

```kara
// Stdlib (sketch — comptime-only module `compiler.ast`):
enum Expr {
    Literal(LiteralValue),
    Variable(Ident),
    Call { callee: Box[Expr], args: Vec[Expr] },
    Block { stmts: Vec[Stmt], tail: Option[Box[Expr]] },
    /* ... */
}

enum Item {
    Fn(FunctionDef),
    Struct(StructDef),
    ImplBlock(ImplBlock),
    /* ... */
}
```

A derive desugars to a call to a `comptime fn` that takes the target type as a parameter and returns a `Vec[Item]` to splice into the surrounding module:

```kara
// Stdlib derive — `#[derive(Eq)]` on a struct desugars to a call to this fn.
comptime fn derive_eq(comptime T: Type) -> Vec[Item] {
    let body = T.fields()
        .map(|f| ast.expr(f"self.{f.name} == other.{f.name}"))
        .reduce(|a, b| ast.expr(f"({a}) and ({b})"))
        .unwrap_or(ast.expr("true"));

    vec![ast.impl_block(
        target = T,
        traits = [ast.path("Eq")],
        items  = [ast.method("eq", &[("self", ast.ref_self()), ("other", ast.ref_t(T))],
                             ast.bool_ty(), body)],
    )]
}
```

The AST builder offers two surfaces for constructing nodes: a **typed builder** (`ast.expr(...)`, `ast.method(...)`, etc. — checked at definition site, no string concatenation) and a **quasi-quote** form (`ast.expr("self.{f.name} == other.{f.name}")` — string interpolation with embedded comptime values, parsed at build time). Quasi-quote is the ergonomic form; the typed builder is the form for programmatic construction over arbitrary shapes.

#### Code generation and derive desugaring

`#[derive(Trait1, Trait2, ...)]` on a struct/enum desugars to one `comptime fn` invocation per derive name. Each derive resolves to a `comptime fn` named `derive_<TraitName>` (snake-case) that must:

- Take exactly one parameter: `comptime T: Type`.
- Return `Vec[Item]` — the items to splice into the same module.
- Live in the same module as the trait (lookup by lexical sibling), or be re-exported under the trait's path.

Built-in derives (`Eq`, `Hash`, `Display`, `Debug`, `Clone`, `Copy`, `PartialEq`, `PartialOrd`, `Ord`, `Arithmetic`, `Serialize`, `Deserialize`) are all stdlib `comptime fn`s with no special compiler treatment beyond the lookup convention. User-defined derives use the same mechanism — there is no separate "proc macro" sub-language.

Splice rules: items returned from a `comptime fn` invoked via `#[derive]` are spliced *after* the derive site at module scope. They can reference items declared earlier in the module but not items declared later (one-pass module-level resolution preserves source-order semantics).

#### Effect system integration

Comptime effects live in their own resource family, distinct from runtime resources:

| Effect | Verb | Meaning |
|---|---|---|
| `reads(CompileTimeEnv)` | reads | inspect compiler state — module table, type registry, attribute reads |
| `writes(CompileTimeEnv)` | writes | emit diagnostics, record metadata for later compilation phases |
| `allocates(CompileTimeHeap)` | allocates | comptime-heap allocation for buffers, AST nodes, intermediate vectors |
| `panics` | panics | a comptime panic is a **compile error**, not a runtime panic — the diagnostic surfaces at the calling site |

All runtime resource verbs (`reads(File)`, `writes(Network)`, `sends(Channel)`, ...) are forbidden inside `comptime fn` — calling a runtime-effectful function from a comptime context is `error[E_RUNTIME_EFFECT_AT_COMPTIME]`. Execution verbs (`blocks`, `suspends`) are forbidden too. The comptime evaluator runs synchronously inside the compiler; there is no scheduler, no I/O, no FFI.

`CompileTimeEnv` and `CompileTimeHeap` are reserved built-in resource names — already pinned in [design.md § Comptime Effect Defaults](design.md#comptime-effect-defaults) (the v1 reservation). When called from a runtime context (via `comptime { ... }` or static initializer), comptime effects are *stripped* — the call site does not need to declare `reads(CompileTimeEnv)` because the work happens before the binary exists.

Cross-reference rule for the embedded/kernel profile: those profiles forbid `allocates(Heap)` but permit `allocates(CompileTimeHeap)`. A comptime fn that builds a 4 KB lookup table at build time and emits it as a `static` array is valid in both `embedded` and `kernel` profiles — the heap allocation happened in the compiler, not on-device.

#### Const-generic, refinement, and default-value integration

Three existing v1 features lower to the comptime evaluator under the hood — once comptime ships, the const-evaluator they depend on stops being a special-case mechanism and becomes a degenerate use of the comptime evaluator:

- **Const generic arguments** (v1, design.md § Const generic parameters): the expression in const-arg position is a comptime expression. In v1 the evaluator only handles literals + literal arithmetic; once comptime lands, any `comptime fn` call is permitted in this position.
- **Refinement-type predicates** (v1, design.md § Refinement Types): predicate evaluation at binding sites uses the comptime evaluator. Same v1-degenerate-form rule.
- **Default parameter values** (v1, design.md § Default Parameter Values): explicitly noted in the v1 spec as "calls to `const fn` will be permitted once the comptime feature lands". When the feature lands, "`const fn`" in that note becomes "`comptime fn`" — single mechanism.

This composition is intentional: v1's restrictive const-eval is a forward-compatible subset of the comptime evaluator. Code written against v1's const-eval continues to compile after comptime ships; the surface only widens.

#### Hygiene rules

Identifiers emitted by a `comptime fn` resolve at the *invocation site*, not the *definition site*, with two exceptions:

1. **Names that are unambiguous at the definition site** — references to stdlib items, items from the comptime fn's own module — resolve at the definition site and are stable across invocation sites.
2. **Names introduced *inside* the emitted code** — a `let` binding emitted by the comptime fn — are scoped to the emitted item, and the comptime fn is responsible for picking names that don't collide with surrounding bindings (the `compiler.fresh_ident()` builder helper produces guaranteed-fresh identifiers).

References to *types*, *traits*, *struct fields*, and *enum variants* always resolve at the invocation site — they're the natural reference targets for derives and template-style code generation. References to *functions* resolve at the invocation site by default; the `ast.path("module::name")` builder fixes resolution at the definition-site path when the comptime fn wants a stable reference.

This hygiene model is closer to scheme/clojure's syntax-case than to C macros — every identifier has a tracked origin, and accidental capture is the exception rather than the norm. It is more permissive than Rust's macro hygiene (which is fully hygienic by default) because Kāra's comptime fns take typed parameters and return typed AST values; the type system catches many mistakes that hygiene rules would have to catch in an untyped macro system.

#### Resource limits

The comptime evaluator runs inside the compiler with hard ceilings:

- **Iteration limit** — `2^24` total instructions per top-level `comptime` invocation. Configurable via `--comptime-iter-limit=N` for build-time tuning, but the default is the language commitment. Exceeding the limit produces `error[E_COMPTIME_ITER_LIMIT_EXCEEDED]` with a stack trace of the comptime call chain.
- **Memory limit** — `512 MiB` of `CompileTimeHeap` allocation per top-level invocation. Configurable via `--comptime-heap-limit=N`. Exceeded ⇒ `error[E_COMPTIME_HEAP_LIMIT_EXCEEDED]`.
- **Recursion limit** — 1024 frames of comptime call depth. Configurable. Exceeded ⇒ `error[E_COMPTIME_RECURSION_LIMIT_EXCEEDED]`.

Cycle detection: the evaluator tracks the in-flight comptime call set and rejects mutual recursion that doesn't terminate (a `comptime fn` calling itself with the same arguments produces `error[E_COMPTIME_INFINITE_RECURSION]` once detected, typically within ~16 stack frames).

#### Tooling integration

- `karac doc` — comptime fns are documented like ordinary fns, with an extra "comptime" badge. Items emitted by derives are documented in the type's doc page under "Auto-generated impls", with a hyperlink to the derive's source.
- `karac explain --expand <span>` — at any source span containing a derive or comptime block, prints the post-expansion AST that the comptime fn produced. This is the answer to "what did `#[derive(Eq)]` actually generate?" — readable, line-numbered, identical to what the rest of the compiler sees.
- Debugger — comptime evaluation is visible to the `karac` debugger as a dedicated comptime frame stack; breakpoints, stepping, and variable inspection all work the same way as for runtime code.
- `karac query monomorphization` — comptime invocations are tracked alongside type-parameter monomorphizations in the per-instance identity tuple `(T1..Tk, const C1..Cm, E1..En, comptime args...)`.

#### Comptime stdlib surface

A `compiler` module (and its `compiler.ast` submodule) is added to the comptime-only prelude. Importing it from runtime code is `error[E_COMPTIME_MODULE_AT_RUNTIME]`. Module contents (sketch):

- `compiler.print(s: String)` — emit text to the build log
- `compiler.diagnostic(severity, span, message)` — emit a diagnostic
- `compiler.fresh_ident() -> Ident` — guaranteed-fresh identifier
- `compiler.callsite_location() -> SourceLocation`
- `compiler.current_module() -> Module`
- `compiler.ast.expr(...)`, `compiler.ast.stmt(...)`, `compiler.ast.item(...)`, etc. — typed builders
- `compiler.ast.parse_expr(s: String) -> Result[Expr, ParseError]` — quasi-quote shim

The `compiler` module is small at the surface — fewer than fifty exported items — because the heavy lifting happens through ordinary value-level code on `Type` and AST values. It is *not* a procedural-macro library; it is a thin window into compiler state plus the AST node constructors.

#### Implementation phases

Comptime ships as a single complete unit post-v1 — the v60 directive is "don't drip-feed". The implementation has four discrete substrates that can be built in order:

1. **Comptime evaluator.** A treewalk interpreter over the typed AST, implementing the runtime-language subset (everything in `comptime fn` bodies that doesn't touch the AST or type-as-value). This is essentially the existing Phase 4 interpreter retargeted to compile-time invocation.
2. **`Type` as first-class value + reflection API.** The compiler's existing type registry exposed as immutable `Type` values; field/variant/method iteration; size/align queries.
3. **AST builder + emission.** Stdlib `compiler.ast` module; typed builder API; quasi-quote parser; splicing rules at the module level.
4. **Derive desugaring.** Replaces compiler-built-in derives with stdlib `comptime fn` calls; adds the lookup convention for user-defined derives.

Substrates 1+2 enable value-level comptime computation and type-inspection diagnostics. Substrate 3 enables programmatic code generation. Substrate 4 unifies the existing derive surface. Every substrate is internally complete on its own — partial deployment never leaks a half-built feature into the v1 language.

---

### `par for` — Data-Parallel Loop Syntax

Surface syntax for data-parallel iteration: `par for item in collection { body }`. The compiler verifies that each iteration's effect set is independent from its siblings (no write-write or read-write conflicts across iterations) before emitting parallel code.

**Current workaround.** The same pattern is expressible today via `TaskGroup`:

```kara
let group = TaskGroup.new();
for item in collection {
    group.spawn(|| process(item));
}
// group joins all tasks on drop
```

This is correct but requires wrapping each iteration as a closure, which is verbose for pure computational loops.

**Current lean:** not in v1. The `TaskGroup` workaround is available; `par for` is syntactic sugar with an independence check, not a new capability.

**Why deferred (not rejected):**

1. **Syntactic sugar over `TaskGroup`.** `par for` lowers to a `TaskGroup` fan-out with a compiler-enforced per-iteration independence check. The independence check is partially covered by the existing effect conflict model (`design.md § Auto-Concurrency`), but the lowering pass and surface syntax are additional implementation scope.
2. **Independence verification is non-trivial for mutable collections.** When the loop body writes to a collection indexed by the loop variable (e.g., `next[x] = f(old[x])`), the compiler must prove no two iterations write the same index. This is decidable in the common cases (index is the loop variable; range is non-overlapping) but requires a dedicated analysis pass. The conservative fallback (reject unless provably independent) is safe but may surprise users who expect the compiler to accept obviously-parallel loops.
3. **`parallel_map` as a stepping stone.** A `parallel_map[T, U](xs: Vec[T], f: Fn(T) -> U) -> Vec[U]` stdlib function covers the pure, non-mutating case with no new syntax — the closure's effect set (`Fn(T) -> U` carries no resource effects) guarantees independence by type. This may ship before the full `par for` syntax.

**Why non-breaking later:** purely additive. `par for` is new syntax; existing `for` loops and `TaskGroup` code are unaffected.

**Re-evaluation triggers (any one of):**

1. A realistic Kāra program (Game of Life step, matrix multiply, batch transform) would be materially cleaner with `par for` than with the `TaskGroup` workaround — and the independence check is straightforward for that class of loop.
2. `parallel_map` ships and reveals that purely-functional data parallelism covers only a fraction of the real-world cases, making `par for` necessary for the mutable case.

**Cross-reference:** `design.md § Explicit Concurrency: par {} and spawn()` — v1 parallel primitives; `design.md § Auto-Concurrency` — the independence-analysis infrastructure this would reuse.

---

### `OrderedMap[K, V]` / `OrderedSet[T]` — Insertion-Ordered Collections

**Decision:** Add `OrderedMap[K, V]` (and its set counterpart `OrderedSet[T]`) as separate stdlib types alongside `Map[K, V]` / `Set[T]`. Iteration yields entries in the order they were first inserted; re-inserting an existing key updates the value but does *not* move the key in the order. Deferred to post-v1.

**Why deferred:** v1 ships with `Map[K, V]` / `Set[T]` (unordered, hash-table-backed) and `TreeMap[K, V]` (sorted by key). A third collection axis — insertion-order — is genuinely useful (deterministic iteration for Display / golden tests, removes the "linked-list-of-(key, value)-pairs" boilerplate users otherwise write), but the use cases are narrow enough that v1 doesn't need to ship three hash-table flavors. Once `Map` / `Set` are stable, lifting the implementation to `OrderedMap` / `OrderedSet` is mechanical.

**Why non-breaking:** Purely additive. New collection types; `Map[K, V]` semantics — including the unspecified-iteration-order guarantee in `design.md` — are unchanged. Code written against `Map` continues to compile and behave identically.

**Why a separate type, not "promote `Map` to insertion-ordered":** Keeping `Map[K, V]` order-unspecified preserves runtime freedom — the implementation can swap hash strategies (Robin Hood, Swiss-table variants, sharded concurrent maps) and rehash on growth without breaking semantics. Pinning insertion-order into `Map` would be a one-way door: every future strategy must preserve it, and concurrent `Map` variants become significantly harder. Users who want stable iteration opt into `OrderedMap` and accept its costs (extra memory for the order spine, branch + pointer writes per insert/remove, harder concurrent variants). This is the Rust ecosystem's split (`HashMap` + `indexmap`); we follow it.

**Design shape:**

- API mirrors `Map[K, V]` / `Set[T]` exactly — `insert`, `remove`, `get`, `contains`, `entry`, `len`, `is_empty`, iteration, etc. The only observable difference is iteration order.
- Two viable implementation strategies:
  - **Linked-list spine.** Hash table entries also carry `prev` / `next` pointers; iteration walks the linked list. Java's `LinkedHashMap` shape. Adds 16 bytes/entry (two pointers).
  - **Compact-dict.** A dense `Vec[(K, V)]` in insertion order plus a sparse hash table storing indices into the dense array. Python 3.6+'s shape. Adds ~1 index/entry to the hash table; deletion tombstones the dense slot and triggers periodic compaction. Better cache locality on iteration.
- Choice between the two is an implementation detail; `OrderedMap[K, V]` semantics don't depend on which is used. Lean toward compact-dict for memory + iteration speed; spine is simpler if compaction proves fiddly.
- Removal semantics: `remove(k)` removes the entry; later iteration skips the removed key. No order shift on remove (the surviving keys keep their original positions). `entry(k).or_insert_with(...)` on a missing key inserts at the end; on a present key, value is updated but order is unchanged.
- Effect parity with `Map[K, V]`: `allocates(Heap)` on growth, `panics` on `unwrap`-style accessors. No new effect surface.

**Cross-reference:** `design.md` Map/Set sections (unordered semantics anchored there); implementation_checklist `phase-7-codegen.md` Display work (which highlighted that deterministic Display output is a real ergonomic gap `OrderedMap` would close).

---

### Karac-Side Bounds-Check Elimination Pass

A compiler-internal pass that pattern-matches common safe-indexing idioms (`for i in 0..xs.len() { xs[i] }`, `if i < xs.len() { xs[i] }`, monotone-step induction over slices) and rewrites the indexing to skip the runtime bounds check before LLVM codegen runs. Sits *above* the v1 BCE strategy (LLVM-friendly emission via `llvm.assume` + cold-attribute panic blocks + SCEV/GVN-friendly idioms — see `design.md § Spatial safety`), catching cases where Karac knows the bound is satisfied but LLVM's range analysis does not.

**Why deferred:** Empirical motivation is missing. The v1 BCE strategy (LLVM-friendly emission + `unsafe { xs.get_unchecked(i) }` escape hatch) is sufficient for every workload measured in the v62 bench suite — sieve (4.3×), brute_force (3.0×), coin_change (1.7×) all sit in stride-1 / step-based induction territory that LLVM's SCEV/GVN handles natively. A Karac-side pass would catch a *different* class of cases (computed indices the user proves safe, multi-dimensional indexing patterns LLVM can't relate, custom range-bound idioms) that haven't surfaced in measured workloads. Building the pass before real-world data shows the gap risks designing for hypothetical patterns.

**Promotion gate:** Promote to P1 (or P0 v1.x) when post-v1 user data shows ≥2 distinct workload classes where the v1 BCE strategy leaves a >1.5× perf gap that the user would have to close via `get_unchecked`. The trigger is *frequency in real code*, not theoretical coverage — one rare pattern doesn't justify a Karac-side pass.

**Why non-breaking:** Purely additive. The pass either eliminates a bounds check (faster) or leaves the LLVM-friendly form in place (current behavior). No semantic change. Existing programs run identically or faster. `unsafe { xs.get_unchecked(i) }` continues to be the user-visible escape hatch regardless of whether the pass exists.

**Design shape (sketch — finalize at promotion):**

- Karac-side pass running between typecheck and codegen.
- Pattern-matches `for i in 0..xs.len()` / `for i in 0..N where N == xs.len()` / `if i < xs.len() { xs[i] }` and similar.
- Marks each matched indexing site as "skip bounds check" before lowering to LLVM IR.
- Falls back to LLVM-friendly emission for non-matched sites.
- Diagnostic affordance: `karac explain` should be able to point at an indexing site and say "this could not be elided because *X*; consider rewriting *Y* or using `get_unchecked`."

**Cross-reference:** `design.md § Spatial safety` (v1 BCE strategy and the `get_unchecked` escape hatch); `implementation_checklist/phase-7-codegen.md` (LLVM-friendly bounds-check emission tracking).

---

## P3 — Post-v1 Build Targets (library / ecosystem)

Items that are **not language features** and will not be added to `design.md` — they are libraries or frameworks built on top of the language. They live here because, post-v1, the project author may choose to build them directly rather than wait for community ownership. Each entry describes the scope, what it rests on in the language, and what would need to be in place before building it.

These are distinct from the P1/P2 deferrals above, which track language / compiler features the project itself would add to the spec. P3 items never go into `design.md`; if any ship, they ship as independent packages.

### Autograd / Neural Network Framework

An automatic differentiation library and neural network layer collection — the foundation for a PyTorch / JAX / Flax equivalent built in pure Kāra.

**What it rests on (all must ship first):**
- Phase 8 user-defined operator overloading (`impl Add for TrackedTensor`) — the critical enabler; without it, a `TrackedTensor` wrapper cannot intercept arithmetic transparently
- Phase 11 `Tensor[T, Shape]` stdlib
- `f16`/`bf16` numeric types — mixed-precision training
- `shared struct` with RC — mutable computation graph (tape) state shared across operations
- Custom effect resources — `resource GradTape` lets the effect system surface gradient tracking in public function signatures

**Architecture sketch:**

```kara
resource GradTape;

shared struct TrackedTensor[T: Float, ...S] {
    data: Tensor[T, S],
    grad: Option[Tensor[T, S]],
    // tape node reference (internal)
}

// Operator overloading intercepts arithmetic and records the operation on the tape
impl Add for TrackedTensor[T, S] with writes(GradTape) { ... }
impl Mul for TrackedTensor[T, S] with writes(GradTape) { ... }
// ... all Tensor ops mirrored

fn backward(loss: TrackedTensor[f32, []]) with writes(GradTape) { ... }
```

**Effect system advantage over PyTorch.** Functions performing gradient-tracked operations declare `with writes(GradTape)`. Inference or preprocessing functions carry no `GradTape` effect — the compiler statically enforces the separation. Accidentally calling a tracked op inside an inference-only function is a compile error, not a silent correctness bug.

**Minimum viable library scope:** `TrackedTensor`, `backward`, optimizers (SGD, Adam, AdamW), `nn` module with `Linear`, `Conv2d`, `LayerNorm`, `Embedding`, `Dropout`, loss functions (`cross_entropy`, `mse`, `huber`). GPU training blocked on Phase 10 GPU backend.

**JAX-style `grad(f)` as a language primitive** — speculative (P2). A pure function transform `grad(f)` where the compiler verifies `f` carries no effects could be offered natively. Deferred until comptime is stable and a tape-based library has revealed what such an API actually needs.

---

### Frontend UI Framework

A React / SwiftUI / Vue / Solid-class framework for building user interfaces, covering the full toolkit a web application needs on top of the `std.web` effect substrate and `host fn` bindings. Scope (any one of these is a substantial library in itself; the full framework bundles all of them):

- **Component model** — how UI components are declared, composed, and given lifecycle (mount / update / unmount). Expected shape: functions that take props and return a declarative view tree; lifecycle hooks modeled as channel subscriptions or provider injection rather than magic names.
- **Reactive primitives** — signals, observables, derived state, or whatever primitive the ecosystem converges on. The effect system + channels are the runtime substrate; the framework decides the user-facing reactivity model.
- **JSX / template syntax for HTML** — declarative view construction. Expected path: an `html!(...)`-style macro once macros ship, not a language feature. f-strings cover the simple interpolation case today.
- **Routing** — URL-to-view mapping, history integration, nested routes. Standard web-framework fare.
- **Styling** — CSS-in-Kara, utility-class generation, or a CSS-module-style convention. Library choice, not language concern.
- **Hydration protocol** — the contract between SSR-rendered HTML and client-side event binding. Depends on the framework's component model; see `design.md § Cross-target Compilation` for the provider-injection pattern that makes the same component run on both targets.

**What it rests on:**
- `design.md § Web / Host Effect Vocabulary` (the `Display` / `Input` / `Timer` / `Storage` / `Console` resources the framework calls into).
- `design.md § Host Functions` (the `host fn` primitives the stdlib exposes for DOM / events / storage).
- `design.md § Cross-target Compilation` (the SSR-shared-component + per-target-provider pattern the framework enforces on user code).
- `design.md § Async Host APIs on WASM` (channel-over-Promise pattern for host API integration).
- A macro system — not yet spec'd. Needed for ergonomic view syntax; without macros the framework works but the DX is `View.div(View.text("hello"))`-style.

**Why post-v1, not a stdlib ship:**
1. Every viable shape (React hooks, Solid signals, SwiftUI declarative, Vue composition API) has active ecosystem evolution. Committing Kāra's stdlib to one of them in v1 is an assumption bet the project cannot afford before the language has users.
2. The framework needs a macro system to be ergonomic. Macros are deferred.
3. Phase 10 (WASM codegen) must land first. Without a working WASM backend, a UI framework has nothing to run on.

**Pre-build checklist (all must be done before building this):**
- [ ] Phase 10 WASM codegen shipped and stable
- [ ] Macro system spec'd and landed (for `html!(...)` ergonomics — framework is buildable without but users will hit the view-syntax wall fast)
- [ ] `std.web` stdlib layer for `Display` / `Storage` / `Console` / `Timer` / `Input` host-fn bindings shipped

This entry is the canonical tracker for a Kāra frontend UI framework.

---

### Browser GPU Graphics Library (`kara-gfx` or equivalent)

A graphics and adjacent-real-time-media library equivalent to Rust's `wgpu` (~15k LoC) — textures, render passes, vertex / fragment pipelines, depth / stencil, swapchains, plus the adjacent Web Audio / Gamepad / PointerLock surfaces that real games and interactive apps need.

**Distinct from** the GPU *compute* story already in `design.md § GPU Subset Constraints` — compute-shader codegen (`#[gpu]` + `gpu.dispatch`) is a Kāra-compiler feature that ships in Phase 10. GPU *graphics* (render passes, pipelines, textures, swapchains) is a library built on top of that substrate plus WebGPU / Vulkan / Metal / DX12 bindings per target.

**What it rests on:**
- `design.md § GPU Subset Constraints` + `#[gpu]` compute shaders (for any shader authoring in the library).
- `design.md § Host Functions` (for WebGPU / Vulkan / Metal / DX12 host bindings — the library would expose one portable API that lowers to whichever host is available per target).
- `design.md § Web / Host Effect Vocabulary` (a `Gpu` resource alongside `Display` if WebGPU is treated as distinct from compute-side `GpuBuffer[_]`, or reuse of the compute-side resource if the distinction isn't useful — library decides).
- Phase 10 WASM codegen for browser delivery.

**Why post-v1, not a stdlib ship:**
1. Graphics API design is a full domain-specific design project (see `wgpu`'s multi-year evolution). The stdlib cannot absorb that cost.
2. The compute / graphics split is itself an open question — reusing the existing compute-side GPU resource vs. introducing a distinct graphics resource is a decision that benefits from ground-truth usage data before being frozen.

**Pre-build checklist:**
- [ ] Phase 10 WASM codegen shipped and stable
- [ ] `#[gpu]` compute-shader codegen shipped (SPIR-V / WGSL emission)
- [ ] `host fn` lowering on WASM stabilized (for WebGPU bindings)

**Cross-reference:** `design.md § GPU Subset Constraints` — the compute-side foundation.

---

### Full-Stack Server Web Framework (Django / Rails / Spring Boot / Phoenix class)

A server-side web framework bundling HTTP server, routing, middleware, ORM integration, templating, authentication, session/CSRF, forms/validation, admin/scaffolding tooling, and observability hooks into one opinionated stack. The slot occupied by Django (Python), Rails (Ruby), Spring Boot (Java), Phoenix (Elixir), Laravel (PHP), and ASP.NET Core MVC (C#).

**Philosophy axis — design decision for when this is built.** The existing frameworks cluster along two axes:

- **Monolithic / batteries-included** (Django, Rails, Laravel) — auto-admin UI, convention-over-configuration, tight ORM integration. "You don't assemble, you customize the provided shape."
- **Modular / DI-assembly** (Spring Boot, ASP.NET Core MVC) — pick starters, compose with dependency injection. Stronger microservices story, no auto-admin default.
- **Real-time-first** (Phoenix LiveView) — server-pushed UI via WebSocket channels, distinctive shape neither of the above has.

A Kāra equivalent picks one (or blends) when it is built; the P3 entry names the slot, not the philosophy.

**What it rests on:**
- Stdlib HTTP **server** (`std.http` is client-only in Phase 8; server is v1.5+ per `implementation_checklist/`).
- Database driver stdlib scope — see `deferred.md § Stdlib Scope for Non-Primitive Resources`.
- ORM story — see `deferred.md § Language-Integrated Query (SQL DSL) and ORM`.
- Effect system + provider injection (already landed — see `design.md § Provider-Rooted Resources`). These are the distinguishing substrate.
- Macros (for templating ergonomics — `html!(...)` or equivalent — and for admin-UI reflection / scaffolding).

**Kāra-specific differentiator — effect-scoped request handlers.** Existing frameworks have no way to express "this endpoint touches the user DB and sends email but nothing else" at the type level. Kāra's effect system does: a request handler's effect set becomes part of its signature; the framework can auto-verify effect budgets per route, inject providers (DB, cache, auth, email) by type, and refuse to register a handler whose effect set violates a per-service policy. This is a shape that Django / Rails / Spring Boot cannot easily retrofit and would be the primary justification for building a Kāra-specific framework rather than targeting compatibility with an existing one.

**Why post-v1, not a stdlib ship:**

1. Each existing framework represents a decade of design iteration around a specific philosophy. Committing Kāra's stdlib to one of those philosophies in v1 is an assumption bet the language cannot afford before it has users.
2. The substrate is not in place — HTTP server, ORM, and templating-macro primitives are all separately deferred or stdlib-scoped.
3. Admin-UI generation (Django admin, Rails scaffolding) depends on a reflection / derive-macro story that is itself post-v1.
4. Effect-scoped handlers — the differentiator — only becomes meaningful once real effect-declaration patterns have emerged in user code. Designing the framework's effect-policy vocabulary before seeing idiomatic effect usage would freeze it too early.

**Pre-build checklist (all must be done before building this):**
- [ ] `std.http` server primitives shipped (currently v1.5+)
- [ ] Database driver(s) available in stdlib or well-established community crate
- [ ] Macros shipped (for templating ergonomics and admin scaffolding)
- [ ] ORM story decided at the library vs. language-integrated level per `deferred.md § Language-Integrated Query (SQL DSL) and ORM`
- [ ] Real-world effect-usage patterns observed in Kāra apps so the framework's effect-policy vocabulary is grounded in data, not speculation

**Cross-reference:** `deferred.md § Stdlib Scope for Non-Primitive Resources` (database driver scope); `deferred.md § Language-Integrated Query (SQL DSL) and ORM` (ORM shape); `design.md § Provider-Rooted Resources` (effect-scoped injection substrate).

---

### Rust ↔ Kāra Bidirectional Transpiler

Two officially supported, stability-committed transforms:

- `karac --emit rust` — Kāra source → idiomatic, maintainable Rust.
- `karac --emit kara --from rust` — Rust source → idiomatic, maintainable Kāra.

Neither is a debugging aid or backup plan; both are published output targets. Common-subset code transpiles mechanically (deterministic, reproducible, no LLM in the loop) — owned/borrowed data, generics, traits, pattern matching, most control flow, most effect annotations. Divergent features (effect system annotations beyond docs, layout blocks, shared structs, anything without a 1:1 Rust equivalent) lower via an LLM-assisted impedance-matching tier that emits best-effort functionally-equivalent Rust. Impedance-matched lowerings are cached and reviewable so non-determinism doesn't leak into every build.

**Strong stance — no design compromise for transpile.** Kāra features are decided on their own merits. The transpiler bends, never the language. If a Kāra construct has no mechanical Rust equivalent, that is the transpiler's problem to solve.

**Ownership mode mapping for the common subset:** `ref T` ↔ `&T`, `mut ref T` ↔ `&mut T`, owned ↔ `T`. Receivers map symmetrically. Effects become Kāra-checker metadata on the Kāra side and Rust-side doc comments / proc-macro annotations on the Rust side — they matter for Kāra checking, not for Rust compilation.

**What it rests on:**
- Kāra's "Rust without being Rust" design principle (semantic compatibility with Rust for the common subset is already committed).
- Stable language spec — transpiling against a moving spec produces churn.
- Stable public AST / semantic IR.
- A high-capability LLM invoked at transpile time plus a maintained prompt / test-suite artifact for impedance matching, and a fallback compile-time error path when verification fails ("this construct cannot be mechanically transpiled and LLM verification failed — file an issue").

**Why post-v1, not a stdlib / compiler ship:**
1. Language must be stable. A transpiler whose output is a stability-committed artifact cannot chase a moving spec.
2. Two backends is real engineering cost. LLVM (performance, Phase 10 GPU) and Rust-transpile (adoption, exit-ramp) both need maintenance.
3. LLM infrastructure — prompt catalog, verification harness, regression suites, offline-capable fallback library — is a nontrivial maintained artifact in itself.
4. Premature adoption work locks in bad decisions for early users. Ship the language coherently first; add the transpiler when adoption becomes an active concern.

**Pre-build checklist (all must be done before building this):**
- [ ] Kāra 1.0 language spec stable.
- [ ] Stable public AST / semantic IR suitable for both emission directions.
- [ ] LLM verification infrastructure decided (property tests against representative inputs, or formal methods, or reviewed test suites).
- [ ] Impedance-matching cache format and review workflow defined.
- [ ] Offline-capable fallback — library of pre-verified lowering recipes for known divergent patterns — so builds don't require network access to the LLM.

**Cross-reference:** `docs/design.md` — the "Rust without being Rust" principle this rests on. Transpiler is a separate backend from Phase 10 LLVM/GPU codegen, not a replacement.

---

### File-Level Rust / Kāra Coexistence

`.kara` and `.rs` files living side by side in the same project — whether Cargo-rooted or Kāra-rooted. Cross-language module imports resolve to the same shared IR: a Kāra file writes `use rust_module::func;`, a Rust file writes `use kara_module::func;`, and both see each other's types and functions as native to their own language for the common subset. Shared `Cargo.toml` / equivalent governs both sides; crates.io is the shared registry; existing Cargo-based CI/CD works unchanged.

**Concrete sketch for the Cargo-rooted direction:** a `build.rs` script invokes `karac --emit rust --out-dir ${OUT_DIR}` on all `.kara` files under `src/`. Cargo compiles the emitted Rust alongside the hand-written Rust. From Cargo's perspective, `build.rs` just generates more Rust source — same category of extension as `bindgen` / `prost`. `cargo build`, `cargo test`, `cargo check` all work unchanged.

**For the Kāra-rooted direction:** symmetric — Kāra's build system detects `.rs` files, transpiles them via Rust → Kāra and compiles through the Kāra pipeline. Alternatively, Rust source is compiled directly to Kāra's IR via a Rust frontend (skipping the source-to-source round-trip for speed). Both paths are viable.

**Why it matters for adoption:** a Rust team drops one `.kara` file where Kāra's ergonomics help most; a Kāra team imports Rust crates as native Kāra libraries without an FFI boundary. No parallel ecosystem to bootstrap — the largest cost of launching a new language is zero here.

**What it rests on:**
- `Rust ↔ Kāra Bidirectional Transpiler` (above) — prerequisite; cross-language imports need both directions well-defined.
- `build.rs` integration on the Cargo-rooted side (standard Cargo extension, nothing novel).
- A Rust frontend to Kāra's IR for the Kāra-rooted direction — tractable given the semantic equivalence, but genuinely newer ground than the Cargo side.
- Debugger story — source maps back to `.kara` / `.rs` as a starting point; native multi-language debugger later.
- LSP — thin Kāra LSP over rust-analyzer via the transpile is plausible as bootstrapping; standalone Kāra LSP is the long-term shape.

**Why post-v1, not a stdlib / compiler ship:**
1. The bidirectional transpiler must exist and be stable first — coexistence is an integration layer on top of it.
2. Cross-language module imports for *divergent* features depend on LLM-assisted binding shims from the transpiler's impedance-matching tier; those need to be reliable before the coexistence story is defensible.
3. Build-tool integration specifics (`build.rs` vs. standalone `karac` Cargo subcommand vs. a more integrated form) are an open design decision that benefits from real user pressure before committing.

**Pre-build checklist (all must be done before building this):**
- [ ] Bidirectional transpiler (above) shipped and mature in both directions.
- [ ] Stable Kāra IR suitable for consumption by a Rust frontend.
- [ ] Build-tool integration approach decided (`build.rs` generator vs. Cargo subcommand vs. rustc plugin — leaning `build.rs` for pragmatic reasons).
- [ ] Debugger source-map story prototyped.

**Cross-reference:** `Rust ↔ Kāra Bidirectional Transpiler` (above) — hard prerequisite.

---

### Rust ↔ Kāra Web Playground

A browser-hosted UI where a user pastes Rust on the left and sees Kāra on the right — and vice versa. No install, no account, no commitment. Evaluates the language against the user's own code in ~30 seconds.

**Why it's a separate P3 entry rather than bundled with the transpiler:** the playground is a distinct engineering investment — a UI over the transpiler, not part of the transpiler itself — and it has different prerequisites (front-end UI framework, WASM codegen so the transpiler can run client-side, or a server-hosted transpile endpoint).

**What it rests on:**
- `Rust ↔ Kāra Bidirectional Transpiler` (above) — the playground is a UI over transforms that already ship.
- `Frontend UI Framework` (above, in this section) — the playground is a UI; if the Kāra-built-frontend slot isn't filled, the playground ships on an existing framework (React, Solid, or similar via `host fn` bindings) as a pragmatic shortcut.
- Phase 10 WASM codegen — if the transpiler runs client-side. If it runs server-side instead (paste-and-POST), WASM codegen is not required for the playground itself, only the transpile-to-browser path.
- Output quality from the transpiler mature enough not to embarrass the project.

**Why post-v1, not a stdlib / compiler ship:**
1. Best deployed once there's a 1.0 to point people at — otherwise the playground shows off an unfinished language and a half-baked transpiler.
2. The playground IS the marketing for the adoption mechanism. Shipping it before the language is coherent undermines the positioning.
3. Costs nothing incremental *after* the transpiler ships — but that's a "then," not a "now."

**Pre-build checklist (all must be done before building this):**
- [ ] Bidirectional transpiler mature in both directions with output quality credible for public exposure.
- [ ] Kāra 1.0 shipped.
- [ ] Transpile execution model decided (client-side WASM vs. server-side endpoint).
- [ ] If client-side: Phase 10 WASM codegen shipped.
- [ ] Landing-page / positioning copy aligned with the peer-language framing (not "Kāra is Rust's Kotlin").

**Cross-reference:** `Rust ↔ Kāra Bidirectional Transpiler` (above) — hard prerequisite.

---

### Stdlib and Ecosystem Security Conventions

Security-related conventions that are not language features but are design choices to make deliberately when the respective stdlib module or ecosystem tooling is built. Listed here so the decisions are considered rather than accreted by default.

**Safe-by-default regex engine.** When `std.regex` ships (Phase 11 per `design.md § Deferred Items` raw-string entry), default to a non-backtracking engine (RE2-style / linear-time matching) to prevent ReDoS. Catastrophic-backtracking regex engines are a recurring source of production outages — a non-backtracking default trades a small syntactic-feature reduction (no backreferences, no lookahead in patterns that cannot be compiled to a DFA) for a large class of DoS bugs eliminated at no cost to the caller. Explicit opt-in to a backtracking engine remains available for use cases (complex PCRE patterns, interactive text tools) where the caller controls the inputs.

**Cryptographic primitive choices.** When `std.crypto` ships, default primitives are chosen from modern, widely-reviewed designs: ChaCha20Poly1305 for AEAD, X25519 for ECDH, Ed25519 for signatures, Argon2id for password hashing, BLAKE3 for non-cryptographic hashing where performance matters. Legacy-compatible defaults (RC4, MD5 for anything, 3DES, SHA-1 for anything) are not shipped. Users needing legacy compatibility import an explicit `crypto.legacy` module — present, but always a visible choice in code review.

**`#[must_use]` on security-sensitive stdlib return types.** Stdlib functions whose return value encodes a security decision — `authenticate(...) -> Result[Session, AuthError]`, `verify(...) -> Result[(), VerifyError]`, `check_csrf_token(...) -> Result[(), CsrfError]` — carry `#[must_use]` in their signatures. Ignoring these results is almost always a bug; `#[must_use]` (the attribute itself lands in v1 — see `design.md § #[must_use] on Types`) makes the mistake a warning. The convention applies to stdlib design going forward; library authors are encouraged to follow.

**Supply-chain signing and SBOM.** Kāra's package manager, when it ships, is expected to support Sigstore (or equivalent) for signed releases and emit SBOM metadata (SPDX or CycloneDX) alongside build artifacts. Entirely outside the language; listed here for continuity of intent so the requirement isn't rediscovered during package-manager design.

**Cross-reference:** `design.md § Feature 2: Effect Types` — the capability substrate these conventions complement; `design.md § Secret Type (Secret[T])` — the in-language primitive that handles the credential-leak vector these conventions round out.

---

### Terminal Control Library (`std.terminal` or `kara-terminal`)

A stdlib module or external package providing cursor movement, screen clearing, and color control — the minimum needed to write CLIs, TUI dashboards, and game-of-life–style display loops without embedding raw ANSI escape sequences.

**Why not in v1 stdlib.** Terminal control is platform-specific (ANSI/VT on Unix/macOS, Console API on Windows), depends on terminal capability queries (`TERM`, `COLORTERM`), and requires graceful fallback for pipes and redirected output. This scope is better owned by a dedicated library (Rust's `crossterm` is the reference point) than baked into the language's core stdlib. For v1, callers write raw ANSI via `print` / `println` — which correctly carries `writes(Stdout)` and participates in effect tracking — at the cost of platform portability.

**Minimum API surface:**

```kara
pub fn clear_screen() with writes(Stdout)
pub fn move_cursor(row: i64, col: i64) with writes(Stdout)
pub fn hide_cursor() with writes(Stdout)
pub fn show_cursor() with writes(Stdout)
pub fn set_color(fg: Color, bg: Color) with writes(Stdout)
pub fn reset_color() with writes(Stdout)

pub enum Color { Black, Red, Green, Yellow, Blue, Magenta, Cyan, White, Reset, Rgb(u8, u8, u8) }
```

All functions carry `writes(Stdout)` so they participate in conflict analysis and are correctly serialized against other stdout writes in a parallel region. Platform dispatch (ANSI vs. Windows Console API) is hidden behind the function boundary.

**What it rests on:**
- Phase 8 stdlib shipping (same phase as the rest of the non-core collections and I/O modules)
- `writes(Stdout)` effect, which is already in v1

**Cross-reference:** `design.md § I/O Functions` — `print`/`println` with `writes(Stdout)` are the v1 primitive; this module is an ergonomic layer above them.

---

### Profile-Guided Optimization Loop

**Decision:** Defer instrumented or sample-based PGO from v1. The compiler queries channel ([design.md § Compiler Queries](design.md#compiler-queries)) ships at v1 covering *intent-shaped* optimization decisions; PGO answers *distribution-shaped* questions and is the complementary signal, not a substitute. Both will eventually exist; v1 ships only the queries channel.

**Why deferred.** PGO requires a full instrumented-or-sampled build flow, a representative-workload protocol, multi-platform replication of the workload, and the storage / merge / version-skew machinery for `.profdata` / AutoFDO files. Large surface, separate from the queries channel architecturally. The architectural prerequisites — debug info quality and symbol-stable identity — are partially helped by the v1 P0 commit (path-based DefId; see `design.md § Compiler Queries`) but neither prerequisite blocks the queries channel itself.

**Distinction from the queries channel.** PGO answers questions the LLM author cannot ("what fraction of inputs are ≤16 bytes?", "which call site is on the hot path in production?"); the queries channel answers questions PGO cannot ("is this branch unreachable in correct usage?", "should this trait method specialize on `i64`?"). The two operate on different signals — runtime measurement vs. spec context — and their outputs are independent. A v1.x or v2 build will likely consume both: the queries channel for intent, PGO for distribution.

**Promotion gate (P2 → scheduled).** Promote when (a) the queries channel has shipped P1.1–P1.3 and observed real-world resolution patterns, AND (b) the v1 stable item identity primitive (DefId) has shown adequate symbol stability across realistic source-edit patterns to support PGO-style profile keying. Without (a), shipping PGO first risks confusing the channel-vs-PGO boundary in user mental models; without (b), profile-key drift dominates the cost-benefit calculus.

**Why non-breaking:** Purely additive. PGO flags (`-fprofile-generate`/`-fprofile-use` analogs), the `.profdata` format, and the corresponding `karac build --profile-guided=...` invocation are all new build-time surface. Existing builds continue unchanged.

**Cross-reference:** `design.md § Compiler Queries` (the v1 channel; PGO is its complementary signal); `design.md § Specification Layers > Reported behavior > Auto-concurrency cost-model decisions` (the cost model that PGO would feed into); brainstorm archive `brainstorming/archive/v63_llm_compiler_query_channel.md` Problem 7 (PGO-vs-queries scope decision).

---

### MLGO Trained Policy Artifacts

**Decision:** Defer MLGO-style trained policy artifacts from v1. A trained model is the *answer* (compiler output), not the *question* (compiler input) — different shape from the queries channel. Possible v2 if real-world data shows the queries channel alone underperforms.

**Why deferred.** LLVM's MLGO trains TFLite policies for inliner / regalloc decisions and ships the trained model as a build-time artifact. Kāra's queries channel takes a different stance: surface the decision back to the LLM author at authorship time and bake the resolution into source. The MLGO and queries-channel approaches are not contradictory — they could coexist post-v1 — but the queries channel is the smaller commitment and ships first.

**Promotion gate (P2 → scheduled).** Promote when (a) the queries channel catalogue has shipped P1.1–P1.6 and observed real-world adoption, AND (b) measurable evidence exists that author-resolved queries fail to capture optimization wins MLGO would capture. Without (a), shipping a policy artifact pre-empties the channel; without (b), the case for MLGO over queries is hypothetical.

**Why non-breaking:** Additive. Trained policy artifacts are build-time inputs to specific optimization passes; they do not change source semantics, public APIs, or the queries channel's interface.

**Cross-reference:** `design.md § Compiler Queries` (the v1 alternative); brainstorm archive `brainstorming/archive/v63_llm_compiler_query_channel.md` Problem 7.

---

### Schedule-Language Layer

**Decision:** Defer Halide-style decoupled schedule languages from v1. `layout` blocks (Feature 1) cover the data-layout half of "separate authoring surface for optimization"; the loop-schedule half is deferred. The queries channel ([design.md § Compiler Queries](design.md#compiler-queries)) is the smaller v1 commitment for surfacing optimization decisions.

**Why deferred.** Halide / TVM / Tiramisu / Exo all separate the algorithm from the schedule, with the schedule being a complete, parallel authoring discipline targeted at perf engineers writing tight numeric loops. This is a substantial language commitment with its own grammar, its own type system for schedule values, and its own audience. The queries channel surfaces only the un-baked residual decisions and resolves them as item attributes — a much narrower surface that fits Kāra's existing attribute discipline. If real Kāra workloads accumulate that need full decoupled schedules, that's the signal to add the schedule layer alongside the queries channel.

**Promotion gate (P2 → scheduled).** Promote when (a) Phase 11 numerical / data-science stdlib has shipped, AND (b) a corpus of Kāra numerical code accumulates that hits the limits of `layout` blocks + queries-channel resolution annotations. Without (a), the audience for a schedule language doesn't exist yet; without (b), the queries channel + `layout` blocks may be sufficient, and shipping a schedule language pre-empts the simpler combination.

**Why non-breaking:** Additive — a new authoring surface that opts in. Existing programs without schedule annotations would compile unchanged.

**Cross-reference:** `design.md § Feature 1` (layout blocks — the v1 form of decoupled-layout authoring); `design.md § Compiler Queries` (the v1 form of decoupled-optimization-decision authoring); brainstorm archive `brainstorming/archive/v63_llm_compiler_query_channel.md` Problem 7.

---

### Verifier-Backed Query Resolution

**Decision:** Defer Alive2-class equivalence verification of author-supplied query resolution annotations from v1. Trust-the-author is the v1 baseline; verification is the known future direction.

**Why deferred.** A wrong `#[likely]` or `#[specialize(T = i64)]` produces suboptimal codegen — no worse than today's annotation surface. But the queries channel deliberately concentrates author claims into a structured surface, which both invites *more* claims and makes those claims *more* tractable for verification than today's scattered annotations. STOKE / Souper / Alive2 / Hydra / Minotaur / Iago demonstrate that verifier-backed claim-checking is feasible at the LLVM-IR level; the same primitives could check author claims like "this branch is unreachable in correct usage" against the program's effect/type structure.

**Promotion gate (P2 → scheduled).** Promote when (a) the queries channel catalogue has shipped P1.1–P1.6 and adoption has surfaced concrete cases of wrong author claims producing observable codegen pessimization, AND (b) a verifier infrastructure exists in Kāra (Alive2-style equivalence checks against author invariants, separate from but adjacent to the existing effect / ownership / type systems). Without (a), the case for verification is theoretical; without (b), shipping verifier-backed resolution requires building the verifier from scratch alongside, which doubles the v2 commitment.

**Intended design shape.** A `karac check --verify-resolutions` mode (or build-time flag) that, for each query resolution annotation in the source tree, attempts to verify the author's claim against the surrounding program structure. Verification failures emit a new diagnostic class — distinct from "this annotation is suboptimal" — that names the specific invariant the verifier could not establish. Authors who don't run verification continue to operate in the v1 trust-the-author mode.

**Why non-breaking:** Purely additive. Existing resolution annotations continue to be honored at trust-the-author level by the codegen path. The new verification mode is opt-in; failure to run it does not change codegen behavior.

**Cross-reference:** `design.md § Compiler Queries` (the v1 trust-the-author baseline); `design.md § Specification Layers > Compiler Queries > Author claims are trusted at v1` (the v1 stance); brainstorm archive `brainstorming/archive/v63_llm_compiler_query_channel.md` Problem 7 and Open Questions on author-claim verification.

---

## Permanent Omissions

Features the project has explicitly chosen *not* to build. Distinct from P1/P2/P3 (which all express some degree of "will or might ship"): permanent-omission entries record decisions that the language will *not* allocate design surface or stdlib infrastructure to a given feature, even post-v1. The category exists so the decision is durable — future contributors can see *why* the omission is intentional rather than rediscovering the question and re-litigating it.

A permanent omission is not "we'll never reconsider." If a future ecosystem reality genuinely changes the trade-offs, an entry can be moved out of this section and reopened. The bar for that motion is high: a concrete real-world need that the omission's stated rationale fails to address.

### Dynamic Linking (Runtime `dlopen` / Plugin Loading)

**Decision:** Static linking is the canonical default. Kāra does not provide dynamic-linking (`dlopen`, `dlsym`, plugin system, runtime `.so`/`.dll` loading) as a first-class feature. The runtime is statically linked into every `karac build` artifact; users who need plugin-style extensibility use one of the alternatives below.

**Why permanent:** Dynamic linking trades binary size for distribution headache (versioned runtime dependency on the target machine, ABI stability burden, package-manager friction, install-dance for the runtime, dlopen-at-startup overhead). Static linking matches Kāra's value proposition: predictable memory layout, zero-cost abstractions, compile-time effect verification, "ship one binary, run it anywhere with the right architecture." Dynamic linking would also blur the effect system at the FFI boundary — runtime-loaded code can't participate in compile-time effect verification, so any plugin ABI ends up bypassing the language's main correctness story.

The binary-size cost that motivates dynamic-linking adoption in C/C++ is addressed in Kāra by other means: monomorphized collections (post-v1, see `roadmap.md`), strip + LTO + DCE in release builds, and runtime decomposition into per-feature archives if shipping data ever justifies it. The wins available from those mechanisms are sufficient without changing the deployment model.

**Alternatives (when extensibility is genuinely needed):**

1. **IPC + effect-typed services.** Separate processes communicating over channels; effects are tracked per-process boundary. This is the recommended pattern for plugin-style extensibility in Kāra — it preserves the language's correctness guarantees and matches the auto-concurrency story.
2. **WASM plugins.** AOT-compiled plugin code, fully sandboxed, effect-safe by construction. Phase 10 (additional compilation targets) will support WASM as a target, enabling this pattern.
3. **Hand-rolled `dlopen` in `unsafe` blocks.** Users who need raw C-style plugins (e.g., loading vendor-supplied `.so` files) can use the FFI surface in `unsafe` blocks. They opt out of the effect system at that boundary and accept manual responsibility for the loaded code's behavior. This is a v1-supported escape hatch, not a recommended default.

**Why non-breaking:** Not a restriction on existing code. Purely a statement that the language does not allocate first-class surface or stdlib infrastructure to dynamic linking. Users who need the feature today construct it through FFI; those mechanisms remain available.

**Cross-reference:** `design.md § Foreign Function Interface` (manual `dlopen` in `unsafe` blocks); `roadmap.md § Phase 10` (WASM target as the supported plugin pattern); `design.md § Auto-Concurrency via Effect Analysis` (IPC + channels as the in-language extensibility pattern).
