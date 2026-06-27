# Pokémon — a self-improving exo agent that plays a Game Boy game

exo plays **Pokémon Red** from the screen: each turn it sees the current
screenshot (plus the previous one) and chooses button presses; the emulator
advances; repeat. The twist versus a normal "agent plays Pokémon" demo: **the
agent improves *itself* as it plays** — it has durable memory, can write its own
tools, and can rewrite its own policy, all live, mid-run, with no resets.

It's a standalone eval (no external benchmark framework) — just a
[PyBoy](https://github.com/Baekalfen/PyBoy) Game Boy emulator and the exo agent
loop, plus a small web frontend to watch it.

> You must supply your own **legally-obtained ROM**. ROMs are copyrighted and are
> not included here.

---

## The idea: one self-contained, self-rewriting file

The agent's entire "brain" is a single TypeScript harness,
`examples/simple-coding-agent/harness-pokemon-selfimprove.ts`:

- **how it plays** (the prompt / policy),
- **what it perceives** (it injects the current + previous screenshot each turn),
- **the self-improvement levers it can use** (memory, building tools, self-edit, introspection).

The agent is given generic self-improvement machinery and asked to *learn to play*
— it is **not** given Pokémon knowledge (no maps, routes, or battle tactics baked
in). It improves itself the exoclaw way, with named, durable levers (it has a
shell, and its harness file is mounted read-write into its sandbox):

1. **Durable memory** (`remember` / `forget`) — facts that persist across turns
   and even conversation resets.
2. **Build its own tools** (`install_agent_tool` / `uninstall_agent_tool`) —
   create persistent, reusable tool modules (e.g. a screen reader, a route
   tracker) that load every later turn, not just this one.
3. **Rewrite its own policy** — edit that one harness file to change strategy,
   perception, anything.
4. **Inspect itself** (`list_conversation_events`) — review its own recent
   history to catch loops and failed actions.

The runner re-imports the file **every turn** (hot-swap), so edits take effect
immediately; it **validates each edit and rolls back** to the last working version
if one won't load (or a tool's schema is rejected). So the agent can experiment
safely while a single continuous playthrough keeps moving forward.

`pokemon_runner.py` is the driver:

1. Capture the emulator screen → `/tmp/exo-pokemon/screen.png` (and the prior
   frame → `prev_screen.png`).
2. Run one exo turn; the harness injects both frames and asks for the next
   button(s). The model reasons, then replies with `{"buttons": ["a","up",...]}`.
3. Parse the buttons, press them in PyBoy, advance frames.
4. Repeat in one persistent conversation (rolled every `--conv-reset-every` turns
   to bound context; durable memory carries continuity across resets).

It also reads game RAM (map id, x/y, badges, party level) **purely to score
progress** — this is never shown to the agent; it plays from the screen alone.

---

## What's here

| Path                | What                                                                          |
| ------------------- | ---------------------------------------------------------------------------- |
| `pokemon_runner.py` | PyBoy driver + exo turn loop; self-edit validate/rollback; game-RAM scoring. |
| `examples/simple-coding-agent/harness-pokemon-selfimprove.ts` | The self-evolving harness (policy + perception + memory/build-tools/self-edit/introspection levers). |
| `live_server.py`    | Web frontend: game screen, reasoning, memory, tools it built, a cumulative-spend chart with self-improvement markers, and a game-progress/minimap panel. |
| `safe_run.sh`       | Launch ONE run with an OOM/container watchdog (recommended wrapper).         |
| `analyze_run.py`    | Summarize a finished run (maps, progress, cost, tools, memory).              |
| `run.sh` / `setup.sh` | Convenience run wrapper; one-time Python/PyBoy setup.                      |
| `EXPLORATION.md`    | Lab notebook — the experiments and findings behind the current design.       |

---

## Prerequisites

- **A Game Boy ROM you own** (e.g. Pokémon Red), passed via `POKEMON_ROM`.
- **Python 3** + the venv built by `./setup.sh` (installs PyBoy).
- The **exo binary** built from this repo: `cargo build --release` (the runner
  uses `target/release/exo`; override with `EXO_BIN`).
- **Docker** — the self-improve mode runs the agent's `shell` (and its self-edits)
  in an exo sandbox container.
- An API key: **`OPENAI_API_KEY`** (default model `gpt-5.5`), or an
  **`ANTHROPIC_API_KEY`** if you run an Anthropic model like Opus (this branch is
  based on `main`, which has Anthropic provider support).

---

## Setup

```bash
cd evaluation/pokemon
./setup.sh                      # creates .venv with PyBoy
# build exo once (from repo root): cargo build --release
# put your ROM somewhere and point POKEMON_ROM at it
```

Optional: start from a save state (skips the intro) by passing `--state`.

---

## Run it

The recommended wrapper is **`safe_run.sh`** — it runs exactly one run with a
watchdog that prunes sandbox containers and aborts if free RAM gets low (a long
self-improving run can otherwise accumulate Docker sandboxes):

```bash
OPENAI_API_KEY=sk-... POKEMON_ROM=$PWD/pokemon_red.gb \
  ./safe_run.sh runs/myrun -- \
    --steps 500 --self-improve --conv-reset-every 40 \
    --state pokemon_red_start.state
```

Everything after `--` is forwarded to `pokemon_runner.py`. Useful flags:

| Flag | Meaning |
| ---- | ------- |
| `--steps N`            | number of turns (one continuous playthrough). |
| `--self-improve`       | enable memory + agent-built tools + policy self-edit + introspection (the whole point). |
| `--conv-reset-every N` | roll the conversation every N turns (bounds context/latency; memory persists). |
| `--state FILE`         | start from a PyBoy save state. |
| `--save-state FILE`    | write a save state at the end. |
| `--out DIR`            | output dir (frames, logs, session.json). |

Pick the model with `MODEL=` (default `gpt-5.5`); to use an Anthropic model,
register it / supply `ANTHROPIC_API_KEY` accordingly.

A plain (non-watchdog) run also works: `OPENAI_API_KEY=... POKEMON_ROM=... ./run.sh --steps 100`.

---

## Watch it live (frontend)

```bash
.venv/bin/python live_server.py --port 8080          # localhost only
.venv/bin/python live_server.py --port 8080 --host 0.0.0.0 --read-only   # shareable, read-only
```

Open `http://localhost:8080`. The page shows, auto-refreshing:

- the **game screen** + current turn / buttons,
- the agent's **reasoning**, its **durable memory**, and the **tools it built**,
- a **cumulative-spend chart** with markers for each self-improvement (tool built
  / policy edit / memory learned), and a **"what it changed about itself"** log
  (the actual code/policy it added to its own file),
- a **game-progress** panel (maps visited, badges, position) + minimap.

`--read-only` hides the live "coach" input and rejects writes — safe for sharing.
(To expose it publicly you can put a tunnel like `tailscale funnel 8080` in front.)

---

## Notes

- **Not committed** (regenerated locally): the ROM and save states (`*.gb`,
  `*.state`), per-run outputs (`runs/`), the `.venv`, and the agent-mutated harness
  copy (`harness-pokemon-self.ts`, created from the committed source each run).
- The agent plays from the **screen only**; game RAM is read solely to score
  progress and is never fed to the model.
- See `EXPLORATION.md` for the history of what worked (and didn't) — e.g. why the
  agent needs the previous frame to notice it's stuck, and why chain-of-thought
  in the output matters.
