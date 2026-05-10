#!/usr/bin/env bash
# demo.sh — narrated wrapper around mend.py for asciinema recording.
#
# Runs the harness on a given example and prints a guided walkthrough
# of each iteration: the LLM's response, the compiler's diagnostic,
# the next iteration's correction. Designed to read top-to-bottom in
# a recorded cast.
#
# Usage:
#     ./demo.sh <example>
# e.g. ./demo.sh welcome_emails

set -euo pipefail

EXAMPLE="${1:?usage: demo.sh <example>}"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
RUNS_DIR="$REPO/examples/mend/runs"

cd "$REPO"

# Run the harness — its own output narrates the iteration count.
echo "════════════════════════════════════════════════════════════════"
echo "  Mend demo: $EXAMPLE"
echo "════════════════════════════════════════════════════════════════"
echo
echo "▸ The task:"
echo
sed -n '/^## Prompt fed to the LLM/,/^##/p' \
    "examples/mend/examples/$EXAMPLE/task.md" \
    | sed '$d' | sed '1d'
sleep 1
echo
echo "▸ Starting the loop. Each iteration: claude -p writes Kāra,"
echo "  karac check runs, diagnostics feed back to claude."
echo
sleep 1

python3 examples/mend/harness/mend.py "$EXAMPLE" 2>&1
LATEST_RUN="$(ls -1t "$RUNS_DIR" | head -1)"
RUN_PATH="$RUNS_DIR/$LATEST_RUN"

# Walk each iteration's artifacts.
ITER_COUNT=0
for ITER_DIR in "$RUN_PATH"/iter_*; do
    [ -d "$ITER_DIR" ] || continue
    ITER_NUM="$(basename "$ITER_DIR" | sed 's/iter_0*//')"
    [ -z "$ITER_NUM" ] && ITER_NUM=0
    echo
    echo "════════════════════════════════════════════════════════════════"
    echo "  iteration $ITER_NUM"
    echo "════════════════════════════════════════════════════════════════"
    echo
    echo "▸ What the LLM wrote:"
    echo
    cat "$ITER_DIR/response.kara"
    sleep 2
    echo
    echo "▸ What karac check reported:"
    echo
    if command -v jq >/dev/null 2>&1; then
        jq -r '.diagnostics[] | "  [\(.code)] \(.phase): line \(.line):\(.column): \(.message)"' \
            < "$ITER_DIR/diagnostics.json" \
            || echo "  (no diagnostics — clean build)"
    else
        python3 -c "
import json, sys
d = json.load(open('$ITER_DIR/diagnostics.json'))['diagnostics']
for x in d:
    print(f\"  [{x['code']}] {x['phase']}: line {x['line']}:{x['column']}: {x['message']}\")
if not d:
    print('  (no diagnostics — clean build)')
"
    fi
    if [ -f "$ITER_DIR/fix.log" ]; then
        echo
        echo "▸ karac fix applied machine-applicable replacements:"
        echo
        sed 's/^/  /' "$ITER_DIR/fix.log"
    fi
    if [ -f "$ITER_DIR/outcome.txt" ]; then
        echo
        echo "▸ outcome: $(cat "$ITER_DIR/outcome.txt")"
    fi
    sleep 2
    ITER_COUNT=$((ITER_COUNT + 1))
done

echo
echo "════════════════════════════════════════════════════════════════"
echo "  Loop converged in $ITER_COUNT iteration(s). Final source:"
echo "════════════════════════════════════════════════════════════════"
echo
cat "$RUN_PATH/final.kara"
echo
echo "(transcript persisted at $RUN_PATH)"
