# Parallax-lite

First demo-shaped consumer of the Kāra auto-concurrency codegen track
(slices 1 + 2 of the Debugger Contract phase). A compact, self-contained
Kāra workload that exercises auto-par lowering on three independent
effect-annotated calls and pins the IR shape, the analyzer's grouping
decision, and the cost-summary surface against regressions.

## What it demonstrates

`workload.kara::process_request` is three top-level call statements,
each carrying a distinct `writes(R_i)` effect on a disjoint resource:

```kara
pub fn process_request() with writes(MetricsA) writes(MetricsB) writes(MetricsC) {
    record_a();
    record_b();
    record_c();
}
```

The analyzer (`src/concurrency.rs`) sees no conflict edges between the
three statements (`writes(MetricsA)` vs `writes(MetricsB)` is safe — the
conflict table only triggers on same-resource, same-category pairs), so
it groups all three into a single `parallel_group`. The codegen path
(`compile_function_body` in `src/codegen.rs`) sees a non-trivial group
(non-empty effect set), no binding-leak (no `let` introduced in the
group), and dispatches the trio through a single `karac_par_run` call
that fans out three branch fns to the runtime's worker pool.

End result: `process_request` runs three CPU-bound branches concurrently
without any explicit `par {}` block in the source — the auto-par codegen
infers the parallel structure purely from effect annotations.

## The demo-shape gap

The canonical Parallax demo (per `docs/demo_ideas.md § Demo 1`) is
*fan-out + join* — multiple parallel reads whose results are joined
into a result struct (`build_dashboard(profile, orders, notif,
recommended)`). Slice 2's `group_defines_binding_used_outside` gate
blocks that exact pattern today: bindings introduced inside a parallel
group that are read by statements *outside* the group cannot
auto-parallelize, because `karac_par_run`'s branch fns capture *into*
their env struct but do not propagate let-bindings *out*.

Lifting the gate requires the still-open
`docs/implementation_checklist/phase-7-codegen.md:182` entry **"Par
codegen: return values"**, which extends the runtime ABI with per-branch
return slots. Until that lands, Parallax-lite ships *effect-only
fan-out* — three independent calls with no joined return — as the v1
auto-par measurement workload. The canonical fan-out + join demo lights
up additively when the return-values entry lands, with no source change
to slice 6's workload required (the analyzer already grouped that
pattern correctly; only the codegen is gappy).

## How to run

The example uses the multi-file project shape (`kara.toml` + `src/*.kara`).
v1 project-mode build (`karac build` from inside `examples/parallax_lite/`)
runs the multi-file pipeline through cross-module typechecking; full
codegen across modules is a CR-24 follow-up. To exercise the auto-par
path end-to-end today, concatenate the workload into a single file and
build it with `karac build <path>.kara`:

```sh
# concat the canonical workload + a tiny main into a single .kara file
cat examples/parallax_lite/src/resources.kara                                 \
    <(grep -v '^import ' examples/parallax_lite/src/workload.kara)            \
    <(echo 'fn main() { process_request(); println("done"); }')               \
  > /tmp/parallax_lite.kara

# auto-par on (default)
karac build /tmp/parallax_lite.kara
./parallax_lite

# sequential baseline (the slice 6 codegen gate)
KARAC_AUTO_PAR=0 karac build /tmp/parallax_lite.kara
./parallax_lite
```

The `KARAC_AUTO_PAR=0` env var flips the slice 6 codegen gate in
`src/codegen.rs` (`Codegen::auto_par_disabled`), short-circuiting all
parallel-group dispatch back to plain sequential `compile_block`
without changing the source. Default is auto-par on; the user-facing
`--sequential` CLI flag is a Phase 8.5 Track 2 deliverable when the
profile system ships (slice 6 stays inside the codegen entry-point
arg budget — adding a CLI flag would push `compile_to_object_with_options`
past 6 positional args, the slice 3 deviation threshold).

## Wall-clock measurement (locally observed)

Three CPU-bound branches across at least two cores should clear ~1.5x
speedup; perfect 3x is the target with three available cores and zero
overhead. Locally measured on a typical laptop after warmup:

| Mode       | Wall-clock | CPU usage |
|------------|------------|-----------|
| Auto-par   | ~0.087s    | ~292%     |
| Sequential | ~0.244s    | ~99%      |
| **Speedup**| **~2.81x** | —         |

