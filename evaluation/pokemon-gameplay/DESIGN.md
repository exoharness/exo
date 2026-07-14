# Pokemon Gameplay: Self-Improving Agent Evaluation

A flashy, watchable demonstration of exo's core thesis — an agent that
incrementally improves every aspect of itself — applied to playing Pokemon
Red/Blue on a Game Boy emulator. The agent starts with a minimal strategy
prompt and a handful of primitive tools, and earns its progress by writing
its own playbook, accumulating knowledge, authoring new tools, and rewinding
its own state when it gets stuck.

## Goal

When the game boots, the agent plays autonomously: reads the screen, decides
what to do, presses buttons, handles battles/dialogs/menus as they come up,
and pursues long-horizon goals (leave Pallet Town, win badges). While doing
so it should **visibly self-improve**: the playbook, memory files, tool set,
and todo list it runs on at turn 500 should look meaningfully smarter than at
turn 1 — and objective progress (milestones from game RAM) should show it.

## Architecture

The agent is an exo TypeScript harness (the exo CLI runs the turn loop);
the emulator is a local sidecar process:

```
evaluation/pokemon-gameplay/
  DESIGN.md              <- this file
  README.md              <- run instructions
  run.sh                 <- venv bootstrap + starts the emulator sidecar
  drive.sh               <- long-run driver: chains exo turns, snapshots, gif/report
  emulator/
    server.py            <- PyBoy sidecar: HTTP JSON API on localhost
    memory_map.py        <- Pokemon Red/Blue RAM addresses -> structured state
    requirements.txt     <- pyboy, pillow
  agent/
    harness.ts           <- exo TypeScript harness: instructions + tool registry hooks
    emulator-client.ts   <- HTTP client for the sidecar
    game-tools.ts        <- press_buttons, wait, checkpoints
    self-tools.ts        <- playbook / memory / todos / install_tool
    skills.ts            <- install_skill / use_skill (agent-skills standard)
    context.ts           <- per-round prompt assembly (playbook, memory, progress, screen)
    events.ts            <- side JSONL event log + progress tracking (feeds viewer.py)
  prompts/
    system.md            <- fixed harness rules (committed, agent cannot edit)
    playbook.seed.md     <- initial strategy playbook, copied to runtime/ on first run
  runtime/               <- gitignored; everything the agent owns and mutates
    playbook.md            agent-edited strategy prompt
    memory/*.md            agent-authored knowledge files
    tools/*.mjs            agent-authored tools, hot-loaded each turn
    todos.json             goal stack
    checkpoints/*.state    PyBoy save states
    screenshots/           per-turn PNGs (demo material)
    events.jsonl           canonical history (agent cannot edit — exo rule)
    progress.jsonl         milestone log derived from RAM
  roms/                  <- gitignored; user supplies pokemon.gb here
```

### Emulator sidecar (`emulator/server.py`)

PyBoy 2.x headless (`window="null"`), wrapped in a small stdlib
`http.server` JSON API — no Python web framework dependency:

- `POST /press {buttons: ["up","up","a"], hold_frames, wait_frames}` — press a
  sequence; returns post-press screenshot + state.
- `POST /tick {frames}` — advance time (dialog scrolls, animations).
- `GET /frame` — screenshot (PNG base64, 3x nearest-neighbor upscale so the
  vision model can read 8px text) + structured state.
- `POST /checkpoint/save {name}` / `POST /checkpoint/load {name}` — PyBoy save
  states.
- `POST /reset` — reboot the ROM.

`memory_map.py` reads well-documented Pokemon Red/Blue RAM addresses into a
structured state object sent with every frame: map id + name, player x/y and
facing, in-battle flag, party (species/level/hp), badges, money, Pokedex
owned count, wram event flags for key milestones. This gives the harness
**objective, un-fakeable progress data** independent of the model's own
claims.

Emulation speed is irrelevant between turns (headless runs ~100x realtime);
the sidecar holds the game paused (no ticking) while the agent thinks, so
the game world never moves without an explicit agent action. Determinism +
turn-based control = clean demo narrative.

### The exo harness (`agent/harness.ts`)

The agent runs over exo: one **turn** = one `exo conversation send`, and exo
owns the model loop, tool round trips, and conversation history. The harness
supplies two hooks:

1. `instructions` — re-runs before **every model round**: re-reads
   `playbook.md` + todos + memory index + skills index (the agent may have
   just edited them), observes RAM for objective milestones
   (auto-checkpointing each one), runs stuck detection, and injects the
   current screenshot + state. Screens are never accumulated: the model
   always sees the frame as it is now.
