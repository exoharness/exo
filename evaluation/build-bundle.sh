#!/usr/bin/env bash
# Rebuild the slim exo bundle for Harbor's ExoAgent from the exo repo.
# Run with the exo repo checked out to a base that includes PR #68.
#
# Usage: ./build-bundle.sh [EXO_REPO]   (default: the exo repo this folder lives in)
# Requires: rust + the x86_64-unknown-linux-musl target (musl-tools), pnpm.
set -euo pipefail
# evaluation/ lives inside the exo repo, so the repo root is one level up.
EXO="${1:-${EXO_REPO:-$(cd "$(dirname "$0")/.." && pwd)}}"
OUT="$(cd "$(dirname "$0")" && pwd)/exo-bundle.tar.gz"
cd "$EXO"

# Ensure deps + a static musl binary exist (portable across task-image glibc versions).
[ -d node_modules ] || pnpm install --frozen-lockfile
cargo build --release --target x86_64-unknown-linux-musl -p exo

tar czf "$OUT" \
  --exclude='node_modules/.pnpm/@anthropic-ai+claude-agent-sdk*' \
  --exclude='node_modules/.pnpm/@img*' --exclude='node_modules/.pnpm/sharp*' \
  --exclude='node_modules/.pnpm/sodium-native*' \
  --exclude='node_modules/.pnpm/@discordjs*' --exclude='node_modules/.pnpm/discord.js*' \
  --exclude='node_modules/.pnpm/@whiskeysockets*' --exclude='node_modules/.pnpm/baileys*' \
  --exclude='node_modules/.pnpm/@cursor*' \
  --exclude='node_modules/.pnpm/playwright*' --exclude='node_modules/.pnpm/@playwright*' \
  --exclude='node_modules/.pnpm/oxlint*' --exclude='node_modules/.pnpm/@oxlint*' --exclude='node_modules/.pnpm/oxfmt*' \
  --exclude='node_modules/.pnpm/rolldown*' --exclude='node_modules/.pnpm/@rolldown*' \
  --exclude='node_modules/.pnpm/@typescript+native-preview*' \
  --exclude='node_modules/.pnpm/vitest*' --exclude='node_modules/.pnpm/@vitest*' \
  --exclude='node_modules/.pnpm/prism-media*' --exclude='node_modules/.pnpm/opusscript*' \
  --exclude='node_modules/.cache' \
  target/x86_64-unknown-linux-musl/release/exo package.json pnpm-lock.yaml tsconfig*.json typescript examples/typescript examples/simple-coding-agent node_modules
echo "wrote $OUT ($(du -sh "$OUT" | cut -f1))"
