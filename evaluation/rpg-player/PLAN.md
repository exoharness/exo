# RPG Player: Plan and Recommendation

A self-improving Exo-style agent that plays a console RPG through
[EmulatorJS](https://emulatorjs.org), improving its playbook, memory, skills,
and tools as it plays. This is the EmulatorJS sibling of
`evaluation/pokemon-gameplay/` (see its `DESIGN.md`), which drives Pokemon
Red/Blue through a PyBoy sidecar.

## What carries over from pokemon-gameplay

The pokemon harness cleanly separates "how to run a game" from "how the agent
improves itself." Nearly all of the agent side is reusable as-is:

- **Turn loop** (`agent/run.ts`): compose context → model tool loop → final
  turn summary → append events. Round-trip caps, history compaction, forced
  reflection every N turns, stuck detection.
- **Self-improvement tools** (`agent/self-tools.ts`): editable playbook,
  memory files, todo stack, agent-authored hot-loaded tools.
- **Skills** (`agent/skills.ts`): the agent-skills standard store
  (install/use/read/uninstall with progressive disclosure).
- **Event log + progress tracking** (`agent/events.ts`): append-only
  `events.jsonl` the agent cannot edit — same canonical-history rule as Exo
  proper.
- **Model client** (`agent/model.ts`): OpenAI Responses API with vision +
  function tools, plain fetch.
- **Emulator client interface** (`agent/emulator-client.ts`): a small HTTP
  JSON contract — `/health`, `/frame`, `/press`, `/tick`,
  `/checkpoint/save|load`, `/reset`.

The pokemon PyBoy sidecar (`emulator/server.py`) is the only piece that is
Game Boy-specific. **Recommendation: keep the exact same HTTP contract and
swap the sidecar implementation**, so `agent/` ports over with only naming
and state-shape changes.

## The EmulatorJS difference

EmulatorJS is a browser frontend for RetroArch cores compiled to WASM. It is
a plugin for a web page, not a library with a native process API. That has
three consequences the design has to absorb:

1. **A browser must host it.** The sidecar becomes a Node process that
   launches headless Chromium via Playwright, serves a minimal page embedding
   EmulatorJS, and drives it over CDP. The HTTP JSON API the agent sees stays
   identical to pokemon-gameplay.
2. **Structured RAM state is harder.** PyBoy gives direct memory reads;
   EmulatorJS does not expose a supported, structured memory-read API across
   cores (its cheat support implies core memory access exists, but it is not
   a stable public surface). Plan: start vision-first, and add best-effort
   memory reads via `EJS_emulator.gameManager` / the core's WASM heap where
   the chosen core allows it. Objective progress then comes from a small
   game-specific "probe" module, mirroring `memory_map.py`.
3. **Determinism is weaker.** The emulator free-runs in the browser between
   commands. The sidecar should pause the core between agent turns
   (EmulatorJS pause/play controls) to preserve the pokemon harness's
   "world only moves on agent action" property.

Why accept this extra machinery at all? EmulatorJS buys:

- **Many consoles from one integration**: NES, SNES, GBA, Genesis, PS1 — one
  sidecar, any RPG with a supported core (vs. PyBoy = Game Boy only).
- **A watchable demo for free**: the same page the sidecar drives headlessly
  can be opened in a real browser as the live viewer — no separate
  `viewer.py`.
- Save states, screenshots, and input injection are all supported EmulatorJS
  features (`EJS_onSaveState`, save-state slots, `gameManager`).

## Recommended architecture

```
evaluation/rpg-player/
  PLAN.md                 <- this file
  README.md               <- run instructions
  run.sh                  <- starts sidecar + agent (pnpm, no venv needed)
  emulator/
    server.ts             <- Node sidecar: HTTP JSON API (same contract as
                             pokemon-gameplay) -> Playwright -> EmulatorJS
    page/index.html       <- minimal EmulatorJS embed (core, ROM, callbacks)
    probes/<game>.ts      <- optional per-game RAM probes -> structured state
  agent/                  <- ported from pokemon-gameplay, renamed env vars
    run.ts model.ts emulator-client.ts game-tools.ts self-tools.ts
    skills.ts context.ts events.ts tool-types.ts
  prompts/
    system.md             <- fixed rules (agent cannot edit)
    playbook.seed.md      <- near-empty seed playbook
  runtime/                <- gitignored: playbook, memory, tools, skills,
                             checkpoints, screenshots, events.jsonl
  roms/                   <- gitignored; user supplies the ROM
```

### Sidecar API (unchanged contract)

- `GET /health` — core + ROM loaded.
- `GET /frame` — PNG screenshot (upscaled for vision) + structured state
  (from the game probe if available, else `{}`) + screen hash.
- `POST /press {buttons, hold_frames, wait_frames}` — inject input via
  EmulatorJS's `gameManager.simulateInput` (RetroPad ids), tick, return
  frame.
- `POST /tick {frames}` — unpause for N frames, repause, return frame.
- `POST /checkpoint/save|load {name}` — EmulatorJS save states, persisted to
  `runtime/checkpoints/`.
- `POST /reset` — reload the ROM.

### Game choice

**Phantasy Star (Sega Master System, 1987)** on the `segaMS` core
(genesis_plus_gx). It is turn-based and menu-driven (pausing between turns is
harmless), historically significant, and hard in an interesting way: its
dungeons are first-person 3D mazes, which forces the agent to build and
maintain its own maps in memory files — a strong test of the
self-improvement loop. SMS quirk worth knowing: the console PAUSE button
opens the in-game command menu, which the sidecar exposes as the `pause`
button. Other consoles (NES, SNES, GBA, Genesis) are a `--core` flag away.

### Objective progress without guaranteed RAM access

Layered, most-reliable first:

1. **Probe-based milestones** (when the core exposes memory): level-ups,
   gold, map transitions, story flags — same as pokemon's `progress.jsonl`.
2. **Screen-hash novelty**: count of distinct screens seen — cheap,
   game-agnostic exploration signal.
3. **Model-claimed milestones, harness-verified later**: the agent reports
   "reached Garinham"; kept out of objective metrics unless a probe confirms.

## Build order

1. **Sidecar skeleton**: Playwright + EmulatorJS page + `/health`, `/frame`,
   `/press`, `/tick` with the segaMS core; verify input injection and pausing
   from a script (no model).
2. **Port `agent/`** from pokemon-gameplay: rename env vars (`RPG_MODEL`,
   `RPG_TURNS`, `RPG_EMULATOR_URL`), make `GameState` generic
   (`Record<string, unknown>` + `describeState` provided by the probe),
   watch it boot the game.
3. **Checkpoints + reset** via EmulatorJS save states; auto-checkpoint on
   milestone.
4. **First game probe** (Dragon Warrior or FF1 RAM map) for objective
   progress; wire `progress.jsonl` and console narration.
5. **Live viewer**: serve the same EmulatorJS page non-headless with a
   read-only "spectator" toggle; plus the runtime-file dashboard if wanted.
6. **Tune seed prompts** against live play; capture demo material (GIF from
   `runtime/screenshots/`).

## Risks

- **Input injection fidelity**: EmulatorJS maps keyboard → core input; timing
  of keydown/keyup under CDP needs verification (step 1 exists to de-risk
  this first).
- **Memory access variance by core**: probes are optional by design; the
  harness must be fully functional vision-only.
- **WASM performance headless**: NES cores are light; if frame stepping is
  slow, batch ticks server-side.
- **ROM licensing**: same rule as pokemon-gameplay — `roms/` is gitignored,
  user supplies the file.
- **EmulatorJS API stability**: `gameManager` internals are not a stable
  public API; pin the EmulatorJS version in `page/index.html` (self-hosted
  `data/` directory, not the CDN "latest").
