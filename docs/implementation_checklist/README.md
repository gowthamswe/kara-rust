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
  - [x] **6. Tuple Display fn** — `(` + recursive per-field calls + `)`. Typed entry `emit_tuple_display_fn(elems)`; per-field recursion goes through the new `emit_display_fn_for_type_expr` dispatcher so nested compound shapes (`Vec[(i64, String)]`, `Map[String, Vec[i64]]`) compose.
  - [x] **7. `compile_print` integration** — `compile_print` recognizes `ExprKind::Identifier` args whose type is Vec or Map (via `vec_elem_types`+`var_elem_type_exprs` / `map_key_type_exprs`+`var_elem_type_exprs` side-tables) and dispatches to `emit_vec_display_fn_te` / `emit_map_display_fn`. Non-collection cases keep the existing `is_struct_value` / `is_pointer_value` / numeric fallbacks. Refactor introduced `emit_display_fn_for_type_expr` as the canonical TypeExpr-aware dispatcher; the by-name `emit_display_fn_for_type` Vec_/Map_/tuple_ arms now panic-redirect to the TypeExpr API; dead `resolve_ty_for_display_name` removed.
  - [x] **8. Test coverage** — 10 codegen E2E tests in `tests/codegen.rs` (search `test_e2e_display_`): `vec_i64`, `vec_empty`, `vec_string`, `vec_nested` (`Vec[Vec[i64]]`), `map_string_i64_singleton`, `map_i64_i64_singleton`, `map_empty`, `vec_tuple_i64_i64` (Vec of tuple via `Map.entries()`), `vec_tuple_i64_string` (heap-bearing tuple field), `map_with_vec_value_singleton` (`Map[String, Vec[i64]]`). Multi-entry Map output is order-dependent (runtime walks bucket array); all Map tests use single entries. `Set[T]` Display test (`test_e2e_display_set_i64`) deferred until subtask 5 / Set codegen lands.

- [ ] **`Set[T]` LLVM codegen.** _(canonical: [phase-8-stdlib-floor.md](phase-8-stdlib-floor.md), search `Set[T] LLVM codegen`)_
  - [ ] **1. Codegen state** — `set_elem_types` side-table + `extract_set_elem_type` helper
  - [ ] **2. `Set.new()` path-call dispatch** — `karac_map_new(elem_size, 0, ...)` (val_size=0)
  - [ ] **3. `compile_set_method`** — `len` / `is_empty` / `insert` / `contains` / `remove` / `clear`
  - [ ] **4. `for x in s`** — `compile_for_set_var` mirrors `compile_for_map_var`
  - [ ] **5. `union` / `intersection` / `difference`** — via iteration, requires `T: Clone`
  - [ ] **6. E2E codegen tests** — 12 cases mirroring the Map suite, plus compound-key coverage (`Set[(i32, i32)]`, `Set[(String, (i32, i32))]`) carried over from the completed Hash codegen work — Hash machinery is shared with Map, so this is the remaining gap
  - [ ] **7. ASAN test** — `Set.new + insert + scope-exit free`
  - [ ] **8. `Set[v1, v2, ...]` prefix-literal element type inference** — typechecker only; closes `phase-4-interpreter.md` line 12

- [ ] **`Map.entry(k)` + `Entry[K, V]` enum — in-place insert-or-modify.** _(canonical: [phase-8-stdlib-floor.md](phase-8-stdlib-floor.md), search `Map.entry(k)`)_

  Queued — touches parser/AST/typechecker/interp/codegen/runtime. Round-scoped subtasks added when round opens.

### List 2 — Parallel-safe (pick up without coordination)

These bullets touch files / functions that don't conflict with the active List-1 work. Any agent can pick them up in parallel; merge-conflict risk is minimal.

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
