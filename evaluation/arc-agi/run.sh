#!/usr/bin/env bash
# Run exo on ARC-AGI tasks and print pass@1 (exact grid match).
#
#   OPENAI_API_KEY=... ./run.sh                     # 10 ARC-AGI-1 eval tasks
#   OPENAI_API_KEY=... ARC_N=50 ./run.sh            # more tasks
#   OPENAI_API_KEY=... ARC_VERSION=2 ./run.sh       # ARC-AGI-2
#   OPENAI_API_KEY=... ARC_SPLIT=training ./run.sh  # training split
#   OPENAI_API_KEY=... ARC_VERSION=2 ARC_N=25 ./run.sh --evolve --out results/evolve25.json
#                                                   # self-evolving persistent agent
#
# Default harness is tool-less pure reasoning, so the agent can't read the
# on-disk answer keys. --evolve switches to harness-arc-evolve.ts (memory +
# self-authored tools + docker-sandboxed shell) with ONE persistent agent across
# the sequence. Extra args pass through to arc_runner.py (e.g. --offset 100).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
export EXO_REPO="$(cd "$HERE/../.." && pwd)"
export EXO_BIN="${EXO_BIN:-$EXO_REPO/target/release/exo}"
export EXO_HARNESS="${EXO_HARNESS:-$EXO_REPO/examples/simple-coding-agent/harness-arc.ts}"
export MODEL="${MODEL:-gpt-5.5}"
PARENT="$(cd "$HERE/../../.." && pwd)"

: "${OPENAI_API_KEY:?set OPENAI_API_KEY}"
[ -x "$EXO_BIN" ] || { echo "exo binary missing at $EXO_BIN — run ./setup.sh"; exit 1; }

ARC_VERSION="${ARC_VERSION:-1}"
ARC_SPLIT="${ARC_SPLIT:-evaluation}"
if [ "$ARC_VERSION" = "2" ]; then
  ARC_REPO="${ARC_REPO_V2:-$PARENT/ARC-AGI-2}"
else
  ARC_REPO="${ARC_REPO_V1:-$PARENT/ARC-AGI}"
fi
DATA_DIR="$ARC_REPO/data/$ARC_SPLIT"
[ -d "$DATA_DIR" ] || { echo "no dataset at $DATA_DIR — run ./setup.sh"; exit 1; }

echo "==> ARC-AGI v$ARC_VERSION/$ARC_SPLIT | model=$MODEL | n=${ARC_N:-10}"
python3 "$HERE/arc_runner.py" --data-dir "$DATA_DIR" "$@"
