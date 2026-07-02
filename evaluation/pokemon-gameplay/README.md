# Pokemon Gameplay — Self-Improving Agent Evaluation

An exo-style self-improving agent playing Pokemon Red/Blue on a headless
Game Boy emulator (PyBoy). The agent starts with a near-empty playbook and
primitive button tools, and improves itself while playing: it rewrites its
own prompt injection (playbook), saves memories, maintains a todo stack,
authors new tools that hot-load into its registry, and rewinds with
checkpoints when wedged. Progress is measured objectively from game RAM.

See [DESIGN.md](./DESIGN.md) for the architecture.

## Run

```bash
cd evaluation/pokemon-gameplay
cp /path/to/pokemon-red.gb roms/          # ROM is not committed (gitignored)
export OPENAI_API_KEY=...
./run.sh                                  # ^C to stop; state persists
```

First run creates a Python venv and installs PyBoy. The agent runs until ^C
(or `POKEMON_TURNS=N ./run.sh` for a bounded run) and resumes where it left
off — history, playbook, memories, tools, and checkpoints all live in
`runtime/` (gitignored).

Env knobs: `POKEMON_MODEL` (default `gpt-5.4`), `POKEMON_REASONING`
(default `low`), `POKEMON_TURNS`, `POKEMON_EMULATOR_PORT`.

## Watching it

- **Live dashboard**: `python3 viewer.py` then open <http://127.0.0.1:8778> —
  current game frame, run log, RAM milestones, the agent's todos, and its
  self-edited playbook, refreshing every second. Reads only `runtime/`, so it
  never slows the agent down.
- Console: one line per turn (location, party, actions, token spend) with
  `★ MILESTONE` banners and self-improvement markers (`[PLAYBOOK updated]`,
  `[NEW TOOL: walk_path]`, `[MEMORY: pallet_town_map]`, `[REWIND: ...]`).
- `runtime/playbook.md`, `runtime/memory/`, `runtime/tools/`,
  `runtime/todos.json` — watch the agent's own artifacts evolve mid-run.
- `runtime/events.jsonl` — append-only canonical log of every tool call,
  milestone, and turn summary (the agent has no tool that can edit it).
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
