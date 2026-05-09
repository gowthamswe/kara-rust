## Phase 11: Standard Library — Long-Tail

**End of this phase = v1 release.** See [`roadmap.md § Phase 11`](roadmap.md#phase-11-standard-library--long-tail) for the canonical scope.

Items currently tracked here (until physical reorganization happens — they live under [Phase 8 — Floor](#phase-8-standard-library--floor) above for now, mixed with floor items because the working tracker predates the Phase 8/11 split):

- **Numerical and data-science stdlib** — entire `### Numerical and data-science stdlib (Phase 11 — long-tail)` sub-section above.
- **Embedded / hardware primitives** — `Volatile memory access`, `Inline assembly`, `Atomic[T] and memory ordering`, `#[interrupt] handler ABI`, `Critical sections`.
- **Security** — `std.secret` module / `Secret[T]` wrapper.
- **Codegen IR optimization pass** — inline hints, alias metadata (`noalias`/`tbaa`), `nsw`/`nuw` arithmetic flags, LTO, static branch hints from effect analysis (`llvm.expect` emission — *not* PGO; see [`deferred.md § Profile-Guided Optimization Loop`](../deferred.md#profile-guided-optimization-loop) for the post-v1 PGO entry).

`std.json` stays in Phase 8 (floor) — every config / API client needs it.

> **v64 reshape (2026-05-09):** `std.regex`, `std.http` (server + client), `std.websocket` (server + client), `std.process`, `std.tracing`, HTTP/2, protobuf, `Pool[T]`, and application-layer backpressure primitives were lifted from Phase 11 long-tail to Phase 8 floor under the [backend-first lead-persona decision](../../brainstorming/archive/v64_backend_first_v1_concurrency.md). Trackers for the lifted items now live in [`phase-8-stdlib-floor.md § Backend Platform (v64-lifted)`](phase-8-stdlib-floor.md). `std.stats` (data-science specific) stays in Phase 11.

