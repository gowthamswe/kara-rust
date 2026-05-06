# WIP — List 1 (serial work, this session)

When picking up work, also mirror the bullet (with the box checked off as
work progresses) into the relevant `phase-N-*.md` tracker so the durable
record lives alongside every other completed phase entry.

---

## Theme: small/contained checklist items (2026-05-05)

Picking up a sequence of small, contained checklist items so each can
ship as its own commit. Original tracker entries get checked off as
each one closes.


- [ ] **Bench script: wire `build_kara` step into `bench/bench.sh`.** Add a `build_kara()` function next to the existing `build_rust()` in `kata-katas/leetcode/1-100/1-two-sum/bench/bench.sh`, invoke `karac build` to produce native binaries, add `kara brute_force (codegen)` and `kara hash_map (codegen)` rows to the hyperfine command. Source: v62 brainstorm decision #8 (execution-only, no design fork).

- [ ] **README §Benchmarks rewrite.** Once the bench script change above lands, rewrite `kata-katas/leetcode/1-100/1-two-sum/README.md` §Benchmarks to lead with codegen numbers; demote interpreter row to "what the compile pipeline costs today" context. Source: v62 brainstorm decision #9.

- [ ] **N=5000 bump in all five bench files.** Change the `N=200` constant to `N=5000` in all five bench source files in `kata-katas/leetcode/1-100/1-two-sum/bench/` (`hash_map.{rs,py,kara}`, `brute_force.{rs,py,kara}` — adjust list to actual files). Single commit. Real N=5000 numbers already captured in v62 doc: kara_brute_force 97.5 ± 0.2 ms (rust 32.8 ± 0.5 ms — 3.0× gap); kara_hash_map 2.8 ± 0.1 ms (rust 2.1 ± 0.2 ms — 1.4× gap). Source: v62 brainstorm decision #10.
