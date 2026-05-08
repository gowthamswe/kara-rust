# WIP — List 1 (serial work, this session)

## Working patterns

**Mirror to the phase tracker.** When picking up work, also mirror the
bullet (with the box checked off as work progresses) into the relevant
`phase-N-*.md` tracker so the durable record lives alongside every
other completed phase entry. The phase tracker is the canonical record;
this list is the at-a-glance execution order.

**Delegate implementation, keep verification.** Slices in this queue
are drafted with self-contained per-item plans in their phase tracker
(goal, sub-steps, tests, files, out-of-scope, stop triggers — enough
that a fresh agent can implement from the plan alone). The main
session can delegate implementation to a subagent and act as
orchestrator — keeps main context manageable across long sessions.
Validated 2026-05-07 on slice 3 (~17 min subagent time, 97 subagent
tool calls, ~112K tokens kept out of main context; main spent 1
prompt + 4 verification commands).

Cycle, per slice:

1. **Main writes a self-contained subagent prompt.** Point at the plan
   section in the relevant `phase-N-*.md`, the working directory, the
   test commands (`cargo test`, `cargo test --features llvm`,
   `cargo clippy --all --tests -- -D warnings`,
   `cargo fmt --all -- --check`), the commit-message style (recent slice
   commits as templates), the hard-stop triggers from the plan
   (inline-fix is the default — the subagent doesn't need to flag
   minor friction in the report), and what to report back (commit
   hash, test counts, design-affecting deviations from plan,
   hard-stops with annotation refs). Include accumulated session
   context the subagent won't otherwise see — known workarounds
   (e.g., the struct-literal generic-arg gap pattern), pre-existing
   drift in unrelated files, code-slot conflicts (e.g., `E02xx` codes
   already taken).
2. **Subagent implements, verifies, closes out, commits, reports.**
   Single contained commit covering: the implementation, the doc
   close-out under the relevant item (same shape as slice 1 / 2 / 3
   close-outs in `phase-4-interpreter.md` — *What landed.* → *Tests.*
   → *Deviations from the proposed plan.* → *Out of scope, still
   open.*), the wip-list1 checkbox flip, **any parent-CR slice-roadmap
   bullet flip** (some CRs in phase trackers maintain a slice ledger
   separate from the per-item close-out — e.g., `phase-4-interpreter.md`
   § method-resolution slice roadmap has `- [x] Slice N — ... Landed
   YYYY-MM-DD (commit X). [brief summary]` bullets parallel to the
   per-item close-outs; both must update on slice landing), and any
   side-effect mappings the plan calls for (e.g., a new diagnostic-code
   entry in `src/cli.rs`).
3. **Main verifies.** Read the diff (`git show <hash> --stat` then
   spot-check critical files), run the new tests independently
   (`cargo test --test <suite> <test_pattern>` is fast), confirm both
   `cargo clippy --all --tests -- -D warnings` and
   `cargo fmt --all -- --check` are clean. Don't trust the subagent's
   summary alone — the report describes intent; the diff is ground
   truth.
4. **Main updates the queue and moves on.** Tick the bullet off here,
   kick off the next slice, or pause for discussion.

**Per-commit gates.** All commits — slice impl, doc updates, chores —
clear four checks before landing: `cargo test`,
`cargo test --features llvm`, `cargo clippy --all --tests -- -D warnings`,
and `cargo fmt --all -- --check`. **Clippy and fmt are both hard
gates.** Neither tolerates drift, including drift in files the commit
didn't touch.

**Pre-slice gate.** First action of any new slice or session: run
`cargo fmt --all -- --check`. If it fails, fix with `cargo fmt --all`
and land as a standalone `chore: cargo fmt cleanup` commit *before*
starting slice work. Without this, post-slice fmt cleanup either
contaminates the slice diff with unrelated drift or gets surgically
reverted (the established-but-flawed workaround that let drift
accumulate across slices 1, 2, 3 until CI flushed it on 2026-05-07).
Pre-slice gate breaks that cycle: slice work happens against a clean
tree, so the post-slice fmt check just verifies the slice's own
changes.

**What main does NOT delegate.** Design conversations; cross-slice
handoff decisions; deciding when to pause vs. continue; anything that
requires conversation history. The subagent operates from its prompt
only — it doesn't see this session's running context. If a slice
needs design judgment that wasn't captured in its plan, that's a
discussion-mode item, not an autonomous-queue item (parent CR roadmap
flags these explicitly — e.g., slice 4 storage-shape change, parser
CR for concrete-type UFCS).

