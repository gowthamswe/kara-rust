## Phase 2: Parser & AST

- [x] **Half-open range expressions and patterns.** `lo..`, `..hi`, `..=hi`, and `..` are accepted in expression and pattern position. `ExprKind.Range.start`/`.end` are `Option<Box<Expr>>`; `PatternKind.RangePattern.start`/`.end` are `Option<LiteralPattern>` with an `inclusive` field. Six range types synthesized: `Range[T]`, `RangeInclusive[T]`, `RangeFrom[T]`, `RangeTo[T]`, `RangeToInclusive[T]`, `RangeFull`. All six accepted as slice indices (`v[..]`, `v[i..]`, `v[..n]`, `v[..=n]`). Interpreter handles half-open slice forms. All downstream phases updated (resolver, typechecker, effectchecker, ownership, lowering, interpreter, formatter, concurrency, provider_escape). 13 new parser tests + 9 new typechecker tests added. LLVM codegen wired: `compile_range_slice` defers end-bound resolution to after `src_len` is known (enabling open-end forms) and applies inclusive +1 adjustment; 6 E2E tests cover all four half-open forms plus `a..=b` on Array and Vec.

- [x] **Prefix dereference operator `*expr`.** ✓ DONE (2026-04-27) — `UnaryOp::Deref` in AST and parser already landed; completed: typechecker strips `ref`/`mut ref` wrapper and rejects `*r = v` on shared ref; ownership `root_identifier` recurses through Deref for `*r = v` mutation tracking; interpreter `*r = v` rebinds param locally + CICO write-back at call site copies mut-ref final values back to caller; codegen `*r` returns `compile_expr(operand)` directly (load_variable already dereferences), `*r = v` uses `get_data_ptr` to store through the raw pointer. 4 interpreter tests + 3 codegen E2E tests.
  1. [x] **AST.** `UnaryOp.Deref` variant added.
  2. [x] **Parser.** `Token::Star` recognized in `parse_prefix`.
  3. [x] **Resolver.** Traverses the new variant.
  4. [x] **Typechecker.** `*expr` typed as inner T; `*r = v` rejected on `ref T`, accepted on `mut ref T`.
  5. [x] **Effect/ownership.** `*r` treated as read; `*r = v` as write-not-consume.
  6. [x] **Interpreter.** `*r` loads referent; `*r = v` writes via CICO write-back.
  7. [x] **Codegen.** `*r` compiles to `compile_expr(operand)`; `*r = v` stores through raw pointer via `get_data_ptr`.
  8. [x] **Tests.** 4 interpreter tests + 3 codegen E2E tests covering read, write, rejection on shared ref.

- [x] **Closure capture-mode prefix (`ref` / `mut ref`).** ✓ DONE (2026-05-01). Parser + AST surface for explicit per-closure borrow-mode override (design.md § Closure Behavior, Rule 2½). Bare `|...|` is captures-by-ownership and needs no prefix — that posture lands at parse time today (a `move`/`own` prefix is rejected with a migration error). Implementation: (1) `parse_closure` now accepts an optional `Option<CaptureMode>` argument; `parse_prefix` uses two-token / three-token lookahead via the new `peek_token_at(offset)` helper to recognize `ref |` / `ref ||` / `mut ref |` / `mut ref ||` as a closure-expression construct, leaving the keyword tokens alone in any other position (parameter mode, type position, call-site `mut` marker). (2) `CaptureMode { Ref, MutRef }` enum and `Closure.capture_mode: Option<CaptureMode>` field added to `src/ast.rs` (no `Move` variant — bare = owned is the only "owned" form). (3) Walker call-sites in `src/typechecker.rs`, `src/interpreter.rs`, `src/resolver.rs` updated to destructure the new field (`capture_mode: _`, no behavior change); the existing `..` patterns at the other walkers continue to absorb the field correctly and will be revisited when item 2 introduces the first reading consumer. (4) Formatter emits `ref ` / `mut ref ` before the opening `|` per syntax.md § 5.11. Tests: 5 new parser tests in `tests/parser.rs` (`ref` prefix, `mut ref` prefix, bare-form regression, `ref ||` empty-params form, `mut ref T` parameter-type-position non-consumption) + 3 new inline formatter tests in `src/formatter.rs` (roundtrip `ref`, roundtrip `mut ref`, bare form does not emit a prefix). Files changed: `src/ast.rs`, `src/parser.rs`, `src/formatter.rs`, `src/interpreter.rs`, `src/resolver.rs`, `src/typechecker.rs`, `tests/parser.rs`.

