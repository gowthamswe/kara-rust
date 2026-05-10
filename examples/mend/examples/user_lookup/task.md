# Task: user_lookup

## Prompt fed to the LLM

> Define a function `get_display_name(users: Map[i64, String], user_id: i64)
> -> String` that looks up a user's display name in the `users` map. If
> the user ID is not present, return the string `"Unknown"` instead.
>
> In `main()`:
> 1. Create a `Map[i64, String]`.
> 2. Insert two users: ID `1` → `"Alice"`, ID `2` → `"Bob"`.
> 3. Call `get_display_name` with the map and ID `1`. Print the result.

## Why this task

It exercises a third end of the Mend loop: **null-handling
discipline**. The LLM's natural mistake under this prompt is to write
`users.get(user_id)` and return the result directly, the way one
would write `users[user_id]` or `users.get(user_id)` in Python or
JavaScript — pretending the lookup is total when it isn't. The
compiler catches this with `E0200` *"expected 'String', found
'Option<String>'"* at the function's tail expression.

The `Option[T]` return type from `Map.get` is not a stylistic choice;
it's a contract enforced at compile time. The LLM can satisfy the
contract any number of ways — `match`, `unwrap_or("Unknown")`, `?` if
the caller propagates — but it *cannot* pretend the value is always
present.

## What the Python contrast shows

`python_buggy.py` writes the equivalent in two natural Python idioms,
both of which `mypy` accepts:

1. `users[user_id]` — type-correct under `dict[int, str]`, raises
   `KeyError` at runtime if the ID is missing.
2. `users.get(user_id, "Unknown")` — type-correct AND functionally
   correct, but only because the author *remembered* the second
   argument. Drop it (`users.get(user_id)`) and the return type
   becomes `Optional[str]`; default mypy doesn't flag the `.upper()`
   call on it without `--strict`.

Kāra forces the question. The author cannot drop the default by
accident; the type system asks for an explicit handler at the lookup
site.
