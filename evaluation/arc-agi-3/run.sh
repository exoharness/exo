#!/usr/bin/env bash
# Run exo on an ARC-AGI-3 game via the official agent framework.
#
#   OPENAI_API_KEY=... ARC_API_KEY=... ./run.sh            # default game ls20
#   OPENAI_API_KEY=... ARC_API_KEY=... ./run.sh vc33       # a specific game
#   OPENAI_API_KEY=... ARC_API_KEY=... GAME=ft09 ./run.sh
#
# The framework (arcprize/ARC-AGI-3-Agents) drives the hosted game over its API
# and calls our exo policy (agents/templates/exo_arc_agent.py -> `exoarc`) each
# turn. exo runs host-side with a tool-less reasoning harness. Extra args pass
# through to main.py.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
export EXO_REPO="$(cd "$HERE/../.." && pwd)"
export EXO_BIN="${EXO_BIN:-$EXO_REPO/target/release/exo}"
export EXO_HARNESS="${EXO_HARNESS:-$EXO_REPO/examples/simple-coding-agent/harness-arc3.ts}"
export MODEL="${MODEL:-gpt-5.5}"
A3="${ARC3_REPO:-$(cd "$HERE/../../.." && pwd)/ARC-AGI-3-Agents}"

: "${OPENAI_API_KEY:?set OPENAI_API_KEY for exo model calls}"
: "${ARC_API_KEY:?set ARC_API_KEY (ARC-AGI-3 game API; get one at arcprize.org)}"
export OPENAI_API_KEY ARC_API_KEY
[ -x "$EXO_BIN" ] || { echo "exo binary missing at $EXO_BIN — run ./setup.sh"; exit 1; }
[ -d "$A3" ] || { echo "framework missing at $A3 — run ./setup.sh"; exit 1; }
grep -q "ExoArc" "$A3/agents/__init__.py" || { echo "exo policy not registered — run ./setup.sh"; exit 1; }

GAME="${1:-${GAME:-ls20}}"; [ "$#" -gt 0 ] && shift || true
echo "==> ARC-AGI-3 | agent=exoarc | model=$MODEL | game=$GAME"
cd "$A3"
uv run main.py --agent=exoarc --game="$GAME" "$@"