**Friction handling — inline fix is the default.** Friction the agent
encounters during slice work gets fixed inline as part of the slice
work. No special flagging in the report; the slice's commit absorbs
the fix. The agent owns code hygiene for the files they touch.
Inline-fix territory includes:
- Clippy lint corner cases (apply the lint or add a scoped `#[allow]`
  with a one-line reason).
- Doc placement nits (pick the more sensible spot, move on).
- Test fixture workarounds (e.g., struct-literal generic-arg gap →
  use function-parameter form to pin receiver type).
- Code-slot conflicts (e.g., proposed `E0237` already taken → pick
  the next free slot, update all references).
- Pre-existing nit-level issues in adjacent code the agent
  encounters naturally while editing (warnings on lines they're
  changing, obvious typos, dead `#[allow]` attributes — fix them).

**Hard-stop = "I need main's input to proceed."** Reserved for the
narrow set of situations where the right next step isn't obvious and
the agent isn't authorized to guess. When a hard-stop fires:

1. Don't commit the slice. Leave the working tree clean (stash or
   discard partial work depending on whether it's salvageable for
   when main returns).
2. Annotate the slice's plan section in the relevant `phase-N-*.md`
   tracker with a `**Blocked (YYYY-MM-DD).**` paragraph explaining
   the trigger, what was investigated, and what input is needed
   from main.
3. Flip the wip-list1 bullet to prefix `**[BLOCKED]**` (keep the
   `[ ]` checkbox unchecked) with a one-line pointer to the blocker
   annotation in the phase tracker.
4. **Move to the next non-dependent slice in the queue.** A blocked
   slice doesn't halt the whole queue. Skip slices that depend on
   the blocked one (the queue's slice-ordering prose names
   dependencies); pick up the next independent slice. Queue only
   ends when every remaining slice is either done or blocked.

Hard-stop triggers (the actual halt conditions; everything else is
inline-fix territory):
- Pre-existing test breakage that requires a design fork to resolve.
- Parser/AST shape changes needed.
- Effect-checker / ownership-checker invariants turning out
  load-bearing in unanticipated ways.
- The slice's premise turns out wrong (e.g., the assumed mechanism
  doesn't exist, or exists in a fundamentally different form than
  the plan assumed).

**When the plan isn't detailed enough yet.** Don't kick off
delegation. Either draft the plan to the autonomous-friendly bar
first (single docs commit, same shape as slice 1 / 2 / 3 plans) or
keep the slice in main where you can iterate on design as you go.

---

## Theme: phase 4–8 autonomous queue (overnight slate, 2026-05-07)

Ten-slice slate spanning phases 4, 5, 7, and 8, queued for autonomous
overnight execution (~8h budget). Initial six slices populated 2026-05-07;
extended by four more slices later the same day after deeper triage
(A3 status correction + N2/N3/N4 from the phase-5/7/8 sub-item scan).
Each slice has its plan drafted under the relevant item in the phase
tracker; this list is execution order + checkbox mirror.

Verified alternates if any queued slice hard-stops:
- Phase-5:99 — let-binding case-class enforcement (resolver-side completion of `[x]` parent)
- Phase-7:47 — `karac query monomorphization` subcommand (data exists in codegen, plan needs minor refinement)

**Discussion mode** (NOT in this queue):
- Method-resolution slice 4 (`impl Option[Ordering]` storage-shape
  change) — architectural impl-table key change with ripples across
  every consumer.
- Concrete-type UFCS parser CR (tracked in `phase-2-parser-ast.md`) —
  parser/AST shape change.
- Bucket B items from the 2026-05-07 phase 4–8 survey (~60 items) —
  all need design discussion before queueing.

Run-time rules (per per-commit and pre-slice gates in the
working-patterns section above):
- Per-slice commit: plan + impl + close-out + wip-list checkbox flip
  combined into one commit.
- Between slices: `cargo test`, `cargo test --features llvm`,
  `cargo clippy --all --tests -- -D warnings`,
  `cargo fmt --all -- --check` all clean.
- Friction handling: inline fix is the default (no flagging in
  reports). Hard-stop only when main's input is genuinely needed
  (design forks, parser/AST shape changes, slice-premise turning out
  wrong); on hard-stop, annotate the slice's plan in the phase
  tracker, prefix this bullet with `**[BLOCKED]**`, and move to the
  next non-dependent slice.

