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

Six-slice slate spanning phases 4, 5, and 7, queued for autonomous
overnight execution (~8h budget, ~5–6h subagent time + verification
headroom). Each slice has its plan drafted under the relevant item in
the phase tracker; this list is execution order + checkbox mirror.

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

- [ ] **Slice 3.5 — Self-receiver dispatch (method-resolution item 8 follow-up).** `self.method()` inside a trait default body resolves through the enclosing trait's own methods + supertrait closure (currently silent fallthrough). Closes the `name != "Self"` exclusion slice 2 left in place. Five pre-existing tests get a real resolution path; new negative test pins the closed silent-fallthrough hole. Plan: `phase-4-interpreter.md` item 8 § "Slice 3.5 plan". Source: slice 2 deferred item.

- [ ] **Slice get_unchecked — `Slice[T].get_unchecked(i)` and `get_unchecked_mut(i)` unsafe escape hatch.** Two `unsafe fn`s on `Slice[T]` returning `ref T` / `mut ref T`, lowered to direct GEP without the bounds-check + panic-block prelude. Mirrors Rust's `<[T]>::get_unchecked` shape; safety contract is caller-guaranteed `i < self.len()`. ~3-4 typechecker tests + 2 codegen tests under `--features llvm`. Plan: `phase-7-codegen.md` § "`Slice[T].get_unchecked(i)` and `Slice[T].get_unchecked_mut(i)` escape hatch" (slice plan section). Source: phase 4–8 survey bucket A4.

- [ ] **Slice binary-size phase 1 + symbol sweep — `strip -x` post-link, `panic = "abort"` in runtime release profile, plus pre-flight runtime symbol audit.** Combined slice covering both Phase 1 binary-size optimization and the pre-flight symbol sweep. Expected size delta: 1.4 MB → ~900 KB on a representative E2E binary (-18% from strip, ~114 KB from `panic = "abort"`). Symbol sweep produces `runtime/SYMBOL_KEEP_LIST.md` documenting `#[used]` / `#[link_section]` / `#[ctor]` / `#[dtor]` / `#[no_mangle]` / `extern "C"` declarations in `runtime/src/`. Plan: `phase-7-codegen.md` § "Phase 1 binary-size optimization" (slice plan section). Source: phase 4–8 survey bucket A5+A6.

- [ ] **Slice perf note — `shared struct` with mut fields (Tier 2 `--perf-report`).** Definition-site walker over `StructDef` items in the typechecker; when `kind == Shared` and at least one field carries `mut`, emits `perf[shared-struct-mut-field]` into the perf-report aggregator. Off by default (Tier 2, predictive). Plan: `phase-7-codegen.md` § "Definition-site perf note: `shared struct` with mut fields" (entry has implementation paragraph; subagent drafts the test + close-out detail). Source: phase 4–8 survey bucket A12.

- [ ] **Slice REPL UAM diagnostic — Notebook-aware use-after-move.** Enrich the existing `UseAfterMove` diagnostic in REPL context to name the cell that consumed the binding and suggest `.clone()` at the consume site. Adds `Session.cell_byte_ranges` cell-tracking infrastructure (foundation for the next slice). Strictness identical to `.kara` files; only diagnostic presentation differs. Plan: `phase-5-diagnostics.md` § "Notebook-aware use-after-move diagnostic" (slice plan section). Source: phase 4–8 survey bucket A9.

- [ ] **Slice REPL auto-clone — `karac repl --auto-clone` opt-in mode.** CLI flag that auto-inserts `.clone()` at consume sites when bindings are referenced cross-cell. Builds on the previous slice's cell-tracking. Never silent — emits `perf[auto-clone-in-repl]` note on every insertion. Inserted clones survive `:save` export. Plan: `phase-5-diagnostics.md` § "`--auto-clone` opt-in mode" (slice plan section). Source: phase 4–8 survey bucket A11. **Depends on the previous slice (REPL UAM diagnostic).**
