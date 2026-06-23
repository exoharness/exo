#!/usr/bin/env bash
# Run exo on a Continual Learning Bench task.
#
#   OPENAI_API_KEY=... ./run.sh                              # exploitable_poker, quick_test
#   OPENAI_API_KEY=... ./run.sh sales_prediction            # another task
#   SCHEDULE=full OPENAI_API_KEY=... ./run.sh exploitable_poker
#
# exo runs on the host (local-process sandbox); EXO_REPO/EXO_BIN tell the system
# where to find it. Extra args after the task are forwarded to `clbench run`.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
export EXO_REPO="$(cd "$HERE/../.." && pwd)"
export EXO_BIN="${EXO_BIN:-$EXO_REPO/target/release/exo}"
# clbench is a continual-learning benchmark, so default to the memory-enabled
# harness (shell + remember/forget + per-turn memory injection). Override with
# EXO_HARNESS=.../harness.ts for a memory-free control run.
export EXO_HARNESS="${EXO_HARNESS:-$EXO_REPO/examples/simple-coding-agent/harness-memory.ts}"
CLBENCH="${CLBENCH_REPO:-$(cd "$HERE/../../.." && pwd)/clbench}"

: "${OPENAI_API_KEY:?set OPENAI_API_KEY}"
[ -x "$EXO_BIN" ] || { echo "exo binary missing at $EXO_BIN — run ./setup.sh"; exit 1; }
[ -e "$CLBENCH/src/systems/exo" ] || { echo "exo system not linked — run ./setup.sh"; exit 1; }

TASK="${1:-exploitable_poker}"; [ "$#" -gt 0 ] && shift || true
SCHEDULE="${SCHEDULE:-quick_test}"

echo "==> clbench run $TASK --schedule $SCHEDULE --system exo ${*}"
cd "$CLBENCH"
uv run clbench run "$TASK" --schedule "$SCHEDULE" --system exo "$@"
