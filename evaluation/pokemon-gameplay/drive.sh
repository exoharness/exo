#!/usr/bin/env bash
# Long-run driver: chain exo turns until a total-turn target, snapshotting
# state at every chunk boundary and producing a GIF + report at the end.
#
#   ./drive.sh --agent pokemon --conversation play --target 250 \
#              [--runtime runtime] [--chunk 50] [--port 8777]
#
# Assumes: the emulator sidecar is running on the port (drive.sh restarts it
# from the runtime's checkpoints if it dies mid-run), and the exo agent +
# conversation already exist (see README.md). Every 10th turn is a
# no-buttons reflection turn where the agent is told to improve its
# playbook/memories/tools/skills instead of playing.
set -u
cd "$(dirname "$0")"

EXO="${EXO_BIN:-exo}"
AGENT=pokemon
CONVERSATION=play
RUNTIME=runtime
TARGET=250
CHUNK=50
PORT="${POKEMON_EMULATOR_PORT:-8777}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --agent) AGENT="$2"; shift 2 ;;
    --conversation) CONVERSATION="$2"; shift 2 ;;
    --runtime) RUNTIME="$2"; shift 2 ;;
    --target) TARGET="$2"; shift 2 ;;
    --chunk) CHUNK="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done
mkdir -p "$RUNTIME/backups"
VENV=$(ls -d runtime*/venv 2>/dev/null | head -1)

PLAY_PROMPT="Continue playing. Act toward your top todo, then finish with a short summary: what you did, what you learned, what to do next turn."
REFLECT_PROMPT="This is a scheduled self-improvement turn. Do NOT press any buttons. Review your recent turns, then update your playbook/memories/todos, and install a tool or skill if you keep repeating a mechanical sequence. Finish with a short summary of what you changed."

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

T=0
while [ "$T" -lt "$TARGET" ]; do
  T=$((T + 1))
  ensure_emulator
  if [ $((T % CHUNK)) -eq 0 ]; then snapshot "$T"; fi
  PROMPT="$PLAY_PROMPT"
  if [ $((T % 10)) -eq 0 ]; then PROMPT="$REFLECT_PROMPT"; fi
  echo "=== drive: turn $T/$TARGET ===" >> "$RUNTIME/agent.log"
  POKEMON_EMULATOR_URL="http://127.0.0.1:$PORT" \
    POKEMON_RUNTIME_DIR="$PWD/$RUNTIME" \
    "$EXO" conversation send "$AGENT" "$CONVERSATION" "$PROMPT" \
    >> "$RUNTIME/agent.log" 2>&1
  if [ $? -ne 0 ]; then sleep 10; fi # crash-loop guard
done

snapshot "$T"
echo "=== drive: finished at $T turns, wrapping up ===" >> "$RUNTIME/agent.log"
"$VENV/bin/python" emulator/make_gif.py --fps 6 \
  --screenshots "$RUNTIME/screenshots" --out "$RUNTIME/run.gif" \
  >> "$RUNTIME/agent.log" 2>&1
RUNTIME_DIR="$RUNTIME" TURNS="$T" python3 - <<'EOF'
import json, os, pathlib
rt = pathlib.Path(os.environ["RUNTIME_DIR"])
milestones = [json.loads(l) for l in (rt/"progress.jsonl").read_text().splitlines()] if (rt/"progress.jsonl").exists() else []
events = [json.loads(l) for l in (rt/"events.jsonl").read_text().splitlines()] if (rt/"events.jsonl").exists() else []
tools = sorted(p.stem for p in (rt/"tools").glob("*.mjs")) if (rt/"tools").is_dir() else []
skills = sorted(p.name for p in (rt/"skills").iterdir() if (p/"SKILL.md").is_file()) if (rt/"skills").is_dir() else []
memories = sorted(p.stem for p in (rt/"memory").glob("*.md")) if (rt/"memory").is_dir() else []
improvements = sum(1 for e in events if e.get("type") == "improvement")
lines = ["# Run report", "",
  f"- turns driven: {os.environ['TURNS']}",
  f"- milestones: {len(milestones)}",
  f"- self-improvement actions: {improvements}",
  f"- agent-built tools: {', '.join(tools) or '(none)'}",
  f"- skills: {', '.join(skills) or '(none)'}",
  f"- memory files: {len(memories)}", "",
  "## Milestones"]
lines += [f"- round {m['turn']}: {m['milestone']}" for m in milestones]
lines += ["", "(Turn summaries live in the exo conversation; inspect with `exo conversation events`.)"]
(rt/"REPORT.md").write_text("\n".join(lines) + "\n")
EOF
echo "drive complete at $T turns"
