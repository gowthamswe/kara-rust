# Task: welcome_emails

## Prompt fed to the LLM

> Write a Kāra program that sends a welcome message to a list of user
> IDs and prints how many were sent. Define:
>
> - `send_welcome(user_id: i64)` — prints a welcome line for the given user.
> - `count_users(user_ids: Vec[i64]) -> i64` — returns the count.
> - `send_all(user_ids: Vec[i64]) -> i64` — sends to each user, returns the count.
> - `main()` — pushes 1, 2, 3 onto a `Vec[i64]`, calls `send_all`, prints `"done"`.
>
> The compiler will check your code with `karac check --output=json` and report
> structured diagnostics. If any errors carry a `replacement` field, run
> `karac fix` to apply them mechanically. Patch any descriptive errors yourself
> and re-check.

## Why this task

It exercises the simplest end of the Mend loop: resolver-level typos with
machine-applicable `did_you_mean` replacements. The reference LLM completion
for this task makes two typos (`count_user` instead of `count_users`,
`send_welcom` instead of `send_welcome`) — both auto-fixed by `karac fix` in
a single pass, demonstrating that for the *easy* class of LLM mistakes the
loop converges with zero LLM reasoning cost.

Later examples in the corpus will exercise the harder shapes (effect-mismatch
diagnostics that require the LLM to read inferred-effect output and patch
manually; ownership-mode mismatches where the suggested fix is descriptive).
