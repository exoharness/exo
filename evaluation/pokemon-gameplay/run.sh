#!/usr/bin/env bash
# Boots the PyBoy emulator sidecar (foreground). The agent side runs through
# the exo CLI — see README.md, or drive.sh for long unattended runs.
#
#   ./run.sh                 # uses first ROM in roms/*.gb
#   ./run.sh --rom roms/pokemon-red.gb
#
# Requires: python3 (venv).
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

VENV=runtime/venv
if [[ ! -x "$VENV/bin/python" ]]; then
  echo "creating venv + installing pyboy (one-time)..."
  mkdir -p runtime
  python3 -m venv "$VENV"
  "$VENV/bin/pip" install -q -r emulator/requirements.txt
fi

PORT="${POKEMON_EMULATOR_PORT:-8777}"
exec "$VENV/bin/python" emulator/server.py --rom "$ROM" --port "$PORT"