2. `registerTools` — rebuilds the tool registry every round from game tools,
   self tools, skill tools, and the agent-authored `.mjs` tools on disk, so a
   tool installed with `install_tool` is callable on the very next round
   trip. The evaluation's `AgentTool` shape is bridged onto exo's registry by
   a small adapter in `harness.ts`.

Turn summaries and tool results persist in exo's event log — nothing
summary-related is hand-rolled here anymore. Model choice is exo agent
config (`--model` at `agent create`).

## Self-improvement mechanisms (the point of the demo)

All mirrors of exo's own architecture, scoped to this evaluation:

1. **Editable playbook** (`update_playbook` tool): `runtime/playbook.md` is
   injected into every turn's prompt and the agent is explicitly told it owns
   it. Starts nearly empty ("You know how to press buttons. Figure the rest
   out."). Expected evolution: button timing lore, battle heuristics, menu
   maps, route strategies.
2. **Memory** (`save_memory` / `read_memory` / `list_memories`): knowledge
   files for things too big for the playbook — town maps, NPC dialog notes,
   type matchups it has verified in-game.
3. **Todos** (`update_todos`): persistent goal stack shown every turn;
   long-horizon planning across turns that individually only see one screen.
4. **Agent-authored tools** (`install_tool` / `uninstall_tool`): the agent
   writes an ES module (`{name, description, parameters, execute}` — same
   shape as exo library tool modules) into `runtime/tools/`; it is hot-loaded
   into the registry on the next turn. Expected: movement macros
   ("walk_path"), battle routines, dialog-mashing helpers that compose the
   press/tick primitives it gets via an injected emulator client.
5. **Checkpoint / rewind** (`save_checkpoint` / `load_checkpoint`): the
   exo snapshot-rewind story. Harness auto-checkpoints on every milestone;
   the agent can rewind when it wedges itself (blacked out, stuck in a menu).
6. **Forced reflection**: every 10th turn, `drive.sh` sends a reflection
   prompt instead of a play prompt: "Review recent turns. Update the
   playbook, memories, todos, and tools before playing on." Self-improvement
   happens even if the model wouldn't volunteer it.
7. **Stuck detection**: the instructions hook hashes (map, x, y, in_battle,
   screen) across model rounds; unchanged for several rounds → inject an
   escalating nudge that names the options: try different buttons, write a
   note about what doesn't work, rewind.

Canonical history is exo's own event log (inspect with `exo conversation
events`); `runtime/events.jsonl` is a side log that feeds the live viewer.
Neither is exposed through any agent tool — history stays trustworthy.

## Progress display

- `progress.jsonl` — timestamped milestones (new map entered, party level up,
  badge earned, money delta, Pokedex growth) derived purely from RAM.
- Console output — a live one-line-per-turn narration: turn number, action
  count, location, party, last milestone, and a marker whenever a
  self-improvement tool ran (`[PLAYBOOK]`, `[NEW TOOL: walk_path]`, ...).
- `runtime/screenshots/` — frame-per-turn, ready to be stitched into the gif
  the top-level README already stubs (`docs/images/exo_playing.gif`).

## What "working well" looks like

Minimum demo bar (Pokemon Red from a fresh save):

- Gets through the title screen and Oak's intro dialog unaided.
- Names itself, leaves the house, triggers the Oak lab sequence, gets a
  starter — first milestones within ~50 turns.
- Playbook and at least one installed tool visibly in use by then.

Stretch: Viridian City, first Pokemon Center heal, Route 1 battles won.

## Risks / open questions

- **Vision fidelity**: models misread 160x144 sprite text. Mitigations:
  3x upscale, RAM-state annotation alongside every frame (position/party/
  battle flag don't depend on vision). Stretch: decode on-screen dialog text
  from the tilemap + charmap and attach it as text.
- **Token burn**: only the current round's screenshot is ever in context;
  `maxToolRoundTrips` on the agent config caps rounds per turn. Long runs
  accumulate exo conversation history — fork the conversation (or start a
  fresh one; playbook/memory/tools carry the learning) when it gets heavy.
- **ROM**: not committed (copyright); `roms/` is gitignored and the user
  supplies the `.gb`. Save states in `runtime/` are derived from it, also
  gitignored.
- **Model choice**: gpt-5.5 assumed good enough at sprite reading; if not,
  the model client is one env var away from any Responses-compatible model.

## Build order

1. `emulator/memory_map.py` + `emulator/server.py` (testable once ROM lands).
2. `agent/` core loop with game tools only — watch it boot the game.
3. Self-tools (playbook/memory/todos/install_tool) + reflection cadence.
4. Stuck detection, checkpoints, progress log, console narration.
5. Tune the seed prompts against live play. Capture demo material.
