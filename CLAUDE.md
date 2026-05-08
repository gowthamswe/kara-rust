# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                            # Build the compiler (no LLVM backend)
cargo test                             # Run non-codegen tests (lexer, parser, resolver, typechecker, effect, ownership, interpreter)
cargo test --features llvm             # Run ALL tests including codegen E2E + memory_sanitizer (ASAN)
cargo test lexer                       # Run a single test file (e.g., tests/lexer.rs)
cargo test -- test_name                # Run a single test by name
cargo clippy --all --tests -- -D warnings  # Lint (must be clean before declaring work done)
cargo fmt --all                        # Format all files
cargo fmt --all -- --check             # Verify formatted (must be clean before declaring work done — peer to clippy)
```

**`cargo fmt --all -- --check` is a hard pre-commit gate, peer to clippy.** Both must clear before any commit lands. **First action of any new coding session or slice:** run `cargo fmt --all -- --check`. If it fails, fix with `cargo fmt --all` and land as a standalone `chore: cargo fmt cleanup` commit *before* starting feature work. Don't pull fmt drift into a feature commit; don't surgically revert drift to keep a commit scoped — both patterns push cleanup to CI and let drift accumulate in the meantime.

**Codegen and memory-sanitizer tests are gated on `--features llvm`.** Plain `cargo test` will skip `tests/codegen.rs`, `tests/par_codegen.rs`, and `tests/memory_sanitizer.rs` entirely (the modules are `#[cfg(feature = "llvm")]`). Always use `--features llvm` when verifying codegen-related work; otherwise you will miss real regressions.

**Codegen E2E + memory_sanitizer require the runtime library.** One-time setup on a fresh checkout:

```bash
cargo build -p karac-runtime --release   # produces target/release/libkarac_runtime.a
```

Without this, the E2E tests (including all `tests/memory_sanitizer.rs` cases) skip with a stderr notice rather than exercise real binaries — they pass vacuously. `tests/memory_sanitizer.rs` additionally requires a `cc` that supports `-fsanitize=address`; if missing (or if `KARAC_SKIP_ASAN_TESTS=1` is set), it skips gracefully.

## Architecture

`karac` is a Rust implementation of the Kāra language compiler. The pipeline flows:

```
Source → Lexer → Parser → AST → Resolver → TypeChecker → EffectChecker → OwnershipChecker → Interpreter
```

Each phase is a separate module under `src/`:

| Module | Role |
|---|---|
| `token.rs` | Token/Span definitions used across all phases |
| `lexer.rs` | Tokenizes source into `Vec<SpannedToken>` |
| `ast.rs` | AST node definitions; every node carries a `Span` |
| `parser.rs` | Recursive-descent parser; produces `ParseResult` with error recovery |
| `resolver.rs` | Name resolution, scope analysis, visibility checking |
| `typechecker.rs` | Type inference, generic instantiation, trait bound checking, pattern exhaustiveness |
| `effectchecker.rs` | Effect inference for private fns; effect verification for public fns; conflict detection |
| `ownership.rs` | Parameter mode inference (own/ref/mut ref), move checking, RC fallback detection |
| `interpreter.rs` | Tree-walk interpreter (Phase 4, in progress) |
| `lib.rs` | Public API — thin wrappers that chain phases together |

The entry point for programmatic use is `src/lib.rs`, which exposes `tokenize`, `parse`, `resolve`, `typecheck`, `effectcheck`, and `ownershipcheck` as top-level functions.

Integration tests live in `tests/` (one file per phase). End-to-end `.kara` programs live in `examples/`.

## Language Design

The language spec lives in `docs/design.md` (authoritative). Implementation plan in `docs/roadmap.md`.

Key Kāra language concepts the compiler must implement:

- **Generics syntax:** `[T]` not `<T>` — `Vec[i32]`, `fn sort[T: Ord](...)`. No turbofish.
- **Effects:** Eight built-in verbs — six *resource verbs* (`reads`, `writes`, `sends`, `receives`, `allocates`, `panics`) that drive conflict analysis and two *execution verbs* (`blocks`, `suspends`) that drive scheduler placement. Resource verbs apply to user-defined resources; execution verbs take no resource parameter. Private function effects are *inferred*; public function effects are *declared and verified*.
- **Ownership tiers:** owned (default) → `ref` → RC. Parameter modes are always declared at the signature — bare `T` is owned, `ref T` / `mut ref T` / `mut Slice[T]` are explicit borrow forms; bare `self` / `ref self` / `mut ref self` follow the same rule for receivers. Body-level ownership analysis is a checking aid (verifies usage matches the declared mode, drives `karac explain` "would-be mode" diagnostics, feeds use-site classification for the RC fallback pass) — it is not a signature-derivation mechanism.
- **Call-site mutation markers:** free-function calls write `mut` on arguments whose place-expression root is a fresh owned binding (or a temporary / literal / function return) when the callee's parameter is `mut ref T` / `mut Slice[T]`. Arguments rooted at a `mut ref` binding already in scope forward without marking. Method calls, field assignment, and index assignment never mark. `ref` is never legal at call sites. See design.md Feature 4 Part 1½.
- **`shared struct`/`shared enum`:** reference-semantics types using RC.
- **Layout blocks:** separate logical struct definition from physical memory layout (SoA, field grouping for cache locality).

## Coding Standards

- Idiomatic Rust; follow `rustfmt` conventions.
- Every compiler phase must emit structured diagnostics with source spans — never just panic.
- Tests for every language construct. Use `tests/` for integration tests, unit tests inside each module for focused coverage.
