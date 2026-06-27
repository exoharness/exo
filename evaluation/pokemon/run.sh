#!/usr/bin/env bash
# Watch exo (gpt-5.5 vision) play Pokémon. Screenshots in, button presses out.
#
#   OPENAI_API_KEY=... POKEMON_ROM=/path/to/pokemon.gb ./run.sh
#   OPENAI_API_KEY=... POKEMON_ROM=... POKEMON_STEPS=100 ./run.sh
#   OPENAI_API_KEY=... POKEMON_ROM=... POKEMON_STATE=/path/save.state ./run.sh   # start mid-game
#
# Extra args pass through to pokemon_runner.py (e.g. --steps 100 --settle-frames 30).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
export EXO_REPO="$(cd "$HERE/../.." && pwd)"
export EXO_BIN="${EXO_BIN:-$EXO_REPO/target/release/exo}"
export EXO_HARNESS="${EXO_HARNESS:-$EXO_REPO/examples/simple-coding-agent/harness-pokemon.ts}"
export MODEL="${MODEL:-gpt-5.5}"

: "${OPENAI_API_KEY:?set OPENAI_API_KEY}"
: "${POKEMON_ROM:?set POKEMON_ROM to a ROM you own (e.g. /path/to/pokemon.gb)}"
[ -x "$EXO_BIN" ] || { echo "exo binary missing at $EXO_BIN — run ./setup.sh"; exit 1; }
[ -d "$HERE/.venv" ] || { echo "venv missing — run ./setup.sh"; exit 1; }

echo "==> exo plays Pokémon | model=$MODEL | rom=$(basename "$POKEMON_ROM") | steps=${POKEMON_STEPS:-40}"
"$HERE/.venv/bin/python" "$HERE/pokemon_runner.py" "$@"
