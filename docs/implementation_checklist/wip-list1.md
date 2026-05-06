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

- [x] **Empty prefix-literal diagnostic — `Vec[]` / `Array[]` / `Set[]` / `Map[]` without binding annotation.** Typechecker-only (phase-5-diagnostics.md, also closes phase-4-interpreter.md line 10). New `report_empty_prefix_literal` helper emits `error[E_EMPTY_PREFIX_LITERAL_NEEDS_ANNOTATION]` with a per-kind annotation skeleton (`Vec[T]`, `Array[T, 0]`, `Set[T]`, `Map[K, V]`) and constructor suggestion (`Vec.new()` / `Set.new()` / `Map.new()`). Synthesis-mode arm of `infer_expr_inner` rejects empty literals with the focused diagnostic; new check-mode arm at the top of `check_expr` recovers via the expected type when shapes match (`Vec`/`Set`/`Map` Named, `Array` fixed-size). Drive-by: `infer_struct_literal` switched from `infer_expr` + `check_assignable` to `check_expr` for field values so the new check-mode arm fires at struct-field initializer positions (parity with how function-call args already work). Tests in `tests/typechecker.rs` cover all 4 kinds in synthesis (positive diagnostic), all 4 kinds with annotation (negative — keep passing), typed call argument, typed struct-field initializer.

- [x] **Identifier case-class enforcement.** Parser-only (phase-5-diagnostics.md). Closed the three gaps in the existing case-class check coverage: const generic params now enforce Type-class (`fn zeros[const N: usize]` — `N` accepted, `n` rejected); associated type decls now enforce Type-class (`type Item;` — accepted; `type item;` — rejected); `layout` def names now enforce Value-class (the binding is collection-shaped, not type-shaped — `layout entities: Vec[Entity]` accepted, `layout MyEntities` rejected). Tests added: `test_ident_class_const_generic_param_must_be_type_class`, `test_ident_class_const_generic_param_single_upper_ok`, `test_ident_class_assoc_type_must_be_type_class`, `test_ident_class_assoc_type_pascal_ok`, `test_ident_class_layout_name_must_be_value_class`. Existing 14+ case-class tests still pass.

- [ ] _next item_
