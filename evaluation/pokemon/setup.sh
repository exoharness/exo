#!/usr/bin/env bash
# One-time setup to watch exo play Pokémon via the PyBoy Game Boy emulator.
# Idempotent.
#
#   ./setup.sh
#
# Creates a uv venv with PyBoy + Pillow, ensures a host exo binary, and checks for
# a ROM. You must supply your own legally-obtained Pokémon ROM (ROMs are
# copyrighted and NOT included): set POKEMON_ROM to its path.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
EXO_REPO="$(cd "$HERE/../.." && pwd)"

echo "==> Python env (uv venv + pyboy + pillow)"
( cd "$HERE" && uv venv --python 3.12 .venv >/dev/null 2>&1 || uv venv .venv >/dev/null 2>&1 ; \
  VIRTUAL_ENV="$HERE/.venv" uv pip install pyboy pillow )

echo "==> host exo binary"
if [ ! -x "$EXO_REPO/target/release/exo" ]; then
  echo "    building (cargo build --release -p exo)"
  ( cd "$EXO_REPO" && cargo build --release -p exo )
else
  echo "    present: $EXO_REPO/target/release/exo"
fi

echo "==> ROM check"
if [ -n "${POKEMON_ROM:-}" ] && [ -f "${POKEMON_ROM}" ]; then
  echo "    ROM: $POKEMON_ROM"
else
  echo "    NO ROM set. Provide one you legally own and export POKEMON_ROM=/path/to/pokemon.gb"
  echo "    (ROMs are copyrighted and intentionally not bundled.)"
fi

echo "==> Done. Run: OPENAI_API_KEY=... POKEMON_ROM=/path/to/rom.gb ./run.sh"