**Slice ordering and dependencies.** Sequence is low-risk warm-up →
mid-risk → REPL pair at the end. One inherited dependency from prior
queue: slice 3.5 depends on slice 3 (closed, commit `eefe7b7`). One
new dependency: A11 depends on A9 (REPL cell-tracking infrastructure).
All other slices are independent.

- [x] **Slice 3.5 — Self-receiver dispatch (method-resolution item 8 follow-up).** ✓ Landed 2026-05-07. `self.method()` inside a trait default body resolves through the enclosing trait's own methods + supertrait closure; the `name != "Self"` exclusion slice 2 left in place is closed. Five pre-existing tests now exercise the real resolution path; new negative test pins the closed silent-fallthrough hole. Close-out: `phase-4-interpreter.md` item 8.

- [ ] **[BLOCKED]** **Slice get_unchecked — `Slice[T].get_unchecked(i)` and `get_unchecked_mut(i)` unsafe escape hatch.** Two `unsafe fn`s on `Slice[T]` returning `ref T` / `mut ref T`, lowered to direct GEP without the bounds-check + panic-block prelude. Mirrors Rust's `<[T]>::get_unchecked` shape; safety contract is caller-guaranteed `i < self.len()`. ~3-4 typechecker tests + 2 codegen tests under `--features llvm`. Plan: `phase-7-codegen.md` § "`Slice[T].get_unchecked(i)` and `Slice[T].get_unchecked_mut(i)` escape hatch" (slice plan section). Source: phase 4–8 survey bucket A4. **Blocked 2026-05-07** on missing unsafe-block enforcement infrastructure (`unsafe { }` is doc-lint only; no typechecker gating, no `unsafe fn` parser form). See close-out paragraph in plan section for predecessor unblock.

- [x] **Slice binary-size phase 1 + symbol sweep — `strip -x` post-link, `panic = "abort"` in runtime release profile, plus pre-flight runtime symbol audit.** ✓ Landed 2026-05-07. Combined slice covering both Phase 1 binary-size optimization and the pre-flight symbol sweep. `panic = "abort"` lives at workspace-root `[profile.release]` (cargo refuses per-package `panic`); `strip -x` runs after `cc` link inside `link_executable_impl` (gated `cfg!(unix)`, skipped on sanitizer builds to keep ASAN stack-trace symbolication legible); `runtime/SYMBOL_KEEP_LIST.md` documents 19 `#[no_mangle]` runtime exports + 1 libc import + 1 private callback + confirms zero `#[used]/#[link_section]/#[ctor]/#[dtor]` (so no Phase 2 DCE keep-list machinery beyond what the audit captures). Measured deltas on this macOS environment: runtime archive -48 KB (panic=abort), example E2E binaries +32 B each (Mach-O strip header rewrite exceeds savings on these tiny ld64-already-pruned binaries — slice plan called this out as the "pick one of the example .kara programs" smoke verification). Close-out: `phase-7-codegen.md` § "Phase 1 binary-size optimization".

- [x] **Slice perf note — `shared struct` with mut fields (Tier 2 `--perf-report`).** ✓ Landed 2026-05-08. Definition-site walker over `StructDef` items emits `perf[shared-struct-mut-field]` into the perf-report aggregator (`src/cost_summary.rs`) when `kind == Shared` and at least one field carries `mut`; one note per offending struct (not per field) with field names enumerated in the message body. New `PerfNote` type + `perf_notes: Vec<PerfNote>` on `CostSummary`; surfaced today through `karac query cost-summary`'s JSON envelope (`"perf_notes":[...]`), ready for the future `karac build --perf-report` UX without further data-shape work. Off by default (Tier 2, predictive). Three tests in `tests/cli.rs` cover positive (shared+mut → note) and both negatives (shared/no-mut → no note; plain/+mut → no note). Close-out: `phase-7-codegen.md` § "Definition-site perf note: `shared struct` with mut fields".

- [ ] **Slice REPL UAM diagnostic — Notebook-aware use-after-move.** Enrich the existing `UseAfterMove` diagnostic in REPL context to name the cell that consumed the binding and suggest `.clone()` at the consume site. Adds `Session.cell_byte_ranges` cell-tracking infrastructure (foundation for the next slice). Strictness identical to `.kara` files; only diagnostic presentation differs. Plan: `phase-5-diagnostics.md` § "Notebook-aware use-after-move diagnostic" (slice plan section). Source: phase 4–8 survey bucket A9.

