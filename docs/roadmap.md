# Kāra Compiler Roadmap

Development plan for the `karac` compiler, aligned with [design.md](docs/design.md).

## Core Strategy

1. **Effect types + auto-concurrency first** — the differentiating features, everything else composes on top.
2. **LLVM codegen is the single execution backend.** The tree-walk interpreter (Phase 4) served its original purpose as a semantic-validation step before codegen (Phase 4 → Phase 7) and is retained as a *dev/debug tool* — useful for stepping through compiler internals, validating semantics during compiler work, and any future reflection-style introspection. It is **not** the runtime backend for user-facing `karac repl` / `karac test` workflows. Those workflows use **always-JIT** via LLJIT (`llvm-sys::orc2`): every function lazy-compiled on first call, fast on subsequent calls. Single execution model across `karac build`, `karac repl`, and `karac test` — no semantic divergence between interactive and compiled execution. (Locked 2026-05-05; see brainstorming archive for the alternatives considered and the principle-driven argument for "always-JIT over hybrid tree-walk + JIT.")
3. **Incremental phases** — each phase produces a working compiler for a growing subset of the language.
4. **Diagnostics are incremental** — structured error output is built alongside each phase, not deferred. Every compiler feature ships with its diagnostics. JSON output format exists from the first error the compiler can report.
5. **North star: self-hosting** — the Kāra compiler should eventually be written in Kāra.
6. **Interactive is first-class, not an afterthought.** REPL and Jupyter kernel are positioned as differentiators, not convenience tooling — Kāra is one of few systems-grade statically-typed languages whose interactive surface runs at compiled-binary speed (lazy LLJIT amortizes the ~100 ms cold-compile across cell lifetime; subsequent calls are native code). The advantage over `evcxr`-style recompile-per-cell or JShell's JVM startup tax is **execution model parity** — REPL cells exhibit the same effect / ownership / perf behavior as `karac build` artifacts. Trivial REPL cells (`let x = 1+1`) cost ~100 ms instead of <1 ms — but the cost is *expected and uniform*, never a mystery slowdown; honest framing for users: "REPL has built-in compile latency by design."

---

## Contents

