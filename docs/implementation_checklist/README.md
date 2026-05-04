# Implementation Checklist

Items to validate, benchmark, or revisit during specific implementation phases. These are not design decisions — they are implementation concerns that should not be forgotten.

Sourced from open gaps identified during design review that don't require design decisions but do require action during implementation.

---

## Work in Progress (updated 2026-05-04)

**Theme: HashMap/HashSet completion.** Finish `Map[K, V]` and `Set[T]` codegen so real test programs and benchmarks run on compiled binaries. Five canonical bullets — four in [Phase 8](phase-8-stdlib-floor.md), one in [Phase 7.2](phase-7-codegen.md#phase-72-compiled-stdlib-types--layout-codegen). Active rounds: gap-closure and Set codegen first; the rest queue behind.

*Scoping context (audit 2026-05-04): `compile_map_method` (`src/codegen.rs:4667`) handles 6 of 11 typechecker-blessed methods (`len`/`is_empty`/`insert`/`get`/`remove`/`contains_key`) and falls through to a silent-`0` catch-all at line 4945 for the other 5 (`get_or` / `keys` / `values` / `entries` / `merge`). `compile_index` (line 5009) handles only Array/Vec/Slice — `m[k]` is wrong on compiled binaries today. No `karac_set_*` runtime exists; `Set[T]` is interpreter-only. Existing `runtime/src/map.rs` already supports `val_size = 0` correctly (line 71's `(key_size + val_size).max(1)`), so Set lowers to `Map[T, ()]` with no new C code. 12 Map E2E codegen tests; 0 Set E2E codegen tests.*

- [~] **Map codegen gap closure.** _(canonical: [phase-8-stdlib-floor.md](phase-8-stdlib-floor.md), search `Map codegen gap closure`)_
  - [x] **1. Catch-all hardening** — `_ => Err(...)` at `src/codegen.rs:4945` (commit `4a3bc3e`)
  - [ ] **2. `m[k]` index op (read)** — `compile_index` Map dispatch + `panics` on missing key
  - [ ] **3. `m[k] = v` index op (write)** — `compile_index_store` Map dispatch
  - [ ] **4. `Map.clear()`** — `karac_map_clear` runtime fn + interp + codegen
  - [ ] **5. `keys()` / `values()` / `entries()` codegen** — materialize Vec via `karac_map_iter_*`
  - [ ] **6. `Display` for collections** — `Vec` / `Map` / `Set` / `VecDeque` / `SortedSet` / `TreeMap`; supersedes the stub at `phase-8-stdlib-floor.md:207` (delete that line)
  - [ ] **7. `Map[k: v, ...]` prefix-literal K/V inference** — typechecker only; closes `phase-4-interpreter.md` line 13

- [~] **`Set[T]` LLVM codegen.** _(canonical: [phase-8-stdlib-floor.md](phase-8-stdlib-floor.md), search `Set[T] LLVM codegen`)_
  - [ ] **1. Codegen state** — `set_elem_types` side-table + `extract_set_elem_type` helper
  - [ ] **2. `Set.new()` path-call dispatch** — `karac_map_new(elem_size, 0, ...)` (val_size=0)
  - [ ] **3. `compile_set_method`** — `len` / `is_empty` / `insert` / `contains` / `remove` / `clear`
  - [ ] **4. `for x in s`** — `compile_for_set_var` mirrors `compile_for_map_var`
  - [ ] **5. `union` / `intersection` / `difference`** — via iteration, requires `T: Clone`
  - [ ] **6. E2E codegen tests** — 12 cases mirroring the Map suite
  - [ ] **7. ASAN test** — `Set.new + insert + scope-exit free`
  - [ ] **8. `Set[v1, v2, ...]` prefix-literal element type inference** — typechecker only; closes `phase-4-interpreter.md` line 12

- [ ] **`Map.entry(k)` + `Entry[K, V]` enum — in-place insert-or-modify.** _(canonical: [phase-8-stdlib-floor.md](phase-8-stdlib-floor.md), search `Map.entry(k)`)_

  Queued — start after gap-closure and Set codegen land. Touches parser/AST verification, prelude registration of `Entry[K, V]`, three Entry methods (`or_insert` / `or_insert_with` / `and_modify`), interpreter `Value::Entry`, new `karac_map_entry` runtime fn, and codegen lowering. Round-scoped subtasks will be added when the round opens.

- [ ] **Effect-checker wiring for `Map[K, V]` and `Set[T]` methods.** _(canonical: [phase-8-stdlib-floor.md](phase-8-stdlib-floor.md), search `Effect-checker wiring`)_

  Queued — independent of the other rounds; can run in parallel. Adds `infer_map_method_effects` + `infer_set_method_effects` paralleling `infer_vec_method_effects`. Effects: `allocates(Heap)` for growth methods, `panics` for index op, none for pure reads. Round-scoped subtasks will be added when the round opens.

- [ ] **Hash codegen for compound key types.** _(canonical: [phase-7-codegen.md](phase-7-codegen.md#phase-72-compiled-stdlib-types--layout-codegen), search `Hash codegen for compound key types`)_

  Queued — tuples first, then enums, then user `#[derive(Hash)]`. Extends `emit_hash_fn_for_type` at `src/codegen.rs:4282` past primitives + `String`. Round-scoped subtasks will be added when the round opens.

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
