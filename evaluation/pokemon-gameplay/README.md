# Pokemon Gameplay — Self-Improving Agent Evaluation

A self-improving **exo agent** playing Pokemon Red/Blue on a headless Game
Boy emulator (PyBoy). The agent starts with a near-empty playbook and
primitive button tools, and improves itself while playing: it rewrites its
own prompt injection (playbook), saves memories, maintains a todo stack,
authors new tools that hot-load into its registry, and rewinds with
checkpoints when wedged. Progress is measured objectively from game RAM.

The agent runs as an exo TypeScript harness
([`agent/harness.ts`](./agent/harness.ts)): exo owns the turn loop,
conversation history, and event log; this evaluation supplies the game and
the self-improvement surface. Self-improvement state (playbook, memories,
tools, skills, checkpoints) lives on disk in `runtime/` exactly as before,
so runs resume and the live viewer keeps working.

See [DESIGN.md](./DESIGN.md) for the architecture.

## Run

**1. Start the emulator sidecar** (leave it running):

```bash
cd evaluation/pokemon-gameplay
cp /path/to/pokemon-red.gb roms/          # ROM is not committed (gitignored)
./run.sh                                  # boots PyBoy on http://127.0.0.1:8777
```

First run creates a Python venv and installs PyBoy.

**2. Create the exo agent and play** (repo root, another terminal):

```bash
exo secret set openai --env OPENAI_API_KEY
exo model register gpt-5.5 --secret openai

exo --harness typescript agent create "Pokemon" \
  --module evaluation/pokemon-gameplay/agent/harness.ts \
  --model gpt-5.5
exo conversation create pokemon "Play"
exo conversation send pokemon play "Start playing. Work toward your top todo."
```

Each `send` runs one turn; the agent's summary persists in the conversation.
For long unattended runs, chain turns with the driver (reflection turn every
10th, state snapshot every chunk, GIF + report at the end):

```bash
./drive.sh --agent pokemon --conversation play --target 250
```

Env knobs: `POKEMON_EMULATOR_URL` / `POKEMON_EMULATOR_PORT` (default
`http://127.0.0.1:8777`), `POKEMON_RUNTIME_DIR` (default `runtime/`).
Model and reasoning are exo agent config now (`--model` at `agent create`).

## Watching it

- **Live dashboard**: `python3 viewer.py` then open <http://127.0.0.1:8778> —
  current game frame, run log, RAM milestones, the agent's todos, and its
  self-edited playbook, refreshing every second. Reads only `runtime/`, so it
  never slows the agent down.
- `exo conversation events pokemon play` — the canonical exo event log:
  every model round, tool call, tool result, and turn summary.
- `runtime/playbook.md`, `runtime/memory/`, `runtime/tools/`,
  `runtime/todos.json` — watch the agent's own artifacts evolve mid-run.
- `runtime/events.jsonl` — append-only side log of rounds, milestones, and
  self-improvement markers (feeds the viewer).
- `runtime/progress.jsonl` — objective milestones derived from RAM.

## GIF of the run

Screenshots of every distinct screen the agent saw are in
`runtime/screenshots/`. Stitch them:

```bash
runtime/venv/bin/python emulator/make_gif.py --fps 6
# -> runtime/run.gif
```

## Fresh start

```bash
rm -rf runtime   # deletes venv too; next run.sh recreates it
```

Plus `exo conversation create pokemon ...` for a fresh conversation (or
`exo agent delete pokemon` to wipe the exo side entirely).
