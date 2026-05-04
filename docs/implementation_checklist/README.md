# Implementation Checklist

Items to validate, benchmark, or revisit during specific implementation phases. These are not design decisions — they are implementation concerns that should not be forgotten.

Sourced from open gaps identified during design review that don't require design decisions but do require action during implementation.

---

## Work in Progress (updated 2026-05-04)

**Theme: HashMap/HashSet completion.** Finish `Map[K, V]` and `Set[T]` codegen so real test programs and benchmarks run on compiled binaries. Work splits into a serial **List 1** (active session, one agent) and a parallel-safe **List 2** (any agent can pick up without coordination — file/function boundaries don't conflict with List 1).

*Scoping context (audit 2026-05-04): `compile_map_method` (`src/codegen.rs:4667`) originally handled 6 of 11 typechecker-blessed methods and fell through to a silent-`0` catch-all for the rest. `compile_index` (line 5009) handled only Array/Vec/Slice. No `karac_set_*` runtime; `Set[T]` interpreter-only. Existing `runtime/src/map.rs` supports `val_size = 0` correctly (line 71's `(key_size + val_size).max(1)`), so Set lowers to `Map[T, ()]` with no new C code. Map gap closure (subtasks 1–7) closed by 2026-05-04 across commits `4a3bc3e`, `ca94b9f`, `3f08a39`, `8806883`, `b150d8c`, `1678d0a`; subtask 6 (Display) split into its own canonical bullet because recursive Display codegen is its own scope. Map E2E codegen tests grew 12 → 27.*

### List 1 — Active (serial, this session)

- [~] **Display for collections (recursive codegen).** _(canonical: [phase-7-codegen.md](phase-7-codegen.md#phase-72-compiled-stdlib-types--layout-codegen), search `Display for collections (recursive codegen)`)_
  - [x] **1. Per-type Display function emission machinery** — `emit_display_fn_for_type` cached by type, parallel to `emit_hash_fn_for_type` (commit `8123a8e`)
  - [x] **2. Primitive Display fns** — i8…i64 / u8…u64 / f32/f64 / bool / char / String (commit `8123a8e`)
  - [x] **3. `Vec[T]` Display fn** — `[` + loop with recursive elem call + `]`
  - [x] **4. `Map[K, V]` Display fn** — `{` + iterator loop with recursive K, V calls + `}` (typed entry `emit_map_display_fn` since two type params don't recover from a flat name)
  - [ ] **5. `Set[T]` Display fn** — depends on Set codegen landing; format aligned with interpreter
  - [ ] **6. Tuple Display fn** — `(` + recursive per-field calls + `)`
  - [ ] **7. `compile_print` integration** — recognize Vec/Map/Set/Tuple types, dispatch to emitted Display fn
  - [ ] **8. Test coverage** — E2E covering primitives + nested collections + interpreter-codegen parity

- [ ] **`Set[T]` LLVM codegen.** _(canonical: [phase-8-stdlib-floor.md](phase-8-stdlib-floor.md), search `Set[T] LLVM codegen`)_
  - [ ] **1. Codegen state** — `set_elem_types` side-table + `extract_set_elem_type` helper
  - [ ] **2. `Set.new()` path-call dispatch** — `karac_map_new(elem_size, 0, ...)` (val_size=0)
  - [ ] **3. `compile_set_method`** — `len` / `is_empty` / `insert` / `contains` / `remove` / `clear`
  - [ ] **4. `for x in s`** — `compile_for_set_var` mirrors `compile_for_map_var`
  - [ ] **5. `union` / `intersection` / `difference`** — via iteration, requires `T: Clone`
  - [ ] **6. E2E codegen tests** — 12 cases mirroring the Map suite
  - [ ] **7. ASAN test** — `Set.new + insert + scope-exit free`
  - [ ] **8. `Set[v1, v2, ...]` prefix-literal element type inference** — typechecker only; closes `phase-4-interpreter.md` line 12

- [ ] **`Map.entry(k)` + `Entry[K, V]` enum — in-place insert-or-modify.** _(canonical: [phase-8-stdlib-floor.md](phase-8-stdlib-floor.md), search `Map.entry(k)`)_

  Queued — touches parser/AST/typechecker/interp/codegen/runtime. Round-scoped subtasks added when round opens.

### List 2 — Parallel-safe (pick up without coordination)

These bullets touch files / functions that don't conflict with the active List-1 work. Any agent can pick them up in parallel; merge-conflict risk is minimal.

- [x] **Hash codegen for compound key types.** _(canonical: [phase-7-codegen.md](phase-7-codegen.md#phase-72-compiled-stdlib-types--layout-codegen), search `Hash codegen for compound key types`)_ — landed 2026-05-04. Tuple, unit-enum, and `#[derive(Hash, Eq)]` struct keys work end-to-end on the codegen path; cache is the module-level function table, with defensive checks in `emit_hash_fn_for_tuple` / `emit_eq_fn_for_tuple`. 9 E2E tests in `tests/codegen.rs` + 4 typechecker tests in `tests/typechecker.rs`. Set-side coverage (`Set[(i32, i32)]`, `Set[(String, (i32, i32))]`) deferred to when List-1 Set codegen lands — the underlying Hash machinery is shared.

- [ ] **Lexer: reserve `expr_<NNNN>` fragment-specifier identifier namespace.** _(canonical: [phase-8-stdlib-floor.md](phase-8-stdlib-floor.md), search `Lexer: reserve` `expr_<NNNN>`)_

  **Files:** `src/lexer.rs` + `tests/lexer.rs`. No `src/codegen.rs` touch. Zero conflict with List-1 Display work or with the in-flight Hash codegen agent.
  **Estimate:** 1 commit.
  **Scope:** 7 slices in canonical (lexer regex check, raw-identifier exemption via `was_raw_escaped`, narrow `expr_` scope at v1, year range `2020..=2099`, diagnostic shape with both fix-its, no connection to literal year value, positive + negative test coverage).
  **Repo conventions:** no Co-Authored-By trailer; prefer `--amend` for tight follow-ups.

- [ ] **Iterator trait — full adaptor surface.** _(canonical: [phase-8-stdlib-floor.md](phase-8-stdlib-floor.md), search `Iterator trait — full adaptor surface`)_

  **Files:** stdlib/prelude registration (`src/prelude.rs`), typechecker method registration (`src/typechecker.rs`), interpreter dispatch (`src/interpreter.rs`), tests (`tests/typechecker.rs` + `tests/interpreter.rs`). Codegen is a follow-up — most adaptors lower to existing `for`-loop / collection ops at the interpreter layer first, keeping the parallel agent off `src/codegen.rs` entirely.
  **Estimate:** ~5–10 commits, one or a small group of adaptors per commit.
  **Scope:** 16 adaptors named in canonical (`chain`, `zip`, `enumerate`, `take(n)`, `skip(n)`, `take_while(pred)`, `skip_while(pred)`, `flat_map(f)`, `peekable`, `chunk_by(key_fn)`, `step_by(n)`, `cycle`, `inspect(f)`, `scan(state, f)`, `windows(n)`, `chunks(n)`). Each is 5–20 lines once `Iterator` is in place. `chunk_by` and `windows` may declare `allocates(Heap)`; the rest are effect-free.
  **Conflict-avoidance:** stay out of `src/codegen.rs` and `tests/codegen.rs`. If you encounter a typechecker or interpreter touchpoint that overlaps an active List-1 round, defer that adaptor to a later commit and pick up the next.
  **Repo conventions:** no Co-Authored-By trailer; prefer `--amend` for tight follow-ups; mark this checkbox `[x]` only when ALL 16 adaptors are landed (or update inline with a `(N/16 done)` annotation).

---

## Contents

- [Phase 1: Lexer](phase-1-lexer.md)
- [Phase 2: Parser & AST](phase-2-parser-ast.md)
- [Phase 3: Effect Checker](phase-3-effect-checker.md)
- [Phase 4: Tree-Walk Interpreter](phase-4-interpreter.md)
- [Phase 5: Structured Diagnostics and Language Refinements](phase-5-diagnostics.md)
- [Phase 6: Auto-Concurrency Runtime](phase-6-runtime.md)
- [Phase 7: LLVM Code Generation](phase-7-codegen.md)
  - [Phase 7.2: Compiled Stdlib Types + Layout Codegen](phase-7-codegen.md#phase-72-compiled-stdlib-types--layout-codegen)
- [Phase 8: Standard Library — Floor](phase-8-stdlib-floor.md)
- [Phase 9: Gradual Verification Enforcement](phase-9-verification.md)
- [Phase 10: Additional Targets](phase-10-targets.md)
- [Phase 11: Standard Library — Long-Tail](phase-11-stdlib-longtail.md)
