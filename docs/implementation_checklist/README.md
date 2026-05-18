# Implementation Checklist

Items to validate, benchmark, or revisit during specific implementation phases. These are not design decisions — they are implementation concerns that should not be forgotten.

Sourced from open gaps identified during design review that don't require design decisions but do require action during implementation.

Status-marker convention (including the `[->]` "explicitly deferred" state) is documented in [`docs/deferred.md § Tracker status markers`](../deferred.md#tracker-status-markers).

---

## Work in Progress (updated 2026-05-04)

WIP entries live in two trackers, split by lane so List-1 and List-2 work
can be edited and committed independently without merge conflicts.

- **[`wip-list1.md`](wip-list1.md)** — serial work (one agent at a time
  owns the in-flight bullet). Currently empty.
- **`wip-list2.md`** — parallel-safe work (any agent can pick up without
  coordination; file / function boundaries are chosen so the work doesn't
  conflict with the active List-1 bullet). Created on demand.

Each file's bullets get migrated into the relevant `phase-N-*.md` tracker
when work begins; the tracker is checked off in both places as work
progresses, and the WIP file's body is cleared once its bullets all close
(the file itself is kept as a scaffold for the next theme).

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
