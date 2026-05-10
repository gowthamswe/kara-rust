#!/usr/bin/env python3
"""
mend.py — the Mend demo loop.

Drives an LLM to write Kāra code, then iterates with the compiler's
structured-diagnostic output until the build is clean (or an iteration
cap is hit).

The loop:

    1. Send task prompt (+ system prompt) to the LLM.
    2. LLM returns Kāra source. Write to working file.
    3. `karac check --output=json` → parse diagnostics.
       3a. No diagnostics → done.
       3b. Any machine-applicable diagnostic? Run `karac fix`.
       3c. Re-check. Clean → done.
       3d. Otherwise: format remaining diagnostics, feed back to LLM
           with the current source, get new response, goto 2.
    4. Stop after --max-iterations regardless.

Two modes:

    --dry-run            Use canned LLM responses from
                         <example_dir>/canned_responses.json.
                         No network, no API key required.

    (default)            Live mode — subprocesses `claude -p` (Claude
                         Code's non-interactive mode). Auth is
                         inherited from the user's existing Claude Code
                         login (keychain / OAuth), so the demo runs on
                         a Max subscription with no separate API key
                         and no incremental cost.

Logs each iteration to runs/<timestamp>/iter_NNN/{prompt,response,
diagnostics,source}.* so the transcript is replayable.
"""

import argparse
import datetime as _dt
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
KARAC = ["cargo", "run", "--quiet", "--release", "--bin", "karac", "--"]
# Set MEND_KARAC_BIN to a prebuilt binary to skip the cargo run step
# (much faster on second-and-later runs; cargo run noops when nothing
# changed but the IPC overhead is still ~80 ms).
if os.environ.get("MEND_KARAC_BIN"):
    KARAC = [os.environ["MEND_KARAC_BIN"]]


# ── compiler invocations ───────────────────────────────────────────