The 2.81x ratio sits in the upper quartile of the expected band — the
auto-par mechanism is working as designed. A regression below ~1.5x
would be signal for v1.x cost-model tuning (per-call cost heuristic,
fork threshold, loop-body parallelization rule); below ~1.3x would
suggest thread-spawn / RC-fallback / env-struct-alloc overhead is
dominating and the cost model needs a rebalance. The wall-clock test
in `tests/parallax_lite.rs` is `#[ignore]`-gated (single-run timing
is too flaky for default CI) and asserts a relaxed 1.3x threshold.

## Cumulative Cost Surface validation

`design.md § Performance Diagnostics > Cumulative Cost Surface` lists
five categories the `karac query cost-summary` JSON renderer reports
per-function and as totals: `rc_ops` (with `rc` / `arc` breakdown),
`arc_provider_wraps`, `borrow_flag_fields`, `partition_guard_sites`,
`auto_clone_insertions`. Observed counts on the Parallax-lite workload
(after `karac query cost-summary <concatenated workload>`):

| Category                | Per-function       | Total |
|-------------------------|--------------------|-------|
| `rc_ops.count`          | 0 (every fn)       | 0     |
| `rc_ops.rc`             | 0 (every fn)       | 0     |
| `rc_ops.arc`            | 0 (every fn)       | 0     |
| `arc_provider_wraps`    | 0 (every fn)       | 0     |
| `borrow_flag_fields`    | 0 (struct-level)   | 0     |
| `partition_guard_sites` | 0 (every fn)       | 0     |
| `auto_clone_insertions` | 0 (every fn)       | 0     |
| `perf_notes`            | (none)             | 0     |
| `by_function`           | (empty)            | (n/a) |

All zeros. The surface is empty because the workload does not exercise
RC types (no `shared struct`), does not call `with_provider[R](...)`
in the entry point (deviation — see below), does not declare `mut`
fields on struct types (no borrow flags), does not partition Vec/Slice
in any flow (no partition guards), and has no auto-clone candidates
(no shared values consumed across multiple use sites). The structural
*shape* of the cost-summary output (totals object with the five
categories, `by_function` list, `perf_notes` list) matches the spec
table — `tests/parallax_lite.rs::test_parallax_lite_query_cost_summary_structural`
pins the shape against future regressions.

For v1.x cost-model tuning ground-truth this row of zeros is the
baseline: a workload that is purely auto-par-driven incurs no
cost-surface debt. Workloads that combine auto-par with RC/Arc
provider injection (the future canonical demo, once "Par codegen:
return values" closes the join-shape gap) will populate non-zero
counts and make the cost-model trade-offs visible.

## Files

- `kara.toml` — project manifest (`name = "parallax_lite"`, `edition =
  "2026"`, no dependencies).
- `src/resources.kara` — `MetricsRecorder` trait, three trait-bound
  effect resources (`MetricsA / MetricsB / MetricsC`), `InMemoryMetrics`
  provider impl with a CPU-bound busy-compute kernel.
- `src/workload.kara` — three driver functions (`record_a / record_b /
  record_c`) each carrying a distinct `writes(R_i)` effect, and the
  `process_request` aggregator whose three-statement body is the
  slice's load-bearing code.
- `src/main.kara` — entry point. Currently calls `process_request()`
  directly without the canonical nested `with_provider` dance, because
  v1 codegen does not yet wire trait-method dispatch inside a provider
  scope to the concrete impl (`db_pipeline/src/db.kara` GAP-N) — a
  `MetricsA.record(...)` call inside `with_provider[MetricsA](...)` is
  a no-op at runtime today. Auto-par lowering keys off the *effect
  annotations*, not provider availability, so the demo-shape's
  load-bearing piece (the analyzer's group decision and the codegen's
  `karac_par_run` dispatch) is unaffected by the deviation. Once
  GAP-N closes, `main.kara` can grow back the canonical
  `with_provider[MetricsA] -> with_provider[MetricsB] ->
  with_provider[MetricsC] -> process_request()` nesting.

## See also

- `docs/implementation_checklist/phase-8-stdlib-floor.md § Auto-Concurrency
  Codegen — Parallax-lite Workload` — the slice 6 plan and close-out.
- `docs/design.md § Performance Diagnostics > Cumulative Cost Surface`
  — the spec for the cost-summary table this workload validates against.
- `docs/roadmap.md § Phase 8 § Parallax-lite — first ground-truth
  measurement workload` — the v1.x cost-model tuning follow-on this
  workload is the prerequisite for.
- `tests/parallax_lite.rs` — IR-shape, concurrency, cost-summary, gate,
  and (`#[ignore]`-gated) wall-clock benchmark coverage.
- `examples/db_pipeline/` — sister project, same multi-file shape, with
  the with_provider mechanism exercised against a richer effect surface.
