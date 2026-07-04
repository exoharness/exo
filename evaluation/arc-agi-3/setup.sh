#!/usr/bin/env bash
# One-time setup to evaluate exo on ARC-AGI-3 (arcprize.org), the INTERACTIVE
# reasoning benchmark. Idempotent.
#
#   ./setup.sh
#
# Clones arcprize/ARC-AGI-3-Agents (the official framework: it owns the game API,
# scorecards, and the play loop), installs its env, links our exo policy into its
# agent registry, and ensures a host exo binary. Unlike v1/v2 (static grids), v3
# is agentic — the framework drives a multi-step game and calls our agent each
# turn. Prereqs you provide: uv, an ARC_API_KEY from arcprize.org, OPENAI_API_KEY.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
EXO_REPO="$(cd "$HERE/../.." && pwd)"
A3="${ARC3_REPO:-$(cd "$HERE/../../.." && pwd)/ARC-AGI-3-Agents}"

echo "==> ARC-AGI-3-Agents repo: $A3"
if [ ! -d "$A3/.git" ]; then
  git clone --depth 1 https://github.com/arcprize/ARC-AGI-3-Agents.git "$A3"
fi

echo "==> uv sync (framework env)"
( cd "$A3" && uv sync )

# Agent discovery is via Agent.__subclasses__(): link our policy into the
# framework's templates dir (so `..agent` / `arcengine` resolve) and import it in
# the registry. Registered name is the lowercased class name -> `exoarc`.
echo "==> linking exo policy into the framework"
ln -sfn "$HERE/exo_agent/exo_arc_agent.py" "$A3/agents/templates/exo_arc_agent.py"
INIT="$A3/agents/__init__.py"
grep -q "exo_arc_agent import ExoArc" "$INIT" || \
  sed -i '/from .templates.random_agent import Random/a from .templates.exo_arc_agent import ExoArc' "$INIT"
grep -n "ExoArc" "$INIT" || { echo "ERROR: failed to register ExoArc in $INIT"; exit 1; }

# .env. main.py does `load_dotenv(".env", override=True)`, which would clobber the
# real OPENAI_API_KEY / ARC_API_KEY we pass via the environment (run.sh) with
# .env.example's PLACEHOLDERS. So create .env but strip the placeholder key lines —
# the env-passed real keys then survive. (Real keys a user later adds to .env are
# kept; only the literal placeholders are removed.)
[ -f "$A3/.env" ] || { [ -f "$A3/.env.example" ] && cp "$A3/.env.example" "$A3/.env"; }
[ -f "$A3/.env" ] && sed -i '/your_openai_api_key_here/d; /your_arc_api_key_here/d' "$A3/.env"
echo "==> .env ready (placeholder keys stripped; pass real keys via env in run.sh)"

echo "==> host exo binary"
if [ ! -x "$EXO_REPO/target/release/exo" ]; then
  echo "    building (cargo build --release -p exo)"
  ( cd "$EXO_REPO" && cargo build --release -p exo )
else
  echo "    present: $EXO_REPO/target/release/exo"
fi

echo "==> Done. Set ARC_API_KEY, then: OPENAI_API_KEY=... ARC_API_KEY=... ./run.sh ls20"
echo "    Public games: ft09 ls20 vc33"
