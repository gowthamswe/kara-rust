# WIP — List 2 (parallel-safe work)

Migrated home of the parallel-safe List-2 entries from the main `README.md` WIP
section, kept here so List-1 work doesn't share a file with List-2 work and
either side can commit independently. Currently contains one in-flight
initiative (Iterator trait full adaptor surface, below). New parallel-safe
items get appended as their own sections; once an initiative completes, its
section is removed and the canonical phase entry is marked `[x]`. When this
file has no remaining sections, delete it.

## Iterator trait — full adaptor surface

Multi-commit work for the canonical phase-8 entry _(`phase-8-stdlib-floor.md`,
search `Iterator trait — full adaptor surface`)_. Once all subtasks land, the
phase-8 entry gets flipped to `[x]` (subtask 15) and this section is deleted.

**Why this file exists.** The canonical `README.md` L2 entry estimates "5–10
commits" and assumes the baseline adaptors (`filter`, `map`, `collect`, `fold`,
`any`, `all`) are already in place. They aren't — the codebase has only
trait-shell registration (`src/typechecker.rs:2074-2093`), `Item` assoc-type
metadata for collections, and for-loop integration that bypasses `iter()`
entirely (walks raw `Value::Vec` / `Value::Map` / etc. in
`src/interpreter.rs:1674-1730`). No `Value::Iterator` exists; no adaptor methods
dispatch through `eval_method_call`. The realistic count is 13–15 commits, with
the first three being foundation work before any adaptor lands.

**Scope.** All 16 long-tail adaptors named in the L2 entry plus the 6 baseline
adaptors that the design.md § Iterator Adaptors section spells out (the
canonical L2 entry assumes these exist). Codegen is deferred — interpreter +
typechecker only, per the L2 conflict-avoidance rule (stay out of
`src/codegen.rs` and `tests/codegen.rs`).

**Repo conventions.** No Co-Authored-By trailer; prefer `--amend` for tight
follow-ups; commit per subtask (or peer-grouped subtask) so each diff is
reviewable in isolation.

---

## Foundation (commits before any adaptor)

