# user_lookup — what Python misses, what Kāra catches

## Python (`python_buggy.py`)

- **Default mypy / pyright pass.** Both natural idioms type-check.
- **Both natural idioms fail at runtime.** `users[user_id]` raises
  `KeyError`; `users.get(user_id).upper()` raises `AttributeError`
  on `None`. The safe shape (`users.get(user_id, default)`) requires
  the author to *remember* a second argument. Forgetting it is a
  one-character deletion with no checker complaint outside
  `--strict`.
- **mypy `--strict` does catch the `.get(...).upper()` shape**, but
  default-strict isn't the modal Python configuration. Most code
  ships under permissive settings.

## Kāra (`solution.kara`)

`Map.get(key)` returns `Option[V]`, not `V`. The unsafe idiom — return
the lookup result directly as the function's tail — fails to compile:

```
error[E0200]: expected 'String', found 'Option<String>'
   --> solution.kara:2:5
```

The compiler names both types in the error: *expected `String`*,
*found `Option<String>`*. The LLM's revision is a `match` (or
`unwrap_or`, or `?` propagation) at the lookup site.

## Demo loop on this task

The harness's live run shows the LLM's first attempt typically:

1. **Iter 0**: writes `users.get(user_id)` as the function's tail
   expression. E0200 fires from the typecheck phase.
2. **Iter 1**: LLM reads the diagnostic, wraps the lookup in a
   `match` with `None => "Unknown"` (or equivalent). Build clean.

The diagnostic carries no machine-applicable `replacement` field —
the fix is structural (insertion of a `match`/`unwrap_or` expression),
not a byte-range substitution. The round trip is purely *descriptive*:
the compiler tells the LLM the type mismatch, the LLM decides the
handling strategy.

## Why this contrast matters

Null-handling is the largest single source of runtime crashes in
modern dynamic-language production code. (Sentry's most common error
class across web apps is some flavor of `NoneType has no attribute`
/ `Cannot read property of undefined`.) `Optional[T]` exists in
Python's type system; it isn't enforced unless the project opts in
to strict mode AND avoids type-ignore comments. Kāra's `Option[T]`
isn't an annotation — it's a different type, and the type system has
no escape hatch that a tired developer (or an LLM rushing through a
task) can elide by default.

For LLMs specifically, this matters because LLMs are heavily biased
toward the modal Python pattern they were trained on. They write
`dict[key]` reflexively. Kāra's compiler refuses that pattern —
every lookup forces an explicit answer to "what does missing mean
here?"