def karac_check(path: Path) -> dict:
    """Return the parsed JSON envelope from `karac check --output=json`."""
    proc = subprocess.run(
        KARAC + ["check", "--output=json", str(path)],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0 and not proc.stdout.strip():
        raise RuntimeError(
            f"karac check failed without JSON output:\n"
            f"  stdout: {proc.stdout!r}\n"
            f"  stderr: {proc.stderr!r}"
        )
    return json.loads(proc.stdout)


def karac_fix(path: Path) -> str:
    """Run `karac fix` against `path`. Return the human-readable output."""
    proc = subprocess.run(
        KARAC + ["fix", str(path)],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    return proc.stdout + proc.stderr


# ── canned-response source ─────────────────────────────────────────


def load_canned_responses(example_dir: Path) -> list[dict]:
    fp = example_dir / "canned_responses.json"
    if not fp.exists():
        raise FileNotFoundError(
            f"--dry-run requested but no canned_responses.json at {fp}"
        )
    return json.loads(fp.read_text())["responses"]


# ── live LLM (claude -p) ───────────────────────────────────────────


def call_claude_cli(system_prompt: str, user_message: str) -> str:
    """
    Subprocess `claude -p` for one LLM turn.

    Auth comes from the existing Claude Code login (keychain / OAuth),
    so this runs on the user's Max subscription without an API key.
    `--tools ""` disables tool use — we want pure text generation, no
    Read / Edit / Bash side effects on the working directory. The full
    user message (including any prior-iteration source + diagnostics)
    is piped on stdin; each iteration is a fresh invocation, so the
    transcript is reconstructed inline rather than via session state.
    """
    proc = subprocess.run(
        [
            "claude",
            "-p",
            "--tools",
            "",
            "--system-prompt",
            system_prompt,
            "--output-format",
            "text",
        ],
        input=user_message,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"claude -p failed (exit {proc.returncode}):\n"
            f"  stdout: {proc.stdout!r}\n"
            f"  stderr: {proc.stderr!r}"
        )
    return proc.stdout


# ── diagnostic formatting (LLM-facing) ─────────────────────────────


def format_diagnostics_for_llm(envelope: dict) -> str:
    """Render JSON diagnostics as a compact block for the LLM."""
    lines: list[str] = []
    for d in envelope.get("diagnostics", []):
        head = (
            f"[{d['code']}] {d['phase']}: {d['file']}:{d['line']}:"
            f"{d['column']}: {d['message']}"
        )
        lines.append(head)
        if "replacement" in d:
            r = d["replacement"]
            lines.append(
                f"    machine-applicable replacement: "
                f"offset={r['offset']} length={r['length']} text={r['text']!r}"
            )
        for hint in d.get("hints", []):
            lines.append(f"    hint: {hint['description']}")
    return "\n".join(lines) if lines else "(no diagnostics)"


def has_machine_applicable_fixes(envelope: dict) -> bool:
    return any("replacement" in d for d in envelope.get("diagnostics", []))


def is_clean(envelope: dict) -> bool:
    return not envelope.get("diagnostics")


# ── the loop ───────────────────────────────────────────────────────


def run_loop(
    example_dir: Path,
    out_dir: Path,
    *,
    dry_run: bool,
    max_iterations: int,
) -> int:
    task_md = (example_dir / "task.md").read_text()
    system_prompt = (example_dir.parent.parent / "harness" / "system_prompt.md").read_text()

    canned: list[dict] = []
    if dry_run:
        canned = load_canned_responses(example_dir)

    out_dir.mkdir(parents=True, exist_ok=True)
    work_kara = out_dir / "current.kara"

    # The conversation transcript — fed to the LLM each iteration in
    # live mode, kept here for log fidelity in dry-run.
    messages: list[dict] = [{"role": "user", "content": task_md}]

    for i in range(max_iterations):
        iter_dir = out_dir / f"iter_{i:03d}"
        iter_dir.mkdir()

        # 1. Get LLM response.
        if dry_run:
            if i >= len(canned):
                print(
                    f"[mend] dry-run exhausted canned responses at iter {i}; "
                    f"loop did not converge.",
                    file=sys.stderr,
                )
                return 2
            response_text = canned[i]["kara_source"]
            (iter_dir / "response.note.txt").write_text(canned[i].get("note", ""))
        else:
            user_message = messages[-1]["content"]
            (iter_dir / "user_message.txt").write_text(user_message)
            response_text = call_claude_cli(system_prompt, user_message)

        (iter_dir / "response.kara").write_text(response_text)
        work_kara.write_text(response_text)
        messages.append({"role": "assistant", "content": response_text})

        # 2. karac check.
        envelope = karac_check(work_kara)
        (iter_dir / "diagnostics.json").write_text(json.dumps(envelope, indent=2))

        if is_clean(envelope):
            print(f"[mend] iter {i}: clean build (no compiler iteration needed).")
            (iter_dir / "outcome.txt").write_text("clean-on-arrival")
            shutil.copy(work_kara, out_dir / "final.kara")
            return 0

        # 3. Apply machine-applicable fixes.
        applied_fix = False
        if has_machine_applicable_fixes(envelope):
            fix_log = karac_fix(work_kara)
            (iter_dir / "fix.log").write_text(fix_log)
            applied_fix = True
            envelope = karac_check(work_kara)
            (iter_dir / "diagnostics.after_fix.json").write_text(
                json.dumps(envelope, indent=2)
            )
            if is_clean(envelope):
                print(
                    f"[mend] iter {i}: clean build after `karac fix` "
                    f"({_count_fixes(fix_log)} replacements applied)."
                )
                (iter_dir / "outcome.txt").write_text("clean-after-karac-fix")
                shutil.copy(work_kara, out_dir / "final.kara")
                return 0

        # 4. Still errors — feed back to LLM.
        feedback = format_diagnostics_for_llm(envelope)
        followup = (
            f"`karac check --output=json` reports the following diagnostics "
            f"on your last response"
            + (" (after running `karac fix` for machine-applicable fixes)" if applied_fix else "")
            + f":\n\n{feedback}\n\n"
            f"The current source on disk is:\n\n```kara\n{work_kara.read_text()}\n```\n\n"
            f"Reply with the full corrected Kāra source."
        )
        (iter_dir / "followup.txt").write_text(followup)
        messages.append({"role": "user", "content": followup})
        print(
            f"[mend] iter {i}: {len(envelope['diagnostics'])} "
            f"diagnostic(s) remaining; iterating."
        )

    print(
        f"[mend] hit --max-iterations={max_iterations} without converging.",
        file=sys.stderr,
    )
    return 1


def _count_fixes(fix_log: str) -> int:
    for line in fix_log.splitlines():
        if "applied" in line and "fix" in line:
            try:
                return int(line.split()[1])
            except (IndexError, ValueError):
                pass
    return 0


# ── CLI ────────────────────────────────────────────────────────────


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description="Mend demo loop driver.")
    p.add_argument(
        "example",
        help="Example directory (e.g. examples/welcome_emails). "
        "Resolved relative to examples/mend/examples/ if not absolute.",
    )
    p.add_argument(
        "--dry-run",
        action="store_true",
        help="Use canned LLM responses (no API call). "
        "Required in slice 0 — live mode lands in slice 1.",
    )
    p.add_argument(
        "--max-iterations",
        type=int,
        default=5,
        help="Cap on LLM iterations (default 5).",
    )
    p.add_argument(
        "--output-dir",
        help="Where to write the per-run transcript "
        "(default: runs/<timestamp>/).",
    )
    args = p.parse_args(argv)

    examples_root = REPO_ROOT / "examples" / "mend" / "examples"
    example_dir = Path(args.example)
    if not example_dir.is_absolute():
        candidate = examples_root / args.example
        if candidate.exists():
            example_dir = candidate
        else:
            example_dir = Path(args.example).resolve()
    if not (example_dir / "task.md").exists():
        print(f"[mend] no task.md under {example_dir}", file=sys.stderr)
        return 2

    out_dir = (
        Path(args.output_dir)
        if args.output_dir
        else REPO_ROOT
        / "examples"
        / "mend"
        / "runs"
        / _dt.datetime.now().strftime("%Y%m%dT%H%M%S")
    )

    print(f"[mend] example: {example_dir}")
    print(f"[mend] output:  {out_dir}")
    print(f"[mend] mode:    {'dry-run (canned)' if args.dry_run else 'live'}")

    return run_loop(
        example_dir,
        out_dir,
        dry_run=args.dry_run,
        max_iterations=args.max_iterations,
    )


if __name__ == "__main__":
    sys.exit(main())