- [x] **1. `Value::Iterator` + `iter()` / `into_iter()` plumbing on collections.**
  Landed alongside this tracker file's introduction. `Value::Iterator { items,
  cursor }` snapshots the source elements eagerly at construction (Map yields
  `(K, V)` tuples; SortedSet flattens to ascending order; Vec/Array/Slice/Set
  pass through). The typechecker registers `iter()` / `into_iter()` as a
  cross-collection dispatch arm in `infer_method_call` (one place, before the
  per-type handlers) using a free `iterator_item_type_for` helper that
  recognizes `Vec`, `Set`, `SortedSet`, `VecDeque`, `Map`, `Array`, `Slice`,
  and `ref` / `mut ref` borrows of those. Both methods return
  `Type::Named { "Iterator", [Item] }`; the borrow-vs-consume distinction is
  immaterial at this layer. New `infer_iterator_method` handler dispatches on
  the Iterator receiver and registers `next() -> Option[T]` (the adaptor
  surface lives in subtasks 3+). The interpreter's `eval_method_call` adds
  `iter` / `into_iter` and `next` arms; `next` writes the advanced cursor
  back through the binding so successive calls observe the new state. 8
  typechecker tests + 6 interpreter tests cover Vec/Array/Map/Set/SortedSet,
  the empty-source `None` case, Map's `(K, V)` tuple shape, and arity errors
  on `iter(arg)` / `next(arg)`. **Note:** `Vec.new()` is not wired in the
  interpreter today; tests on empty Vecs use the `Vec[]` prefix-literal form.
  Subtask 2 (for-loop integration) is the next foundation step.

- [x] **2. `for` loop consumes `Value::Iterator`.** New arm in the for-loop
  driver (`src/interpreter.rs`) drains the iterator via `items.into_iter().
  skip(cursor)` — observably identical to a `next()`-call loop today because
  subtask 1's iterator is eager (no closure-bearing adaptors yet). When
  closures land in subtask 3+, this arm migrates to a true `next()` pull so
  adaptor closures fire per step. Raw-collection arms (Array/Tuple/Set/
  SortedSet/Map) preserved for backwards compat. Typechecker side: `Iterator`
  registered in `env.structs` + `impl_assoc_types` alongside Vec/Array/Set/
  SortedSet so `element_type_of` resolves the bound element via the same
  generic-substitution path as collections; `for x in v.iter()` and
  `for (k, v) in m.iter()` typecheck and bind correctly. 3 typechecker tests
  + 5 interpreter tests cover Vec/Map/Set, cursor resumption (manually
  advance with next() then drop into for-loop), and break/continue inside
  iterator-driven loops.

## Baseline adaptors (the 6 the canonical L2 entry assumes already exist)

- [x] **3. `map(f)` + `filter(pred)`.** New `IteratorStep` enum (`Map(Value)`
  + `Filter(Value)`) extends `Value::Iterator` with a lazy adaptor chain.
  `eval_method_call` arms for `map` / `filter` evaluate the closure once at
  construction, push it as the matching step, and return the modified
  iterator. New `iterator_step` helper drains items one at a time, applying
  steps in order — `Filter` may reject; the loop retries until an item passes
  every step or the source exhausts. New `invoke_function_value` helper
  invokes a `Value::Function` against pre-evaluated args (no CICO write-back,
  no default-eval, no type-substitution stack). The for-loop arm and `next()`
  arm both route through `iterator_step` so adaptor closures fire per pull.
  Typechecker side: `infer_iterator_method` adds `map(f: Fn(T) -> U) ->
  Iterator[U]` (U solved from the closure's actual return type via
  check_expr's closure-pushdown — `Type::Function { return: TypeParam(U) }`
  pushed in, body inferred freely, return type read back) and `filter(pred:
  Fn(T) -> bool) -> Iterator[T]` (return type known so plain check_expr
  suffices). 6 typechecker tests + 7 interpreter tests cover map's element
  type-change, filter's preservation, stacked maps threading types
  (i64 → bool → String), explicit closure-param annotations, non-bool
  predicate rejection, arity errors, lazy step-by-step pulls, the
  empty-after-filter case, and `(K, V)` destructuring on `Map.iter().map(...)`.

- [x] **4. `collect()` + `fold(init, f)` + `count()`.** Three terminals,
  all routed through `iterator_step` so adaptor chains fire during the
  drain. `count()` walks the iterator and returns the element count as
  `i64`. `collect()` v1 is `Vec`-only — drains into `Value::Array` with
  typechecker return `Vec[T]`; `FromIterator`-driven dispatch into other
  collections is a follow-up CR. `fold(init, f)` infers the accumulator
  type `A` from `init`, then `check_expr`s the closure against
  `Fn(A, T) -> A` (both params and return concrete, so plain check_expr
  suffices). 10 typechecker tests + 10 interpreter tests cover plain
  walks, empty-source cases, composition with filter / map adaptors,
  String accumulator, type-mismatch on closure return, and arity errors.

- [x] **5. `any(pred)` + `all(pred)`.** Short-circuit terminal predicates,
  routed through `iterator_step` so adaptor closures fire only for the
  prefix the predicate has to inspect. `any` returns true on the first
  `true`; `all` returns false on the first `false`. Both share the same
  `Fn(T) -> bool` signature so closure-pushdown via `check_expr`
  suffices (no fresh type variable). Empty-source semantics are the
  identity element of each: `any` → `false`, `all` → `true`. 7
  typechecker tests + 9 interpreter tests cover positive / negative /
  empty-source cases, short-circuit (predicate println side effects
  observed for the prefix only), composition with map for the
  predicate's element type, and arity / non-bool errors.

## Long-tail adaptors (the 16 named in the L2 entry)

- [x] **6. `enumerate()` + `take(n)` + `skip(n)`.** Three positional /
  count adaptors. Required extending `IteratorStep` with stateful
  variants (`Enumerate(idx)` / `Take(remaining)` / `Skip(remaining)`)
  and refactoring `iterator_step` to mutate the cloned step chain
  in-place and write it back into the iterator's `steps` field before
  return — earlier closure-only steps were stateless. `take`
  exhaustion drains the source cursor so subsequent pulls return
  `None` without re-evaluating downstream adaptors. Negative `n` clamps
  to zero at the runtime layer (typechecker accepts any i64). 8
  typechecker tests + 11 interpreter tests cover index plumbing,
  bound semantics for `take(0)` / `take(n>len)` / `skip(n>len)`,
  state persistence across separate `next()` pulls, composition
  (`skip.take` window, `filter.take` first-n-passing,
  `map.enumerate` mapped tuple), tuple type threading, and arity /
  non-int errors.

- [ ] **7. `chain(other)` + `zip(other)`.** Two-source combinators. `chain`
  exhausts self then exhausts other; `zip` pairs until the shorter source ends.
  Both take `other: impl Iterator` typed args.

- [ ] **8. `take_while(pred)` + `skip_while(pred)`.** Predicate-bounded
  adaptors. `take_while` stops on first failing element; `skip_while` skips
  while the predicate holds, then yields the rest unconditionally.

- [ ] **9. `flat_map(f)`.** Closure returns an iterator; flatten the result.
  Each `next()` advances through the inner iterator until exhausted, then pulls
  the next outer element and re-enters.

- [ ] **10. `step_by(n)` + `cycle()`.** `step_by` yields every n-th element
  (n ≥ 1). `cycle` requires the source to be cloneable — when exhausted,
  restarts from a stored copy. Document the cloning requirement in the
  diagnostic.

- [ ] **11. `inspect(f)` + `scan(state, f)`.** `inspect` runs the closure on
  each element for side effects; passes the element through. `scan` threads
  mutable state through the closure, yielding what the closure returns;
  closure returns `Option[U]` so the iterator can short-circuit.

- [ ] **12. `peekable()`.** Wraps the source so `peek()` returns the next
  element without consuming it. Implementation buffers one element. The
  resulting iterator type exposes `peek()` as an additional method beyond the
  Iterator trait.

- [ ] **13. `chunk_by(key_fn)`.** Buffering adaptor — allocates a `Vec[T]`
  per group when consecutive elements share the same key. Carries
  `allocates(Heap)` in its effect set per the canonical scope note.

- [ ] **14. `windows(n)` + `chunks(n)` on Iterator.** Both currently exist as
  inherent methods on `Vec` (`src/interpreter.rs:3491` and `:3505`); this
  subtask adds the Iterator-trait variants. Confirm whether to promote or
  duplicate during implementation — the Vec inherent methods may stay as
  fast-path shortcuts. Both buffer; both `allocates(Heap)`.

## Wrap-up

- [ ] **15. Close out.** Flip canonical `phase-8-stdlib-floor.md` line 21
  checkbox to `[x]` with a closure note pointing at the final adaptor commit.
  Delete this WIP file. Reference the test count + adaptor list in the closure
  note so future readers can audit coverage without rebuilding history.
