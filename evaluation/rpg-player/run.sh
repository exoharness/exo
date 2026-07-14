#!/usr/bin/env bash
# Boots the EmulatorJS sidecar + the self-improving RPG agent.
#
#   ./run.sh                 # uses first ROM in roms/*.sms, runs until ^C
#   RPG_TURNS=25 ./run.sh
#   ./run.sh --rom roms/phantasy-star.sms
#   RPG_HEADED=1 ./run.sh    # watch the emulator in a real Chromium window
#
# Requires: node >= 22, pnpm (repo root `pnpm install` done), OPENAI_API_KEY.
# First run downloads the Playwright Chromium build (one-time).
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
  ROM=$(ls roms/*.sms 2>/dev/null | head -1 || true)
  COUNT=$(ls roms/*.sms 2>/dev/null | wc -l | tr -d ' ')
  if [[ "$COUNT" -gt 1 ]]; then
    echo "note: multiple .sms files in roms/; using '$ROM' (pick one with --rom)" >&2
  fi
fi
if [[ -z "$ROM" || ! -f "$ROM" ]]; then
  echo "No ROM found. Put a Phantasy Star .sms file in roms/ (gitignored, and unzip it — only .sms files are picked up)." >&2
  exit 1
fi
echo "using ROM: $ROM"
if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY is not set." >&2
  exit 1
fi

# One-time Chromium download for Playwright (no-op when already installed).
pnpm exec playwright install chromium

# pnpm exec can resolve relative paths against the workspace root, so pass
# absolute paths.
ROM="$PWD/${ROM#"$PWD"/}"
PORT="${RPG_EMULATOR_PORT:-8777}"
pnpm exec tsx "$PWD/emulator/server.ts" --rom "$ROM" --port "$PORT" &
EMULATOR_PID=$!
trap 'kill "$EMULATOR_PID" 2>/dev/null || true' EXIT

# The sidecar answers /health as soon as the HTTP server is up, but reports
# booted:false until EmulatorJS has loaded the core; /frame just works either
# way once health is ok because requests are queued.
for _ in $(seq 1 150); do
  if curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$EMULATOR_PID" 2>/dev/null; then
    echo "emulator failed to start" >&2
    exit 1
  fi
  sleep 0.2
done

RPG_EMULATOR_URL="http://127.0.0.1:$PORT" pnpm exec tsx "$PWD/agent/run.ts"
