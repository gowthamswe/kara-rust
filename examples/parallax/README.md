# Parallax

A worked example of Kāra's auto-parallelization: write straight-line
sequential code, the compiler runs it concurrently.

## What it demonstrates

The canonical fan-out + join shape (`docs/demo_ideas.md § Demo 1`) —
four typed effect resources, four provider implementations, and a
`get_dashboard(user_id)` function whose four `let` bindings the
compiler auto-parallelizes into a single concurrent group, joined
into a `Dashboard` aggregate.

```kara
pub fn get_dashboard(user_id: i64) -> Dashboard
    with reads(UserDB) reads(OrderDB) reads(NotifDB) reads(RecommendDB)
{
    let profile = fetch_profile(user_id);
    let latest_order = fetch_latest_order(user_id);
    let top_notification = fetch_top_notification(user_id);
    let top_recommendation = fetch_top_recommendation(user_id);
    Dashboard { profile, latest_order, top_notification, top_recommendation }
}
```

The auto-par analyzer sees no conflict edges between the four `let`s
(`reads(UserDB)` vs `reads(OrderDB)` are disjoint resources, no shared
writes), groups them into one parallel group, fans out across four
worker threads, and joins their results back into the outer scope so
the tail expression's `Dashboard { ... }` constructor reads them as
ordinary in-scope bindings.

No `async`. No `await`. No `Promise.all`. No explicit `par {}` block.
The parallelism is inferred entirely from the effect declarations.

## Differences from `parallax_lite`

[`examples/parallax_lite/`](../parallax_lite/) is the sister demo —
three resources, write-only effects, no joined return. Lighter surface,
runs as a microbenchmark.

This one adds:

- **Four resources instead of three** — exercises a four-deep
  `with_provider[R]` chain in `main.kara`.
- **Typed return values** — `Profile`, `Order`, `Notification`,
  `Recommendation` flow back from each parallel branch into the
  joined `Dashboard`.
- **Read-only fetches** with `ref self` receivers — the natural shape
  for a database-fetch workload.

## How to run

The project uses the multi-file layout (`kara.toml` + `src/*.kara`).
Multi-file codegen ships in Theme 4 (2026-05-10) — `karac build` from
the project root drives all six modules through resolve / typecheck
per-module, then concatenates the items in topological order and
runs effect / ownership / concurrency / codegen / link end-to-end:

```sh
cd examples/parallax/

# auto-parallelized (default)
karac build
./parallax

# sequential baseline — same source, same binary, opt-out via env var
KARAC_AUTO_PAR=0 karac build
./parallax
```

`KARAC_AUTO_PAR=0` short-circuits parallel-group dispatch back to
plain sequential execution at codegen time. A user-facing
`--sequential` CLI flag is planned.

`karac build --concurrency-report` prints a storyboard of what auto-
parallelized and why — the four-call group on `get_dashboard` is the
headline output.

## Files

- `kara.toml` — project manifest.
- `src/types.kara` — `Profile`, `Order`, `Notification`,
  `Recommendation`, `Dashboard`.
- `src/traits.kara` — four single-method provider traits
  (`UserDatabase`, `OrderDatabase`, `NotificationDatabase`,
  `RecommendationDatabase`), each with one `fetch_*` method
  taking `ref self`.
- `src/resources.kara` — four typed effect resources
  (`UserDB / OrderDB / NotifDB / RecommendDB`) bound to their
  respective traits.
- `src/providers.kara` — four `InMemory*DB` provider implementations.
  Each `fetch_*` runs a CPU-bound busy-compute kernel sized
  proportionally to "simulated I/O latency" (10M / 30M / 15M / 20M
  iterations) before returning a canned result.
- `src/workload.kara` — `fetch_*` wrapper functions (each declared
  `with reads(R)`) and the `get_dashboard(user_id)` workload itself.
- `src/main.kara` — entry point. Four-deep `with_provider` chain
  wraps a single `get_dashboard(42)` call.

## See also

- [`examples/parallax_lite/`](../parallax_lite/) — sister demo;
  three-resource fan-out without join.
- `tests/parallax.rs` — IR-shape, concurrency, end-to-end, and
  (ignore-gated) wall-clock benchmark coverage.
- `docs/demo_ideas.md § Demo 1` — the demo's design storyboard.

## Implementation notes

Tracker references for contributors digging into the implementation:

- Auto-parallelization analyzer: `src/concurrency.rs`.
- Per-branch return-slot ABI:
  `__karac_ParGroup_<id>_Returns` synthesis in `src/codegen.rs`
  (Slice A, commit `ab611d3`).
- `with_provider[R]` lowering: Theme 6 sub-steps 3+4+5
  (parallax-lite's three-deep nest, extended to four-deep here).
- Slice plan and close-out:
  `docs/implementation_checklist/phase-8-stdlib-floor.md §
  Provider Implementations` (Slice C, commit `f5c7b31`).
- The originally-planned `Vec[Order]` return for `fetch_latest_order`
  is now wired end-to-end. Three codegen bugs were uncovered and
  fixed during the path reconstruction (2026-05-09): the analyzer's
  non-contiguous parallel grouping; `SelfValue` missing from the
  auto-par capture-set walk; and the slot rebind's unconditional
  scope-exit free that corrupted moved Vec values. The other three
  fetches still return single-element struct shapes (`Profile`,
  `Notification`, `Recommendation`) — those match their natural
  domain semantics and the storyboard.