- [x] **Trait associated function dispatch — general case.** Closed: tree-walk dispatch works end-to-end for `T.method()`, bare-call expected-type inference, and concrete-prefix calls; effect ceilings propagate; codegen routes through the existing `Type.method` symbol convention. Mangled `Trait::Type::method` form deferred until a real method-name collision appears. Generic-function monomorphization at codegen time is a separate larger track. Ordered implementation steps:
  1. [x] **Resolver.** Track trait bounds on generic parameters. `SymbolTable.generic_param_bounds: HashMap<SymbolId, Vec<TraitBound>>` records the union of inline + where-clause bounds per `TypeParam`. New helpers wired into every generic-bearing definition (function, struct, enum, trait incl. method-level, impl, type alias). Trait supertraits land as bounds on `Self`. Trait paths inside bounds are resolved into the resolution map. 10 new tests in `tests/resolver.rs`.
  2. [x] **Typechecker — type-prefixed dispatch generalization.** `T.method(args)` (parsed as `Call(Path(["T","method"]))`) intercepted in `infer_call` via `try_dispatch_typeparam_assoc_fn`. Reads the receiver's `TypeParam` symbol via `resolve_result.resolutions`, searches its recorded bounds for traits declaring `method`, lowers the trait method's signature with `Self → Type::TypeParam(target)` substitution, validates args. Multiple bound traits matching → new `E0233 AmbiguousAssocFn` (UFCS `Trait.method(...)` suggestion). Concrete-type prefix calls continue through the existing `resolve_path_type` impl-iteration path. 8 new tests in `tests/typechecker.rs`.
  3. [x] **Typechecker — expected-type inference.** Resolver suppresses the undefined-name error for bare-identifier callees matching a trait-declared no-self associated function. Typechecker `check_expr` intercepts via `try_apply_expected_assoc_fn_inference`: `Type::TypeParam(t)` reads `t`'s bounds from a new `enclosing_bounds` map (populated in `check_function` / `check_impl_block` / `check_trait_def` — supertraits land as `Self` bounds); `Type::Named { name }` searches `env.impls`. Synthesis fallback in `infer_call` emits `E0234 CannotInferAssocFn`. 8 new tests in `tests/typechecker.rs`.
  4. [x] **Effect inference.** Ceiling collection already covered no-self trait methods — call-site dispatch was the gap. New `EffectChecker.fn_bounds_index` precomputes per-fn generic-param bounds; `collect_calls_in_*` thread bounds; `extract_trait_assoc_fn_keys` redirects `Path([T,m])` / bare `Identifier(m)` to `Trait.m` when the head resolves through a typeparam bound. `MethodCall` on `Self` redirected via `Self` bounds (supertraits). Concrete-type prefix calls untouched — they keep the impl's actual effects. 6 new tests in `tests/effectchecker.rs`.
  5. [x] **Interpreter.** Lowering rewrites bare-call assoc-fn dispatch to `Path([Target, name])` via new `bare_assoc_fn_targets` side-table. Interpreter has new `type_subs_stack` populated from `call_type_subs` (typechecker emits per-call substitutions, both arg-driven and expected-type-driven). `Path([T, m])` consults the stack to resolve `T` to a concrete impl. 5 new tests in `tests/interpreter.rs`.
  6. [x] **Codegen.** Existing impl-block pass already emits trait-impl methods as `Type.method` symbols (no `self`-receiver discrimination). Bare-call dispatch lands via lowering rewriting to `Path([Target, name])`. 4 new tests in `tests/codegen.rs` (IR + 3 E2E). Mangled `Trait::Type::method` form deferred until a real collision case appears.
  7. [x] **Tests.** Per-layer coverage at items 1-5; 5 broad integration tests in `tests/interpreter.rs` (FromStr-style trait, two traits on one type, generic-helper chain, where-clause bound, bare in arg position). No-expected-type failure covered by `test_bare_call_no_expected_type_errors`.

