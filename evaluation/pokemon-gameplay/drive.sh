#!/usr/bin/env bash
# Long-run driver: chain agent runs until a total-turn target, snapshotting
# state at every run boundary and producing a GIF + report at the end.
#
#   ./drive.sh --runtime runtime2 --target 250 [--chunk 50] [--port 8777]
#
# Assumes the emulator sidecar is already running on the port (drive.sh will
# restart it from the given runtime's checkpoints if it dies mid-run).
set -u
cd "$(dirname "$0")"

RUNTIME=runtime
TARGET=250
CHUNK=50
PORT="${POKEMON_EMULATOR_PORT:-8777}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --runtime) RUNTIME="$2"; shift 2 ;;
    --target) TARGET="$2"; shift 2 ;;
    --chunk) CHUNK="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done
mkdir -p "$RUNTIME/backups"
VENV=$(ls -d runtime*/venv 2>/dev/null | head -1)
ITER=0

turns() {
  python3 -c "import json;print(len(json.load(open('$RUNTIME/history.json'))))" 2>/dev/null || echo 0
}

ensure_emulator() {
  if ! curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
    echo "=== drive: emulator down, restarting ===" >> "$RUNTIME/agent.log"
    nohup "$VENV/bin/python" emulator/server.py --rom roms/pokemon_red.gb \
      --port "$PORT" --checkpoint-dir "$RUNTIME/checkpoints" \
      >> "$RUNTIME/emulator.log" 2>&1 &
    for _ in $(seq 1 60); do
      curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
      sleep 1
    done
    # Resume from the newest checkpoint rather than a fresh boot.
    LATEST=$(ls -t "$RUNTIME/checkpoints"/*.state 2>/dev/null | head -1)
    if [[ -n "$LATEST" ]]; then
      NAME=$(basename "$LATEST" .state)
      curl -s -X POST "http://127.0.0.1:$PORT/checkpoint/load" \
        -H 'Content-Type: application/json' -d "{\"name\":\"$NAME\"}" -o /dev/null
      echo "=== drive: resumed from checkpoint $NAME ===" >> "$RUNTIME/agent.log"
    fi
  fi
}

snapshot() {
  local t="$1"
  curl -s -X POST "http://127.0.0.1:$PORT/checkpoint/save" \
    -H 'Content-Type: application/json' -d "{\"name\":\"boundary_t$t\"}" -o /dev/null
  tar czf "$RUNTIME/backups/boundary-t$t.tar.gz" \
    --exclude=venv --exclude=backups --exclude=screenshots -C "$RUNTIME" . 2>/dev/null
}

# Wait out any currently running agent before taking over.
while pgrep -f "agent/run.ts" >/dev/null; do sleep 30; done

while [ "$ITER" -lt 12 ]; do
  ITER=$((ITER + 1))
  T=$(turns)
  ensure_emulator
  snapshot "$T"
  if [ "$T" -ge "$TARGET" ]; then break; fi
  REM=$((TARGET - T))
  [ "$REM" -gt "$CHUNK" ] && REM=$CHUNK
  echo "=== drive: run $ITER, $REM turns (at $T/$TARGET) ===" >> "$RUNTIME/agent.log"
  POKEMON_EMULATOR_URL="http://127.0.0.1:$PORT" POKEMON_TURNS=$REM \
    POKEMON_RUNTIME_DIR="$PWD/$RUNTIME" \
    pnpm exec tsx "$PWD/agent/run.ts" >> "$RUNTIME/agent.log" 2>&1
  sleep 10 # crash-loop guard
done

echo "=== drive: finished at $(turns) turns, wrapping up ===" >> "$RUNTIME/agent.log"
"$VENV/bin/python" emulator/make_gif.py --fps 6 \
  --screenshots "$RUNTIME/screenshots" --out "$RUNTIME/run.gif" \
  >> "$RUNTIME/agent.log" 2>&1
RUNTIME_DIR="$RUNTIME" python3 - <<'EOF'
import json, os, pathlib
rt = pathlib.Path(os.environ["RUNTIME_DIR"])
history = json.loads((rt/"history.json").read_text())
milestones = [json.loads(l) for l in (rt/"progress.jsonl").read_text().splitlines()] if (rt/"progress.jsonl").exists() else []
tools = sorted(p.stem for p in (rt/"tools").glob("*.mjs")) if (rt/"tools").is_dir() else []
skills = sorted(p.name for p in (rt/"skills").iterdir() if (p/"SKILL.md").is_file()) if (rt/"skills").is_dir() else []
memories = sorted(p.stem for p in (rt/"memory").glob("*.md")) if (rt/"memory").is_dir() else []
improvements = sum(len(h.get("improvements", [])) for h in history)
lines = ["# Run report", "",
  f"- turns played: {len(history)}",
  f"- milestones: {len(milestones)}",
  f"- self-improvement actions: {improvements}",
  f"- agent-built tools: {', '.join(tools) or '(none)'}",
  f"- skills: {', '.join(skills) or '(none)'}",
  f"- memory files: {len(memories)}", "",
  "## Milestones"]
lines += [f"- turn {m['turn']}: {m['milestone']}" for m in milestones]
lines += ["", "## Last 10 turn summaries"]
lines += [f"### Turn {h['turn']}\n{h['summary']}" for h in history[-10:]]
(rt/"REPORT.md").write_text("\n".join(lines) + "\n")
EOF
echo "drive complete at $(turns) turns"
