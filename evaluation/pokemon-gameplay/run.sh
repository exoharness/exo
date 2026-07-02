#!/usr/bin/env bash
# Boots the PyBoy sidecar + the self-improving Pokemon agent.
#
#   ./run.sh                 # uses first ROM in roms/*.gb, runs until ^C
#   POKEMON_TURNS=25 ./run.sh
#   ./run.sh --rom roms/pokemon-red.gb
#
# Requires: python3 (venv), node >= 24, OPENAI_API_KEY.
set -euo pipefail
cd "$(dirname "$0")"

ROM=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --rom) ROM="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done

if [[ -z "$ROM" ]]; then
  ROM=$(ls roms/*.gb roms/*.gbc 2>/dev/null | head -1 || true)
fi
if [[ -z "$ROM" || ! -f "$ROM" ]]; then
  echo "No ROM found. Put a Pokemon Red/Blue .gb file in roms/ (gitignored)." >&2
  exit 1
fi
if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY is not set." >&2
  exit 1
fi

VENV=runtime/venv
if [[ ! -x "$VENV/bin/python" ]]; then
  echo "creating venv + installing pyboy (one-time)..."
  mkdir -p runtime
  python3 -m venv "$VENV"
  "$VENV/bin/pip" install -q -r emulator/requirements.txt
fi

PORT="${POKEMON_EMULATOR_PORT:-8777}"
"$VENV/bin/python" emulator/server.py --rom "$ROM" --port "$PORT" &
EMULATOR_PID=$!
trap 'kill "$EMULATOR_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$EMULATOR_PID" 2>/dev/null; then
    echo "emulator failed to start" >&2
    exit 1
  fi
  sleep 0.2
done

POKEMON_EMULATOR_URL="http://127.0.0.1:$PORT" pnpm exec tsx agent/run.ts
