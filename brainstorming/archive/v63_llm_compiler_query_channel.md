# 63 — LLM ↔ compiler query channel

Date: 2026-05-06. Topic: a structured channel by which the Kāra AOT compiler enumerates optimization decisions it had to hedge on, surfaces them back to the author, and bakes the author's resolution into the compiled artifact. Goal: close the JIT-vs-AOT gap not by adding runtime profiling but by routing semantic intent through the LLM author whose understanding of the spec is the relevant signal.

Framing: AOT compilers lack runtime distribution data; JITs have it but pay re-compile cost and runtime overhead. Mainstream attempts to bridge — PGO, autotuners, schedule languages, and recent LLM-in-the-compile-loop work — all sit on the *compiler's* side of the authorship boundary. None enumerate decisions back to the author and accept structured answers as source-level annotations. That empty quadrant is what this doc explores. The proposal is not that Kāra replaces PGO, but that a query channel is the one piece of LLM-first compiler UX no other language has. Architecturally: **stable item identity is the load-bearing P0 prerequisite; everything else is additive P1.** V1 (P0+P1) target.

---

## Problem 1 — the shape of the channel

**Mechanism.** Each compile produces, alongside the binary, a `karac.queries.json` (or equivalent) listing every decision the compiler hedged on. Each query carries:

- A **stable ID** (path-based, not byte-offset; see Problem 4).
- The **decision site** — function, expression, type — with source span for display.
- The **options** (e.g. `inline | no_inline | inline_always`) with the compiler's rationale for each.
- The **default the compiler chose**, plus its confidence ("conservative — would rather defer").
- The **resolution surface** — which attribute, written where, would pin the answer.

The author (LLM) reads the queries report, decides which to resolve, writes annotations on the relevant items. Next compile re-emits the report — resolved queries drop out, new ones may appear as the codebase changes.

The unresolved queries are not warnings. They are the compiler saying "I picked the safe default; you can do better if you have spec context I don't." A clean codebase has zero open queries the author cares about, not zero queries.

**What this is *not*.**

- Not a runtime-profile substitute. Distribution-shaped questions ("what fraction of inputs are ≤16 bytes?") cannot be answered from spec; PGO / sampling / live feedback is the correct tool. The query channel is for **intent-shaped** questions ("is this the hot path?", "should this trait method specialize on type X?", "is this allocation expected to escape?").
- Not a lint pass. Lints fire on suspected mistakes; queries fire on optimization opportunities the compiler explicitly chose not to take.
- Not a schedule language. Halide-style schedules are a separate authoring discipline that runs alongside the algorithm. Queries are diff-shaped — only the un-baked decisions are surfaced, not the full optimization plan.
- Not LLM-in-the-compile-loop. The LLM operates at *authorship time*, not inside a single compile. Decisions persist as committed source annotations, auditable like any other code.

---

## Problem 2 — prior art map

Distilled from research pass 2026-05-06. Nine axes; the empty quadrant matters.

### Profile-Guided Optimization

GCC `-fprofile-generate`/`-fprofile-use`, LLVM PGO (instrumented + AutoFDO sample-based), MSVC PGO. **BOLT** (post-link binary optimizer, ASPLOS '19) and **Propeller** (relink-time, ASPLOS '23) push it later in the pipeline; both report ~5–10% on top of AutoFDO on warehouse workloads. Linux kernel mainlined AutoFDO+Propeller in 2024. Drives inlining, block layout, branch hints, function ordering, devirtualization, register allocation. Persistence: binary counters (`.gcda`, `.profdata`) or text-format AutoFDO. Pain: needs a representative workload, instrument-or-sample build, multi-platform replication.

**Direction:** runtime → compiler. Orthogonal to query channel.

### Author-intent annotations

