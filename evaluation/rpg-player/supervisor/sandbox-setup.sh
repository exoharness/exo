#!/usr/bin/env bash
# One-time setup for supervising rpg-player from inside an exo agent sandbox
# (Ubuntu container with the repo mounted at /workspace/exo).
#
#   bash /workspace/exo/evaluation/rpg-player/supervisor/sandbox-setup.sh
#
# Creates a container-local working copy at ~/rpg (the mounted repo's
# node_modules belongs to the host OS and must not be reused or overwritten),
# installs Node 22 + pnpm if missing, project deps, Playwright's Linux
# Chromium with system libraries, and copies the ROM over.
set -euo pipefail

SRC="${SRC:-/workspace/exo}"
WORK="${WORK:-$HOME/rpg}"

if ! command -v rsync >/dev/null 2>&1; then
  apt-get update -qq && apt-get install -y -qq rsync curl ca-certificates
fi

echo "==> copying repo to $WORK (excluding host artifacts)"
mkdir -p "$WORK"
rsync -a --delete \
  --exclude node_modules --exclude .git --exclude .exo --exclude target \
  --exclude 'evaluation/rpg-player/runtime*' \
  "$SRC/" "$WORK/"

if ! command -v node >/dev/null 2>&1 || [[ "$(node -e 'console.log(process.versions.node.split(".")[0])')" -lt 20 ]]; then
  echo "==> installing Node 22 (NodeSource)"
  curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
  apt-get install -y -qq nodejs
fi

if ! command -v pnpm >/dev/null 2>&1; then
  echo "==> enabling pnpm via corepack"
  corepack enable
  corepack prepare pnpm@10.26.2 --activate
fi

cd "$WORK"
echo "==> installing dependencies"
pnpm install

echo "==> installing Playwright Chromium + system deps"
pnpm exec playwright install --with-deps chromium

echo "==> copying ROM from the mounted repo"
mkdir -p "$WORK/evaluation/rpg-player/roms"
cp "$SRC"/evaluation/rpg-player/roms/*.sms "$WORK/evaluation/rpg-player/roms/" 2>/dev/null ||
  echo "WARNING: no .sms ROM found in $SRC/evaluation/rpg-player/roms/ — copy one manually"

echo "==> smoke check"
if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "WARNING: OPENAI_API_KEY is not set; the player agent will not start without it"
fi
echo "setup complete. Run chunks with:"
echo "  cd $WORK/evaluation/rpg-player && RPG_TURNS=25 ./run.sh"
