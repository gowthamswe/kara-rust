# Mend asciinema casts

Recorded terminal casts of the Mend live loop converging on
representative examples from the corpus.

## Files

| Cast                     | Example         | Compiler axis              | Iterations (recorded run) |
|--------------------------|-----------------|----------------------------|---------------------------|
| `welcome_emails.cast`    | `welcome_emails` | Ownership (use-after-move on `Vec`) | 3 |
| `order_status.cast`      | `order_status`   | Pattern interpretation (qualified path → bind, W0237) | 2 |
| `demo.sh`                | (driver)         | Narrated wrapper around `mend.py` for recording use | — |

The recorded iteration counts are *one observed run each* — the live
loop is non-deterministic. Re-recording will produce different
iteration counts depending on how the LLM responds to the prompt and
the compiler's diagnostic. Two iterations is a clean ideal; three to
five is realistic when the LLM tries multiple approaches before
settling on one that satisfies the ownership / type / exhaustiveness
constraint.

## Playback

Locally:

```sh
asciinema play examples/mend/casts/welcome_emails.cast
asciinema play examples/mend/casts/order_status.cast
```

Convert to GIF for embedding in slides / web pages:

```sh
# requires asciinema-agg (or terminalizer / svg-term-cli)
agg examples/mend/casts/welcome_emails.cast welcome_emails.gif
```

Embed on a webpage:

```html
<script src="https://asciinema.org/a/<id>.js" async></script>
```

(once uploaded via `asciinema upload <file>.cast` to an asciinema
server; the local cast file is a self-contained portable artifact
without that step.)

## Re-recording

```sh
# from the repo root
rm -f examples/mend/casts/welcome_emails.cast
asciinema rec \
    --idle-time-limit 2 \
    --window-size 100x32 \
    --command "examples/mend/casts/demo.sh welcome_emails" \
    examples/mend/casts/welcome_emails.cast
```

The `--idle-time-limit 2` flag caps idle time at 2 seconds — useful
because the LLM call subprocess spends 5-30 seconds per turn waiting
on the model. Without it the cast plays back with long dead-air
gaps. The recorded LLM response and compiler output are both
captured; only inter-character idle time is compressed.

`--window-size 100x32` produces a portable cast that renders cleanly
in narrow embedding contexts. Match your target rendering width if
embedding inline.

## Why no `user_lookup` cast

The third corpus example (`user_lookup`) consistently converges in
**zero iterations** under live Claude — the model already knows
`Option<T>` discipline from its Rust priors and writes the
`match users.get(...)` shape on the first attempt. A cast of a
zero-iteration run is a clean build with no compiler iteration to
narrate; it's documentary evidence that the LLM-loop infrastructure
*supports* this case but doesn't make for an interesting recording.

The `welcome_emails` (ownership) and `order_status`
(pattern-interpretation) examples are the ones with naturally-
occurring LLM friction that the compiler resolves — those are the
recordable demo headlines.

## What the cast shows, in narration order

1. **The task.** The natural-language prompt fed to the LLM
   (extracted from `task.md`).
2. **Loop status lines.** The harness's per-iteration progress
   (`[mend] iter N: ...`) — clean build, diagnostics remaining,
   `karac fix` applications.
3. **Per-iteration walkthrough.** For each iter:
   - The Kāra source the LLM wrote that turn.
   - The compiler diagnostics (extracted from
     `iter_NNN/diagnostics.json`).
   - Any `karac fix` actions (machine-applicable replacements).
4. **Convergence.** Final source + iteration count, with a pointer
   to the persisted run transcript.
