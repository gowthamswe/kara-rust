# WIP — List 1 (serial work, this session)

When picking up work, also mirror the bullet (with the box checked off as
work progresses) into the relevant `phase-N-*.md` tracker so the durable
record lives alongside every other completed phase entry.

---

## Theme: small/contained checklist items (2026-05-05)

Picking up a sequence of small, contained checklist items so each can
ship as its own commit. Original tracker entries get checked off as
each one closes.

- [x] **Trait method parameters require names — focused diagnostic for anonymous-parameter form.** Parser-only diagnostic upgrade (phase-8-stdlib-floor.md). Speculative `parse_type` from parameter position; if it succeeds and lands on `,`/`)`/`=`, emit `E_TRAIT_METHOD_ANONYMOUS_PARAM` (inside `trait { fn … }`) or `E_FN_ANONYMOUS_PARAM` (free / impl / extern). Help line names the recovered type so `_: <T>` / `arg: <T>` is copy-pasteable. Recovery: drop in a `Wildcard` pattern + the parsed type so the rest of the param list keeps parsing without a cascade. New `FnContext` enum + `fn_context_stack` on the parser, pushed at the three signature sites (`parse_function`, `parse_trait_method`, `parse_extern_function`). Free-function `render_type_for_diagnostic` walks every `TypeKind` variant for the help text. Tests in `tests/parser.rs` cover free-fn, trait-method, multi-param recovery, generics, ref types, `_: T` (negative), `i32: i32` shadowing primitive (negative), tuple/struct destructure (negative), and the multi-anon-per-signature case.
