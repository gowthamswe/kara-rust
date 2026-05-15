# Appendix D: Attributes

Attributes are metadata attached to declarations. Two syntactic forms are supported:

```kara
#[attribute_name]               // marker
#[attribute_name(arg, ...)]     // with arguments
@attribute_name                 // shorthand marker (selected attributes only)
```

Attributes appear immediately before the item they annotate.

---

## Derive

### `#[derive(Trait, ...)]`

Generates trait implementations for the annotated `struct` or `enum`. See [Appendix C](appendix-c-derivable-traits.md) for the full list of derivable traits and their dependencies.

```kara
#[derive(PartialEq, Eq, Hash, Display, Clone)]
struct UserId { value: u64 }
```

---

## Lint control

### `#[allow(lint_name)]`

Suppress a specific lint within the annotated item. The lint fires nowhere inside the item.

### `#[warn(lint_name)]`

Ensure a lint is at warning level even if it would otherwise be suppressed.

### `#[deny(lint_name)]`

Promote a lint to a hard error within the annotated item.

**Available lint names:**

| Lint name | Default | What it checks |
|-----------|---------|----------------|
| `undocumented_unsafe` | warning | Every `unsafe { }` block must be preceded by a `// Safety:` comment |
| `ffi_float_eq` / `ffi_float_eq` | warning | Comparing an `extern "C"` float return with `==` or `!=` |
| `redundant_suffix` | warning | Literal suffix that matches the default type (e.g., `42i64`) |
| `mutual_recursion_note` | note | Note when the SCC pass detects a mutual-recursion group |
| `module_mut_binding` | warning (`lib` profile) | `let mut` at module scope |
| `layout_unassigned_fields` | warning | Fields not assigned to a `group` in a `layout` block |
| `repr_c_layout_ignored` | warning | `layout` block on a private struct (has no FFI effect) |
| `float_in_serialized_type` | warning | `f32`/`f64` field in a `#[derive(Serialize, Deserialize)]` type |
| `rc_fallback` | note | Compiler chose RC tier to satisfy ownership analysis |

---

## Safety

### `#[noblock]`  /  `@noblock`

On an `extern "C"` or `extern "C-unwind"` function: removes `blocks` from the default effect set. Use this for pure-CPU foreign functions (math routines, `strlen`, etc.) that are known not to block.

```kara
@noblock
extern "C" fn sqrt(x: f64) -> f64;
```

---

## Linker control

### `#[unsafe(no_mangle)]`

Use the Kāra identifier as the exported symbol name without any name mangling. Required when a foreign caller (C, linker script, debugger) must reference the symbol by its exact Kāra name. Does not imply `extern "C"` — the calling convention is independent.

The `#[unsafe(...)]` wrap is mandatory: disabling name mangling can collide with foreign symbols, an obligation the compiler cannot verify. Bare `#[no_mangle]` is rejected at parse time.

```kara
#[unsafe(no_mangle)]
pub fn kara_entry() { ... }
```

### `#[used]`

Prevent dead-code elimination for the annotated symbol even if no Kāra code references it. Use for linker-section entries, interrupt vectors, or other symbols that are referenced only from outside the compiler's visibility (linker scripts, hardware, debuggers). Stays plain (no `#[unsafe(...)]` wrap) — `#[used]` only suppresses DCE, no soundness obligation.

```kara
#[unsafe(link_section(".vectors"))]
#[used]
let interrupt_table: [fn(); 16] = [...];
```

### `#[unsafe(link_section("name"))]`

Place the annotated symbol in a named linker section. Required for embedded targets that map specific sections to specific memory regions (flash, DTCM RAM, etc.).

The `#[unsafe(...)]` wrap is mandatory: section placement carries layout and aliasing obligations the compiler cannot verify. Bare `#[link_section(...)]` is rejected at parse time.

```kara
#[unsafe(link_section(".dtcmram"))]
let fast_buffer: [u8; 1024] = [0; 1024];
```

---

## FFI

### `#[kara_name = "identifier"]`

On an `extern` item: rebinds a non-conforming foreign name to a valid Kāra identifier. The Kāra-visible name must follow the identifier case-class rules; the foreign name may be arbitrary ASCII.

```kara
#[kara_name = "GlxFbConfig"]
extern type GLXFBConfig;
```

---

## Embedded targets

### `#[interrupt]`

Mark a function as an interrupt service routine (ISR) entry point. The compiler emits `extern "interrupt"` ABI, sets up the ISR stack frame, and places the handler in the `.vectors` linker section. Valid on `embedded` profile builds only.

```kara
#[interrupt]
fn TIM2() {
    // handle timer interrupt
}
```

ISRs may not call `panic!`, allocate heap memory, or block. The effect checker enforces this at compile time. For an ISR that writes to a shared resource, wrap the resource in `Atomic[T]` or use a lock-free flag.