- [x] **`.try_into()` method-call sugar.** Closed: `let y: Result[Target, E] = x.try_into()` resolves to `Target.try_from(x)` via the same desugar architecture as `.into()`. Effect propagation through `.try_into()` falls out of the round-6 step-3 lowered-call path automatically. Ordered implementation steps:
  1. [x] **TypeEnv: `find_tryfrom_impl` helper.** New method on `TypeEnv` parallel to `find_from_impl` at typechecker.rs:717. Searches `impls_by_trait["TryFrom"]`, matches `target_type` and the `try_from` method's first param type against `source`. Returns `Option<&ImplInfo>`.
  2. [x] **Typechecker: `.try_into()` recognizer at expected `Result[T, E]` position.** New side-table `try_into_conversions: HashMap<SpanKey, String>` on `TypeChecker` + `TypeCheckResult` (parallel to `into_conversions`). New helper `try_apply_tryinto_coercion(expr, expected)` parallel to `try_apply_into_coercion`: extracts `Target` from `Result[Target, _]` expected, looks up `find_tryfrom_impl(src_ty, target)`, records and returns expected on hit, emits "no `impl TryFrom[S] for T`" diagnostic on miss. Wired into `check_expr` after `try_apply_into_coercion`.
  3. [x] **Lowering: rewrite `x.try_into()` to `Target.try_from(x)`.** New `rewrite_try_into_call` at lowering.rs (parallel to `rewrite_into_call`). Reads `tc.try_into_conversions`. Method-call dispatch at lowering.rs:285 chains with `.or_else()` so `into` and `try_into` both flow through the same arm.
  4. [x] **Typechecker tests.** 5 new tests in `tests/typechecker.rs`: happy path, missing TryFrom impl, multi-impl disambiguation by source, non-Result expected does not fire recognizer (asserted by inspecting `try_into_conversions` size), source-type mismatch (recognizer fires but `find_tryfrom_impl` returns None, diagnostic names the source type). *Out of scope, surfaced during testing:* method-call inference for unknown methods is currently silent (`let v: Validated = r.unknown_method()` typechecks); separate language-quality issue, not introduced by this round.
  5. [x] **Effectchecker tests.** 1 new test in `tests/effectchecker.rs` using the existing `effectcheck_full_pipeline` helper: TryFrom impl with `writes(Log)` propagates through `.try_into()` to the caller. Confirms the round-6 step-3 lowered-call path now applies via the sugar with no further effectchecker changes.

