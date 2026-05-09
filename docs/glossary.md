# Kāra — Technical Glossary

Terms and concepts used in the Kāra design docs. Grouped by topic for self-paced learning.

---

## Contents

- [Memory Management](#memory-management)
- [Concurrency](#concurrency)
- [Compiler Concepts](#compiler-concepts)
- [Language Design](#language-design)
- [Kāra-Specific Concepts](#kāra-specific-concepts)
- [Operating System Concepts](#operating-system-concepts)
- [Rust-Specific Terms](#rust-specific-terms-referenced-in-kāras-design)
- [Other Terms](#other-terms)

---

## Memory Management

**Ownership**
The rule that every value in memory has exactly one "owner" — the variable responsible for freeing it. When the owner goes out of scope, the memory is freed. Rust's core innovation. Eliminates use-after-free and double-free bugs without a garbage collector.

**Borrow / Borrowing**
Temporarily lending access to a value without transferring ownership. A "shared borrow" (`&T` in Rust, `ref T` in Kāra) allows reading. A "mutable borrow" (`&mut T` / `mut ref T`) allows writing. The original owner retains ownership.

**Lifetime (`'a`)**
Rust's way of tracking how long a borrow is valid. Written as `'a` annotations on function signatures. Kāra eliminates these by defaulting to owned returns and using `ref` for the rare borrow-return case.

**Reference Counting (RC)**
A memory management strategy where each value has a counter tracking how many references point to it. When the counter hits zero, the value is freed. Simple but adds ~2ns overhead per access. Kāra uses RC as a fallback when ownership can't be determined statically.

**RC Creep**
The gradual, often unnoticed accumulation of reference-counted values in a codebase. Each individual RC is harmless, but hundreds of them degrade performance. Especially risky with AI code generation, where the AI takes the easiest path (RC) instead of restructuring for zero-cost ownership.

**Rc vs Arc**
`Rc` = Reference Counting (single-threaded, cheap). `Arc` = Atomic Reference Counting (thread-safe, ~10x slower due to atomic CPU instructions). Kāra's compiler automatically uses `Arc` when a value crosses a concurrency boundary, `Rc` otherwise. Under `Arc`, `shared struct` is `Send` but not `Sync` — concurrent mutation requires `Mutex[T]`.

**Mutex (Mutual Exclusion)**
A synchronization primitive that ensures only one task can access data at a time. In Kāra, `Mutex[T]` wraps a value and access uses `lock` block syntax: `lock m { m.field += 1 }`. The lock is acquired on block entry and released on block exit (including early return, break, or panic). No `.lock()` method or guard values — scope is always visible. Required when multiple tasks need to mutate the same `shared struct` concurrently.

**RAII (Resource Acquisition Is Initialization)**
A pattern where resources (memory, file handles, mutex locks) are tied to variable scope. When the variable goes out of scope, the resource is automatically released. No explicit `close()` or `free()` calls needed. C++ and Rust use this extensively.

**Arena Allocation**
A memory management strategy where you allocate a big block of memory (the "arena") and hand out pieces of it. All allocations are freed at once when the arena is destroyed. Useful for graph structures where individual ownership is complex. Values reference each other via indices into the arena, not pointers. In Kāra, `shared struct` is the primary mechanism for trees and graphs; `Arena<T>` is available as a stdlib performance primitive for cache-friendly bulk allocation when needed.

**Garbage Collection (GC)**
Automatic memory management where a runtime periodically scans memory to find and free unreachable values. Used by Go, Java, Python. Simpler for the programmer but adds latency (GC pauses) and memory overhead. Kāra avoids GC entirely.

**Stack vs Heap**
Stack: fast, fixed-size, automatically freed when a function returns. Local variables live here. Heap: slower, dynamically sized, requires explicit management (ownership/RC/GC). `Vec`, `String`, and large data structures live here.

**AoS vs SoA (Array of Structs vs Struct of Arrays)**
Two ways to lay out a collection in memory. AoS: `[{x,y,z}, {x,y,z}, {x,y,z}]` — each struct is contiguous. SoA: `{[x,x,x], [y,y,y], [z,z,z]}` — each field is contiguous. SoA is faster when you only access some fields (better cache utilization). Kāra's `layout` blocks let you choose.

**Cache Line / Cache Miss**
CPUs don't read memory one byte at a time — they load 64-byte "cache lines." A cache miss happens when the data you need isn't in the CPU cache, forcing a slow main memory fetch (~100ns vs ~1ns for cache hit). Data layout directly affects cache miss rates.

---

## Concurrency

**Concurrency vs Parallelism**
Concurrency: dealing with multiple things at once (structurally). Parallelism: doing multiple things at once (physically, on multiple CPU cores). A single-core machine can be concurrent (switching between tasks) but not parallel.

**OS Thread**
A thread of execution managed by the operating system. Each thread has its own stack (~8MB). Context switching between OS threads is expensive (~1-10μs). Limited to ~10K concurrent threads before memory and scheduling overhead become problematic.

**Green Thread / Goroutine**
A lightweight thread managed by the language runtime, not the OS. Much smaller stack (~4-8KB). Go calls them goroutines. Enables millions of concurrent tasks. Downside: needs a runtime scheduler, complicates FFI.

**Work-Stealing Scheduler**
A concurrency strategy where each thread has its own task queue. When a thread's queue is empty, it "steals" tasks from other threads' queues. Provides good load balancing without a central coordinator. Used by Tokio (Rust), Go's runtime, and Kāra's planned runtime.

**Event Loop**
A programming pattern where a single thread waits for events (network data arriving, timers firing) and dispatches handlers. Instead of blocking a thread per connection, one thread handles thousands of connections by polling an OS facility (epoll/kqueue). Node.js is built on this model.

**epoll / kqueue / io_uring**
OS-level facilities for efficiently monitoring many file descriptors (sockets, files) for readiness. `epoll` is Linux, `kqueue` is macOS/BSD, `io_uring` is newer Linux (true async I/O with kernel-managed completion queues). These are what event loops are built on.

**Blocking I/O**
When a function call (like reading from a network socket) pauses the entire thread until the operation completes. Simple to program but wastes the thread while waiting. Kāra v1 uses this model.

**Non-Blocking I/O / Async I/O**
When an I/O operation returns immediately (or registers for later notification) instead of blocking the thread. The thread can do other work while waiting. Requires an event loop or async runtime to manage the waiting.

**Async/Await (other languages)**
A programming pattern (used in Rust, JavaScript, Python, C#) where `async` marks a function as potentially suspending, and `await` marks the suspension points. Creates "colored functions" — async and sync code can't easily call each other. Kāra eliminates both: there is no `async fn`, no `.await`, and no yield-point syntax. The compiler infers the `suspends` effect from the call graph and inserts yield points automatically. The programmer writes plain function calls; the compiler handles async machinery invisibly.

**Colored Functions**
The problem where async and sync functions are incompatible types. An async function can't be called from a sync context without a runtime. A sync function blocks the async executor if called from async context. This splits the ecosystem into two worlds. Kāra's effect system eliminates this by making all functions look the same.

**State Machine Transform**
When a compiler converts an async function into a state machine (an enum where each variant represents a suspension point and its captured local variables). This is how Rust compiles `async fn` — no runtime stack is needed, just a small struct. The complexity of this transform is why Rust's async took years to stabilize.

**Structured Concurrency**
The principle that concurrent tasks form a tree: a parent task spawns children, and the parent doesn't complete until all children complete (or are cancelled). Prevents "fire and forget" tasks that leak resources. Kāra uses this — when one branch fails, sibling branches are cancelled cooperatively (see "Cooperative Cancellation"). Two refinements: *completion wins cancellation* — a sibling that has already passed its last effect-boundary check when cancel fires completes naturally and its mutations/`defer` blocks are honored, so real work is never retroactively converted to `Cancelled`. And *cancellation cascades* into nested parallel regions through the same effect-boundary mechanism, without any special cross-scope machinery.

**Data Race**
A bug where two threads access the same memory concurrently, and at least one is writing. Results are unpredictable — the program may crash, corrupt data, or appear to work (making the bug intermittent and hard to find). Kāra's effect system prevents data races by serializing conflicting effects on the same resource.

**Deadlock**
When two or more threads are each waiting for the other to release a resource, so none can proceed. Thread A holds lock X and waits for lock Y; thread B holds lock Y and waits for lock X. Both wait forever.

**Fork-Join**
A concurrency pattern: spawn N independent tasks (fork), wait for all to complete (join), combine results. Kāra's auto-concurrency generates this pattern from non-conflicting effects.

**Backpressure**
When a system slows down producers to match the speed of consumers. If your code spawns 10K database queries but the connection pool only allows 10 concurrent connections, backpressure queues the excess. In Kāra, providers implement backpressure via connection pools and semaphores. v1 ships application-layer primitives (`Semaphore`, `BoundedChannel[T]`, `RateLimiter`) alongside the deployment-layer provider machinery — graduated from brainstorm v64.

**Backend-First**
Kāra v1's lead persona — the language ships at v1 with the floor needed to host real backend workloads (HTTP/1.1 server, TLS, WebSocket, 1M+ concurrent connections per process, `std.tracing`). Other personas (REPL, data-engineering, AR/WASM) compose on top of the same v1 floor rather than competing for runtime budget. Decided in brainstorm v64 (2026-05-09). See `design.md § v1 Positioning — Backend-First`.

**Idle-Keep-Alive**
A workload class — long-lived mostly-idle connections (WebSocket, SSE, long-poll, real-time messaging) where most connections are parked waiting for events rather than actively transferring data. The cliff that distinguishes "credible backend language" from "moderate-scale only": thread-per-connection cannot scale here at any stack size; only an event loop crosses the threshold. Drives Kāra's promotion of Phase 6.3 (network event loop) into v1.

**Network-Boundary Functions**
Functions whose inferred or declared effect set includes `sends(Network)` or `receives(Network)`. The state-machine transform applies to this bounded subset at v1 — not to arbitrary `suspends` functions. Routing happens via the effect system: callers of network-boundary functions automatically park on the event loop instead of blocking an OS thread.

**RAII-Across-Yield**
The rule that resources held across a suspension point (e.g., a `MutexGuard` held while a network call yields control) must release cleanly when the task is cancelled, when a panic unwinds, or when the task is destroyed in a non-completion path. v1 promotes this from a warning to a compile error for network-boundary functions — the compiler rejects code that holds a non-cancel-safe resource across a yield point. Decided in v64 (2026-05-09).

**M1 / M2 / M3 Staging**
The staged concurrency milestones for Kāra v1's pre-release: M1 (Phase 6.3 lands, 100K stable on flagship demo), M2 (polish layer 1, 250K stable), M3 (io_uring + cross-platform parity, 1M+ stable). Public v1 launch is gated on M3 — the headline number is consolidated reality, not a promise. Decided in v64.

**Flagship Demo**
The verification gate workload that pins the concurrency claim. v1 layers three demos: (1) minimal HTTP+WebSocket server proves 1M+ idle connections (P0 unconditional); (2) Parallax (full or "lite") proves auto-concurrency under realistic load (P0); (3) data-engineering pipeline (Kafka→S3→DuckDB-shape) proves the compounding-into-other-personas claim (P1). Demos are CI benchmark gates — regression > 5% on the steady-state number blocks merge.

---

## Compiler Concepts

**Lexer (Tokenizer)**
The first phase of a compiler. Reads raw source code characters and groups them into tokens (keywords, identifiers, numbers, symbols). `let x = 42;` becomes `[Let, Identifier("x"), Equal, Integer(42), Semicolon]`.

**Parser**
The second phase. Reads tokens and builds an Abstract Syntax Tree (AST) — a tree structure representing the program's grammar. Validates that the token sequence is syntactically valid.

**AST (Abstract Syntax Tree)**
A tree representation of source code. Each node is a language construct (function declaration, if statement, binary expression). The compiler operates on the AST for type checking, optimization, and code generation.

**Semantic Analysis**
The phase after parsing that checks meaning, not just syntax. Type checking ("you can't add a String to an i64"), name resolution ("does this variable exist?"), exhaustiveness checking ("does this match cover all enum variants?").

**Type Inference**
When the compiler figures out the type of a variable without the programmer writing it. `let x = 42;` — the compiler infers `x` is `i64`. Kāra similarly infers private function effects from their bodies. Parameter modes (`own` / `ref` / `mut ref`), by contrast, are *declared* at the signature — the compiler verifies usage against the declaration and surfaces a "would-be tighter mode" diagnostic, but does not pick the mode for you. See **Parameter Modes**.

**Monomorphization**
Compiling generic code by generating a separate, specialized copy for each concrete type used. `Vec<i32>` and `Vec<String>` become two different types with two different compiled functions. Zero runtime cost (no dynamic dispatch) but increases binary size and compile time.

**LLVM**
A compiler infrastructure project. Kāra (and Rust, Clang, Swift) don't generate machine code directly — they generate LLVM IR (Intermediate Representation), and LLVM's optimizer and code generator produce the final binary. LLVM handles optimization passes, register allocation, and targeting different CPU architectures.

**Tree-Walk Interpreter**
The simplest way to execute code: walk the AST node by node and evaluate each one directly. Slow but easy to build. Kāra uses this first to validate language semantics before investing in LLVM code generation.

**Recursive-Descent Parser**
A parsing technique where each grammar rule becomes a function. The function for "expression" calls the function for "term," which calls the function for "factor," etc. Simple to write by hand, easy to debug.

**Span**
Source location metadata attached to every token and AST node: line number, column, byte offset, length. Used for error messages that point to the exact location of the problem in source code.

**SCC (Strongly Connected Component)**
A maximal set of nodes in a directed graph where every node is reachable from every other node. In compiler analysis, SCCs identify mutual recursion groups — functions that call each other in a cycle. Kāra's effect inference processes SCCs in reverse topological order using Tarjan's algorithm, iterating to a fixed point within each SCC.

**Name Resolution (Resolver)**
The compiler phase that maps every identifier to its definition — determining which `x` refers to which variable, function, type, or module. Handles scoping rules (shadowing, block scopes), three-level visibility (`pub`, default, `private`), and import resolution.

**Incremental Compilation**
Only recompiling the parts of a program that changed. If you modify one function, only that function (and its dependents) are recompiled. Essential for large codebases. Kāra's public-boundary effect firewalls naturally bound recompilation scope.

**Canonicalization**
Producing a single "canonical" representation for equivalent inputs. Kāra's formatter (`karac fmt`) canonicalizes code so that two programs with the same meaning produce identical formatted output. This makes AI-generated diffs clean and reviewable.

**Compiler Query**
A specific optimization decision the compiler hedged on, surfaced back to the author as a structured entry in the queries report (`karac query queries`). Each query carries a stable ID, the decision site, the options the compiler considered with rationale per option, the default it picked, and the resolution surface (which attribute, written where, would pin the answer). Distinct from a *diagnostic* (which fires on suspected mistakes) and from an *optimization remark* (which is read-only with no structured response surface).

**Query Channel / Queries Report**
The mechanism by which Kāra closes the JIT-vs-AOT optimization gap: the AOT compiler enumerates residual decisions; the LLM author resolves them at authorship time via source annotations; resolved queries drop out of the next compile's report. PGO answers distribution-shaped questions; the query channel answers intent-shaped ones (hot path, specialization, escape-or-not). The two are complementary signals, not substitutes. See [design.md § Compiler Queries](design.md#compiler-queries).

**Hedged Decision**
An optimization point where the compiler picked a safe default but a tighter choice exists if spec context clarifies intent. RC fallback (`Rc` vs `Arc` vs owned), generic specialization, inlining, branch hints, layout choice, and auto-concurrency fork thresholds are all hedged decisions. The query channel surfaces them; resolution annotations bake the author's answer into source.

**Resolution Surface**
The specific attribute (or sidecar entry) that, when written on a given item, resolves a particular compiler query. Source annotations are the common case (`#[no_rc]`, `#[prefer_rc]`, `#[specialize(T = i64)]`, `#[inline]`, `#[likely]`, `#[fork_at(N)]`); a sidecar `karac.queries.toml` is the fallback for sub-item or per-call-site decisions where attribute syntax is awkward.

**Intent vs Distribution**
The boundary between what the query channel answers and what PGO answers. Intent-shaped questions come from program understanding ("is this the hot path?", "is this allocation expected to escape?") and are answerable from spec; distribution-shaped questions come from production traffic ("what fraction of inputs are ≤16 bytes?") and require runtime measurement. Kāra v1 ships the intent channel; PGO is deferred to post-v1.

**PGO (Profile-Guided Optimization)**
Compiler optimization that uses runtime measurement of a representative workload to drive code-generation decisions (block layout, inlining heuristic weights, branch hints, function ordering, register allocation priorities). Two flavors: **instrumented** (build with counters → run workload → counters dump to `.profdata` → re-build using `.profdata`) and **sample-based / AutoFDO** (no instrumented build; sample a release binary in production with `perf record` → convert with `create_llvm_prof` → re-build). Kāra defers PGO to P2; the v1 `llvm.expect` static-branch-hint emission is *not* PGO.

**`.profdata`**
LLVM's binary profile data format. Structural-hash-keyed for source-drift resilience. Concatenable across multiple workload runs via `llvm-profdata merge`. The canonical artifact format for both instrumented PGO and AutoFDO.

**AutoFDO (Sample-Based PGO)**
Profile-guided optimization driven by `perf`-collected production samples rather than instrumented builds. Lower deployment cost (no separate workload run, no instrumentation overhead) but higher source-drift sensitivity — the sampled binary and the rebuilt binary may not be byte-identical, and function-name stability across rebuilds becomes load-bearing. Linux kernel mainlined AutoFDO + Propeller in 2024; published numbers show ~5–10% over instrumented PGO on warehouse workloads.

**Hot-Swap (Runtime Code Replacement)**
Replacing the running version of a function (or module) without restarting the process. In Kāra's planned form (P2, confirmed under v64 backend-first): production binary collects PGO counters live → background recompile produces `v2.so` → running process `dlopen`s the new shared object and redirects function-pointer indirection to the new bodies → old bodies stay live until in-flight calls drain (RCU-style quiescence). Latency is minutes, not microseconds — fine for warehouse-scale services. Granularity is module-level, not function-level.

**Drain Protocol**
The rule for when old code can be unmapped after a hot-swap. Kāra's planned approach: tie to the `suspends` effect verb — loops with suspend points are drain-safe; loops without get a compile warning. RCU-style quiescence: wait until every thread has crossed at least one suspend point since the swap before retiring the old code.

**Runtime Monomorphization JIT**
Just-in-time compilation of a generic function body for a concrete type `T` only known at runtime — typically because `T` arrived via JSON/msgpack/protobuf deserialization, FFI, or dynamic plugin load. Kāra's planned form (P2): bitcode for `#[jit_template]`-annotated generics is embedded in the binary's `.kara_jit_template` section; on first call with an unseen `T`, runtime pulls the bitcode, runs Cranelift to produce native code, caches it. Effects, ownership, and trait bounds are AOT-checked on the *generic* body — JIT performs codegen substitution only, no fresh verification.

**W^X (Write-XOR-Execute)**
Memory-protection policy that disallows pages from being both writable and executable at the same time. Increasingly common in production: browsers (Chrome's V8 hardening), iOS, Android, hardened kernels, gVisor sandboxes, FIPS deployments. WASM lacks `mmap(PROT_EXEC)` entirely. Any in-process JIT (runtime monomorphization, hot-swap reload) cannot run in W^X-enforced environments — falls back to AOT-only.

**In-Process JIT**
A JIT compiler embedded in the running program rather than in a separate compiler tool. Kāra's planned in-process JIT (Cranelift-based, post-v1) supports runtime monomorphization (3.3) and may be shared with the REPL JIT (per archive/v62). Distinguished from out-of-process JIT (separate `karac` invocation that produces a `v2.so`) which is what continuous PGO + hot-swap uses.

**Code Cache**
LRU-bounded mapping from `(generic_def_id, type_arg_pattern)` to JIT-compiled native code, kept in writable+executable memory pages. Each entry is the cached output of one runtime-monomorphization JIT compile. Cache is invalidated on binary upgrade (since the IR ABI is version-pinned in v1).

---

## Language Design

**FFI (Foreign Function Interface)**
The mechanism for calling functions written in another language (usually C). Kāra uses C-compatible FFI to access OS syscalls and existing C/Rust libraries. FFI functions are the "trust boundary" of the effect system — the compiler can't verify their annotations.

**ABI (Application Binary Interface)**
The low-level calling convention: how function arguments are passed (registers vs stack), how return values are delivered, how structs are laid out in memory. C ABI is the lingua franca — almost every language can call C functions.

**Algebraic Data Types (ADTs)**
Types that combine "product types" (structs — this AND that) and "sum types" (enums — this OR that). Rust's `enum` with data is the canonical example. `Option<T>` is `Some(T) | None`. Pattern matching exhaustively handles all variants.

**Pattern Matching**
A control flow mechanism that destructures values and branches based on their shape. More powerful than `switch` statements — can match on nested structures, bind variables, and the compiler ensures all cases are handled.

**Traits**
Interfaces that types can implement. Define shared behavior (`Display`, `Add`, `Processor`) without inheritance. A function constrained by a trait bound (`fn f<T: Display>(x: T)`) works with any type that implements the trait.

**UFCS (Uniform Function Call Syntax)**
The rule that `user.validate()` and `User.validate(user)` are the same call. Methods are just functions with `self` as the first parameter. You can call them either way.

**Effect System**
A type-system feature that tracks what side effects a function can perform (read a database, send a network request, allocate memory). Kāra uses six verbs (`reads`, `writes`, `sends`, `receives`, `allocates`, `panics`) with user-defined resources. The compiler uses effects for automatic parallelization.

**Algebraic Effects (Koka-style)**
A more powerful form of effect system where effects can be "handled" — intercepted and reinterpreted. Requires delimited continuations (stack manipulation). Kāra simplifies this to trait-based injection — no continuations, no hidden control flow.

**Semver (Semantic Versioning)**
A versioning scheme: MAJOR.MINOR.PATCH. Major = breaking changes, Minor = new features (backwards compatible), Patch = bug fixes. In Kāra, adding an effect to a public function is a major (breaking) change; removing one is minor.

**Case Class (of an identifier)**
In Kāra, every identifier belongs to one of three grammar-level classes determined by its character pattern: **Type** (PascalCase — `UserAccount`, `IoError`), **Const** (all uppercase — `MAX_RETRIES`), or **Value** (snake_case or leading underscore — `read_to_string`, `_unused`). The compiler enforces the class to match the declaration position — a `struct` name must be Type-class, a function-body `let` binding must be Value-class, a module-level `let` binding must be Const-class, and so on. This serves two purposes: it makes `.`-separated path expressions lexer-deterministic (a Type-class leading segment starts a path; a Value-class leading segment starts a value expression), and it gives the ecosystem one uniform style without per-project bikeshedding. See `design.md § Identifiers and Naming`.

---

## Kāra-Specific Concepts

### Effect System Details

**Effect Resources**
User-defined names that represent external systems a function interacts with. Declared with `effect resource UserDB: DatabaseProvider;` and used in effect annotations: `reads(UserDB)`, `writes(OrderDB)`. Resources are backed by swappable trait implementations (providers), making them testable by design.

**Effect Groups**
Named bundles of effects that reduce annotation burden: `effect group Validation = reads(UserDB), sends(FraudService);`. A function can annotate with the group name instead of listing individual effects. Groups compose with `+`: `effect group OrderProcessing = Validation + Fulfillment;`. Adding to a non-`stable` group is a minor (non-breaking) semver change.

**Effect Polymorphism (`with E` / `with _`)**
When a function's effects depend on its arguments. `fn map[T, U, with E](list: Vec[T], f: Fn(T) -> U with E) -> Vec[U] with E` — `E` stands for "whatever concrete effects the caller provides." Named variables (`with E`) can be referenced elsewhere in the signature; the anonymous wildcard (`with _`) is shorthand when threading is not needed. Underlying rule: effect sets are ordered by subset inclusion, and function types are covariant in their effect set — a function with fewer effects is a subtype of one with more. That's why a pure or narrowly-effectful closure can be passed where a wider effect slot is expected.

**Transparent Effects**
Effects that don't participate in conflict analysis and don't propagate through function signatures. Declared with `transparent effect verb traces;`. Logging, tracing, and metrics use this — they never block parallelization. Transparent verbs are purely documentary and cannot appear inside effect groups.

**Provider Injection (`with_provider`)**
Kāra's mechanism for binding an effect resource to a concrete implementation at runtime: `with_provider[UserDB](InMemoryUserDB.new(), || { ... })`. Inside the block, any code that uses `reads(UserDB)` or `writes(UserDB)` is routed to the provided implementation. Providers are swapped for testing — production uses Postgres, tests use an in-memory impl. Provider-rooted resources cannot escape their provider scope — closures that capture the resource may not be returned, stored, channel-sent, or handed to a spawned task that outlives the block. This is what preserves test isolation and predictable teardown.

**Parameterized Resources**
Effect resources with a compile-time partition key for finer-grained conflict analysis: `effect resource UserDB[user_id: i64];`. Two operations on `UserDB[42]` and `UserDB[99]` are provably independent and can run in parallel; two operations on `UserDB[42]` conflict and are serialized.

**`stable` Effect Groups**
A group marked `stable` promises its effect set will not grow: `stable effect group Validation = ...;`. Adding an effect to a `stable` group is a compile error. Used when the group represents a fixed, closed contract.

### Type System Features

**Refinement Types**
Types with attached value constraints: `type NonZero = i32 where self != 0`. The constraint is checked at construction (via `try_from`); once a value has the refined type, the constraint is guaranteed. No SMT solver — constraints are simple predicates (numeric comparisons, `self.len()`). Widening (refined → base) is implicit and free; narrowing (base → refined) requires explicit `try_from`.

**Distinct Types (Newtypes)**
Zero-cost wrapper types that prevent mixing structurally identical values: `distinct type UserId = i64`. A `UserId` and a `PostId` are both `i64` underneath but cannot be used interchangeably — the compiler rejects it. No operations carry through by default; opt in with `#[derive(Eq, Hash)]`. Can combine with refinements: `distinct type ValidPort = u16 where self >= 1 and self <= 65535`.

**Associated Types**
Named output types in traits that are fixed by the implementing type, not supplied by the caller: `trait Iterator { type Item; fn next(mut ref self) -> Option[Self.Item]; }`. The implementor binds `Item` once. This avoids redundant type parameters — the caller writes `I: Iterator`, not `I: Iterator[i64]`. Accessed via projection: `I.Item`.

**Never Type (`Never`)**
The bottom type — a type with no values. Returned by `todo()`, `unreachable()`, `panic()`, and infinite `loop` blocks with no `break`. `Never` coerces to any type, so these expressions are valid in any position (match arm, `if` branch, `let` initializer).

**`Copy` and `Clone`**
Two traits for value duplication. `Copy` means bitwise-copyable with no side effects — the compiler silently copies instead of moving. All primitives are `Copy`. `Clone` means explicitly duplicable — may involve heap allocation (e.g., cloning a `Vec`). `Copy` implies `Clone`. User types opt in via `#[derive(Copy, Clone)]`; the compiler rejects `Copy` if any field is not `Copy`.

**Orphan Rules**
A crate may only write `impl Trait for Type` if it defines either `Trait` or `Type`. Prevents two crates from providing conflicting impls for the same `(Trait, Type)` pair. The escape hatch is wrapping in a local newtype (distinct type).

### Ownership and Memory

**`shared struct` / `shared enum`**
Types with reference semantics — assignment shares (RC increment) instead of moving. Used for inherently shared data: trees, graphs, linked lists. Fields are immutable by default; `mut` opts in per field. Assignment creates a shared reference, not a copy. `shared enum` supports recursive types like JSON or AST nodes.

**`weak` References**
Cycle-breaking annotation on `shared struct` fields: `mut neighbors: Vec[weak GraphNode]`. Strong → weak conversion is implicit on assignment. Accessing a `weak` field yields `Option[T]` — `None` if the referent was deallocated. No runtime cycle collector; `weak` is the only mechanism for back-edges. For `dyn Trait` fields in `shared struct`, `weak` is enforced by the compiler when the trait is marked `#[cyclic]`.

**`#[cyclic]` Annotation**
Marks a trait whose implementations may form reference cycles through `dyn Trait` in `shared struct`. When a trait is `#[cyclic]`, the compiler requires `weak` on `shared struct` fields of that trait's `dyn` type. Most traits don't need this — it applies to bidirectional patterns like trees with parent pointers or observer systems. A debug-mode leak detector catches missed cases (compiled out in release builds).

**`Pool[T]` and `Handle[T]`**
Generational arena for indexed-access patterns. `Pool[T]` is a collection; `Handle[T]` is a typed `(index, generation)` pair. `pool.get(handle)` returns `Option[ref T]` — `None` if the handle is stale (the slot was reused). Used for ECS engines, compilers (AST arenas), connection IDs. Distinct from `shared struct` (which is for inherently shared graph data) and `Arena[T]` (which is for bulk allocation).

**Three-Level Visibility**
Kāra's visibility model: `pub` (visible to end users — public API), default/no keyword (visible to all files in the project but not to end users), and `private` (visible within the same directory only). Directories are organizational folders for namespacing — they don't affect visibility beyond the `private` keyword. The directory structure defines the module tree implicitly; no `mod` declarations are needed.

**Parameter Modes**
The three forms a parameter type can take in a function signature: `T` (owned — caller transfers ownership), `ref T` (immutable borrow), `mut ref T` (mutable borrow). Modes are *declared* at the signature, never inferred — the signature is the contract callers depend on. Body-level analysis verifies the declared mode is consistent with usage and surfaces a `karac explain` "would-be tighter mode" diagnostic when a stricter mode would also be valid. The programmer chooses whether to apply the suggestion; the compiler does not silently rewrite signatures.

**`#[must_use]`**
Attribute on a type or function that warns when a value is silently dropped: `#[must_use = "connections must be explicitly disconnected"]`. Completes the typestate pattern by ensuring a value reaches a terminal state. `Result` return values are implicitly `#[must_use]`.

### Control Flow and Syntax

**`defer` / `errdefer`**
Scope-exit cleanup statements. `defer expr` runs when the scope exits regardless of how. `errdefer expr` runs only on the error path (`?` propagation or `return Err(...)`). Multiple `defer` statements execute in reverse order (LIFO). No `?` inside defer blocks. Replaces the need for RAII wrapper types in many cases.

**Pipe Operator (`|>`)**
Left-to-right function chaining: `raw |> normalize |> parse |> validate`. Passes the left-hand value as the first argument to the right-hand function. Use `_` as a placeholder when the value isn't the first argument: `data |> filter(_, is_valid)`. Complementary to UFCS method chaining.

**`collect_all`**
Concurrency primitive that runs all branches to completion regardless of failures. Returns a tuple of `Result` values. Contrast with auto-concurrency's default fail-fast behavior (cancel siblings on first error). Use for multi-field validation, parallel pre-flight checks, or fan-out fetches where all errors are actionable. Compiler builtin, max 8 branches.

**`seq` Block**
Opt-out of auto-parallelism: `seq { step_a(); step_b(); step_c(); }`. Forces source-order execution when there is a semantic ordering requirement the effect system cannot see (protocol steps, hardware register sequences). Use sparingly — prefer expressing dependencies via data flow when possible.

### Compilation and Tooling

**Project Profiles**
Project-wide effect constraints declared in `kara.toml`: `no_effects = ["allocates(Heap)", "panics"]`. Built-in profiles include `kernel` (no heap, no panics, no std) and `embedded` (no heap, no panics, no concurrency). Custom profiles are supported. The compiler treats a profile as a global `#[no_effect(...)]` applied to every function.

**Typestate**
A pattern (not a language feature) where a generic type parameter encodes protocol state at compile time: `Connection[Disconnected]` vs `Connection[Connected]`. The state parameter is phantom (zero-size, no runtime cost). Ownership ensures state cannot be forked — consuming the old state prevents use-after-transition. Combined with `#[must_use]` to prevent forgetting terminal states.

**`#[tailrec]`**
Attribute that guarantees tail call elimination. The compiler verifies every recursive call is in tail position; if not, it's a compile error. Without `#[tailrec]`, LLVM may or may not optimize tail calls. With it, stack overflow is impossible regardless of input size.

### Embedded and Hardware

**Volatile Access (MMIO)**
Memory-mapped I/O registers must be read/written with volatile semantics — the compiler must not reorder, coalesce, or eliminate these accesses. Two `unsafe` intrinsics: `volatile_read` and `volatile_write`. `VolatileCell[T]` is a stdlib wrapper for ergonomic register map definitions. Both imply `reads(Hardware)` / `writes(Hardware)` effects.

**`Atomic[T]`** (Phase 10 — deferred)
A `shared struct` for lock-free inter-context communication. Methods: `load`, `store`, `swap`, `compare_exchange`, `fetch_add`. Each takes an `Ordering` parameter (`Relaxed`, `Acquire`, `Release`, `AcqRel`, `SeqCst`) matching the C11 memory model. Available in the `embedded` profile for ISR-to-main signaling. Full spec in [deferred.md](deferred.md).

**Critical Section**
Temporarily disabling interrupts via an RAII guard: `let _guard = critical_section.acquire();`. Interrupts are re-enabled when the guard is dropped. The guard type is `#[must_use]` — accidentally discarding it immediately re-enables interrupts.

**`#[interrupt]`**
Attribute that marks a function as an interrupt service routine: `#[interrupt(TIMER1)]`. The compiler emits the correct calling convention (all registers saved/restored), places the function in the vector table, and forbids direct calls from normal code. Implicitly applies ISR profile restrictions (no heap, no panics).

---

## Operating System Concepts

**Syscall (System Call)**
A request from a program to the OS kernel. Reading a file (`read()`), opening a network connection (`connect()`), getting the current time (`clock_gettime()`) — all syscalls. They're the boundary between user code and the OS. Kāra's primitive effect resources map directly to syscall categories.

**File Descriptor (fd)**
An integer that represents an open file, socket, or pipe in Unix/Linux. `stdin` is fd 0, `stdout` is fd 1, `stderr` is fd 2. Network sockets are also file descriptors. The event loop monitors file descriptors for readiness.

**mmap / brk**
Syscalls for allocating memory from the OS. `brk` extends the heap. `mmap` maps a region of virtual address space. These are what `malloc()` uses internally. In Kāra, `allocates(Heap)` maps to these syscalls.

**Virtual Address Space**
Each process gets its own view of memory, as if it has the entire address space to itself. The OS and CPU translate virtual addresses to physical addresses. This is why each thread's ~8MB stack doesn't literally consume 8MB of RAM (it's virtual until touched).

**Context Switch**
When the OS switches from running one thread to another. The CPU saves the current thread's registers, loads the next thread's registers, and resumes execution. Costs ~1-10μs and can flush CPU caches, making subsequent memory accesses slower.

---

## Rust-Specific Terms (referenced in Kāra's design)

**Pin**
A Rust type that prevents a value from being moved in memory. Required for self-referential types (a struct with a field that points to another field in the same struct). Notoriously confusing. Kāra avoids this by using `shared struct` for shared/recursive data and Arena allocation for performance-critical paths.

**Tokio**
The most popular async runtime for Rust. Provides an event loop, work-stealing scheduler, and async I/O. ~50K lines of code. Kāra's v1.1 runtime is conceptually similar but compiler-managed rather than library-managed.

**`unsafe` Block**
A Rust construct that disables certain compiler safety checks (ownership, bounds checking, type safety) within a delimited block. Required for raw pointer operations, FFI, and some performance-critical code. Kāra has the same concept — `unsafe` disables ownership checks but NOT the effect system.

**Borrow Checker**
Rust's compile-time system that enforces ownership and borrowing rules. Prevents data races, use-after-free, and dangling references. The source of most "fighting the borrow checker" complaints. Kāra's tiered ownership is designed to be less restrictive while maintaining safety.

**`dyn Trait` (Trait Object)**
Runtime polymorphism in Rust. A `dyn Processor` is a pointer to any type that implements `Processor`, with a vtable for dynamic dispatch. Unlike generics (monomorphized, zero-cost), trait objects have runtime overhead but allow heterogeneous collections.

---

## Other Terms

**SMT Solver (Z3)**
A tool that checks whether a logical formula can be satisfied. Used by formal verification systems (Dafny, F*) to prove program correctness. Kāra's deferred Level 3-4 verification would require Z3. ~1M lines of C++, can time out on complex constraints.

**Comptime**
Zig's term for compile-time function evaluation. Functions execute during compilation, producing constants, lookup tables, or specialized code. Eliminates the need for a separate macro system. Deferred in Kāra.

**Tagged Union**
How enums are compiled: a "tag" byte (which variant is active) followed by the variant's data. `Option<i64>` might be: tag=0 for None, tag=1 + 8 bytes of i64 for Some. The compiler generates `match` as a branch on the tag.

**Vtable (Virtual Table)**
A table of function pointers used for dynamic dispatch. When you call a method on a trait object (`dyn Trait`), the runtime looks up the correct function pointer in the vtable. Adds one pointer indirection per call.

**Zero-Cost Abstraction**
An abstraction that has no runtime overhead compared to hand-written code. Rust's generics (monomorphized), iterators (optimized away), and ownership (compile-time checks only) are zero-cost. Kāra's effect system aims to be zero-cost — effects are checked at compile time and erased in the binary.

**Cooperative Cancellation**
A cancellation strategy where the cancelled task checks a flag at designated points and stops itself. Contrast with preemptive cancellation (`pthread_cancel`), which forcibly stops a thread (unsafe — can leave resources leaked). Kāra inserts cancellation checks at effect boundaries. The same mechanism carries cancellation from an outer parallel scope into nested inner scopes — the outer cancel becomes an effect-boundary observation inside the child, which then fails fast and propagates to its own siblings. Worst-case latency is the sum of effect-boundary distances along the nesting path.