- **Rust** — `#[inline]`, `#[inline(always|never)]`, `#[cold]`, `#[target_feature]`, `#[track_caller]`, `#[repr(...)]`, `core::hint::{likely, unlikely, black_box}`. `#[hot]` and `#[optimize(size|speed)]` remain unstable as of 2026.
- **C/C++** — `__builtin_expect`, `[[likely]]/[[unlikely]]`, `__attribute__((hot|cold|flatten|always_inline|pure|const))`, `restrict`, `[[clang::musttail]]`, `#pragma omp simd`.
- **GHC** — `{-# INLINE #-}`, `{-# INLINABLE #-}`, `{-# SPECIALIZE #-}`, `{-# RULES #-}` (author-supplied rewrite rules — closest mainstream "author teaches compiler"), `{-# UNPACK #-}`, `{-# SCC #-}`.
- **Julia** — `@inbounds`, `@simd`, `@inline`, `@nospecialize`, `@fastmath`, `Base.@assume_effects`. `@code_warntype` is a feedback channel of sorts.
- **OCaml flambda2** — `[@inline]`, `[@unrolled n]`, `[@specialise]`, `[@local]`, `[@unboxed]`. Definition- and call-site forms.
- **Swift** — `@inline(__always)`, `@inlinable`, `@_specialize`, `@frozen`, `@_effects(...)`.

**Direction:** author → compiler, single-shot, write-only. The compiler accepts but never requests. This is the *answer* surface for the query channel; the channel adds the *question* surface.

### Schedule languages

Halide (algorithm/schedule split, PLDI '13, CACM '18); TVM (AutoTVM templates, Ansor template-free auto-scheduler); Tiramisu (polyhedral); MLIR transform dialect (schedules as IR); Exo / Exo 2 (PLDI '22, ASPLOS '25 — "exocompilation": users grow the scheduling language).

**Direction:** author writes a separate program (the schedule). Closest to a structured authoring channel for optimization. But: the schedule is *complete*, not *enumerated by the compiler as residual*. And it targets a different unit (loop nests) at a different audience (perf engineers).

### Auto-tuners

ATLAS (BLAS, '98), Spiral (DSP, '05), OpenTuner (domain-agnostic, PACT '14), Halide auto-scheduler (NN cost model, '19), Futhark autotune (named integer thresholds → `.tuning` file).

**Direction:** compiler enumerates parameter space; *empirical search* picks values. The query channel is the same enumeration shape with a different oracle: LLM author with spec context instead of a benchmark runner with a workload.

### Interactive / proof-style compilation

Coq tactics, Lean `exact?`/`apply?` proof search suggestions, Idris hole-driven development, F\*/Dafny SMT loop, Agda holes + Agsy. **The compiler-asks / author-answers UX has decades of precedent — but only for types and proofs.** No mainstream-ish system applies the pattern to optimization decisions. This is the strongest analogy and the strongest evidence the UX works; it just hasn't been pointed at this problem.

### Compiler diagnostic suggestions

rustc `--explain` + machine-applicable `Suggestion` payloads; `cargo fix`/`cargo clippy --fix`; clang-tidy FixIts. **LLVM optimization remarks** (`-Rpass`, `-Rpass-missed`, `-Rpass-analysis` → YAML structured output, `cargo-remark` aggregator) are the closest existing thing — the compiler reports inlining/vectorization/devirtualization decisions made and missed. **One-way, read-only, no structured response surface.** GCC `-fopt-info-missed` is the analog. Lift remarks to stable IDs with machine-applicable answers and you are roughly in the query-channel space.

### LLM ↔ compiler interaction (2024–2026)

