# order_status — what Python misses, what Kāra catches

## Python (`python_buggy.py`)

- **mypy / pyright pass.** The if/elif/else chain is well-typed.
- **Three of four cases are correct.** Only `CANCELLED` falls through
  to the `else` branch, returning `"unknown status"`.
- **The bug is silent.** No exception, no log line, no test failure
  unless a test specifically exercises the cancelled-order path.
  Bug-discovery happens via downstream UI reports.
- **No Python static checker can flag this by default.** `Literal`
  + `assert_never` *can* enforce exhaustiveness — but only if the
  author opts in *and* doesn't write a default `return` / `else`
  fallback. The moment an `else` exists, the checker has nothing to
  flag, even though the author's intent was clearly "handle every
  case explicitly."

## Kāra (`solution.kara`)

The reference Kāra solution uses a `match` over `OrderStatus`. Match
exhaustiveness is enforced at the function body — the missing-case
bug is **structurally impossible to write**. Removing any one of the
four arms produces:

```
error[E0205]: non-exhaustive match: missing variants: Cancelled
   --> solution.kara:9:5
```

The compiler names the missing variant. The LLM's revision is a
mechanical insertion of one arm.

## Demo loop on this task

The harness's live run (recorded in `examples/mend/runs/`) shows the
LLM's first attempt typically:

1. **Iter 0**: writes a match with 3 of 4 arms (most often forgets
   `Cancelled`, the "edge case" variant). Compiler emits `E0205`
   naming the missing variant.
2. **Iter 1**: LLM reads the diagnostic, adds the missing arm with a
   reasonable description string, build clean.

The diagnostic carries no machine-applicable `replacement` field, so
`karac fix` does not run on this example — the round trip is
purely *descriptive*: the compiler tells the LLM *what is missing*,
the LLM decides *what to fill in*.

## Why this contrast matters

The pattern-exhaustiveness story is the AI-first thesis at its
sharpest:

> When a human writes a function over an enum, the temptation to leave
> a "default" branch is everywhere. When the language refuses, both
> humans and LLMs are forced to make every case explicit. The LLM
> can't paper over an unhandled variant with `"unknown status"` —
> the compiler will not let it.

This is the property that makes Kāra a credible target for
LLM-generated production code: not "the compiler is strict" as an
aesthetic, but "the compiler closes the silent-fallthrough class of
bugs that LLMs are statistically prone to writing."