- [x] **Path expression with generic args — concrete-type UFCS support.** Discovered 2026-05-07 during method-resolution slice-2 grounding (see `phase-4-interpreter.md` item 5). `Vec[i64].new()` currently parses as `MethodCall { object: PrefixCollectionLiteral { type_name: "Vec", items: [Identifier("i64")] }, method: "new", args: [] }` — i.e., `Vec[i64]` is treated as a one-element collection literal containing the identifier `i64`. The intended parse for concrete-type UFCS (per `design.md:226` example `Vec[i32].default()`) is a path-with-generic-args expression form, e.g. `MethodCall { object: PathExpr { segments: ["Vec"], generic_args: Some([i64]) }, method: "new", ... }` or similar. Today's `ExprKind::Path` is bare `Vec<String>` with no generic-args slot, so even if the parser disambiguated, the AST couldn't carry the args.

  **Design lock (2026-05-08): option (a) — extend `ExprKind::Path`.**
  Sized via a grep pass that found 59 `ExprKind::Path` sites across 15
  source files (~4 construction, ~15 wildcard, ~40 destructure).
  Rejected option (b) (new `TypedPath` variant) on coverage-risk
  grounds: ~15 wildcard leaf-detection sites would silently fall
  through under (b), forcing per-site judgment about whether each
  should treat `TypedPath` as `Path`. Option (a) gets compile-time
  coverage for free at the cost of ~40 mechanical destructure-pattern
  updates. Long-term maintenance also favors (a) — concrete-type UFCS
  + `impl Option[Ordering]` will make path-with-generic-args a
  recurring shape, not exotic.

  Two-prong CR (now locked):
  1. **AST shape.** Extend `ExprKind::Path(Vec<String>)` to
     `ExprKind::Path { segments: Vec<String>, generic_args: Option<Vec<TypeExpr>> }`
     in `src/ast.rs`. Construction sites populate `generic_args: None`
     by default; the parser populates `Some(...)` only at the
     disambiguation site below.
  2. **Context-aware parsing.** When the parser sees `TypeName[...]`
     (uppercase first segment, generic-args-shaped contents),
     currently produces a `PrefixCollectionLiteral` expression. The
     disambiguation rule: when the `[...]` is immediately followed by
     `.method(`, prefer the path-with-generic-args interpretation over
     the collection-literal one. Lookahead at parse time; no name
     resolution required (the case-class disambiguation per
     `design.md:374` is purely syntactic).

  Once both prongs land, the typechecker side is a small extension to
  the existing `resolve_path_type` impl-walk (slice 1 of
  method-resolution wired the bound-discharge engine but intentionally
  skipped this site because it had no receiver args at the call site
  — a path-with-generic-args expression provides exactly those args).

  Unblocks [`phase-4-interpreter.md` § "TypeChecker: implement full
  method resolution algorithm" sub-item 5B](phase-4-interpreter.md)
  (concrete-type UFCS) and the
  [`impl Option[Ordering]`](phase-4-interpreter.md#impl-option-ordering-deferred)
  follow-up (whose user-facing call form is
  `Option[Ordering].partial_cmp_by(...)` per design.md). Not blocking
  for slice 2 of the method-resolution CR (which scopes to
  receiver-form via item 8 only).

  **Slice plan (drafted 2026-05-08).** Two slices; Slice A is pure
  refactor with zero behavioral change, Slice B lands the feature.

  - [x] **Slice A — AST extension + mechanical fixup.** Pure
    refactoring; zero behavioral change.
    - *Goal.* `ExprKind::Path` carries
      `generic_args: Option<Vec<TypeExpr>>` (always `None` after this
      slice — populated by Slice B's parser rule). Every destructure
      site updated to the new pattern syntax.
    - *Sub-steps.* (1) Change `ExprKind::Path(Vec<String>)` to
      `ExprKind::Path { segments: Vec<String>, generic_args: Option<Vec<TypeExpr>> }`
      in `src/ast.rs`. (2) Compile-driven fixup across all 59 sites:
      ~4 construction sites populate `generic_args: None`; ~40
      destructure sites change `Path(segments)` → `Path { segments, .. }`;
      ~15 wildcards stay as-is. (3) Verify zero behavioral change:
      `cargo test`, `cargo test --features llvm`,
      `cargo clippy --all --tests -- -D warnings`,
      `cargo fmt --all -- --check` all clean.
    - *Tests.* No new tests in this slice — coverage comes from the
      existing test suite passing unchanged (~2293 non-LLVM, ~2495
      LLVM with `KARAC_SKIP_ASAN_TESTS=1`).
    - *Files touched (15).* `src/ast.rs`, `src/concurrency.rs`,
      `src/effectchecker.rs`, `src/unsafe_lint.rs`, `src/lowering.rs`,
      `src/cost_summary.rs`, `src/cfg.rs`, `src/use_classifier.rs`,
      `src/formatter.rs`, `src/ownership.rs`, `src/ffi_lint.rs`,
      `src/logical_lint.rs`, `src/provider_escape.rs`, `src/codegen.rs`,
      `src/resolver.rs`, `src/parser.rs` (construction sites only —
      no disambiguation rule yet), `src/interpreter.rs`, `src/cli.rs`,
      `src/typechecker.rs`. (Counting `src/ast.rs` separately; `src/parser.rs`
      counts construction-only.)
    - *Out of scope.* Parser disambiguation rule (Slice B).
      Typechecker propagation of `generic_args` (Slice B). Construction
      of `Some(...)` `generic_args` (Slice B).
    - *Stop triggers.* Test breakage that doesn't fix mechanically by
      adding `..` to a destructure pattern. Suggests a site uses
      `segments` content in a way that breaks under struct-shape —
      investigate before proceeding (likely indicates a site needs
      option (b)-style parallel handling, which would invalidate the
      design lock).

  - [x] **Slice B — Parser disambiguation + typechecker tail.**
    Behavioral change; lands concrete-type UFCS as a working surface.
    - *Goal.* `Vec[i64].new()` parses to `MethodCall { object: Path { segments: ["Vec"], generic_args: Some([i64]) }, method: "new", … }`
      and typechecks through the bound-discharge engine. Sub-item 5B
      under [`phase-4-interpreter.md` § method resolution](phase-4-interpreter.md)
      flips `[→]` → `[x]`.
    - *Sub-steps.* (1) **Parser disambiguation rule** in
      `src/parser.rs`: when a `TypeName[...]` (uppercase first
      segment, generic-args-shaped contents) is immediately followed
      by `.method(`, prefer the path-with-generic-args interpretation
      over `PrefixCollectionLiteral`. Pure syntactic lookahead; no
      name resolution. Balanced-bracket scanning handles nested
      generics like `Vec[Map[K, V]].new()`. (2) **Typechecker tail**
      (= sub-item 5B): extend `resolve_path_type` impl-walk in
      `src/typechecker.rs` to consume the populated `generic_args`
      and route through `find_method_with_args` +
      `impl_bounds_discharge` (slice 1 of method-resolution wired
      this engine but skipped this site for lack of receiver args —
      now provided by the parser).
    - *Tests.* (a) **Parser positives:** `Vec[i64].new()`,
      `HashMap[String, i32].default()`, `Self[T].method()`,
      `Vec[Map[K, V]].new()` (nested generics), `Vec[i64].new().push(1)`
      (chained method calls). (b) **Parser negatives — regression
      guards:** `[1]`, `[var]`, `[expr.field]`, `[some_call()]` still
      parse as collection literals. (c) **End-to-end typecheck:**
      `Vec[i64].new()` typechecks against `impl Vec[T]` and dispatches
      correctly. (d) **Bound-discharge:** trait-bounded UFCS
      (`Vec[i64].method_requiring_T_Display()`) discharges if i32
      impls Display, rejects if not. (e) **5B graduation:** flip
      `phase-4-interpreter.md` sub-item 5B from `[→]` to `[x]`.
    - *Files touched.* `src/parser.rs`, `src/typechecker.rs`,
      `tests/parser.rs`, `tests/typechecker.rs`,
      `docs/implementation_checklist/phase-4-interpreter.md` (5B
      checkbox flip).
    - *Out of scope.* Sub-item 5C (inherent-vs-trait priority on UFCS
      form) — separate slice once 5B lands. `Option[Ordering]`
      impl-table specialization (Theme 4 of wip-list2 — different
      concern: impl-table key shape, not parser).
      `[Vec[i64]]` (collection literal containing a typed path —
      would require recursive disambiguation, post-v1 polish).
    - *Stop triggers.* Disambiguation breaks an existing test that
      uses `[X]` shape in a position where path-with-generic-args is
      wrong (would require refining the rule). Pre-existing case-class
      disambiguation per design.md:374 conflicts with the new rule
      (would require co-design). Bound-discharge engine doesn't
      handle the new `target_args` shape (would require typechecker
      engine extension, larger than 5B).

  Once both slices land, **closes:**
  - `phase-2-parser-ast.md:33` parent (this entry).
  - `phase-4-interpreter.md` § method resolution sub-item 5B (`[→]` → `[x]`).
  - The parent item 5 (`[~]`) graduates to `[x]` once 5C also lands
    (still open after this CR).