- Meta **LLM Compiler** (arXiv:2407.02524, '24) — 7B/13B Code-Llama variants on LLVM IR; replaces autotuner search.
- **Compiler-Generated Feedback for LLMs** (Grubisic et al., '24) — LLM proposes pass list, compiler returns metrics + IR, LLM revises. Inside-the-compile loop.
- **Reasoning Compiler** (NeurIPS '25) — LLM-proposed transforms inside MCTS; 5× speedup over TVM.
- **Compiler-R1**, **CompileAgent** ('24–'26) — agentic frameworks wrapping clang/LLVM as tools.
- Surveys: arXiv:2501.01277 ("LMs for Code Optimization"), arXiv:2601.02045 ("New Compiler Stack").

**Pattern:** every published system places the LLM *inside* the compiler search loop, not as the *author* answering enumerated questions across compile invocations. The query channel takes the LLM out of the compile and back to authorship-time, where decisions persist as committed annotations.

### Superoptimization / verified optimization

STOKE (MCMC over x86, ASPLOS '13), Souper (SMT-synthesis peepholes), Alive2 (LLVM IR equivalence verifier — load-bearing for LLVM correctness), Hydra & Minotaur (OOPSLA '24), Iago + LLM peephole-detection work ('25). **Adjacent**: oracle-driven optimization with verification. Not the channel shape, but the verification primitive (Alive2-class equivalence) is what makes it safe to *act* on author-declared invariants ("yes, this branch is dead in my workload") without losing soundness.

### Profile representation formats

`.gcda` / `.profdata` (binary, structural-hash-keyed for source-drift resilience), AutoFDO text format (line-offset symbolic, commit-friendly), MLGO TFLite policy artifacts (LLVM ML-driven inliner / regalloc), Halide schedule strings, OpenTuner DBs, Futhark `.tuning`. **Source-adjacent, regeneratable, version-controllable** is the right shape for a "queries answered" file. Structural-hash keying (LLVM `.profdata`) is the precedent for surviving source edits.

### Empty quadrant

Across all nine axes:
- No system has the **compiler enumerate residual optimization decisions** at item granularity, with stable IDs, surfaced back to authorship.
- No system uses the **interactive proof-style UX** (axis 5) for optimization rather than typing/proofs.
- LLVM remarks (axis 6) are the closest existing thing but lack stable IDs and structured response.
- 2024–26 LLM-compiler work treats the LLM as a search-loop participant; *author-time intent through a typed channel* is unexplored.

The novelty is not a new optimization, a new annotation, or a new search algorithm. It's the **UX shape** — enumerated queries, stable IDs, source-level resolutions — applied to optimization for the first time. Plausibly defensible as net-new.

---

## Problem 3 — fit with existing Kāra infrastructure

Repo scan 2026-05-06 confirms the channel rides on existing scaffolding rather than requiring fresh subsystems.

- **Attribute AST exists** (`src/ast.rs:641`): `Attribute { name, args, string_value, span }`. Parser accepts unknown attributes without rejection (`src/parser.rs:1962`). New query-resolving attributes are additive.
- **Diagnostics are per-phase** (no unified `Diagnostic`). Each phase has its own result struct (`OwnershipCheckResult`, `TypeCheckResult`, `ResolveError`, `LintDiagnostic`). The channel adds a `queries: Vec<CompilerQuery>` field per phase result rather than refactoring diagnostics.
- **`karac query` already exists** (`src/cli.rs:73`) with kinds `Effects | Ownership | Concurrency | CostSummary`. New `Queries` kind slots in alongside.
- **`karac explain` does NOT exist yet** — open question whether queries surface via extending `karac query` or via the long-promised `karac explain`.
- **Decision sites already hedge in the compiler:**
  - RC flavor selection (`src/ownership.rs:360`) — Rc → Arc when thread-crossing detected; emits `RcFallbackNote`.
  - Generic monomorphization (typechecker) — count of instantiations is reportable.
  - Auto-concurrency-group fork decisions (`src/concurrency.rs`) — cost-model unspecified for v1.
  - Private-function effect inference vs declared boundary.
- **Strong design.md precedent at §113–146** (Specification Layers): the spec already classifies "reported behavior" — inferred effects, would-be parameter modes, RC representation choices, monomorphization counts — as *reportable but unstable across compiler versions*. **Quote (§142): "the inferred 'would-be' parameter mode shown by `karac explain` alongside the declared mode, for performance diagnostics... the inferred value is diagnostic, not contractual."** The query channel formalizes and extends this layer.
- **`#[compiler_builtin]` is the implementation template** (CR-202 slice 2, recent commits): parse → resolver gate (`E0237`) → typechecker registration → phase-specific dispatch. Each new query-resolving attribute follows the same pattern.
- **Critical gap — stable identity.** Today the compiler uses `SpanKey` (byte offset + length) as item identity (`src/resolver.rs`, `src/use_classifier.rs`). Brittle across edits; inserting a line shifts every downstream offset. **No DefId-style path-based identity exists.** Queries that don't survive trivial edits are useless, so this becomes the load-bearing P0 item.

---

## Problem 4 — design surface (sketch, not lock)

Six axes the brainstorm needs to settle. Listed as a/b/c options where alternatives exist; recommendations are tentative.

### A. Stable identity for query targets

a. **Path-based DefId** — `module::function::param[3]`, or `module::function::expr#7` for sub-item sites. Survives line/byte edits as long as the item path is unchanged. Standard in rustc (`DefPathHash`), LLVM `.profdata` keys, etc.

b. **Structural hash** — hash the AST subtree at the decision site, key the query by hash. Survives renames inside a body but not signature changes. Used by LLVM PGO for source-drift resilience.

c. **Hybrid** — DefId for the carrying item + structural hash for the sub-item slot. Survives both renames-within-body and adjacent edits.

**Tentative:** (c). The DefId-only approach loses sub-item resolution; structural-hash-only loses readability when the report is shown to a human. Hybrid keeps the DefId visible in the report (`my_module::sort_inplace`) and the hash invisible (used to disambiguate which `if` inside the body).

### B. Resolution surface — annotation in source vs sidecar file

a. **Annotations on the item** — `#[hot]`, `#[likely(branch)]`, `#[specialize_on(T = i64)]`. Lives in the source. Mirrors existing `#[compiler_builtin]` pattern.

b. **Sidecar file** — `karac.queries.toml` next to source, queries answered by ID. Doesn't pollute source.

c. **Both** — annotations for item-level decisions (the common case), sidecar for sub-item or per-call-site decisions where annotation syntax is awkward.

**Tentative:** (a) for the common case, (c) only if specific decisions cannot be expressed as item attributes. Reasoning: annotations are reviewable as code, version-controlled, scoped to the item, and survive moves (with the item). Sidecars are write-once-by-tool and degrade quickly; the LLVM optimization-remarks YAML format is a cautionary precedent — almost no one reads or commits them.

### C. Query report format

a. **JSON** — machine-first, generic LLM tooling can parse it.

b. **Markdown** — human-first, rendered review.

c. **Both** — JSON canonical, markdown derived view.

**Tentative:** (c). The LLM author and human reviewer are both consumers; one canonical machine-readable form with a `karac query queries --format=md` view satisfies both.

### D. Where queries are emitted from

a. **One pass at the end** — a dedicated `query_collector` phase walks the typed AST and applies a catalogue of "could I optimize this?" probes.

b. **Per-phase emission** — each phase (typechecker, ownership, codegen) emits its own queries via the phase result struct.

c. **Hybrid** — phases emit raw "I hedged here" markers; a final pass cross-references them, deduplicates, and produces the report.

**Tentative:** (b) for v1 (each phase already has a result struct; adding `queries` is local), (c) post-v1 if cross-phase dedup becomes a real problem.

### E. Stability promise

a. **No promise** — query IDs change between compiler versions; resolution annotations remain valid (they target attributes), but the report is regenerable not committed.

b. **Stable across patch versions, mutable across minors** — same SemVer band as the language itself.

c. **Stable forever for shipped queries** — once a query ID is published, it never changes; new compiler versions add IDs but never rename or remove.

**Tentative:** (b). The report is a *diagnostic* layer per design.md §142; that section already classifies inferred effects / would-be modes as unstable across versions. Treating the queries report the same way is consistent. Resolution annotations are language items and follow language SemVer separately.

### F. Initial query catalogue (P1 rollout order)

The catalogue is open-ended; the question is which queries ship first. Tentative ordering by ratio of (optimization win × spec-context-resolvability) ÷ (implementation cost):

1. **RC vs own at use site.** Compiler already hedges; "would-be mode" already surfaced. Resolution: existing `#[no_rc]` + a new `#[prefer_rc]`.
2. **Generic specialization.** "I monomorphized 14 times for {T = i64, i32, ...}; should I specialize the body for any of these?" Resolution: GHC-style `#[specialize(T = i64)]`.
3. **Inlining.** "Function `f` is called from 3 hot-looking sites; inline?" Resolution: `#[inline]` / `#[inline(never)]`.
4. **Branch hints.** "This `match` arm appears unlikely; confirm?" Resolution: `#[likely]` / `#[unlikely]` or `core::hint::likely(...)` analog.
5. **Effect-set narrowing.** "Inferred effect set is `{reads(file), allocates}` but I could narrow to `{reads(file)}` if the allocation is gated by an unreachable branch — confirm reachable?" Resolution: existing effect declaration on the function.
6. **Layout choice.** Where layout block ambiguity exists (SoA vs AoS, field grouping), surface the alternatives. Resolution: existing layout-block syntax.
7. **Auto-concurrency fork threshold.** "I would fork this group at N=64 estimated cost; raise/lower?" Resolution: `#[fork_at(...)]` (new).

Each entry needs a P1 line in `implementation_checklist.md` (per memory: P1 entries also need a checklist entry).

---

## Problem 5 — P0 architectural floor

The decisions that *must* land in v1 because deferring them would force breaking changes for any tool that stores LLM-resolved answers.

1. **Stable item identity (path-based DefId)** — load-bearing. Without this, every later query addition is a breaking change for the LLM's resolved-answer store. Concretely: extend the AST/HIR with a `def_path: Vec<PathSegment>` carried alongside `Span`, computed at resolve time, stable under rename of unrelated items. Hybrid DefId+structural-hash addressing follows from this. **P0 even if zero queries ship in v1** — the identity primitive is what makes future query channels non-breaking.
2. **Attribute extension surface in AST/parser** — already mostly there (`src/ast.rs:641`); confirm unknown attributes propagate through resolver / typechecker without information loss so future query-resolving attributes don't need a parser change.
3. **Per-phase `queries: Vec<CompilerQuery>` field** — even if the catalogue is empty in v1, the field shape and the `CompilerQuery` struct in `src/lib.rs` (or a new `src/queries.rs`) must be defined so phases can populate it incrementally without API breakage.
4. **`karac query queries` CLI surface** — extend the existing `QueryKind` enum (`src/cli.rs:128`) with a `Queries` variant. JSON output, no-op-but-present in v1 if needed.
5. **Stability classification in design.md** — extend §113–146 to classify the queries report as a reported-behavior layer (same band as inferred effects / would-be modes), with explicit "unstable across minor versions" framing. Locks the SemVer story before any external tooling depends on it.

These five together are the **architectural commit** that makes V1 query-channel-ready. None of them require a single concrete query to ship.

---

## Problem 6 — P1 incremental queries

The P1 rollout is the catalogue from Problem 4.F. Each entry is additive on top of the P0 floor and breaks nothing if deferred to a later v1.x. Sequencing:

- **P1.1 — RC fallback query.** Reuses existing ownership-pass diagnostic infrastructure; smallest implementation surface; demonstrates the loop end-to-end. Resolution attribute pair (`#[no_rc]` exists; add `#[prefer_rc]`).
- **P1.2 — Specialization query.** Builds on monomorphization counting (already reportable via `karac query monomorphization`). Resolution attribute is new.
- **P1.3 — Inlining + branch hints.** Requires codegen-side hooks (Phase 7+). Naturally lands once codegen stabilizes.
- **P1.4 — Effect-narrowing, layout, auto-concurrency.** Each can be added independently in v1.x.

P1 entries each need a corresponding line in `implementation_checklist.md` per the standing rule.

---

## Problem 7 — out of V1

Items deliberately deferred. Listed so the v1 design doesn't accidentally precommit.

- **PGO loop (instrumented or sample-based).** Separate build flow, large surface, distinct from the query channel (PGO answers distribution-shaped questions; queries answer intent-shaped). Architectural prerequisite is debug info quality + symbol-stable identity, both of which P0 helps with but neither blocks P0. Likely P2.
- **MLGO-style trained policy artifacts.** A trained model is the *answer*, not the *question* — different shape from the query channel. Possible v2 if real-world data shows queries alone underperform.
- **Schedule-language layer.** Halide-style decoupled schedules for tight numeric loops. Not on the v1 roadmap; query channel is the smaller commitment.
- **Verifier-backed query resolution.** Alive2-class equivalence checks would let "yes this branch is dead" answers be checked rather than trusted. Interesting but separable; trust-the-author is the v1 baseline.

---

## Open questions

- ❌ **DefId scheme.** Path-based vs structural-hash vs hybrid (Problem 4.A). Tentative hybrid; needs to be settled before P0 lands because identity choice ripples through every subsequent query.
- ❌ **Resolution surface.** Source attributes vs sidecar vs both (Problem 4.B). Tentative source-first; sidecar only as fallback for sub-item decisions.
- ❌ **Where queries live in the CLI.** Extend `karac query` (existing) vs new `karac explain` (long-promised, doesn't exist) vs both. Probably both: `karac query queries --format=json` for tooling, `karac explain` as the human-friendly umbrella that includes queries among other reports.
- ❌ **Initial query catalogue scope for v1.** Five queries from Problem 4.F? Just RC fallback as a demonstrator? Zero (P0-only commit, defer all P1)? Aggressive vs conservative; depends on Phase 7 / 8 ordering.
- ❌ **Resolution-attribute grammar.** Each query type needs a specific attribute (e.g. `#[specialize_on(T = i64)]`). Some attributes need expression-level args; the parser supports this today but the namespace needs design (Rust uses `path_attribute = path '(' meta_list ')'`; Kāra has the same shape, needs query-attribute conventions).
- ❌ **Multi-resolution conflicts.** What happens if two annotations on the same item resolve a query in conflicting ways (e.g. `#[inline]` + `#[cold]`)? Compiler error vs compiler resolves vs warning. Probably error, matching `#[compiler_builtin]` precedent.
- ❌ **Aging.** A query resolved by an annotation today: when the body changes such that the query no longer applies, what happens to the orphaned annotation? Warn? Auto-strip? Leave it? Standard `#[allow(dead_code)]`-style handling probably.
- ❌ **Verification of author claims.** If the author writes `#[likely]` and it's wrong, the codegen has done the wrong layout — but that's no worse than today. PGO would catch this; without PGO, do we add a runtime check (debug builds only?) that compares author annotation to actual frequency? Out of scope for v1, but worth a position.
- ❌ **Cross-phase deduplication.** If typechecker says "should I specialize?" and codegen says "should I inline?" on the same function, are they two queries or one? Probably two — different decisions, different attributes. Confirm.
- ❌ **External tooling story.** Does a future `karac.toml` workspace flag let projects opt into "fail build if open queries above severity N"? P0+ feature or post-v1?
- ⊘ **Whether to support PGO at all in v1.** Resolved 2026-05-06: no — out of scope (Problem 7), revisit post-v1 with empirical signal.
- ⊘ **Whether the channel replaces JIT-style adaptation.** Resolved 2026-05-06: no — distribution data and intent data are different, complementary signals (Problem 1).

---

## Cross-references

- **design.md §113–146** (Specification Layers) — existing classification of reported-but-unstable behavior; the queries report joins this layer.
- **design.md §142** — "would-be parameter mode" precedent for surfacing a hedged compiler decision diagnostically.
- **design.md §684** (`#[track_caller]`), **§207–209** (`#[kara_name]`), **§1227–1241** (`#[thread_local]`) — existing compiler-meaningful attributes the query-resolution attributes will live alongside.
- **CR-202 slice 2** (recent commits, `#[compiler_builtin]`) — the implementation template: parse → resolver gate → phase registration → phase dispatch.
- **`src/ast.rs:641`** — Attribute AST node (already present).
- **`src/cli.rs:73`** — `Query { kind: QueryKind, ... }` subcommand; `Queries` variant slots into `QueryKind` (line 128).
- **`src/ownership.rs:360`** — RC fallback decision site, first P1.1 query target.
- **`src/resolver.rs`**, **`src/use_classifier.rs`** — current `SpanKey` identity; needs DefId augmentation per P0 item 1.
- **brainstorming/archive/v62** — interpreter perf / lazy LLJIT lock; not directly related but reinforces the "interactive is first-class" framing the queries channel is part of.

---

## Resolution path

This doc resolves into:
- **design.md** — new section "Compiler Queries" covering the channel's spec layer (P0 architectural commit), under § Specification Layers (extending §113–146). Stability classification.
- **design.md** — P1 query-resolving attributes added to the attribute table.
- **implementation_checklist.md** — P0 entry for stable DefId + per-phase queries field + `QueryKind::Queries` CLI; one P1 entry per query type from Problem 4.F per the standing P1-needs-checklist-entry rule.
- **roadmap.md** — phase placement (likely Phase 8 stdlib floor for the P0 commit; P1 queries staged across Phase 7+ codegen and post-1.0 minors).

Then this brainstorm doc is archived.
