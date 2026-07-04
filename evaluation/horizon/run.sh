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
# Horizon tasks set a 900s agent timeout; the host-agent runs exo over the proxy
# bridge (each shell exec round-trips host<->sandbox), so per-turn latency is high
# and a multi-turn task overshoots 900s even when it solves correctly (task 01:
# 28 turns / ~1023s, reward 1 but flagged AgentTimeoutError). Default to 2x.
AGENT_TIMEOUT_MULTIPLIER="${AGENT_TIMEOUT_MULTIPLIER:-2.0}"
export PYTHONPATH="$HERE${PYTHONPATH:+:$PYTHONPATH}"
[ -x "$HERE/../../target/release/exo" ] || { echo "build the host exo binary first: cargo build --release -p exo"; exit 1; }

run_harbor() {  # $@ = dataset/path selector args for one harbor invocation
  harbor run \
    "$@" \
    --agent-import-path exo_agent.host_agent:ExoHostAgent \
    -m "$MODEL" \
    --ae "OPENAI_API_KEY=$OPENAI_API_KEY" \
    --agent-timeout-multiplier "$AGENT_TIMEOUT_MULTIPLIER" \
    --n-concurrent "$N_CONCURRENT"
}

if [ "$#" -eq 0 ]; then
  # Full public set as a single dataset job (registry copy — no local patches).
  echo "==> horizon (host agent) | model=$MODEL tasks=ALL (dataset)"
  run_harbor -d orinlabs/horizon-public
else
  # Local patched tasks: harbor's -p is single-valued, so run one job PER task
  # (otherwise only the last -p registers — n_total_trials=1).
  for t in "$@"; do
    echo "==> horizon (host agent) | model=$MODEL task=$t"
    run_harbor -p "$HORIZON_REPO/evals/$t"
  done
fi