- [Phase 0: Proof of Value](#phase-0-proof-of-value--complete) — COMPLETE
- [Phase 1: Lexer](#phase-1-lexer--complete) — COMPLETE
- [Phase 2: Parser & AST](#phase-2-parser--ast--complete) — COMPLETE
- [Phase 3: Semantic Analysis](#phase-3-semantic-analysis--complete) — COMPLETE
- [Phase 4: Tree-Walk Interpreter](#phase-4-tree-walk-interpreter--complete) — COMPLETE
- [Phase 5: Compiler Query API & Tooling](#phase-5-compiler-query-api--tooling--complete) — COMPLETE
- [Phase 6: Auto-Concurrency Runtime](#phase-6-auto-concurrency-runtime) — COMPLETE (6.1 + 6.2)
- [Phase 7: LLVM Code Generation](#phase-7-llvm-code-generation) — COMPLETE
  - [Phase 7.1: Core Code Generation](#phase-71-core-code-generation--complete) — COMPLETE
  - [Phase 7.2: Compiled Stdlib Types + Layout Codegen](#phase-72-compiled-stdlib-types--layout-codegen--complete) — COMPLETE
- [Phase 8: Standard Library — Floor](#phase-8-standard-library--floor)
- [Interactive Development: REPL + Browser Playground (P0, v1) + Jupyter Kernel (P1, v1.1)](#interactive-development-repl--browser-playground-p0--jupyter-kernel-p1-delivery)
- [Package Manager v1.1: Build & Dependency Tooling](#package-manager-v11-build--dependency-tooling)
- [Phase 9: Gradual Verification Enforcement](#phase-9-gradual-verification-enforcement)
- [Phase 10: Additional Compilation Targets](#phase-10-additional-compilation-targets)
- [Phase 11: Standard Library — Long-Tail](#phase-11-standard-library--long-tail)
- [Phase 12: Self-Hosting](#phase-12-self-hosting)
- [Future: Gradual Verification](#future-gradual-verification-feature-6)
- [Future: Comptime](#future-comptime-compile-time-code-execution)
- [Future: Language Server and Reactive Query-Based Compilation](#future-language-server-and-reactive-query-based-compilation)
- [Resolved Design Primitives](#resolved-design-primitives)
- [Phase Dependency Graph](#phase-dependency-graph)

---

## Phase 0: Proof of Value — COMPLETE

**Goal:** Demonstrate *why Kāra exists* before the compiler is fully built.

Build a hand-compiled demo that shows the effect system + auto-concurrency story end-to-end:

- [x] **Write a Kāra program** (~50 lines) that fetches data from three independent sources, processes it, and writes results — using effect annotations
- [x] **Hand-translate to Rust** showing: (a) the sequential version, (b) the auto-parallelized version the Kāra compiler would generate
- [x] **Benchmark both** — show the speedup from auto-concurrency with zero programmer effort
- [x] **Show the compiler output** — mock the structured JSON diagnostics, concurrency report, and effect query output

This is a *pitch artifact*, not a compiler. It validates the thesis concretely: "here's what the compiler does for you, and here's the output it gives you."

**Done when:** The demo can be presented in under 5 minutes and clearly shows: (1) the programmer writes sequential-looking code with effects, (2) the compiler parallelizes it, (3) the diagnostics explain what happened and why.

---

## Phase 1: Lexer — COMPLETE

**Goal:** Tokenize Kāra source code with the current keyword set.

- [x] All current keywords (struct, enum, trait, impl, effect, reads, writes, etc.)
- [x] All symbols (=>, .., ..=, ?, #, etc.). `&&`/`||`/`!` retained in the lexer for deprecation diagnostics — user code uses `and`/`or`/`not`.
- [x] Span tracking (line, column, byte_offset) on every token
- [x] Numeric literals: hex, binary, octal, underscore separators
- [x] Block comments with nesting
- [x] Character literals: `'a'`, escape sequences (`\n`, `\t`, `\r`, `\\`, `\'`, `\0`), unicode escapes (`\u{1F600}`)
- [x] String interpolation: `f"..."` with `{expr}` blocks and nested brace tracking
- [x] Multi-line strings: `"""..."""`
- [x] Reserved keywords: `where` (gradual verification), `dyn` (trait objects)
- [x] Pipe operator token: `|>` as a distinct token (not `|` followed by `>`)
- [x] 35 integration tests covering all constructs

**Done when:** Every token defined in design.md is recognized, with source location tracking. All tests pass.

---

## Phase 2: Parser & AST — COMPLETE

**Goal:** Recursive-descent parser producing an AST for the core language.

### 2.1: Expressions and Statements
- [x] Literals: integers, floats, strings, booleans, characters
- [x] Operators: arithmetic, comparison, logical, bitwise
- [x] Variable bindings: `let` (immutable), `let mut` (mutable)
- [x] Assignment: `=` for `let mut` bindings
- [x] Blocks: `{ ... }` as expressions
- [x] If/else: `if condition { ... } else { ... }`
- [x] While loops: `while condition { ... }`
- [x] While let: `while let Pattern = expr { ... }`
- [x] For loops: `for item in collection { ... }`
- [x] Loop: `loop { ... }` with `break` / `break value` / `continue`
- [x] Return: `return expr` and implicit returns
- [x] `?` operator: postfix error propagation
- [x] Cast expressions: `expr as Type`
- [x] Tuple access: `tuple.0`, `tuple.1`

### 2.2: Functions and Types
- [x] Function definitions: `fn name(params) -> ReturnType { body }`
- [x] Struct definitions: `struct Name { field: Type, ... }`
- [x] Enum definitions: `enum Name { Variant { fields }, ... }`
- [x] Impl blocks: `impl Type { fn method(self, ...) { ... } }`
- [x] Trait definitions: `trait Name { fn method(self, ...) -> T; }`
- [x] Trait implementations: `impl Trait for Type { ... }`
- [x] Generics: `fn name[T: Bound](x: T)`, `struct Name[T] { ... }`
- [x] `where` clauses: parse after return type/effects/contracts on `fn`, `impl`, `struct`, `enum`, `trait`
- [x] Distinct type declarations: `distinct type Name = BaseType` with optional `where` constraint and `#[derive]`
- [x] Refinement type declarations: `type Name = BaseType where constraint` — numeric comparisons, `len()`, boolean combinators
- [x] Contracts: `requires`/`ensures` clauses on functions; `invariant` on struct bodies
- [x] Default parameter values: `param: Type = expr` syntax; trailing-only rule
- [x] Destructuring in function/closure parameters: `fn add((a, b): (i64, i64))`, struct destructuring

### 2.3: Effects Syntax
- [x] Effect resource declarations: `effect resource UserDB: DatabaseProvider;`
- [x] Effect annotations on functions: `fn f() with reads(UserDB) writes(OrderDB) { ... }`
- [x] Effect groups: `effect group Name = reads(X) + writes(Y);`
- [x] `with` keyword: `pub fn f() with OrderProcessing { ... }`
- [x] `with _` (effect polymorphism): on closures and trait methods
- [x] `transparent` effect modifier: `transparent effect verb traces;`
- [x] Parameterized resources: `effect resource UserDB[user_id: u64];`

### 2.4: Ownership Syntax
- [x] `ref` keyword: `fn f(s: ref String) -> ref String`
- [x] Multi-ref borrow returns: `fn f(a: ref String, b: ref String) -> ref String` (compiler uses conservative overapproximation — return borrows from all `ref` params)
- [x] `mut ref`: `fn f(s: mut ref String)`
- [x] `weak` annotation: `struct Child { parent: weak Parent }`
- [x] `shared struct` and `shared enum`: reference-semantics types with RC

### 2.5: Modules and Visibility
- [x] Imports: `import path.to.Item;` (brace-grouped multi-item, renames, `pub import` re-exports)
- [x] Visibility: `pub` keyword
- [x] Fully qualified paths: `module.Type`
- [n/a] Module declarations: design.md v41 resolved that modules come from the directory tree — no `mod` declarations. Parser rejects `mod name;` with a diagnostic.

### 2.6: Other Syntax
- [x] Match expressions: exhaustive pattern matching with `=>`
- [x] Named/labeled arguments: parse `label: expr` at call sites; store in `CallArg { label, value }` AST node
- [x] Pipe operator `|>`: parse as left-associative binary expression; `_` placeholder in argument position; lower precedence than call `()`, higher than `=`
- [x] `defer`/`errdefer`: parse as statements; `errdefer(e) { ... }` binding form; store cleanup list in AST scope node; no `?` inside block (compile error)
- [x] Subscript syntax: desugar `expr[expr]` into `Index.index` / `IndexMut.index_mut` call in AST; `expr[expr] = expr` assignment form routes to `IndexMut.index_mut`
- [x] Map literals: `["key": val, ...]` syntax; disambiguate from array literals by `:` after first expression
- [x] `seq {}` block: parse as expression; all statements execute in source order; auto-parallelism suppressed
- [x] Closures: `|params| expr` and `|params| { body }`
- [x] Tuples: `(a, b, c)`, destructuring
- [x] Array literals: `[1, 2, 3]`, empty `[]`, nested `[[1, 2], [3, 4]]`
- [x] Attributes: `#[no_rc]`, `#[rc_budget(max: N)]`, `#[concurrency(max_tasks: N)]`
- [x] Unsafe blocks: `unsafe { ... }`
- [x] FFI: `extern "C" fn name(...) effect_list;`
- [x] Layout blocks: `layout name: Collection[T] { group name { fields } }`
- [x] Constants: `const NAME: Type = value;`
- [x] Type aliases: `type Name = ExistingType;`
- [x] Comments: `//`, `/* */`, `///` (doc comments) — handled by lexer

### 2.7: AST Design
- [x] Span tracking: Every AST node carries source location
- [x] Canonicalization-ready: AST structure supports deterministic formatting
- [x] Parser tests: 101 tests validate correct AST construction for all constructs
- [x] Error recovery: Parser continues after errors to report multiple diagnostics

### 2.8: Diagnostics (built alongside parser)
- [x] Structured parse errors with source spans and context
- [x] Multiple error reporting (don't stop at first error)

**Done when:** Every syntactic construct in design.md parses into a well-typed AST. Parse errors include source locations and are validated by tests.

---

## Phase 3: Semantic Analysis — COMPLETE

**Goal:** Validate program correctness. Each sub-phase is independently shippable.

### 3.1: Name Resolution and Scoping
- [x] Symbol table: Track identifiers, types, and scopes
- [x] Module resolution: Resolve `use` imports and qualified paths
- [x] Visibility checking: Enforce `pub` vs private access
- [x] Diagnostics: "undefined variable", "private function accessed from outside module", with suggestions for typos

**Done when:** The compiler resolves all names, reports undefined/private access errors with source locations, and handles multi-module programs.

### 3.2: Type Checking
- [x] Basic type checking: Parameter types, return types, assignment compatibility
- [x] Generic type inference: Infer type parameters where unambiguous
- [x] Trait bound checking: Verify types satisfy trait constraints (includes `Eq` enforcement on `==`)
- [x] Pattern exhaustiveness: Verify `match` covers all enum variants
- [x] Pattern guards: `if EXPR` guard verified as `bool`; guarded arms excluded from exhaustiveness
- [x] `todo()`/`unreachable()`: special-cased in `infer_call`; return `Type.Never`; validate 0-or-1 `str` arg
- [x] Struct/enum field access: Validate field names and types
- [x] Diagnostics: type mismatch errors with "expected X, found Y", missing match arms with suggested additions
- [x] Named/labeled arguments: resolve labels against parameter names; enforce declaration order; error on unknown labels, out-of-order labels, and non-contiguous partial labels
- [x] Pipe operator `|>`: desugar to function call in type-checker; resolve `_` placeholder to left-hand value; union effects of all stages; verify `?` applies to pipe output not whole chain
- [x] `defer`/`errdefer`: verify no `?` inside block; type-check cleanup expr; `errdefer(e)` binding typed as enclosing function's `Err` variant; cleanup effects contributed to enclosing function
- [x] Subscript trait resolution: resolve `Index[Idx, Output]` and `IndexMut[Idx, Output]` trait bounds; verify `ref` / `mut ref` return modes; infer `panics` effect for `index` calls; type-check `.get()` fallible alternative
- [x] Integration tests — pattern guards: guard type error, guarded exhaustiveness error, guard effects
- [x] Integration tests — `todo()`/`unreachable()`: expression position, `panics` effect propagation, runtime panic message format
- [x] `where` clause bound verification: resolve type constraints in generic contexts
- [x] Associated types: declaration in traits, binding in impls, projection syntax `T.Assoc`, equality constraints in `where`
- [x] `Copy` trait: auto-derive validation (all fields must be `Copy`), implicit copy insertion at own-inferred call sites
- [x] Default parameter values: verify pure constant expressions; enforce trailing-only and no cross-parameter references
- [x] Integration tests — named arguments: unknown label, out-of-order label, partial label, UFCS receiver label (error), closure outer vs inner label scoping

**Done when:** Type-incorrect programs are rejected with clear errors. Generic functions type-check correctly. `match` exhaustiveness is enforced. A non-trivial program (100+ lines) type-checks correctly.

### 3.3: Effect System
- [x] Effect inference for private functions: Trace call graph, infer effects from body
- [x] Effect verification for public functions: Check declared effects match inferred effects
- [x] Effect conflict detection: Build conflict table (reads/writes × same/different resources)
- [x] Effect group expansion: Expand named groups to individual effects
- [x] Transparent effect handling: Exclude transparent effects from conflict analysis
- [x] Parameterized resource analysis: Static distinguishability of resource parameters
- [x] Diagnostics: compiler-suggested effect annotations with fix diffs, "undeclared effect originates from..." tracing

**Done when:** Private function effects are correctly inferred across a 5+ function call chain. Public functions with incomplete effect declarations produce errors with exact fix diffs. The conflict table correctly identifies reads/reads as safe and reads/writes on the same resource as conflicting. `karac query effects` returns structured JSON for any function.

### 3.4: Ownership Analysis
- [x] Parameter mode inference: Analyze function body → own / ref / mut ref per parameter
- [x] Move checking: Track consumed values, prevent use-after-move
- [x] Borrow checking: Validate `ref` and `mut ref` usage
- [x] Cycle detection: Analyze type graph for ownership cycles, require `weak` on back-edges
- [x] Diagnostics: "value moved here, used again here" with restructuring suggestions, RC fallback notes with `#[allow(rc_fallback)]` suggestion, RC budget violations

**Done when:** Parameter modes are correctly inferred for a set of test functions covering own/ref/mut ref cases. Use-after-move is caught. RC fallback triggers with a visible performance note. `#[no_rc]` rejects functions that would need RC. `karac query ownership` returns structured JSON.

---

## Phase 4: Tree-Walk Interpreter — COMPLETE

**Goal:** Execute Kāra programs from the AST, validate language semantics without codegen complexity.

- [x] Expression evaluator: Arithmetic, comparison, boolean logic, string operations (scaffolded, needs tests)
- [x] Integer overflow trapping: Runtime error on overflow, `wrapping_add` etc. for explicit wrapping (scaffolded, needs tests)
- [x] String interpolation: `f"..."` desugaring and evaluation
- [x] Function/method execution: Call dispatch, UFCS
- [x] Closure execution: Capture semantics, `Fn` invocation
- [x] Pattern matching: Exhaustive match execution
- [x] Pattern guard evaluation: evaluate `if EXPR` guard, skip arm if false
- [x] Ownership simulation: Move semantics, borrow tracking at runtime (infrastructure added; enforcement deferred to Phase 7 codegen)
- [x] Effect tracking: Runtime effect tracking for validation
- [x] Standard library builtins: `print`, `println`, `read_line`, `read_file`, `write_file`, `env.args`, `env.get`, `env.set`, `exit`
- [x] `dbg()`: transparent debug printing (stderr, file/line/expression/value, stripped in release builds)
- [x] `Result`/`Option` and `?` operator (both types, matching error types only — cross-type conversion via `From` deferred to Phase 8), `?.` optional chaining, `??` nil coalescing
- [x] `defer`/`errdefer`: maintain a scope-exit cleanup stack; on normal exit run `defer` list LIFO; on `Err` exit run `errdefer` list first (LIFO), then `defer` list (LIFO)
- [x] `unwrap()` / `expect()` with `panics` effect tracking
- [x] `#[derive(Eq, Hash, Display, Clone)]`: Compiler-generated trait implementations (Eq at typechecker, Clone/Display implicit in interpreter)
- [x] Generic instantiation: Runtime monomorphization simulation (tree-walk interpreter is dynamically typed — generics work naturally)
- [x] E2E tests: `.kara` programs → interpreter → verify output

**Done when:** `karac run examples/word_count.kara` executes correctly and produces expected output. At least 10 end-to-end test programs covering: arithmetic, control flow, pattern matching, error handling with `?`, struct/enum usage, trait method dispatch, and effect-annotated I/O.

---

## Phase 5: Compiler Query API & Tooling — COMPLETE

**Goal:** Machine-friendly compiler interface for AI agents.

Note: Basic diagnostics (`--output=json`, source spans, error suggestions) are built incrementally in earlier phases. This phase adds the *query* interface and *formatter* — tools that expose the compiler's internal analysis programmatically.

### 5.1: Tooling (CLI, Formatter, Testing)

- [x] CLI: `karac build`, `karac run`, `karac check`, `karac fmt`, `karac query`
- [x] `karac check`: Type-check without executing (builds on Phase 3 analysis pipeline)
- [x] `karac fmt`: Canonical formatter with deterministic output
- [x] `--output=json` flag for machine-readable diagnostics across all phases
- [x] `--output=jsonl` streaming mode: newline-delimited JSON, six event types (`build_start`, `phase_start`, `phase_complete`, `phase_skipped`, `diagnostic`, `build_complete`). Strict superset of `--output=json` — collecting all events reconstructs the batch document
- [x] Phase boundary contract for streaming: `lex`, `parse`, `resolve`, `typecheck`, `effect`, `ownership` emit observable phase events in Phase 5. `concurrency` adds its events in Phase 6; `codegen` adds its events in Phase 7. Phase names are part of the public contract — renaming is a breaking change for AI clients
- [x] Fail-fast semantics under streaming: each phase runs unless its immediate predecessor produced zero usable output (predecessor-usable-output rule). Skipped phases emit `phase_skipped` with a `reason` string and a `blocking` array of diagnostic IDs from prior phases. Per-diagnostic `phase` field added to both streaming and batch modes (non-breaking)
- [x] Diagnostic snapshot tests: golden-file tests for error messages — freeze error output format and catch unintended regressions
- [x] Fuzz testing: `cargo-fuzz` harness for lexer and parser — catch panics, hangs, and crashes on malformed input
- [x] `karac query effects`: Return inferred + declared effects for a function
- [x] `karac query ownership`: Return parameter modes and RC values

### 5.2: Language Features

- [x] Labeled loops: `label: for/while/loop`, `break label`, `continue label`, `break label expr`
- [x] Named/labeled function arguments: `name: expr` at call sites, declaration-order enforcement, contiguity validation
- [x] `seq {}` block: suppress auto-parallelism, block expression semantics
- [ ] Const generics — full surface: `[T, const N: i64]` declarations; `i64` / `i8`–`i128` / `bool` / `char` / fieldless-`enum` permitted param types; const-expression instantiation (`Array[T, N + 1]`); const-expression bounds in `where` clauses (`where N >= 0`); call-site inference for const params in argument positions; explicit-only solving for return-type-only and bounds-only const params; checked-arithmetic evaluation at type-check time. Spec in `design.md` § Type Inference > *Const generic parameters*. Const-`fn` and user-code calls in const-arg position remain deferred to comptime.

### 5.3: Advanced Diagnostics

- [x] Error return traces: ring buffer (depth 64) at each `?` site; `"error_return_trace"` field in JSON output; pushed on Err/None, cleared on Ok/Some
- [x] Enhanced per-SCC effect diagnostics: Tarjan's SCC detection, full effect resolution trace in `"mutual_recursion_groups"` JSON field

**Done when:** An AI agent can: (1) compile a program and get structured JSON errors, (2) apply the suggested fix diffs, (3) query the compiler for effect/ownership decisions, (4) format code canonically so diffs are semantic-only, (5) consume a streaming build in real time via `--output=jsonl` and react to each phase's completion — or stop reading at the first failing phase — before the full build finishes. All query and diagnostic outputs are valid JSON matching a documented schema; the streaming mode is a strict superset of the batch mode.

---

## Phase 6: Auto-Concurrency Runtime — COMPLETE (6.1 + 6.2; 6.3 deferred to v1.1)

**Goal:** Compiler-driven parallel execution using effect analysis.

### 6.1: Concurrency Analysis — COMPLETE
- [x] Data dependency graph: Build dependency graph from variable usage
- [x] Effect conflict analysis: Identify non-conflicting effect sets for parallelization
- [x] Task granularity heuristics: `ParallelGroup.is_trivial` flag marks pure-computation groups; codegen can skip thread dispatch for trivial groups
- [x] Sync point insertion: Insert join points where data dependencies require it
- [x] `seq {}` block enforcement: suppress auto-parallelism within block; execute in source order
- [x] Parameterized-resource distinctness rules: conservative collapse — two parameterized resources are distinct only when their partition key is a distinct literal or a variable provably bound to different values. When distinctness is ambiguous, the compiler serializes (safe default). Documented in implementation_checklist/.
- [x] Concurrency report: `karac query concurrency`
- [x] `karac query concurrency`: Return parallelization decisions with reasoning (moved from Phase 5 — requires concurrency analysis)

**Done when:** Given a function with three independent `reads` on different resources, the compiler identifies them as parallelizable. `karac query concurrency` shows the parallelization decision with reasoning. Given a function with conflicting `writes` on the same resource, the compiler correctly serializes them.

### 6.2: v1 Runtime (Blocking I/O) — COMPLETE
- [x] Parallel execution: `par {}` block spawns concurrent branches via `std.thread.scope`
- [x] Structured cancellation: Cancel sibling tasks on first error via `AtomicBool` flag (fail-fast)
- [x] Sequential fallback: `karac run --sequential` disables parallel execution in par blocks
- [x] Zero-cost when unused: No thread spawning for programs without `par {}` blocks, or single-statement par blocks
- [x] Output ordering: branch outputs merged in source order (deterministic)
- [x] Work-stealing task scheduler: Fixed-size thread pool (min(branches, available_parallelism) workers); atomic work-distribution counter; no external dependencies
- [x] Cooperative cancellation: Branch functions accept cancel_flag parameter; cancel checked at branch start. Effect-boundary cooperative checks (mid-branch insertion) deferred to implementation_checklist/.
- [x] Completion wins cancellation: a branch already executing its body completes naturally (cancel check is at entry, not mid-body). Full effect-boundary granularity deferred.
- [x] Cascading cooperative cancellation: works at branch granularity (outer cancel observed at nested branch start). Mid-execution propagation into running nested pars deferred.
- [x] Scheduler minimum invariants: no lost work (atomic counter ensures every branch is picked up exactly once); cancel eventually observed (checked before each new branch pickup); termination guaranteed (workers exit when counter >= count or cancel set); deadlock-free (no locks in hot path, only atomic operations).
- [x] `collect_all`: deferred as language syntax feature — needs `collect_all { }` block syntax and runtime gather mode. Tracked in implementation_checklist/.

**Done when:** A benchmark program with three independent I/O calls runs ~3x faster with auto-concurrency than with `--sequential`. Cancellation works: if one branch fails, siblings are cancelled and the first error is returned. Pure programs have zero scheduling overhead (measured).

### 6.3: v1.1 Runtime (Network Event Loop)
- [ ] Event loop integration: epoll (Linux) / kqueue (macOS) for network I/O
- [ ] Effect-routed execution: Compiler routes `sends(Network)` / `receives(Network)` to event loop
- [ ] Task parking: Network I/O tasks park without blocking threads, resume on completion
- [ ] RAII-across-yield warnings: Warn when mutex guards or file handles span effect boundaries
- [ ] State machine transform: For network-boundary functions only (limited scope)

**Done when:** A simple TCP echo server handles 100K concurrent idle connections on a single machine without running out of threads. The same server code runs identically (correct output) under `--sequential` mode, just slower.

---

## Phase 7: LLVM Code Generation — COMPLETE

**Goal:** Compiled, high-performance output replacing the tree-walk interpreter.

Phase 7 splits into two sub-phases by the nature of the work:

- **7.1 Core Code Generation** — language-construct codegen: types, control flow, generics, closures, RC, FFI, par blocks. COMPLETE.
- **7.2 Compiled Stdlib Types + Layout Codegen** — codegen for `Array[T, N]`, `Vec[T]`, `String` (memory layout and minimum methods only; full API surface remains in Phase 8), plus layout codegen which is blocked on them.

**Rationale for 7.2.** The original roadmap deferred all stdlib types to Phase 8 ("operates against the minimal stdlib surface introduced as interpreter builtins in Phase 4"). Layout codegen revealed this boundary was imprecise: `layout entities: Vec[Entity] { ... }` targets a collection type that must be compiled — not an interpreter builtin — and layout codegen itself is codegen, unambiguously Phase 7 work. Splitting the phase keeps codegen-work in the codegen phase and leaves Phase 8 narrowly scoped to API completeness (full method sets, iterator traits, I/O wrappers, provider impls).

### Phase 7.1: Core Code Generation — COMPLETE


- [x] LLVM IR emission: Integrate `inkwell` crate (LLVM 18, `inkwell` 0.9, optional `llvm` feature)
- [x] Function codegen: Translate functions to LLVM IR (arithmetic, control flow, recursion, `main` → `i32`)
- [x] Struct/enum codegen: Structs as LLVM struct types, enums as tagged unions (`{ i64 tag, i64... }`)
- [x] `karac build`: wire CLI build command to LLVM codegen → object file → native executable via system linker
- [x] Generic monomorphization: Generate specialized code per concrete type
- [x] Effect polymorphism resolution: Resolve `with _` through monomorphization (moved from Phase 3.3 — requires monomorphization infrastructure)
- [x] Closure compilation: Function pointer + captured environment
- [x] RC codegen: Reference counting increment/decrement insertion
- [x] Shared types: `shared struct` reference semantics with RC (moved from Phase 4)
- [x] Shared enums: `shared enum` with same RC semantics (moved from Phase 4)
- [x] RC fallback detection (Phase 1): triggers 1 (branch-divergent re-use after consume) and 2 (closure capture + outer use). Trigger 3 (container store + later use) deferred — see implementation_checklist/.
- [x] RC budget enforcement: `#[no_rc]` per-function, `@no_rc` per-type, `#[allow(rc_fallback)]` to suppress notes. `#[rc_budget(max: N)]` module-level deferred — needs module-level attribute parsing.
- [x] Rc vs Arc (Phase 2): per-function pass that promotes any Rc binding whose use-site lies inside a `par {}` block to Arc. Conservative live-range overlap; one decision per value.
- [x] Effect-based parallelism codegen (MVP): explicit `par {}` blocks lower to a `karac_par_run` call into the bundled runtime library (`libkarac_runtime.a`, statically linked). Branches run on one OS thread each; join before scope exit; captures by value. Cancellation, error propagation, and auto-parallelization of non-`par` regions deferred — tracked in implementation_checklist/ Phase 7. See [design.md § Runtime Distribution](design.md#runtime-distribution).
- [x] FFI codegen: C-compatible ABI for `extern` functions
- [x] Unsafe blocks: Disable ownership/bounds checking within unsafe

### Phase 7.2: Compiled Stdlib Types + Layout Codegen — COMPLETE

Scope: memory layouts and the minimum method set needed to exercise those layouts and make layout codegen possible. Full method sets (`map`, `filter`, `fold`, `retain`, `split`, string formatting, etc.) remain in Phase 8.

- [x] `Array[T, N]` codegen: fixed-size array as LLVM `[N x T]`; construction via literals, indexing (`a[i]`) with bounds check, `.len()` (compile-time constant fold), `for` iteration. Zero-init constructor (`new` / `[0; N]` repeat) deferred.
- [x] `Vec[T]` codegen: heap-allocated `{ ptr data, i64 len, i64 capacity }`; `Vec.new()`, `push` (with 2x/floor-4 growth), `pop`, `.len()`, indexing `v[i]` with bounds check, `for` iteration, scope-exit buffer free
- [x] `String` codegen: heap-allocated UTF-8 buffer `{ ptr, i64 len, i64 cap }`; `String.new()`, `.len()`, `push_str`, string literals as `{ global_ptr, strlen, 0 }` (cap=0 = static). Concatenation via `Add` and comparison deferred to Phase 8.
- [x] Drop/RC integration: owned Vec/String go out of scope → free the backing buffer (cap > 0 check skips static string data). `shared struct` RC interaction unchanged.
- [x] Move and ref semantics for collection parameters: `fn f(v: Vec[T])` moves (pass by value); `fn f(v: ref Vec[T])` borrows (pass pointer, callee reads through indirection). Works for Vec, Array, String. `mut ref` type-lowered but method mutation not yet wired.
- [x] Layout validation pass: resolver links `LayoutDef` → `StructDef`, validates field existence, rejects duplicates across groups, warns on unassigned fields, validates `split_by_variant` only on enums
- [x] Layout codegen: SoA (struct-of-arrays) physical representation. Layout-annotated `Vec[T]` compiles to `{ ptr_g0, ptr_g1, ..., i64 len, i64 cap }` with one heap allocation per group. Push decomposes struct into group fields; growth reallocates each group independently. SoA field-access translation (`entities[i].position` → group-indexed load) and iteration are future follow-ups.

**Done when:** `karac build examples/word_count.kara` produces a native binary that runs correctly. A layout-annotated program compiles to SoA memory and `karac query layout` (or equivalent introspection) shows the physical grouping. The binary is within 2x performance of equivalent hand-written Rust for a compute-bound benchmark (the gap is IR emission quality — naive emission without inlining hints, `noalias`/`nsw`/`nuw` flags, or LTO — not a fundamental limit; performance parity is the Phase 11 codegen-optimization goal). Auto-concurrency works in compiled output (not just interpreter).

---

## Phase 8: Standard Library — Floor

**Goal:** Floor standard library — the surface every non-trivial program needs regardless of domain. Full method sets for collections and core types, complete trait surface (operators, conversions, iterators, hash, ordering), I/O wrappers, providers, and auto-concurrency codegen, plus the universal modules `std.json`, `std.time`, `std.path`, `std.error`, `std.mem`, `std.bytes`, `std.cmp`, `std.hash`. Built on top of the compiled types from Phase 7.2.

Note: Core stdlib types (`Option`, `Result`, `Vec`, `String`, `Array[T, N]`) are introduced as interpreter builtins in Phase 4 with minimal APIs, and their codegen (memory layout + minimum method set) ships in Phase 7.2. Phase 8 adds the full method sets, plus the remaining collections, iterator traits, operator traits, I/O with effect annotations, error conversion (`From` trait for `?`), and provider implementations. **This phase owns the floor API surface, not type codegen and not domain-specific stacks.**

**Scope boundary:** Domain-specific stdlib (numerical/data-science, security, embedded primitives, scripting-critical helpers like `std.regex`/`std.http`/`std.process`, codegen IR optimization pass) ships later in [Phase 11: Standard Library — Long-Tail](#phase-11-standard-library--long-tail). The split lets v1 ship semantically locked (Phase 9) and target-complete (Phase 10) before the long-tail stdlib lands; full v1 release is at the end of Phase 11.

### Collections (Full APIs)

> **v1 design property — collections monomorphize.** `Vec[T]`, `Map[K, V]`, `Set[T]`, and future v1 collections (e.g., `BTreeMap[K, V]`) emit one specialized implementation per concrete type tuple at codegen, like Rust's `std::collections::HashMap[K, V]`. The original v0 design used a type-erased C runtime (function-pointer dispatch on hash/eq, byte-blob storage); v1 shifts to monomorphized source compiled per user crate. Eliminates indirect-call tax on hot collection operations (~25% of the Karac-vs-std hash_map gap measured 2026-05-06); restores LLVM's optimizer reach into collection internals. `libkarac_runtime.a` shrinks accordingly to non-monomorphizable primitives. See [`design.md § Generics and Monomorphization Strategy`](design.md#generics-and-monomorphization-strategy) for the design lock; trait-bounds-at-codegen enforcement is a P0 prerequisite (currently parsed/validated but not enforced — see `implementation_checklist/phase-7-codegen.md`).

- [ ] `Vec[T]` — full method set on top of Phase 7.2 codegen: `map`, `filter`, `fold`, `retain`, `sort`, `reverse`, `extend`, `concat`, iterator impls. Monomorphized per concrete `T`.
- [ ] `Map[K, V]` — codegen + full API: hash table representation, `insert`, `get`, `remove`, `contains_key`, iteration. Monomorphized per concrete `(K, V)` tuple — direct hash/eq calls, full inlining, no function-pointer indirection.
- [ ] `Set[T]` — codegen + full API: unique-value container built on `Map` infrastructure. Monomorphized per concrete `T`.
- [ ] `String` — full method set on top of Phase 7.2 codegen: `split`, `replace`, `trim`, `to_uppercase`, `chars()`, format specifiers, etc.
- [ ] `StringSlice` — borrowed view into a `String` (pointer + offset + length); zero-copy parsing/splitting
- [ ] `InternedString` — deduplicated handle via global intern table; O(1) equality
- [x] `Slice[T]` — full read-only + in-place method surface: `len`, `is_empty`, `first`, `last`, `get(i) -> Option[ref T]`, `contains`, `binary_search`, `chunks(n)`, `windows(n)`, `split_at(i)`, `sort`, `sort_by(cmp)`, `reverse`, `fill`, `swap(i, j)`. Typechecker `infer_slice_method` handles `Type::Slice { element, mutable }` dispatch; interpreter `eval_method_call` pattern-matches `Value::Array` for each arm with fallthrough for non-Array objects. `value_compare` free function added since `Value` does not implement `Ord`. 14 typechecker tests + 14 interpreter tests added.

### Core Types
- [ ] `Option[T]` — nullable values (enum)
- [ ] `Result[T, E]` — error handling (enum)
- [ ] `ref_eq(a, b)` — reference identity comparison for `shared` types (free function, returns `bool`)

### Operator Traits
> **Slice 1 shipped** (commit `1c8cb26`): trait registration + impl-table infrastructure (`env.impls_by_trait`, `find_impl`, `find_from_impl`); ~150 built-in stdlib impls (arithmetic, bitwise, Eq/Ord, String Add); arithmetic + `Neg` lowered through `src/lowering.rs`; resolver restriction on user operator-trait impls; `From[T]` dispatch + 19 numeric widening impls; `?` cross-error conversion via typechecker side-table.
>
> **Slice 2 shipped:** operator lowering extended to equality (`==`/`!=`), comparison (`<`/`<=`/`>`/`>=`), bitwise binary (`&`/`|`/`^`/`<<`/`>>`), and unary `~` (bitwise not) plus `not` (logical not) — all route through `Call(Path([T, method]))` and the interpreter/codegen fast-paths. Short-circuit `and`/`or` and range `..`/`..=` stay as `Binary` deliberately. v1 comparison shortcut: `<` lowers to `T.lt` directly (bool-returning) instead of `Ord.cmp(...).is_lt()` — the `Ordering`-detour form lands alongside Ord derivation. Eq/Ord impls register `ne`/`lt`/`le`/`gt`/`ge` as callable methods for API symmetry with `add`/`sub`; type-receiver method calls on primitives (`i32.lt(a, b)` etc.) route through the same fast-path as the lowered form. User-defined `impl Eq for MyStruct` / `impl Ord for MyStruct` are now accepted by the resolver and drive `==`/`<` dispatch through the lowering pass (`TypeCheckResult.trait_impls` exposes the registered (trait, target) set). `!=` on user types desugars to `not T.eq(a, b)` — user Eq impls only need to provide `eq`. Codegen gained a user impl-block pass: each method becomes an LLVM function named `Type.method`, and both `Call(Path([T, m]))` and receiver-form `obj.method(args)` route through it — so user-type operator dispatch works end-to-end through LLVM, not just the interpreter.

- [x] `Add`, `Sub`, `Mul`, `Div`, `Rem`, `Neg` — arithmetic (`a + b` lowers to `Add.add(a, b)` in the lowering phase, after type checking). Homogeneous in v1 — `fn add(self, rhs: Self) -> Self`, no associated `Output`, no heterogeneous `Rhs`. Typed-variable-to-typed-variable mixes require explicit `as` cast (`i32 + i64` with both operands typed is an error); literal-involved promotion is permitted (`arr + 1`, `x: i32; x + 5` — the literal takes the typed operand's type). *(Slice 1: numeric primitives + String Add lowered; effect tracking for String Add's `allocates(Heap)` pending. Literal promotion lands with Phase 11 numerical stdlib.)*
- [x] `Eq`, `Ord` — `a == b` lowers to `T.eq(a, b)`; `a != b` to `T.ne(a, b)`. For comparison, v1 takes a shortcut: `a < b`/`a <= b`/`a > b`/`a >= b` lower directly to `T.lt`/`T.le`/`T.gt`/`T.ge` (bool-returning), sidestepping the `Ord.cmp(a, b).is_lt()` detour through `Ordering`. The `Ordering`-detour form remains viable and lands alongside user-type Ord support. `Ord` as a derivable trait (generating lexicographic field-order comparison) is a separate follow-up. *(Slice 2: interpreter `dispatch_lowered_op` + codegen `compile_assoc_call` extended with method-name maps.)*
- [x] `BitAnd`, `BitOr`, `BitXor`, `Shl`, `Shr`, `Not` — bitwise operators on integer primitives and `bool`. `and`/`or` stay as distinct short-circuit keywords (not trait-dispatched) — their semantics can't be faithfully expressed as a strict method call. *(Slice 2: `~int` → `T.not`, `not bool` → `bool.not`; runtime value disambiguates `UnaryOp::Not` vs `BitNot` in interpreter, `type_name == "bool"` disambiguates in codegen.)*
- [ ] `Index[Idx]` / `IndexMut[Idx]` with associated `type Output` — indexing operator (`a[i]`, `a[i] = v`); `Pool[T]` implements `Index[Handle[T]]` with `Output = T`; range indexing (`a[lo..hi]`) via separate `Index[Range[i64]]` impl with `Output = Slice[T]`. *(Trait names registered in slice 1; no impls yet.)*
- [ ] `Display` — string conversion for `f"..."` interpolation (`to_string()`). *(Trait name registered in slice 1; no impls, f-string interp not yet dispatched through it.)*
- [ ] Stdlib implementations: numeric primitives (`i8..i64`, `u8..u64`, `usize`, `isize`) for all arithmetic/comparison/bitwise traits; `f32`/`f64` for arithmetic and `PartialEq`/`PartialOrd` only (no `Eq`/`Ord`/`Hash` — IEEE NaN); `F32`/`F64` total-order wrappers implementing `Eq`/`Ord`/`Hash` with NaN sorting last; `String` for `Add` (heap concatenation, `allocates(Heap)`); `String`/`StringSlice`/`InternedString` for `Eq`/`Ord`; `Vec[T]`, `Option[T]`, `Result[T, E]`, tuples for `Eq`/`Ord` under the obvious conditional bounds. *(Slice 1: primitives + String + F32/F64 registered; `PartialEq`/`PartialOrd` for `f32`/`f64`, `StringSlice`/`InternedString`, generic `Vec`/`Option`/`Result`/tuples pending.)*
- [x] Compound assignment (`+=`, `-=`, `*=`, `/=`, `%=`, `&=`, `|=`, `^=`, `<<=`, `>>=`) desugars to `a = a op b` in v1 — no separate `AddAssign` etc. traits. Deferred additive extension.
- [x] **Resolver restriction:** user-defined `impl Add for MyType` (and peers) rejected in v1 with a clear diagnostic pointing at the stdlib trait. Restriction is a one-line feature flag; lifting it is a non-breaking additive change. Associated `Output` + heterogeneous `Rhs` land alongside the lift for mixed-type arithmetic (SIMD, decimal, duration).
- [x] **No `impl Add for Vec[T]`.** `vec1 + vec2` is a compile error; diagnostic points at `.concat(other)` or `.extend(other)`. Ambiguity between concatenation and elementwise addition is deliberate.

### Conversion Traits
- [ ] `From[T]` / `Into[T]` — infallible conversions; blanket impl derives `Into` from `From`. *(Slice 1: `T.from(x)` dispatch shipped via source-typed lookup; user `impl From` resolves and runs. Slice 3a: `.into()` with expected-type threading at let-annotation, let-else-annotation, assignment, call-arg, return, and function-body-final positions — rewritten to `Target.from(x)` by the lowering pass via `TypeCheckResult.into_conversions`. Slice 3b: resolver rejects user `impl Into` / `impl TryInto` with a suggestion to implement `From` / `TryFrom` instead.)*
- [ ] `TryFrom[T]` / `TryInto[T]` — fallible conversions with associated `Error` type. *(Trait names registered in slice 1; no impls or dispatch yet.)*
- [ ] `?` cross-error-type propagation via `From` impl chain. *(Slice 1: typechecker validates, `TypeCheckResult.question_conversions` side-table records target err type, interpreter calls `<Target>.from(e)` at propagation. Codegen `?` deferred — requires Result/Option as built-in enums in codegen.)*
- [ ] Standard impls: numeric widening/narrowing, `String` from literals, `Option`/`Result` wrapping. *(Slice 1: numeric widening table shipped (19 impls: signed→signed, unsigned→unsigned, unsigned→wider-signed, f32→f64). Narrowing (needs TryFrom), `String` from literals, `Option`/`Result` wrapping pending.)*

### Associated Types
- [ ] Associated type declarations in traits (`type Item`) and binding in impls (`type Item = i64`)
- [ ] Projection syntax (`I.Item`) in type position and `where` clauses
- [ ] Equality constraints in `where` clauses (`where I.Item = i64`)

### Iterator Traits
- [ ] `trait Iterator { type Item; fn next(mut ref self) -> Option[Self.Item] }` — core iteration protocol using associated types
- [ ] `trait Iterable { type Item; fn iter(ref self) -> impl Iterator[Item = Self.Item] }` — collection protocol
- [ ] `filter`, `map`, `collect`, `fold`, `any`, `all` — standard iterator combinators
- [ ] Implementations for `Vec[T]`, `Map[K, V]`, `Set[T]`, ranges

### Auto-Concurrency Codegen
- [ ] **Auto-parallelization of non-`par` regions.** The concurrency analysis already identifies parallelizable statement groups outside explicit `par {}` blocks (`ConcurrencyAnalysis.function_decisions`), but codegen currently ignores them and emits those groups sequentially. Wire codegen to honor `parallel_groups` on non-`par` blocks: for each group of two or more statements the analysis marks parallel, emit the same `karac_par_run` call path as explicit `par {}`. Requires threading `ConcurrencyAnalysis` into `Codegen` (not currently passed to `compile_to_object`). Guard with the Phase 6.1 granularity heuristic — don't spawn threads for trivial pure statements. This is the feature that makes the "write sequential code, compiler parallelizes it" story true in compiled binaries.

- [ ] **Debugger Contract — runtime metadata emission.** Co-developed with auto-concurrency codegen because this is the moment the runtime first emits `par`/`suspend` code; the contract has to be in place or it gets locked in by accident. Four runtime structures required, per design.md § AI-First Compiler Interface > Debugger Contract: (1) static `SpawnSiteId` (`u32`) per `par {}` block, embedded in the executable's metadata table; (2) parent-frame reference field on every worker frame produced by `par`/`spawn`/`TaskGroup`, with a `"root"` sentinel for the root task; (3) await-chain pointer on every suspended task pointing to its `WaitTarget` (peer task or typed I/O handle); (4) `std.runtime::list_tasks()` and `std.runtime::list_par_blocks()` enumeration functions plus `std.runtime::has_debug_metadata() -> bool` for runtime detection. Profile-gated: default-on for `[profile.dev]`, default-off for `[profile.release]`; controlled via `runtime_debug_metadata = true|false` in the active profile. Embedded/`isr` profiles default-off (incompatible with `panics_off` / `default_no_alloc`). The metadata is part of the language-level contract and stable within a major version.

### Performance Primitives
- [ ] `Arena[T]` — arena allocation for cache-friendly bulk allocation (stdlib, not language feature)
- [ ] `ArenaRef[T]` — non-owning index into an arena

### I/O (with effect annotations)

> **Interpreter MVP shipped (Phase 8 slice 1):** `IoError` enum registered in prelude + typechecker; `Stdin`, `Stdout`, `Stderr`, `FileSystem` added to `PRELUDE_EFFECT_RESOURCES`; interpreter builtins: `Stdin.read_line()` / `Stdin.read_to_string()` → `Result[String, IoError]`; `Stdout.flush()` / `Stderr.flush()` → `Unit`; `FileSystem.read_to_string(path)` / `FileSystem.write(path, contents)`. `env.args()` → `Vec[String]` and `env.var(name)` → `Result[String, VarError]` shipped; resolver registers lowercase `env` as a module alias. 5 typechecker tests + 4 interpreter tests added. Open: codegen path, `File` handle type, `BufReader`, `env.set`, `impl From[VarError] for IoError`.

- [ ] File I/O: `read_file`, `write_file` — `reads(FileSystem)` / `writes(FileSystem)` *(partial: `FileSystem.read_to_string` / `FileSystem.write` done in interpreter MVP above)*
- [ ] Console: `print`, `println` — `writes(Stdout)`; `eprintln` — `writes(Stderr)`; `io.read_line`, `io.read_to_string` — `reads(Stdin)` *(partial: `Stdin.read_line` / `Stdin.read_to_string` / `Stdout.flush` / `Stderr.flush` done in interpreter MVP)*
- [ ] Network: TCP/UDP primitives — `sends(Network)` / `receives(Network)`
- [ ] Environment: `env.args`, `env.var(name)`, `env.set` — `reads(Env)` / `writes(Env)` *(partial: `env.args` + `env.var` done; `env.set` open)*
- [ ] Clock: `now` — `reads(Clock)`
- [ ] Random: `random` — `reads(RandomSource)`

### String Operations
- [ ] Concatenation, length, slicing, search, replace, split, join, formatting

### Math
- [ ] Integer and float math, constants, bitwise operations

### Provider Implementations
- [ ] `with_provider[R]` — trait-based effect injection
- [ ] In-memory test providers for standard resources

### Logging (`std.log`)
- [ ] `log.debug`, `log.info`, `log.warn`, `log.error` — structured logging with severity levels
- [ ] Uses `transparent effect verb traces;` and `traces(Logger)` — never propagates, never affects concurrency
- [ ] Configurable output destination (stderr, file, custom sink via provider trait)

### Diagnostics — `std.panic` and `std.runtime`

- [ ] **`std.panic` — crash report writer.** Implements the wire format specified in design.md § AI-First Compiler Interface > Crash Report Format. Eight required structured-JSON fields: panic site, panic kind discriminant, message, logical stack (per-block for `par`, per-task for `suspends`), provider stack, RC-fallback annotations, parallel context, build metadata. Output discipline: stderr 5–10 line summary + crash file path; default path `/tmp/kara-crash-{pid}-{timestamp}.json` (Unix) / `%TEMP%\...` (Windows); `KARA_CRASH_DIR` env var override; empty `KARA_CRASH_DIR` suppresses file output. Edge cases: panic-during-panic-report (fall back to abort + minimal stderr line, no loop), drop-time panic (capture as `panic_kind: "drop_during_unwind"` with `caused_by` preserving the original triggering panic), concurrent panics (each task writes its own file with cross-references in `concurrent_with`), embedded `panics_off` (panic-report path compiled out — zero overhead). Override hook follows Rust `set_hook` precedent.

- [ ] **`std.runtime` — runtime introspection (Debugger Contract surface).** Companion to `std.panic`; co-developed because they share metadata sources. Exposes the four Debugger Contract elements as a Kāra-callable API: `list_tasks() -> Vec[TaskInfo]` (every suspended task with `WaitTarget`, source location, effect summary), `list_par_blocks() -> Vec[ParBlockInfo]` (every active `par {}` block with `SpawnSiteId`, worker count, per-worker source location), `has_debug_metadata() -> bool` (profile-gated). Both list functions return empty when the binary was built without `runtime_debug_metadata = true` — generic tooling can try-then-degrade. WASM target replaces filesystem-backed crash files with a JS-side handler hook (`window.karac_crash` default, configurable via `KARA_CRASH_HANDLER` import); GPU panics surface as host-side panics at the kernel-launch site with `panic_kind: "gpu_kernel_failed"` and a `gpu_marker` field. Full GPU stack reconstruction is post-v1; WASM/GPU adaptations land in Phase 10 alongside the respective backends.

### Script mode
- [ ] Files without `fn main` synthesize `fn main() -> Result[Unit, Error]` wrapping top-level statements. Aligns with v34 REPL cell-as-main-body model.

### `std.json`
- [ ] `Json` enum + parse/stringify — universal config/API surface; every CLI / service / data-pipeline program needs it. Typed `(de)serialization` lands in v1.5.

### `std.time`
- [ ] `Duration` and `Instant` types; arithmetic (`Instant - Instant -> Duration`, `Instant + Duration -> Instant`); ISO 8601 parse/format. `Clock` resource provides the source via `reads(Clock)`.
- [ ] `std.time.sleep` — `blocks` execution verb.

### `std.path`
- [ ] `Path` type — separator-aware path manipulation (Windows `\` vs. Unix `/`); `join`, `parent`, `file_name`, `extension`, `components`; conversion to/from `String` with validation.

### `std.error`
- [ ] `Error` trait — `description() -> String`, `source() -> Option[ref dyn Error]`; structured chaining. `From` impls for cross-error `?` already in conversion-traits section.

### `std.mem`
- [ ] `swap`, `replace`, `take` — ownership-driven idioms for value movement without consume.
- [ ] `forget` (`unsafe`) — suppress destructor; reserved for FFI handoff.

### `std.bytes`
- [ ] `Bytes` type — slice-into-shared-buffer with cheap clone; critical for parser internals, network-protocol code, request-handling perf without per-call allocation.

### `std.cmp`
- [ ] `Ordering` enum (`Less`, `Equal`, `Greater`); `min`, `max`, `clamp` free functions.

### `std.hash`
- [ ] `Hash` trait, `Hasher` interface, default hasher; `#[derive(Hash)]` codegen path (interpreter form already shipped).

### Standard Library Layers (`core` / `alloc` / `std`)
- [ ] `core` layer: primitives, `Option`, `Result`, `Array[T, N]`, traits, effect system, math — no OS or allocator dependency
- [ ] `alloc` layer: `Vec[T]`, `Map`, `String`, `f"..."` interpolation, `shared struct`/`shared enum` (RC), `Pool[T]` — requires heap allocator
- [ ] `std` layer: file I/O, networking, threads, environment, channels — requires OS
- [ ] Profile mapping: `kernel` → `core` only; `embedded` → `core` + optional `alloc`; default → all three

### Parallax-lite — first ground-truth measurement workload

Parallax-lite is a stripped-down precursor to Demo 1 (Parallax — Auto-Concurrency API Gateway, see `docs/demo_ideas.md`) — same shape (HTTP server, providers for upstream services, fan-out + join), narrower surface (one upstream instead of four, single resource per endpoint). It is the first program in the codebase with non-trivial Provider-Rooted Resources + auto-concurrency + (likely) RC fallback in one place — the right shape to ground-truth the spec's quantitative claims. Two measurements feed off the same workload:

- [ ] **Cumulative Cost Surface validation.** Run `karac query cost-summary` against Parallax-lite to validate the static-count surface specified in `design.md § Performance Diagnostics > Cumulative Cost Surface`. Discrepancies between the table's order-of-magnitude estimates and observed counts feed back as edits to the table. Runtime attribution (sampling-profiler-driven %wall-clock against the same workload) lands as a separate post-v1 step; the static-count form ships in Phase 5.3.

- [ ] **Cost-model tuning (v1.x).** Use Parallax-lite to drive the empirical tuning that lets the v1.x auto-concurrency cost-model spec land. Today's interim cost model is degenerate (parallelize whenever distinctness allows and `ParallelGroup.is_trivial` is false). The v1.x specification work — per-call cost heuristic, fork threshold, loop-body parallelization rule, distinctness policy under dynamic keys — is tracked under `implementation_checklist/` Phase 6 ("Cost-model specification (v1.x)"); this entry is the workload that gives that work its measurement target. Same binary as the Cumulative Cost Surface item above; two analyses against one program.

- [ ] **`par struct` single-task overhead measurement.** Build a `par struct` variant of the Parallax-lite types that would otherwise be `shared struct`, run the same workload, and measure the overhead in single-task mode: per-field uncontended atomic load/store, `Mutex[T]` lock/unlock on the no-contention fast path, `Arc` refcount cost vs. `Rc`. The threshold question this answers: **if single-task overhead is below ~5 ns per field access, inverting the default (`par struct` becomes the default; `shared struct` becomes the narrow opt-in) is a credible v2 RFC.** Above that threshold, the v1 polarity stands and the migration tooling (`karac migrate shared-to-par`) is the right answer. Same binary as the Cumulative Cost Surface and cost-model-tuning items above; three analyses against one program.

- [ ] **`shared` → `par` transition frequency observation.** Track how often `shared struct` definitions in `examples/` and the demo programs are migrated to `par struct` over the v1 development window. Empirical signal: high frequency (>~1 per 500 LOC of new examples) raises the inverted-default proposal from "v2 RFC" to "should-have for v1.x"; low frequency confirms the v1 default polarity was correct. Tracked manually via grep over `git log` once the workload exists.

**Done when:** The floor stdlib is sufficient to write any non-domain-specific program (CLIs, services, libraries, data-processing pipelines that don't need numerical primitives) entirely in Kāra with no FFI escape hatches beyond what the stdlib wraps. Auto-concurrency works in compiled output. Specialty stacks (numerical, security, embedded primitives, regex/http/process) ship in [Phase 11](#phase-11-standard-library--long-tail).

---

## Interactive Development: REPL + Browser Playground (P0) + Jupyter Kernel (P1 delivery)

**Goal:** First-class interactive surface that positions Kāra as a notebook-friendly systems language. Delivery is split into two tiers — the `karac repl` binary and a browser playground ship in v1 (the frictionless first-try path that drives adoption); the Jupyter kernel ships in v1.1 alongside a stable stdlib (so first-run notebook users don't hit "function not found" on common types). Runs as a parallel track to Phase 8; does not block phase progression. Semantics are specified in [`docs/design.md § Interactive Evaluation Model`](design.md#interactive-evaluation-model). Execution backend: always-JIT via LLJIT (per Core Strategy #2) — the same code path users get from `karac build`, just compiled lazily per function on first call.

**Why P0.** The execution-model-parity story (REPL cells run on the same LLJIT path as `karac build` produces, not on a parallel interpreter) gives Kāra a differentiator that neither Rust nor Java can match cheaply:

- Rust's `evcxr` recompiles a dylib per cell — slow, fragile, not officially supported. Per-cell cold-start is comparable to Kāra's, but `evcxr` lacks `cargo`-managed dependencies in cell scope and can't surface effect/ownership analysis interactively.
- Java's JShell pays JVM startup + speaks Java verbosity — works but not notebook-native feeling. Subsequent calls are JIT'd, but the language doesn't have effect / ownership analysis to surface.
- Kāra: lazy LLJIT amortizes ~100 ms cold-compile across cell lifetime, syntax readable to Python-origin users, *and* surfaces the language's differentiators (effects, ownership) in cells where other languages have nothing to show. Trivial cells (`let x = 1+1`) pay the ~100 ms compile cost — uniform and expected, not a mystery slowdown. For data-science / engineering workloads where REPL serves as production tooling, that cold-start is amortized by the data-processing time itself; it hurts most for trivial exploratory cells where serious users don't dwell.

**Why the split.** Adoption is the dominant concern for a new language, not dev effort. The mental barrier to trying a systems language ("cargo new, edit TOML, fight IDE, *then* learn ownership") is what sends Python-origin users away. The REPL binary and browser playground remove that barrier at v1. The Jupyter kernel — while strategically important for data-science audiences and shareable notebook content — depends on a stable stdlib for a good first impression, and ships with v1.1 when that's in place.

### Tier 1: REPL binary + browser playground (P0, ships in v1)
- [ ] `karac repl` subcommand — line-based REPL over the LLJIT execution backend; multi-line continuation; persistent session bindings; `:help`, `:quit`, `:type`, `:effects`, `:save <file.kara>`, `:provide R = expr` / `:end-provide R` meta-commands. Cell semantics per `design.md § Interactive Evaluation Model > Cell Scope`; cross-cell provider scoping per `design.md § Cross-Cell Providers`. Lazy compilation: each defined function is compiled on first call, cached for subsequent calls — published cold-start latency is the v1 perf headline alongside binary size and steady-state perf.
- [ ] Notebook-aware rendering of use-after-move diagnostics when consume and use straddle cells — strict semantics, softened presentation (names the consuming cell, suggests `.clone()` at call site).
- [ ] `--auto-clone` opt-in flag for users who prefer Python-like ergonomics — inserts `.clone()` at consume sites, emits `perf[auto-clone-in-repl]` note. Never silent.
- [ ] Session export (`:save session.kara`) that produces a `.kara` file compiling identically to the session. `:provide`/`:end-provide` pairs compile to `with_provider[R](expr, || { /* cells */ })` blocks in the saved file.
- [ ] Browser playground (`play.kara-lang.org` or equivalent) — zero-install entry point. Server-side `karac repl` behind a WebSocket shim, or WASM-compiled interpreter in the browser (decide during implementation). Minimum UX: editor, run button, output pane, share-by-URL.

### Tier 2: Jupyter kernel MVP (P0 priority, P1 delivery — ships in v1.1)
- [ ] `jupyter_client` protocol compliance — ZMQ shell/iopub/stdin/control channels, cell execution, stderr diagnostics with **clickable source spans in JupyterLab**, Ctrl+C cooperative interrupt, tab completion over session + prelude.
- [ ] `pip install karac-kernel` packaging — Python launcher + kernelspec registration; precompiled `karac` binaries per platform.
- [ ] `%magic` surface (MVP): `%effects`, `%ownership`, `%explain <name>`, `%set auto-clone on|off`, `%provide R = expr` / `%end-provide R` (parity with REPL meta-commands — same compilation path). Per-cell effect footer rendered automatically on every execution. **This is where the language differentiators become visible in the notebook.** `%rc` is deferred to post-MVP — RC-fallback analysis is still settling and its introspection surface is not yet stable.

### Tier 3: Rich interactive (stretch, post-MVP)
- [ ] Rich `text/html` display for structs and collections; `image/png` for any plotting primitive.
- [ ] Effect-conflict timeline — sidebar showing per-cell effect sets and cross-cell dependency arrows.
- [ ] `%rc` magic — RC-fallback decision list with trigger reasoning, once the underlying analysis surface stabilizes.
- [ ] Widget protocol (IPython-widgets equivalent) — probably v2+.

### Tier 4: Book coverage (P0 prose, v1)
- [ ] **"Getting Started, Part 2: Two Surfaces"** — dedicated chapter positioned right after "Getting Started / Installation." `.kara` file and `karac repl` shown side by side on the same binary-search example; teaches session model, cell scope, ownership across cells, `:effects` / `:save` meta-commands. Browser playground gets a sidebar callout. Ownership is taught honestly from day one — Q2's softened diagnostic means no retraction later. v1 surfaces only (no notebook content yet). When the Jupyter kernel ships in v1.1, either extend this chapter to a third surface or add a standalone "Notebooks" chapter.

**Done when (v1):** `karac repl` gives a first-run Python user a productive session in under 5 minutes. Browser playground loads in under 2 seconds with no install. A user can save a REPL session to a `.kara` file that compiles and runs identically.

**Done when (v1.1):** `pip install karac-kernel && jupyter lab` gives a notebook environment where Kāra feels first-class, effect / ownership information shows up alongside normal cell output, and a user can save the session to a `.kara` file that compiles and runs identically.

---

## Package Manager v1.1: Build & Dependency Tooling

**Goal:** Flip `[dependencies]` from parsed-but-ignored (v1 posture) to load-bearing. Runs as a parallel track alongside Phase 8 / Phase 11; does not block phase progression. Detailed implementation entries under [`implementation_checklist/phase-5-diagnostics.md § 5.5`](implementation_checklist/phase-5-diagnostics.md#55-package-manager-v11).

### Tier 1: Resolver + lockfile (lands first; everything else builds on it)

- [ ] **PubGrub-style resolver** — conservative semver, latest compatible by default, lockfile pins, full constraint-chain conflict diagnostics.
- [ ] **`kara.lock`** — package name, exact version, source URL (proxy mirror or git URL), BLAKE3 content hash, dependency tree. Single lockfile across targets. Bin-yes / lib-no commitment.
- [ ] **Registry proxy client** — Go-style decentralized identity (git URL) + immutable proxy mirror. Records both URLs in lockfile. `--no-proxy` for development.
- [ ] **Build artifact cache** — global `~/.kara/cache/` keyed on `(compiler-version, package-version, edition, profile, target-triple)`. Per-project `dist/` already exists.
- [ ] **`[package].kara-version` MSRV constraint** — enforced by resolver; mismatch is a structured diagnostic with the constraint chain.

### Tier 2: CLI surface + cross-cutting

- [ ] **`karac update` / `karac update <pkg>`** — bare form bumps everything within semver-compatible range; surgical form bumps one package.
- [ ] **`karac install <bin-spec>`** — install a binary from path / git / proxy reference into `~/.kara/bin/`.
- [ ] **`karac clean` / `karac clean --global`** — project `dist/` and global cache eviction.
- [ ] **`karac vendor` + `karac build --offline`** — air-gap workflow; copies resolved deps into `vendor/`, refuses network on subsequent build.
- [ ] **`[target.X.dependencies]` / `[target.X.profile]`** — per-target dependency and profile blocks for cross-compilation.
- [ ] **`[dev-dependencies]` excluded from non-test builds** — wiring in the existing test/non-test split.
- [ ] **`karac-toolchain.toml` reader** — `version` (required), `targets` (optional). Channels / components / install profiles deferred. Read by `karac` and by the eventual `karaup` toolchain manager.

### Tier 3: Interactive parity

- [ ] **`:dep` REPL meta-command** — adds a package to the session's in-memory manifest. State in-memory only; symmetric with `:provide`. Jupyter parity via the existing kernel meta-command channel.
- [ ] **`karac run <script>` script-dir manifest discovery** — walk upward from the script's own directory (not cwd). `--manifest` / `--no-manifest` overrides.

### Reproducibility CI

- [ ] **Build-twice-and-hash CI for the compiler itself** — enforces the bit-exact reproducible-builds promise (see `design.md § Package System > Reproducibility guarantee`). Failure on diff is a compiler bug, not a user issue.

**Out of scope for v1.1** (v1.5+ or v2 RFC):
- `karac bench` (needs bench harness + statistical reporting).
- `karac publish` (needs registry publish protocol; gated on adoption signals).
- `karac audit` (needs vulnerability database with package-identity keys).
- Per-package feature flags / `[features]` axis (v2 RFC slot — opens if "ship multiple packages" pattern becomes widely lamented; rejected in v1 to avoid Cargo's worst pain point).
- Centralized registry (deferred indefinitely; git-URL identity + proxy stays canonical).

**Done when (v1.1):** `karac build` in a project with non-trivial `[dependencies]` resolves through the proxy, writes `kara.lock`, caches compiled deps in `~/.kara/cache/`, and produces a bit-exact-reproducible artifact given a pinned `karac-toolchain.toml`. `karac repl` inside the project tree picks up project deps automatically; `:dep http = "1.2"` works in a session outside any project.

---

## Phase 9: Gradual Verification Enforcement

**Goal:** Enforce the gradual verification features whose syntax was parsed in Phase 2. Adds the correctness layer on top of the working MVP compiler — language semantics are fully locked before new backends.

### Refinement Types (Level 2)
- [ ] Constraint validation at construction boundaries
- [ ] Compile-time elision when provable (v1 two-rule procedure)
- [ ] `TryFrom` generation for fallible construction
- [ ] Reject implicit runtime-value narrowing — require explicit `R.try_from(x)?` or `x as R`

### Distinct Types
- [ ] Enforce opacity (no implicit operations on the underlying type)
- [ ] Verify `#[derive]` compatibility
- [ ] Interaction with refinement types: `distinct type ValidPort = u16 where self >= 1 and self <= 65535`

### Contracts (`requires` / `ensures` / `invariant`)
- [ ] Verify contract expressions are pure (effect set ⊆ `{panics}`)
- [ ] Insert runtime checks in debug builds
- [ ] `old(expr)` desugaring with `Clone` requirement
- [ ] Invariant insertion at every `pub` method exit
- [ ] Strip all contract machinery in release builds

### Extended Patterns
- [ ] Range patterns: `LITERAL "..=" LITERAL` in match arms (integer and `char` types)
- [ ] `@` bindings: `IDENT "@" PATTERN` — capture value while testing pattern
- [ ] Exhaustiveness: range patterns integrate with Maranget's algorithm (cover their value set)
- [ ] Composition: range + or-pattern, `@` + range, `@` + or-pattern, nested in struct/enum fields

**Done when:** `type Percentage = f64 where self >= 0.0 and self <= 100.0` compiles with boundary checks. `distinct type UserId = i64` rejects implicit operations. `requires`/`ensures` annotations produce runtime checks in debug and are stripped in release. All three features compose correctly (e.g., distinct + refinement types). Range patterns and `@` bindings work in match, `if let`, and `while let` contexts.

---

## Phase 10: Additional Compilation Targets

**Goal:** Same language compiles to multiple targets.

- [ ] **WebAssembly:** LLVM WASM backend. Concurrency lowering: sequential cooperative scheduling on the main thread by default; `--features wasm-threads` opts into Web Workers + SharedArrayBuffer + atomics (user deploys with COOP/COEP headers). Compiler-managed transparent threading (ownership-proven partitioning without opt-in) is deferred post-v1 — see `docs/deferred.md § Compiler-Managed Transparent Threading on WASM`. Source-level `go`/channel/`par` semantics are target-agnostic — see `design.md § Concurrency Across Targets`.
- [ ] **GPU compute shaders.** Compile `#[gpu]`-annotated functions to GPU kernels and wire `gpu.dispatch` to invoke them. Full design of the `#[gpu]` constraint, `GpuSafe` type bound, and `gpu.dispatch` effect semantics is already in `design.md § GPU Subset Constraints`.

  **Compilation strategy — two paths:**

  - **Primary path (wgpu/WGSL):** `#[gpu]` functions compile to WGSL shaders. At runtime, [wgpu](https://github.com/gfx-rs/wgpu) selects the best available GPU API for the platform and uses the highest-performance GPU device (discrete preferred over integrated):
    - macOS / iOS → Metal API
    - Linux → Vulkan API (works on NVIDIA, AMD, and Intel GPUs)
    - Windows → DX12 API, Vulkan fallback (works on NVIDIA, AMD, and Intel GPUs)
    - Browser (WASM target) → WebGPU API
    Vulkan and DX12 are APIs, not hardware — an NVIDIA GPU on Linux uses Vulkan by default and is fully utilized. No `--target` flag needed. The same compiled binary runs on all wgpu-supported platforms. Build normally: `karac build`.

  - **CUDA path:** `#[gpu]` functions compile to PTX via LLVM's NVPTX backend. Requires an explicit target flag: `karac build --target cuda`. NVIDIA hardware only. Use this path when you need NVIDIA-specific libraries (cuBLAS, cuDNN) or are squeezing out the last bit of NVIDIA-specific performance. For general GPU compute on NVIDIA hardware, the wgpu/Vulkan path already works — CUDA is not required just to run on an NVIDIA GPU.

  **Runtime GPU selection:**

  - wgpu auto-selects the highest-performance available device (discrete GPU preferred over integrated).
  - Users with multiple GPUs can override via the `KARAC_GPU=<index>` environment variable (0-indexed, ordered by wgpu's device enumeration). No API or recompile needed.
  - If no compatible GPU device is found at runtime, the program exits with a structured error:
    ```
    error: no GPU device available
    hint: this program requires GPU support (gpu.dispatch called at runtime)
    hint: set KARAC_GPU_BACKEND=cpu to run kernels on CPU for debugging (performance will be severely degraded)
    ```
    No silent CPU fallback. The `KARAC_GPU_BACKEND=cpu` escape hatch is for debugging only and is explicitly labelled as such.

  **Implementation tasks:**
  - [ ] WGSL codegen: lower `#[gpu]` function bodies to WGSL compute shaders
  - [ ] wgpu integration: device initialization, buffer management, shader compilation, dispatch
  - [ ] `gpu.dispatch` runtime call: pack arguments into GPU buffers, submit compute pass, read results back
  - [ ] Layout groups → GPU buffers: `group physics { position, velocity }` maps to a single GPU buffer with coalesced access
  - [ ] `GpuSafe` type checking: reject heap types (`String`, `Vec[T]`, etc.) in `#[gpu]` call graphs (already specified in design.md)
  - [ ] Effect enforcement: reject `allocates(Heap)`, `panics`, I/O effects in `#[gpu]` call graphs (via existing effect checker)
  - [ ] CUDA path: NVPTX codegen for `--target cuda` builds
  - [ ] `KARAC_GPU` / `KARAC_GPU_BACKEND` environment variable handling
- [ ] **FPGA bitstreams (future goal):** As described in design.md Feature 7; not yet designed in detail
- [ ] **Atomic RMW operations:** `swap`, `compare_exchange`, `fetch_add`, `fetch_and`, `fetch_or` on `Atomic[T]` (v1 shipped `load`/`store` only)
- [ ] **Hardware fences:** `fence(Ordering)` (unsafe) / `compiler_fence(Ordering)` (safe) — hardware and compiler barriers

**Done when:** A compute-bound Kāra program compiles to WASM and runs in a browser. A data-parallel Kāra program with `layout` blocks compiles to a GPU compute shader and runs on a GPU. FPGA support is a stretch goal beyond this phase.

---

## Phase 11: Standard Library — Long-Tail

**Goal:** Domain-specific stdlib that programs need beyond the floor — numerical/data-science stack, scripting helpers, security types, embedded primitives, plus codegen IR optimization. **End of this phase = v1 release.** The split from [Phase 8](#phase-8-standard-library--floor) lets v1 ship semantically locked (after Phase 9) and target-complete (after Phase 10) before the long-tail lands. Co-locating the long-tail with target work pays off concretely: the numerical stack composes with the GPU call-site backend, embedded primitives co-design with the embedded target, and WASM portability is already proven for new modules.

### `f16` / `bf16` Numeric Primitives
- [ ] Reserve `f16` and `bf16` as lexer-level keywords in v1 (compile error if used as identifiers — prevents future source-breaking rename).
- [ ] Type system: add `f16` (IEEE 754-2008 half-precision) and `bf16` (bfloat16) as primitive types with the same trait surface as `f32`/`f64` (`PartialEq`, `PartialOrd`, arithmetic traits, `Copy`) but NOT `Eq`/`Ord`/`Hash`.
- [ ] Codegen: lower `f16` → LLVM `half`, `bf16` → LLVM `bfloat`. Native instruction emission on capable targets; software promotion to `f32` on others with a `f16_software_emulated` performance lint.
- [ ] Implicit widening: `f16` → `f32`, `bf16` → `f32` (both lossless).
- [ ] Literal suffixes: `1.0f16`, `1.0bf16`.
- [ ] Stdlib: `F16`, `BF16` total-order wrappers (same pattern as `F32`/`F64`).
- [ ] `Tensor[f16, Shape]` and `Tensor[bf16, Shape]` valid once both this and the numerical stdlib ship.

See `design.md § f16 / bf16 Implementation` for full design shape.

### Numerical and data-science stdlib

Semantics in `design.md § Numerical Types`, `§ Numeric Semantics > Literal-involved promotion`. Implementation tasks in `implementation_checklist/ § Numerical and data-science stdlib (Phase 11 — long-tail)`.

**Type system (forcing functions).**
- [ ] `Tensor[T, Shape]` — shape-typed N-D container with static + dynamic (`?`) dims. Q1 (1A).
- [ ] Shape as a new generic-parameter kind; shape literal grammar; `Dim`-kinded params with compile-time unification. Q2 (2C). Arithmetic on shape params (`[A + B]`) deferred to v1.5.
- [ ] Implicit scalar-tensor broadcasting (`arr + 1`); explicit methods (`arr.broadcast_add(row_vec)`) for tensor-tensor. Q3 (3B+3C hybrid).
- [ ] Literal-involved promotion in numeric binary operators — `arr + 1` works, `arr + typed_var` still requires matching types. Q4 (4B).

**Data types (Arrow commitment).**
- [ ] `Column[T]` — bitmap-backed nullable 1D column, Arrow layout. Q5 (5A) + Q6 (6C).
- [ ] `Tensor` is dense-only; nullability is a `Column` concern.
- [ ] `DataFrame` — schema-bearing table of named columns.
- [ ] Arrow IPC, Parquet, CSV readers/writers with effect annotations.

### Scripting-critical stdlib (10B surface)
- [ ] `std.regex` — compile patterns, match/find/replace.
- [ ] `std.http` — client only (server v1.5+).
- [ ] `std.websocket` — client only (server v1.5+); `WebSocket` connection type with `send`/`recv`/`close`; `sends(Net)` / `receives(Net)` effect annotations; pairs with `std.http` for upgrade handshake. Browser playground uses a WebSocket shim — this is load-bearing for the interactive tools track.
- [ ] `std.process` — `Command`/`Child`; new `ProcessTable` effect resource.
- [ ] `std.stats` — mean, stddev, percentile, median, min/max, argmin/argmax, sort, argsort. Trait-dispatched via `Reduce` / `ElementwiseMap` / `ElementwiseOrd` so future `GpuTensor` implements the same surface.

**Fallback.** If Phase 11 slips, `regex` and `http` are the most at-risk items (pushable to v1.1). Rest stays in v1.

### Security (`std.secret`)
- [ ] `Secret[T]` — compiler-enforced wrapper that blocks `Debug`/`Display`/`Serialize`/`Deserialize`/`PartialEq`/`Eq`/`PartialOrd`/`Ord`/`Hash`/`Deref`/`Borrow`/`AsRef`/`Copy` impls on itself; `.expose()` / `.expose_mut()` are the only access paths; `.clone()` re-wraps
- [ ] `ConstantTimeEq` trait — constant-time equality replacing `PartialEq` for `Secret[T]`; stdlib impls for `String`, `Vec[u8]`, fixed-size `[u8; N]`, integer primitives
- [ ] `Zeroize` trait (`fn zeroize(mut ref self)`) — stdlib impls for the same set; `Drop` on `Secret[T]` dispatches through it before field destructors
- [ ] Derive codegen: `#[derive(Debug)]` / `#[derive(Display)]` on containing types emits `Secret[T]` fields as `<redacted>`; `#[derive(Serialize)]` on containing types is a compile error with a pointer to `.serialize_expose()` for explicit wire transit
- [ ] `undocumented_unsafe` lint — warn (default-on) on `unsafe` blocks without a preceding `// Safety:` comment; same rule for `unsafe fn` via `# Safety` doc-comment section

### Embedded / Hardware Primitives
- [ ] `volatile_read[T: Copy]` / `volatile_write[T: Copy]` — unsafe intrinsics for MMIO register access
- [ ] `VolatileCell[T: Copy]` — stdlib wrapper for ergonomic register map definitions
- [ ] Inline assembly: `asm` keyword expression inside `unsafe`; operand forms (`in`, `out`, `inout`); options (`nomem`, `nostack`, `pure`, `volatile`)
- [ ] `global_asm` — file-scope raw assembly for vector tables and bootstrap
- [ ] `Atomic[T: Copy]` — `shared struct` for ISR-to-main signaling; v1 scope: `new`, `load(ord)`, `store(val, ord)` on `bool`/`u8`/`u16`/`u32`/`u64`/`usize`. Advanced RMW ops (`swap`, `compare_exchange`, `fetch_add`, `fetch_and`, `fetch_or`) and fences land alongside the embedded target work in Phase 10.
- [ ] `Ordering` enum: `Relaxed`, `Acquire`, `Release`, `AcqRel`, `SeqCst` — C11/LLVM memory model
- [ ] `#[interrupt(NAME)]` — ISR attribute: interrupt calling convention, vector table placement, implicit `isr` profile restrictions
- [ ] `CriticalSectionGuard` — RAII interrupt disable/re-enable; `#[must_use]`
- [ ] Linker control: `#[link_section("name")]`, `#[no_mangle]`, `#[used]`
- [ ] C calling convention variants: `extern "C"`, `"C-unwind"`, `"interrupt"` (implemented); `"stdcall"`, `"fastcall"`, `"win64"`, `"sysv64"` (reserved)
- [ ] `float_in_serialized_type` lint: warn when `#[derive(Serialize)]` or `#[derive(Deserialize)]` contains an `f32`/`f64` field — JSON has no NaN encoding, format consumers follow IEEE. Suppressible per-field with `#[allow(float_in_serialized_type)]`. (Lands alongside Serialize/Deserialize derives, post-v1.)

### Codegen Optimization (IR quality pass)
- [ ] Inline hints: emit `alwaysinline` / `noinline` attributes based on call-site analysis
- [ ] Alias metadata: `noalias` on owned parameters, `tbaa` type-based alias analysis tags
- [ ] Arithmetic flags: `nsw`/`nuw` on integer ops where overflow is defined-UB in Kāra semantics
- [ ] LTO: enable link-time optimization in `karac build --release`
- [ ] PGO stubs: `llvm.expect` intrinsic on branch conditions where effect analysis can predict likelihood

**Goal of this pass:** Reduce the Phase 7 ≤2x gap to ≤10% of equivalent hand-written Rust on compute-bound benchmarks. Ships at the end of v1 because IR-quality polish only pays off once the long-tail stdlib is the last thing being measured.

### Deferred from v1 (P1, ships post-v1)
- [ ] **v1.5 — Axis-indexed reductions.** `sum[AXIS]()`, `mean[AXIS]()`, `min[AXIS]()`, `max[AXIS]()`, `argmin[AXIS]()`, `argmax[AXIS]()` with fully typed return shapes (`remove_dim(Shape, AXIS)`). Held for v1.5 because shipping with `Tensor[T, [?]]` return types would be a breaking change when shape arithmetic tightens them. See `design.md § Axis-Indexed Reductions`.
- [ ] **v1.5 — Lazy evaluation / pipeline fusion.** `.lazy()` → `LazyColumn` / `LazyTensor` / `Iterator` specializations; fused filter/map/reduce with effect composition. Revisit alongside GPU call-site design — lazy is load-bearing for kernel fusion.
- [ ] **Phase 11+ (P1) — `std.einsum`.** String-notation Einstein summation. See `deferred.md § std.einsum`.
- [ ] **Phase 11+ (P1) — `std.linalg`.** SVD, eigendecomposition, QR, Cholesky, `lstsq`, norm, inverse, determinant, rank. See `deferred.md § std.linalg`.
- [ ] **Phase 11+ (P1) — `std.fft`.** 1D/N-D FFT, IFFT, `rfft`, `fftfreq`. See `deferred.md § std.fft`.
- [ ] **Phase 11+ (P1) — `std.random` distributions.** Normal, binomial, Poisson, exponential sampling on top of basic uniform. See `deferred.md § std.random`.
- [ ] **Phase 11+ (P1) — `Tensor.where`, boolean indexing, fancy indexing, `meshgrid`.** Conditional element selection, mask-based filtering, index-array access, coordinate grid generation. See `deferred.md` entries.
- [ ] **Phase 11+ (P1) — Tensor element-wise math and `clip`.** `exp`, `log`, `sqrt`, `abs`, `sign`, `floor`, `ceil`, `round`, `sin`/`cos`/`tan` and inverses, `atan2`, `clip(lo, hi)`. See `deferred.md § Tensor Element-Wise Math`.
- [ ] **Phase 11+ (P1) — Tensor construction functions.** `zeros`, `ones`, `full`, `eye`, `diag`, `arange`, `linspace`, `from_fn`. See `deferred.md § Tensor Construction Functions`.
- [ ] **Phase 11+ (P1) — Scan operations.** `cumsum`, `cumprod` (global and axis-indexed). Axis-indexed scans preserve input shape so they do not require v1.5 shape arithmetic. See `deferred.md § Scan Operations`.
- [ ] **Phase 11+ (P1) — Shape-manipulating ops.** `concat`, `stack`, `reshape`, `flatten`, `expand_dims`, `squeeze`. Ship with partially-dynamic return shapes; v1.5 shape arithmetic provides fully-typed versions. See `deferred.md § Shape-Manipulating Operations`.
- [ ] **Phase 11+ (P1) — Set-like operations.** `unique` (with counts and inverse), `searchsorted`. See `deferred.md § Set-Like Operations`.
- [ ] **Phase 11+ (P1) — NaN/Inf handling.** `is_nan`, `is_inf`, `is_finite`, `nansum`, `nanmean`, `nanmin`, `nanmax`, `fill_nan`, `f64.NAN`/`f64.INF` constants. See `deferred.md § NaN and Inf Handling`.
- [ ] **Phase 11+ (P1) — `.npy`/`.npz` file I/O.** `std.io.npy` — load/save single arrays and multi-array archives. See `deferred.md § .npy / .npz Array File I/O`.
- [ ] **Phase 11+ (P1) — `Complex[T]` stdlib struct.** Canonical complex number type shared across `std.fft`, `std.linalg`, and signal-processing libraries. Interleaved memory layout (FFTW/C99 compatible). `Tensor[Complex[f64], Shape]` as the FFT output type. See `deferred.md § Complex[T]`.
- [ ] **Phase 11+ (P1) — `RichDisplay` trait.** MIME-typed display protocol for the Jupyter kernel. Plotting and DataFrame libraries implement this to render charts and tables inline. See `design.md § Rich Output Display Protocol`.
- [ ] **Phase 11+ (P1) — `std.crypto`.** Constant-time cryptographic primitives: ChaCha20-Poly1305 (AEAD), X25519 (key exchange), Ed25519 (signatures), Argon2id (password hashing), BLAKE3 (general hashing). Delegates to a vetted C library via FFI. `reads(EntropySource)` effect on key-generation calls. See `deferred.md § std.crypto`.
- [ ] **Phase 11 (P1) — `CircularBuffer[T]`.** Fixed-capacity ring buffer; O(1) push/pop at both ends; allocation-free after construction. Enables audio DSP, networking packet queues, and embedded sensor pipelines to share a common type. See `deferred.md § CircularBuffer[T]`.
- [ ] **v1.5 — `std.http` server + `std.websocket` server.** HTTP client ships in Phase 11; server-side HTTP and WebSocket upgrade handler land in v1.5 alongside shape arithmetic and lazy evaluation.
- [ ] **Post-Phase-10 — GPU call-site backend.** Revisit once Phase 10 codegen has ground truth. Expected shape: `GpuTensor[T, Shape]` with `.on(gpu)` / `.to_cpu()` boundary ops (CuPy / PyTorch / JAX semantics). Numerical stdlib composes with whatever GPU story lands — trait-dispatched ops keep API open.

**Done when:** The numerical/data-science stack (Tensor, Column, DataFrame, Arrow IPC, Parquet, CSV) is usable. Specialty stdlib (regex, http client, websocket client, process, stats) ships. Security types (`Secret[T]`, `ConstantTimeEq`, `Zeroize`) are enforced. Embedded primitives (`volatile_read`/`write`, inline `asm`, `Atomic`, `#[interrupt]`) compile and run on a target board. Codegen IR optimization closes the Rust performance gap to ≤10% on compute-bound benchmarks. **End of this phase = v1 release.**

---

## Phase 12: Self-Hosting

**Goal:** Rewrite the Kāra compiler in Kāra.

- [ ] Lexer in Kāra
- [ ] Parser in Kāra
- [ ] Semantic analyzer in Kāra
- [ ] Interpreter or codegen in Kāra
- [ ] Bootstrap: Kāra compiler compiles itself

**Done when:** `karac build src/main.kara` produces a binary that can itself compile Kāra programs, producing identical output to the Rust-based compiler.

---

## Future: Gradual Verification (Feature 6)

**Goal:** Progressively stronger correctness guarantees beyond the type system — from constrained types to full formal verification.

**Level 2 (Refinement Types) and Level 2.5 (Contracts) are committed** — fully designed in design.md. Parsing is complete (Phase 2). Type-checker enforcement and interpreter runtime checks are tracked in Phase 9 (Gradual Verification Enforcement). Level 3-4 require an SMT solver (Z3) and are indefinitely deferred.

```
// Level 2: Refinement types (committed — no SMT solver needed)
type PositiveInt = i64 where self > 0;
type Percentage = f64 where self >= 0.0 and self <= 100.0;

// Level 2.5: Contracts (committed — runtime-checked in debug, stripped in release)
fn binary_search[T: Ord](haystack: ref Vec[T], needle: ref T) -> Option[i64]
    requires haystack.is_sorted()
    ensures(result) match result { Some(i) => haystack[i] == *needle, None => true }
{ ... }

// Level 3-4: Full formal verification (deferred — requires SMT solver, may never be built)
fn transfer(from: Account, to: Account, amount: Money)
    requires from.balance >= amount
    ensures from.balance + to.balance == old(from.balance) + old(to.balance)
{ ... }
```

- [x] **Level 2 — Refinement types (committed).** `type Percentage = f64 where self >= 0.0 and self <= 100.0`. Numeric comparisons + `len()`, boolean combinators, no SMT solver. Runtime checks at construction; compile-time elision when provable. Parsing complete (Phase 2); enforcement in Phase 9.
- [x] **Level 2.5 — Contracts (committed).** `requires`/`ensures` on functions, `invariant` on structs. Runtime-checked in debug builds, stripped in release. Pure expressions only. Parsing complete (Phase 2); enforcement in Phase 9.
- [ ] **Level 3 — Pre/post conditions with SMT.** Z3 integration for formal proof of contracts. Deferred indefinitely.
- [ ] **Level 4 — Full formal verification.** `old()` references for state before/after. Quantifiers. Proofs of complex invariants. Deferred indefinitely.

**Interaction with other features:** D8 (AI-first) noted that "formal specification as primary artifact" becomes more central if Level 3-4 ever ship. Effect annotations are a lightweight form of Level 2 verification — the gradient exists from day one.

**Done when (Level 2):** `type Percentage = f64 where self >= 0.0 and self <= 100.0` compiles, inserts boundary checks at assignment sites, and the constraint appears in `karac query` output.

---

## Future: Comptime (Compile-Time Code Execution)

**Goal:** Zig-inspired compile-time code execution in the Kāra language itself. Enables custom derives, compile-time validation, and code generation without a separate macro language.

**Not currently scheduled** — add when the built-in `#[derive]` trait set feels limiting. No current design decisions prevent this; purely additive.

- [ ] `comptime` keyword: mark functions that execute at compile time
- [ ] Type reflection: compiler provides struct/enum field information to comptime functions
- [ ] Custom derives: user-defined `#[derive(MyTrait)]` via comptime functions
- [ ] Compile-time validation: SQL query checking, regex compilation, config validation
- [ ] `const fn` calls in const-arg / module-binding positions (extends the v1 const-generic surface; see Phase 5.2)

**How it compares to Rust proc macros:**

| | Rust proc macros | Kāra comptime |
|---|---|---|
| Language | Separate Rust code manipulating token streams | Same Kāra language, runs at compile time |
| Separate package needed? | Yes | No — same file, same module |
| Access to type info | Indirect (parse token streams) | Direct (compiler provides type/field info) |
| Error messages | Often cryptic | Normal Kāra errors |
| Compile time impact | Major (#1 cause of slow Rust builds) | Bounded (resource limits on compile-time execution) |

**Done when:** A user can write a custom `#[derive(Validate)]` that generates validation code from struct field types and attributes, using normal Kāra syntax, in the same project.

---

## Future: Editions and Migration Pipeline

**Goal:** post-v1 editions (`2027`, `2030`, …) ship breaking language changes through a **warning-promotion pipeline** rather than as sudden hard breaks. Every edition-gated change graduates from soft signal to hard rejection across three stages: warn-by-default lint → deny-by-default lint → hard error at the next edition boundary.

**Not currently scheduled** — v1 ships under edition `2026` only. The full migration-pipeline tooling is needed when a second edition is ready to ship; building it before then would be infrastructure without a customer.

**The three-stage promotion** (specced in `design.md § Editions > Migration policy`):

1. **Warning stage** — the upcoming behavior change is detected and reported as a warn-by-default lint in the *current* edition. The lint name is permanent (per the lint-namespace stability rule); programs can suppress with `#[allow(<lint_name>)]` per the lint-level attribute machinery. Authors see the warning during normal `karac build` runs and have lead time to migrate before any compilation failure.
2. **Deny stage** — the lint promotes to deny-by-default at a chosen point during the current edition's lifetime, typically one minor compiler release after stage 1. Programs that haven't addressed the lint now require an explicit `#[allow(<lint_name>)]` to compile. The opt-in is a deliberate "I see the warning and choose to defer."
3. **Hard-error stage** — at the next edition boundary, the lint disappears and the underlying behavior change becomes a hard error. Code under the new edition must conform; un-migrated source fails to compile until updated.

**Tooling support:**

- [ ] **`karac explain --edition <NEXT>`** — projects the full migration timeline for the current package: every warn-stage lint that will become deny-stage in the next minor release, and every deny-stage lint that will become a hard error at the next edition boundary.
- [ ] **`karac fix --edition <NEXT>`** — applies mechanical migrations where the warning's help line provides a direct fix (rewriting `expr_2027` to `r#expr_2027` for keyword reservations, inserting per-binding allows for breaking changes the user has chosen to defer, applying type-swap suggestions, etc.). Non-mechanical changes are left for the user.
- [ ] **`[lints]` table in `kara.toml`** — per-lint policy override: `[lints] foo = "deny"` to escalate immediately, `[lints] bar = "allow"` to defer past the deny stage. Project-level policy in the manifest, parallel to the existing per-attribute `#[allow]` / `#[deny]` machinery.

**Concrete examples already in v1 design** (these will be the first migrations the pipeline carries):

- **Private-function effect inference broadening across edition boundaries** (per `design.md § Specification Layers, Reported behavior`) — the canonical motivating case for the pipeline.
- **Future fragment-specifier reservations** (per `design.md § Reserved Fragment-Specifier Identifier Namespace`, item 62) — extending the reservation to additional prefixes (`pat_<NNNN>`, `ty_<NNNN>`, etc.) post-v1 is a warn-stage addition under the current edition, deny in a later release, hard error at the next edition.
- **Future keyword reservations** — when a new edition reserves a token (e.g., `async`), warn-stage lint flags every binding using the name in user code (suppressible via `r#async`); deny-stage forces the explicit `r#` escape; edition-boundary hard error rejects the bare form.

**Why three stages, not a single hard break.** The warning stage gives ecosystem authors lead time to migrate before any user-visible compilation failure. The deny stage forces awareness without forcing immediate migration (the `#[allow]` opt-out is the escape valve for code that genuinely needs to defer). The edition-boundary hard error is the final stop — code under the new edition must conform, but at that point every author has had a deny-stage warning + a deliberate `#[allow]` to suppress it, so the hard error is never a surprise. The pipeline matches Rust's edition-migration discipline (Rust 2021 → 2024 graduates patterns through warn → deny → error across edition boundaries) and is the load-bearing answer to "how do post-v1 breaking changes ship without breaking everyone overnight?"

**Done when:** the migration pipeline tooling (`karac explain --edition <NEXT>`, `karac fix --edition <NEXT>`, `[lints]` manifest table) ships alongside the first post-v1 edition (`2027` or whichever year is next), with the canonical migrations from the v1 design (effect-broadening, fragment-specifier reservations, keyword reservations) carried through the pipeline as the first real test of the discipline.

---

## Future: Language Server and Reactive Query-Based Compilation

**Goal:** rust-analyzer-class tooling for Kāra — broad query surface, subscribe model, incremental re-computation driven by file watchers.

**Not currently scheduled — v2+ direction, post-self-hosting.** Phase 5 ships a narrow batch query surface (`karac query effects|ownership|concurrency`) that answers one question per invocation. For AI clients iterating rapidly on a single file, the batch model is sufficient when paired with `--output=jsonl` streaming. The reactive model becomes necessary only at scale — large codebases where re-running even a function-local pipeline on every edit is too slow, or IDE integrations where sub-100ms query latency matters.

The Compilation Model principles (function-local analysis, SCC as cache unit, named inter-phase dependencies) are the substrate that makes this layer feasible later. Building it now would double compiler complexity for a prototype that does not yet need it.

- [ ] Broad query surface: type of expression at position, resolved name at position, visible trait impls at position, monomorphizations of a generic function, effect derivation chain for a call site
- [ ] Subscribe protocol: clients register interest in a query and receive notifications when the answer changes due to file edits
- [ ] Incremental re-computation: Salsa-style or rustc-queries-style dependency tracking across pipeline phases, keyed on function-local units and SCCs
- [ ] Language Server Protocol binary: separate `kara-lsp` (or similar) speaking LSP over stdin/stdout; shares analysis code with the `karac` compiler but runs as a long-lived process
- [ ] IDE integrations: VS Code, Neovim, JetBrains — shipped after the LSP binary stabilizes

**Done when:** An editor user can hover over an expression and see its inferred type and effects within 100ms, and the answer updates live as they edit surrounding code without re-running the full pipeline.

---

## Resolved Design Primitives

Design decisions for language primitives. Most are resolved; see design.md for canonical definitions:

| Item | Question | Likely answer |
|---|---|---|
| Integer sizes | Just `i64` or full set (`i8`/`i16`/`i32`/`i64`/`u8`/.../`usize`)? | Full set (systems language needs them). `usize` is FFI-only; idiomatic Kāra uses `i64` for indices/sizes (decided) |
| Default integer type | What type is `42`? | `i64` (decided). Explicit annotation for smaller types |
| Integer overflow | Trap or wrap? | Always trap. `wrapping_add` etc. for explicit wrapping (decided) |
| Float sizes | Just `f64` or `f32` + `f64`? | Both (GPU/games need `f32`) |
| Float NaN semantics | `NaN == NaN`? Total order? | `f32`/`f64` are IEEE (`NaN != NaN`, no `Eq`/`Ord`/`Hash`); `F32`/`F64` are stdlib total-order types (`NaN` sorts last, implement `Eq`/`Ord`/`Hash`) — decided |
| Numeric widening | Implicit conversions? | Guaranteed-lossless only. `i64→f64` blocked (decided) |
| Entry point | `fn main()` or `fn main(args: Vec[String])`? | `fn main()` with `env.args()` via `reads(Env)` |
| Import syntax | `use path.item` or `import path.item`? | `import path.item` (v41: mainstream-syntax tiebreaker; see `docs/design.md § Module System`) |
| File extension | `.kara` | `.kara` (already used in examples) |
| String interpolation | `format!()` macro or `f"hello {name}"` syntax? | `f"..."` — language feature, compiler desugars to concatenation (decided) |
| Operator overloading | Via traits (`Add`, `Eq`, `Ord`)? | Yes (Rust model) |
| Type aliases | `type Name = ExistingType`? | Yes (standard) |
| Numeric literals | Underscores (`1_000_000`)? Hex (`0xFF`)? Binary (`0b1010`)? | Yes to all (standard) |
| Closure syntax | `\|x\| x + 1` or `fn(x) { x + 1 }`? | `\|x\| x + 1` (Rust convention) |
| Range syntax | `0..10`, `0..=10`? | Yes (Rust convention) |
| Variable shadowing | Allowed? | Yes, all scopes (decided) |
| Default values | Zero values or require initialization? | Require explicit initialization (decided) |
| Loop variable capture | Shared reference or fresh binding? | Fresh binding per iteration (decided) |
| Testing | Where do tests live? | `_test.kara` co-located files, `test_` function prefix, `karac test`. No `#[test]` attribute needed (decided) |
| Derive | Manual trait impls or compiler-generated? | `#[derive(Eq, Hash, ...)]` compiler built-in (decided) |
| Optional chaining | Syntax for nested Option access? | `?.` and `??` operators (decided) |
| `?` operator scope | Result only or also Option? | Both Result and Option (decided) |
| `unwrap()` safety | Tracked or untracked? | Produces `panics` effect, tracked through call chain (decided) |
| Debug printing | Separate from `print`? Effect behavior? | `dbg()` builtin — transparent `debugs` effect, stderr, shows file/line/expr/value, returns value, stripped in release builds. `print`/`println` are for program output (`writes(Stdout)`) (decided) |

---

## Phase Dependency Graph

```
Phase 0 (Proof of Value) ← can be done anytime, no compiler dependency

Phase 1 (Lexer)
  │
  ▼
Phase 2 (Parser)
  │
  ▼
Phase 3 (Semantic Analysis)    ← diagnostics built incrementally at each sub-phase
  │
  ▼
Phase 4 (Interpreter)          ← core stdlib types (Option, Result, Vec, String) introduced here
  │
  ▼
Phase 5 (Query API & Tooling)
  │
  ▼
Phase 6 (Concurrency Runtime)  ← concurrency analysis + runtime execution
  │
  ▼
Phase 7.1 (Core Codegen)       ← language-construct codegen (DONE)
  │
  ▼
Phase 7.2 (Compiled Stdlib Types + Layout Codegen)  ← Array[T,N], Vec[T], String codegen + layout SoA
  │
  ▼
Phase 8 (Stdlib — Floor)        ← full method sets, traits, I/O, providers, std.json/time/path/error/mem/bytes/cmp/hash, auto-concurrency codegen
  │
  ▼
Phase 9 (Verification)          ← refinement types, distinct types, contracts (parsing done in Phase 2; enforcement here)
  │
  ▼
Phase 10 (WASM/GPU Targets)
  │
  ▼
Phase 11 (Stdlib — Long-Tail)   ← numerical/data-science, regex/http/process/stats, security, embedded primitives, codegen IR pass.  END = v1 RELEASE.
  │
  ▼
Phase 12 (Self-Hosting)

Notes:
- Phases are linear — each phase builds on the previous
- Phase 7 splits into 7.1 (core codegen, done) and 7.2 (compiled stdlib type codegen + layout codegen). 7.2 owns memory layouts and minimum method sets; Phase 8 + Phase 11 own full API surface.
- Stdlib is split across two phases: **Phase 8** owns the floor (universal modules every program needs); **Phase 11** owns the long-tail (numerical/data-science stack, scripting helpers, security, embedded primitives, codegen IR optimization). Verification (Phase 9) and target backends (Phase 10) ship between the two so v1 is fully semantically locked and target-complete before the long-tail lands.
- Refinement types (Level 2) and contracts (Level 2.5) are committed — parsing complete (Phase 2), enforcement in Phase 9
- v1 release = end of Phase 11. Phase 12 (self-hosting) is post-v1.
- Phase 0 has no compiler dependency and can be done anytime
```
