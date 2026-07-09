#!/usr/bin/env bash
# Overnight driver: chain agent runs until 250 total turns, with a state
# snapshot at every run boundary and a final GIF + report.
set -u
cd "$(dirname "$0")/.."
TARGET=250
CHUNK=50
PORT="${POKEMON_EMULATOR_PORT:-8777}"
ITER=0

turns() {
  python3 -c "import json;print(len(json.load(open('runtime/history.json'))))" 2>/dev/null || echo 0
}

ensure_emulator() {
  if ! curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
    echo "=== overnight: emulator down, restarting ===" >> runtime/agent.log
    nohup runtime/venv/bin/python emulator/server.py --rom roms/pokemon_red.gb \
      --port "$PORT" >> runtime/emulator.log 2>&1 &
    for _ in $(seq 1 60); do
      curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
      sleep 1
    done
  fi
}

snapshot() {
  local t="$1"
  curl -s -X POST "http://127.0.0.1:$PORT/checkpoint/save" \
    -H 'Content-Type: application/json' -d "{\"name\":\"boundary_t$t\"}" -o /dev/null
  tar czf "runtime/backups/boundary-t$t.tar.gz" \
    --exclude=venv --exclude=backups --exclude=screenshots -C runtime . 2>/dev/null
}

# Wait out the currently running agent (run 2), then loop.
while pgrep -f "agent/run.ts" >/dev/null; do sleep 30; done

while [ "$ITER" -lt 12 ]; do
  ITER=$((ITER + 1))
  T=$(turns)
  ensure_emulator
  snapshot "$T"
  if [ "$T" -ge "$TARGET" ]; then break; fi
  REM=$((TARGET - T))
  [ "$REM" -gt "$CHUNK" ] && REM=$CHUNK
  echo "=== overnight: run $ITER, $REM turns (at $T/$TARGET) ===" >> runtime/agent.log
  POKEMON_EMULATOR_URL="http://127.0.0.1:$PORT" POKEMON_TURNS=$REM \
    pnpm exec tsx "$PWD/agent/run.ts" >> runtime/agent.log 2>&1
  sleep 10 # crash-loop guard
done

echo "=== overnight: finished at $(turns) turns, wrapping up ===" >> runtime/agent.log
runtime/venv/bin/python emulator/make_gif.py --fps 6 --out runtime/run.gif \
  >> runtime/agent.log 2>&1
python3 - <<'EOF'
import json, pathlib
rt = pathlib.Path("runtime")
history = json.loads((rt/"history.json").read_text())
milestones = [json.loads(l) for l in (rt/"progress.jsonl").read_text().splitlines()]
tools = sorted(p.stem for p in (rt/"tools").glob("*.mjs"))
memories = sorted(p.stem for p in (rt/"memory").glob("*.md"))
improvements = sum(len(h.get("improvements", [])) for h in history)
lines = ["# Overnight run report", "",
  f"- turns played: {len(history)}",
  f"- milestones: {len(milestones)}",
  f"- self-improvement actions: {improvements}",
  f"- agent-built tools: {', '.join(tools)}",
  f"- memory files: {len(memories)}", "",
  "## Milestones"]
lines += [f"- turn {m['turn']}: {m['milestone']}" for m in milestones]
lines += ["", "## Last 10 turn summaries"]
lines += [f"### Turn {h['turn']}\n{h['summary']}" for h in history[-10:]]
lines += ["", "## Inspect",
  "- runtime/agent.log (full narration)",
  "- runtime/events.jsonl (every tool call)",
  "- runtime/run.gif (the whole run animated)",
  "- runtime/playbook.md + runtime/memory/ + runtime/tools/ (self-authored)",
  "- runtime/backups/ (state snapshot at every run boundary)"]
(rt/"REPORT.md").write_text("\n".join(lines) + "\n")
EOF
echo "overnight driver complete"
