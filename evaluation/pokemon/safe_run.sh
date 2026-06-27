#!/usr/bin/env bash
# Launch exactly ONE Pokémon run with an OOM watchdog. After the crash on
# 2026-06-26 (uncleaned Docker sandboxes piled up -> OOM -> reboot), every
# experiment goes through this.
#
#   ./safe_run.sh <out_dir> -- <args passed to pokemon_runner.py>
#
# The watchdog: every 30s prunes exited exo-* containers; if available RAM
# drops below MIN_FREE_MB or running exo-* containers exceed MAX_CONTAINERS,
# it TERMs (then KILLs) the run and force-removes all exo-* containers.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:?usage: safe_run.sh <out_dir> -- <runner args>}"; shift
[ "${1:-}" = "--" ] && shift
MIN_FREE_MB=${MIN_FREE_MB:-3000}
# Containers are lightweight (sleep-infinity sandboxes); RAM (MIN_FREE_MB) is the
# real OOM guard. The runner force-removes old sandboxes on each conv-reset, so
# this is just a high backstop against a genuine runaway.
MAX_CONTAINERS=${MAX_CONTAINERS:-30}

reap_exited(){ docker ps -aq -f name=exo- -f status=exited 2>/dev/null | xargs -r docker rm >/dev/null 2>&1; }
running_exo(){ docker ps -q -f name=exo- 2>/dev/null | wc -l; }
free_mb(){ free -m | awk '/^Mem:/{print $7}'; }

echo "[safe_run] pre-flight: kill stale runners + force-remove all exo containers"
for pid in $(pgrep -f pokemon_runner.py); do [ "$pid" != "$$" ] && kill "$pid" 2>/dev/null; done
sleep 1
docker ps -aq -f name=exo- 2>/dev/null | xargs -r docker rm -f >/dev/null 2>&1

mkdir -p "$OUT"
echo "[safe_run] launch: pokemon_runner.py --out $OUT $* | floor=${MIN_FREE_MB}MB cap=${MAX_CONTAINERS} containers"
cd "$HERE"
.venv/bin/python pokemon_runner.py --out "$OUT" "$@" > "$OUT/run.log" 2>&1 &
RUN_PID=$!
echo "[safe_run] run pid=$RUN_PID log=$OUT/run.log"

while kill -0 "$RUN_PID" 2>/dev/null; do
  reap_exited
  fm=$(free_mb); rc=$(running_exo)
  if [ "${fm:-99999}" -lt "$MIN_FREE_MB" ] || [ "${rc:-0}" -gt "$MAX_CONTAINERS" ]; then
    echo "[safe_run] ABORT free=${fm}MB containers=${rc} -> stopping run"
    kill -TERM "$RUN_PID" 2>/dev/null; sleep 8; kill -KILL "$RUN_PID" 2>/dev/null
    docker ps -aq -f name=exo- 2>/dev/null | xargs -r docker rm -f >/dev/null 2>&1
    echo "[safe_run] ABORTED (see $OUT/run.log)"
    exit 1
  fi
  sleep 30
done
reap_exited
echo "[safe_run] DONE rc=ok (free=$(free_mb)MB, exo containers=$(running_exo))"
