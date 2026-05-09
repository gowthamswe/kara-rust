# WIP — List 1 (serial work, this session)

This file holds **delegate-ready** items only — slices whose plans are
drafted to autonomous-friendly bar in their phase tracker, with all
prerequisites cleared. Items in active triage live in
[`wip-staging.md`](wip-staging.md); long-term themed parking lives in
[`wip-list2.md`](wip-list2.md).

## Working patterns

See [`wip-patterns.md`](wip-patterns.md) — file-family pipeline,
pre-slice gate, per-commit gates (`cargo test`,
`cargo test --features llvm`, `cargo clippy --all --tests -- -D warnings`,
`cargo fmt --all -- --check`), delegation cycle, friction-handling
defaults (inline-fix), hard-stop protocol, and
mirror-to-phase-tracker discipline.

---

## Active queue

- [ ] **`Range` / `RangeInclusive` implements `Iterator`.** Sibling extension to the closed adaptor-surface entry (commit `6d23f94` 2026-05-04). Lifts the gap between two already-shipped surfaces — the for-loop desugar sees Range as iterable; the method-dispatch surface doesn't. Implements option 1 — Range and RangeInclusive *are* Iterators (matches Rust precedent), so `(start..end).step_by(n)` / `(start..=end).map(f).collect()` / `(0..n).take(k)` work directly without a redundant `.iter()` layer; the for-loop desugar's existing range special-case stays in place. Four sub-steps cover typechecker + interpreter only: (a) adaptor dispatch on Range receivers in `infer_method_call` (`src/typechecker.rs:9789-9802`), (b) `iterator_item_type_for` extension so `.iter()` returns `Iterator[Item=T]` (`src/typechecker.rs:509-521`), (c) Range eval produces `Value::Iterator { Eager, … }` instead of `Value::array_of` (`src/interpreter.rs:2895-2912`), (d) `iter`/`into_iter` pass-through arm on Iterator receivers (`src/interpreter.rs:5746-5776`). Bounded ranges only; unbounded forms (`RangeFrom` / `RangeTo` / `RangeToInclusive` / `RangeFull`) keep the existing `record_runtime_error` paths. **Codegen out of scope** — matches the existing typechecker+interpreter-only iterator surface (verified 2026-05-09: codegen fails today even on `xs.iter().collect()` for `Vec[i64]`). Test surface: 8 tests (6 interpreter — `step_by` / inclusive / redundant `.iter()` / chained adaptors / for-loop regression / `take`; 2 typechecker — adaptor accepts / unknown-method rejects). ~25 LoC of compiler change + ~160 LoC of test scaffolding. **No prerequisites** — Iterator infrastructure shipped 2026-05-04 (`6d23f94`); Range pseudo-struct registration shipped well before that. Surfaced 2026-05-09 by the LeetCode 3629 kata; closes the manual stride-loop shape (`let mut j = i; while j <= cap { ... j = j + i; }`) in `bfs_sieve.kara`'s sieve. Plan source: [`phase-8-stdlib-floor.md`](phase-8-stdlib-floor.md) § "`Range` / `RangeInclusive` implements `Iterator`" → "Slice plan (drafted 2026-05-09) — `Range` / `RangeInclusive` Iterator" (commit `d723623`). Promoted from staging 2026-05-09.

---

## Timing log

| # | Slice | Started | Landed | Duration | Commit |
|---|---|---|---|---|---|