### `#[max_stack(N)]`  *(embedded profile)*

Assert that the annotated function's maximum stack depth (including all transitive callees) does not exceed `N` bytes. The compiler verifies this statically for `embedded` profile builds and emits an error if the bound cannot be guaranteed. Useful for ISR handlers, which run on a fixed-size interrupt stack.

```kara
#[interrupt]
#[max_stack(512)]
fn CAN1_RX0() { ... }
```

---

## Module-level bindings

### `#[thread_local]`

On a module-level `let mut` binding: gives each OS thread (and each task under the runtime) its own independent copy. The binding's initializer must still be a compile-time constant.

```kara
#[thread_local]
let mut request_count: i64 = 0;
```

---

## Memory layout

### `#[repr(C)]`

On a `struct`: lay out fields in C ABI order (declaration order, with C padding rules). Required for types passed through `extern "C"` boundaries.

### `#[repr(packed)]`

On a `struct`: remove all padding. Fields may be unaligned — use `unsafe` for pointer access to packed fields.

### `#[repr(align(N))]`

On a `struct` or as a wrapper type: require at least `N`-byte alignment.

---

## Functions

### `#[must_use]`  /  `#[must_use = "reason"]`

On a **type**: every binding site where a value of this type would be silently dropped produces a warning. Use for types that must be explicitly handled (e.g., a connection that must be closed).

On a **function**: the return value must not be silently discarded. `Result` return values are implicitly `#[must_use]`.

```kara
#[must_use = "connections must be explicitly disconnected"]
struct Connection { ... }
```

---

## GPU compute

### `#[gpu]`

Declare that a function uses only the GPU-safe subset of Kāra and may be called from a GPU kernel. The compiler validates the full call graph from each `#[gpu]` root: forbidden effects (`panics`, `allocates(Heap)`, I/O) are caught by the effect checker; forbidden structural features (heap types, recursion, dynamic dispatch, host-capturing closures) are caught during type checking. Dispatch to the GPU is always explicit via `gpu.dispatch` — `#[gpu]` is a constraint declaration, not a routing instruction.

```kara
#[gpu]
fn dot_product(a: Slice[f32], b: Slice[f32]) -> f32 { ... }
```

A generic function must be explicitly annotated with `#[gpu]` to be callable from a GPU kernel — GPU-callability is never inferred from the concrete type parameters.

---

## Shared types and RC

### `#[cyclic]`

On a `trait`: declare that the trait participates in ownership cycles. Any `shared struct` that holds a field of type `dyn Trait` (or a container of `dyn Trait`) for a `#[cyclic]` trait must use `weak` on that field. Without `#[cyclic]`, `dyn Trait` fields in `shared struct` are allowed without `weak`. In debug builds, a leak detector catches missed cycle annotations at program exit (compiled out in release).

```kara
#[cyclic]
trait Node {
    fn children(ref self) -> Slice[dyn Node];
    fn parent(ref self) -> weak dyn Node;
}
```

---

## Testing

### `#[test]`

Mark a `test_`-prefixed function as a test case.

### `#[test(requires = [resource, ...])]`

Mark a test that needs a live external resource. When the resource is unavailable, the test is skipped (or fails with `reason: "unsatisfied_requires"` when `karac test --all` is used).

### `#[property]`

Mark a `test_`-prefixed function as a property test. The framework generates random inputs via `Arbitrary` and runs the body for each, shrinking on failure.

### `#[snapshot]`

Mark a `test_`-prefixed function as a snapshot test. First run saves output; subsequent runs compare against the saved snapshot.

### `#[with_provider(resource_path, constructor_fn)]`

Supply an in-memory provider for a test. The provider scope wraps the entire test body. Multiple `#[with_provider]` attributes are allowed; source order is outer-to-inner.

---

## Post-v1 / reserved

### `#[generational_fallback]`  *(post-v1)*

On a `struct`: opt into generational reference semantics for values of this type instead of RC. A generational handle holds an index into a `Pool[T]`; the pool validates liveness before each access. When a value outlives all its borrows, the pool slot is reclaimed and the generation counter is incremented, making stale handles detectable. This is a future opt-in for graph workloads where RC overhead is measurable — `Pool[T]` with explicit handles is the v1 alternative.

---

## Serialization (post-v1)

### `#[serde(rename = "name")]`

On a field in a `#[derive(Serialize, Deserialize)]` type: use `"name"` as the serialized key instead of the field name.

### `#[serde(skip)]`

On a field: skip during both serialization and deserialization.

### `#[serde(skip_serializing)]`  /  `#[serde(skip_deserializing)]`

On a field: skip during one direction only.

### `#[serde(default)]`

On a field: use the field's `Default` value when the key is absent during deserialization.

### `#[serde(tag = "type")]`  /  `#[serde(untagged)]`

On an `enum`: use internally-tagged or untagged representation instead of the default externally-tagged form.
