# Mend harness

Driver for the Mend demo loop. See [`../README.md`](../README.md) for
the demo's overall thesis and current scope.

## Quick start

```sh
# from the repo root — pick any example under examples/mend/examples/

# live mode (default, uses your Claude Code login)
python3 examples/mend/harness/mend.py welcome_emails
python3 examples/mend/harness/mend.py order_status
python3 examples/mend/harness/mend.py user_lookup

# dry-run mode (deterministic, no API call)
python3 examples/mend/harness/mend.py welcome_emails --dry-run
```

The harness reads `task.md`, sends it through the LLM (or replays
canned responses), runs `karac check`, applies `karac fix` where
machine-applicable, feeds remaining diagnostics back, and writes the
per-iteration transcript under `examples/mend/runs/<timestamp>/`.

The harness invokes `karac` via `cargo run --quiet --release --bin
karac --`. Set `MEND_KARAC_BIN=/path/to/karac` to skip the cargo step
once you have a built binary — saves ~80 ms per call after warm-up.

## Live mode (default)

```sh
python3 examples/mend/harness/mend.py welcome_emails
```

Live mode subprocesses `claude -p` (Claude Code's non-interactive mode).
Auth is inherited from your existing Claude Code login (keychain /
OAuth), so the demo runs on your Max subscription with **no separate
API key and no incremental cost**. Each iteration is a fresh
invocation; the conversation transcript is reconstructed inline in the
follow-up prompt rather than via session state.

Flags passed to the subprocess:

- `-p` non-interactive mode, prompt via stdin
- `--tools ""` disables tool use (Read / Edit / Bash) — we want pure
  text generation only; the LLM should never touch the working directory
- `--system-prompt <…>` replaces the default Claude Code system prompt
  with `system_prompt.md` (the Mend-specific primer)
- `--output-format text` plain text response on stdout

## Output layout

```
examples/mend/runs/<timestamp>/
├── current.kara                  the working file (last iteration)
├── final.kara                    the converged source (if loop succeeded)
└── iter_NNN/
    ├── response.kara             the LLM's reply this iteration
    ├── response.note.txt         dry-run only — annotation from canned data
    ├── diagnostics.json          karac check output BEFORE karac fix
    ├── diagnostics.after_fix.json  same, AFTER karac fix (if fix ran)
    ├── fix.log                   karac fix human-readable output
    ├── followup.txt              feedback prompt sent to the LLM next iteration
    └── outcome.txt               "clean-on-arrival" | "clean-after-karac-fix"
```

## Adding a new example

A example is a directory under `examples/mend/examples/<name>/`
containing:

| File                    | Required | Role                                                              |
|-------------------------|----------|-------------------------------------------------------------------|
| `task.md`               | yes      | The natural-language prompt fed to the LLM.                       |
| `solution.kara`         | yes      | Reference solution that compiles clean. Used for documentation, not by the harness directly. |
| `canned_responses.json` | dry-run  | List of LLM responses for `--dry-run` mode.                       |
| `python_buggy.py`       | optional | Same task in Python; demonstrates the bug Kāra catches.           |
| `notes.md`              | optional | Pedagogy: what Python misses, what Kāra catches.                  |

## Caveats

- Slice 0 only — the harness has no retry logic, no rate-limit handling,
  and no resumable transcripts. It runs once per invocation.
- The LLM's output is written to disk verbatim. If the LLM wraps its
  output in markdown fences or adds prose, the build will fail at parse
  time and the harness will surface that as a diagnostic; it does not
  attempt to strip fences.
- `karac fix` is invoked on the same path the LLM wrote to. There's no
  staging; if a fix is wrong, the corrupted file is what the next
  iteration sees.
