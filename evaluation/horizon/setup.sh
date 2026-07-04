#!/usr/bin/env bash
# One-time setup to run the Horizon benchmark (orinlabs/horizon) with the
# host-side exo agent. Idempotent — safe to re-run.
#
#   ./setup-horizon.sh
#
# Does:
#   1. Clone orinlabs/horizon to $HORIZON_REPO (default ../../horizon) if absent.
#   2. Patch the example tasks for two upstream incompatibilities (see below).
#   3. Ensure the harbor CLI and a host exo binary (with the `proxy` provider) exist.
#
# Then run with: OPENAI_API_KEY=... ./run-horizon-host.sh [task-dir]
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"                     # the exo repo
HORIZON_REPO="${HORIZON_REPO:-$(cd "$HERE/../../.." && pwd)/horizon}"
PUB_TRACES="https://huggingface.co/datasets/orinlabs/horizon-1-example-traces/resolve/main"

echo "==> Horizon repo: $HORIZON_REPO"
if [ ! -d "$HORIZON_REPO/.git" ]; then
  echo "    cloning orinlabs/horizon"
  git clone --depth 1 https://github.com/orinlabs/horizon.git "$HORIZON_REPO"
fi

# Patch 1 — trace URL. The base image's horizon-download-trace defaults to the
# PRIVATE dataset (orinlabs/horizon-example-traces -> 401). The public traces are
# at orinlabs/horizon-1-example-traces. Inject it as build env before the download.
echo "==> Patching trace URL in task Dockerfiles (idempotent)"
for d in "$HORIZON_REPO"/evals/*/environment/Dockerfile; do
  [ -f "$d" ] || continue
  grep -q HORIZON_TRACE_BASE_URL "$d" || \
    sed -i "s#^RUN horizon-download-trace#ENV HORIZON_TRACE_BASE_URL=$PUB_TRACES\nRUN horizon-download-trace#" "$d"
done

# Patch 2 — verifier output. Horizon's judge writes reward.json as
# {"reward": 0|1, "metrics": {...bools}, "reply": "<text>"}, but harbor 0.15.0
# reads the whole file as VerifierResult.rewards: dict[str, float|int] and rejects
# the nested dict / string. Rewrite to a flat numeric dict (score-preserving:
# reward + metrics as 0/1, drop reply).
echo "==> Patching judge reward.json output to flat-numeric (idempotent)"
for j in "$HORIZON_REPO"/evals/*/tests/judge.py; do
  [ -f "$j" ] || continue
  grep -q 'int(v) for k, v in metrics.items()' "$j" || \
    sed -i 's#json.dumps({"reward": reward, "metrics": metrics, "reply": body}, indent=2)#json.dumps({"reward": reward, **{k: int(v) for k, v in metrics.items()}}, indent=2)#' "$j"
done

# harbor CLI
if ! command -v harbor >/dev/null 2>&1; then
  command -v uv >/dev/null 2>&1 || { echo "ERROR: uv not installed — see https://docs.astral.sh/uv/"; exit 1; }
  echo "==> Installing harbor CLI"
  uv tool install harbor
fi

# Host exo binary (must include the `proxy` sandbox provider this repo adds).
if [ ! -x "$REPO/target/release/exo" ]; then
  echo "==> Building host exo binary (cargo build --release -p exo)"
  (cd "$REPO" && cargo build --release -p exo)
fi
"$REPO/target/release/exo" agent create --help 2>&1 | grep -q 'proxy' \
  || { echo "WARNING: host exo binary lacks the 'proxy' provider — rebuild: cargo build --release -p exo"; }

echo "==> Done. Run: OPENAI_API_KEY=... ./run-horizon-host.sh 01-example-catering-vendor"
