# welcome_emails — what Python misses, what Kāra catches

## Python (`python_buggy.py`)

- **mypy / pyright pass.** Every type annotation is correct.
- **Single-threaded run is correct.** The bug is invisible without
  contention.
- **Multi-threaded run undercounts.** `self.sent += 1` is two
  bytecodes; a thread switch between read and store drops the
  increment. Output is non-deterministic, typically 95-99% of the
  expected count.
- **No Python static checker can flag this.** The types are right.
  The control flow is right. The bug is a property of *shared mutable
  state under concurrency* — outside the type system's remit.

## Kāra (`solution.kara` — current scaffold)

The current scaffold is the **simplest end of the loop**: a sequential
program with two resolver-level typos, both auto-fixed by `karac fix`.
It demonstrates the mechanism — JSON diagnostics with `replacement`
spans → mechanical application → clean build — without yet engaging
the effect system.

## What changes in later corpus examples (slice 2+)

A future `concurrent_emails` example will pose the same task with a
parallelism ask. The LLM's natural attempt — fan out the
`send_welcome` calls to run concurrently while sharing a `SentCount`
resource — produces an auto-par diagnostic:

```
error: cannot parallelize: branches conflict on writes(SentCount)
  --> concurrent_emails.kara:42:5
   |
42 |     send_welcome(1);
   |     ^^^^^^^^^^^^^^^ branch 0 writes SentCount
43 |     send_welcome(2);
   |     ^^^^^^^^^^^^^^^ branch 1 writes SentCount  (conflict)
   |
   = note: two parallel branches that both write the same resource race.
   = help: either run sequentially, or shard the counter into per-branch
           resources and merge after the join.
```

This is the demo's punchline: *the same shape that races silently in
Python is rejected at compile time in Kāra*. The LLM either accepts
sequential execution or restructures to a shardable pattern — the
compiler forces the question.

## Why the contrast matters

> "A language designed to be written by AI" isn't a slogan if the
> compiler can't keep an LLM honest. The LLM is incentivized to write
> code that *looks* like what works in the languages it was trained on
> (Python, JavaScript, Go). Kāra's compiler is the part that catches
> the patterns that *look right* but aren't.
