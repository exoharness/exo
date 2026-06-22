#!/usr/bin/env bash
# Run the exo Simple Coding Agent against the Horizon continual-learning benchmark
# (orinlabs/horizon) via Harbor.
#
#   OPENAI_API_KEY=... ./run-horizon.sh                          # full public set
#   OPENAI_API_KEY=... ./run-horizon.sh 01-example-catering-vendor   # one task (by evals/ dir name)
#
# Horizon sandboxes have NO internet (allow_internet=false) and run no agent code
# themselves — they only execute shell commands. Our installed ExoAgent works here
# because the bundle is self-contained (no install-time network) and we open just
# the model host for the agent run via --allow-agent-host.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"

: "${OPENAI_API_KEY:?set OPENAI_API_KEY}"
[ -f exo-bundle.tar.gz ] || { echo "exo-bundle.tar.gz missing — run ./setup.sh first"; exit 1; }

HORIZON_REPO="${HORIZON_REPO:-/home/worker/horizon}"
MODEL="${MODEL:-openai/gpt-5.5}"
N_CONCURRENT="${N_CONCURRENT:-2}"
MODEL_HOST="${MODEL_HOST:-api.openai.com}"
export PYTHONPATH="$HERE${PYTHONPATH:+:$PYTHONPATH}"

# Default: the full public dataset. With args: run those tasks locally from the clone via -p.
DATASET_ARGS=(-d orinlabs/horizon-public)
if [ "$#" -gt 0 ]; then
  DATASET_ARGS=()
  for t in "$@"; do DATASET_ARGS+=(-p "$HORIZON_REPO/evals/$t"); done
fi

# Local Docker supports only NO_NETWORK / PUBLIC (not per-host ALLOWLIST), so we
# don't pass --allow-agent-host here (it would force ALLOWLIST and be rejected).
# ALLOWLIST_HOST=api.openai.com only applies on providers that support it (Daytona).
ALLOW_ARGS=()
[ -n "${ALLOWLIST_HOST:-}" ] && ALLOW_ARGS=(--allow-agent-host "$ALLOWLIST_HOST")

echo "==> horizon run | model=$MODEL concurrency=$N_CONCURRENT tasks=${*:-ALL}"
harbor run \
  "${DATASET_ARGS[@]}" \
  --agent-import-path exo_agent.agent:ExoAgent \
  -m "$MODEL" \
  --ae "OPENAI_API_KEY=$OPENAI_API_KEY" \
  "${ALLOW_ARGS[@]}" \
  --n-concurrent "$N_CONCURRENT"

echo "==> done. results under jobs/<ts>/ ; inspect with: harbor view jobs"
