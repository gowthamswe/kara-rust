# Kāra

<p align="center">
  <img src="docs/img/wip.png" alt="Work In Progress" width="150">
</p>

**A language where the programmer declares *what* and *why*, and the compiler decides *how* and *where*.**

```
  compiling the compiler
  [▓▓▓▓▓▓▓▓▓▓▒▒▒▒░░░░░░]
```

Kāra is an experimental systems programming language designed for the AI era. The compiler handles memory layout and concurrency; the programmer handles intent — and hardware targets, like GPU, when they matter.

Questions, ideas, or design feedback? [Start a GitHub Discussion](https://github.com/gowthamswe/karac-rust/discussions/new/choose) — all input welcome.

---

## What Makes Kāra Different

### Effect System — No Async/Await, No Colored Functions

Every function declares what it does to the world. The compiler uses this for automatic parallelization:

```
effect resource UserDB: DatabaseProvider;
effect resource OrderDB: DatabaseProvider;

fn load_dashboard(user_id: u64) -> Dashboard
    reads(UserDB), reads(OrderDB), reads(NotifDB) {

    let profile = fetch_profile(user_id);       // reads(UserDB)
    let orders = fetch_orders(user_id);         // reads(OrderDB)
    let notifications = fetch_notifs(user_id);  // reads(NotifDB)

    // Compiler sees non-conflicting effects → runs all three concurrently
    // Data dependency on all three → inserts sync point here
    build_dashboard(profile, orders, notifications)
}
```

No `async fn`. No colored functions. No `Promise.all`. The compiler handles concurrency because it understands effects and data dependencies.

### Tiered Ownership — No Lifetime Annotations

Rust's ownership model without `'a` noise:

```
// Parameter modes are declared at the signature: bare T is owned,
// ref T / mut ref T are explicit borrows. No lifetimes required.
fn process(data: Data, config: ref Config) -> Summary {
    let result = transform(data, config.threshold);
    //                     ^^^^ consumed (owned)
    //                           ^^^^^^ read through borrow
    result.summarize()
}

// Zero-copy returns borrow from a parameter — no 'a annotation needed.
fn first_word(s: ref String) -> ref String {
    s.split(' ').first()
}
```

Escalation path: owned → `ref` → RC. Each step is an explicit choice, not a compiler surprise.

### Data Layout Separation

Logical structure stays clean. Physical layout is a separate, opt-in concern:

```
struct Entity {
    id: u64, name: String,
    position: Vec3, velocity: Vec3,
    health: f32, armor: f32, is_alive: bool,
}

layout entities: Collection<Entity> {
    group physics { position, velocity }   // hot path: physics tick
    group combat { health, armor, is_alive } // hot path: combat
    group metadata { id, name }              // cold
}
```

### AI-First Compiler Interface

All compiler output available as structured JSON with machine-applicable fix diffs. Compiler query API for programmatic access to effect inference, ownership decisions, and concurrency analysis. Canonical formatter for clean semantic diffs.

## Docs

- **[docs/design.md](docs/design.md)** — The language specification. Authoritative source for all committed design decisions.
- **[docs/syntax.md](docs/syntax.md)** — Syntax reference and quick lookup.
- **[docs/glossary.md](docs/glossary.md)** — Terminology used across the design and compiler.
- **[docs/roadmap.md](docs/roadmap.md)** — Compiler implementation plan, phase by phase.
- **[docs/implementation_checklist/](docs/implementation_checklist/)** — Items to validate, benchmark, or revisit during specific phases.
- **[docs/deferred.md](docs/deferred.md)** — Committed designs for deferred features (P1: decided/non-breaking, P2: speculative).
- **[docs/demo_ideas.md](docs/demo_ideas.md)** — Programs that showcase Kāra's differentiating features.

## Project Status

Actively developed, pre-1.0. The frontend, interpreter, query API, auto-concurrency runtime, and LLVM codegen are in place; the standard library is being filled in. End-to-end compilation works for a growing subset of the language. See [docs/roadmap.md](docs/roadmap.md) for the current phase breakdown.

We took a **tree-walk interpreter first** approach: language semantics were validated with an interpreter before LLVM code generation.

## Prior Art

| Language/System | What Kāra takes |
|---|---|
| **Rust** | Ownership, enums, pattern matching, traits, `Result<T,E>` |
| **Koka** | Algebraic effect system (simplified: no handlers, trait injection instead) |
| **Zig** | Memory layout control, comptime (deferred) |
| **Go** | Simple concurrency model (blocking I/O on threads) |
| **Swift** | Inferred reference counting (as fallback, not primary) |
| **Unity DOTS / Bevy** | Data-oriented design, SoA layouts |

## Getting Started

```bash
cargo build                          # build the compiler (no LLVM backend)
cargo test                           # run the front-end tests (lexer, parser, resolver, typechecker, effect, ownership, interpreter)
cargo test --features llvm           # also run codegen E2E and memory-sanitizer tests
cargo clippy --all --tests -- -D warnings   # lint
cargo fmt                            # format
```

Codegen E2E tests (`tests/codegen.rs`, `tests/par_codegen.rs`, `tests/memory_sanitizer.rs`) are gated on `--features llvm` and need the runtime library built once via `cargo build -p karac-runtime --release`. The memory-sanitizer suite additionally needs a `cc` toolchain that supports `-fsanitize=address`; it skips gracefully on hosts that don't.

See [docs/roadmap.md](docs/roadmap.md) for current progress and [docs/design.md](docs/design.md) for the language specification.
