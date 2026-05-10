# Task: order_status

## Prompt fed to the LLM

> Define an enum `OrderStatus` with four variants: `Pending`, `Shipped`,
> `Delivered`, `Cancelled`. Write a function `describe(status: OrderStatus)
> -> String` that returns a one-line human-readable description for each
> status:
>
> - `Pending` → `"awaiting payment"`
> - `Shipped` → `"in transit"`
> - `Delivered` → `"received by customer"`
> - `Cancelled` → `"cancelled by customer or merchant"`
>
> Include a `main()` that calls `describe(Shipped)` and `println`s the
> result.

## Why this task

It exercises a different end of the Mend loop than `welcome_emails`:
**pattern exhaustiveness** rather than ownership. The LLM's natural
mistake under this prompt is to write a `match` arm that handles three
of the four variants (the most common shape is to forget `Cancelled`,
since it's the "edge case"). The compiler returns `E0205` —
*"non-exhaustive match: missing variants: Cancelled"* — with the
specific variant named. The diagnostic carries no machine-applicable
replacement (an exhaustiveness fix is a structural insertion, not a
byte-range replacement), so this iteration is a *descriptive* round
trip: the LLM reads the message, adds the missing arm, recompiles
clean.

## What the Python contrast shows

`python_buggy.py` writes the same logic as a chained `if/elif/else`,
which `mypy` and `pyright` accept even when one case is missing. Under
the `else: return "unknown"` fallback, an unhandled status is silently
mapped to `"unknown"` — observable as a downstream display bug rather
than a localized type error. The Kāra version cannot compile in that
state; the missing case is rejected at the function definition, not
the call site.
