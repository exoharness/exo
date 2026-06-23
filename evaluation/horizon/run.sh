#!/usr/bin/env bash
# Run the HOST-SIDE exo agent against Horizon (orinlabs/horizon) via Harbor.
#
#   OPENAI_API_KEY=... ./run-horizon-host.sh 01-example-catering-vendor
#   OPENAI_API_KEY=... ./run-horizon-host.sh            # full public set
#
# exo runs on the host (model calls use the host network); its shell exec is
# proxied into the no-internet sandbox via the exo `proxy` provider + the
# ExoHostAgent bridge. No --allow-agent-host needed (sandbox stays offline).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"
: "${OPENAI_API_KEY:?set OPENAI_API_KEY}"
HORIZON_REPO="${HORIZON_REPO:-$(cd "$HERE/../../.." && pwd)/horizon}"
MODEL="${MODEL:-openai/gpt-5.5}"
N_CONCURRENT="${N_CONCURRENT:-2}"
export PYTHONPATH="$HERE${PYTHONPATH:+:$PYTHONPATH}"
[ -x "$HERE/../../target/release/exo" ] || { echo "build the host exo binary first: cargo build --release -p exo"; exit 1; }

DATASET_ARGS=(-d orinlabs/horizon-public)
if [ "$#" -gt 0 ]; then
  DATASET_ARGS=()
  for t in "$@"; do DATASET_ARGS+=(-p "$HORIZON_REPO/evals/$t"); done
fi

echo "==> horizon (host agent) | model=$MODEL tasks=${*:-ALL}"
harbor run \
  "${DATASET_ARGS[@]}" \
  --agent-import-path exo_agent.host_agent:ExoHostAgent \
  -m "$MODEL" \
  --ae "OPENAI_API_KEY=$OPENAI_API_KEY" \
  --n-concurrent "$N_CONCURRENT"