- [ ] **Slice REPL auto-clone — `karac repl --auto-clone` opt-in mode.** CLI flag that auto-inserts `.clone()` at consume sites when bindings are referenced cross-cell. Builds on the previous slice's cell-tracking. Never silent — emits `perf[auto-clone-in-repl]` note on every insertion. Inserted clones survive `:save` export. Plan: `phase-5-diagnostics.md` § "`--auto-clone` opt-in mode" (slice plan section). Source: phase 4–8 survey bucket A11. **Depends on the previous slice (REPL UAM diagnostic).**

- [ ] **Slice atomic-RC — wire `arc_values` to atomic-RC codegen (RC integration substep 2).** `ownership.rs` flags `arc_values` (subset of `rc_values`) for bindings that cross `par {}` thread boundaries; codegen currently ignores the subset and uses non-atomic `load`/`add`/`store` for inc/dec, racing on the refcount. Wire `atomicrmw add` / `atomicrmw sub` (`SeqCst`) for the `arc_values` subset. Substeps (1) box-and-RC and (3) drop-at-scope-end already landed under the RC fallback Phase 1 umbrella; this slice closes substep (2). Plan: `phase-7-codegen.md` § "RC values: codegen integration" (slice plan section). Source: 2026-05-07 deeper triage (corrects A3 false-positive `[x]` to `[~]`).

- [ ] **Slice env.set — `env.set(name, value)` stdlib method + `writes(Env)` effect.** Standard I/O Phase 8 follow-up; `env.var()` and `env.args()` are shipped, `env.set()` is the missing companion. Pure pattern-extension of the existing `env.var` / `env.args` registration. Plan: `phase-8-stdlib-floor.md` § "`env.set(name: String, value: String)` + `writes(Env)` effect" (slice plan section). Source: 2026-05-07 deeper triage, factored from Standard I/O `[x]` parent.

- [ ] **Slice From[VarError] → IoError — `impl From[VarError] for IoError`.** Standard I/O Phase 8 follow-up; needed for `?`-propagation from `env.var(...) -> Result[String, VarError]` into functions returning `Result[T, IoError]`. Single trait-impl addition in baked stdlib. Variant mapping: `VarError.NotPresent → IoError.NotFound`; `VarError.NotUnicode → IoError.InvalidUtf8`. Plan: `phase-8-stdlib-floor.md` § "`impl From[VarError] for IoError`" (slice plan section). Source: 2026-05-07 deeper triage, factored from Standard I/O `[x]` parent.

- [ ] **Slice `?` JSON trace mode — runtime JSON / JSONL output for compiled binaries.** Add `KARAC_ERROR_TRACE_FORMAT=json|jsonl|text` env-var-driven format selector to the runtime's atexit error-trace printer; default `text` preserves existing behavior. JSON shape matches the interpreter's existing trace format. Runtime-only change in `runtime/src/lib.rs`. Plan: `phase-8-stdlib-floor.md` § "`?` codegen follow-up: `error_return_trace`..." → "Slice plan (drafted 2026-05-07) — JSON / JSONL trace output mode". Source: 2026-05-07 deeper triage, the open follow-up of the parent `[~]` item.

---

## Timing log (overnight run, 2026-05-07)

Run started: **2026-05-07 21:41:40 PDT**.

Per-slice durations recorded as each lands. Subagent wall-clock is the implementation phase; main verification is folded in (read diff, run tests, fmt/clippy spot-check).

| # | Slice | Started | Landed | Duration | Commit |
|---|---|---|---|---|---|
| 1 | 3.5 — Self-receiver dispatch | 2026-05-07 21:41 | 2026-05-07 21:53 | 12 min | `f7cad93` |
| 2 | get_unchecked | 2026-05-07 22:00 | _—_ | ~10 min (investigation) | `BLOCKED` |
| 3 | binary-size phase 1 | 2026-05-07 23:45 | 2026-05-07 23:55 | 10 min | `0731fd2` |
| 4 | perf note (shared struct mut) | 2026-05-08 00:01 | 2026-05-08 00:12 | 11 min | _pending fill_ |
| 5 | REPL UAM diagnostic | _—_ | _—_ | _—_ | _—_ |
| 6 | REPL auto-clone | _—_ | _—_ | _—_ | _—_ |
| 7 | atomic-RC | _—_ | _—_ | _—_ | _—_ |
| 8 | env.set | _—_ | _—_ | _—_ | _—_ |
| 9 | From[VarError] → IoError | _—_ | _—_ | _—_ | _—_ |
| 10 | `?` JSON trace mode | _—_ | _—_ | _—_ | _—_ |

Total elapsed: _pending overall completion_.
