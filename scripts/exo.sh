#!/usr/bin/env bash
# First-run entry point: build exo if needed, then start a chat session with
# the quickstart agent. When the default model is not registered yet, the CLI
# runs an interactive bootstrap wizard before the REPL starts.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXO_BIN="${EXO_BIN:-$ROOT_DIR/target/debug/exo}"
AGENT_FILE="${EXO_AGENT_FILE:-$ROOT_DIR/examples/quickstart-agent.md}"

if [[ ! -x "$EXO_BIN" ]]; then
  echo "Building exo (first run only)..." >&2
  (cd "$ROOT_DIR" && cargo build -p exo)
fi

exec "$EXO_BIN" run "$AGENT_FILE" "$@"
