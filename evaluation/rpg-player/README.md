# RPG Player — Self-Improving Agent Evaluation (EmulatorJS)

An exo-style self-improving agent playing **Phantasy Star** on a Sega Master
System emulated by [EmulatorJS](https://emulatorjs.org) (RetroArch cores in
WASM) inside headless Chromium. The agent starts with a near-empty playbook
and primitive button tools, and improves itself while playing: it rewrites
its own prompt injection (playbook), saves memories, maintains a todo stack,
authors new tools that hot-load into its registry, packages procedures as
skills, and rewinds with checkpoints when wedged.

This is the EmulatorJS sibling of
[`evaluation/pokemon-gameplay`](../pokemon-gameplay/) — same agent
architecture and sidecar HTTP contract, different emulator backend. One
sidecar supports many consoles (`--core nes`, `snes`, `gba`, `segaMD`, ...);
Phantasy Star on `segaMS` is the default. See [PLAN.md](./PLAN.md) for the
architecture and trade-offs, and [HOW-IT-WORKS.md](./HOW-IT-WORKS.md) for
how the turn loop and learning machinery work.

## Run

```bash
pnpm install                              # repo root, once
cd evaluation/rpg-player
cp /path/to/phantasy-star.sms roms/       # ROM is not committed (gitignored)
export OPENAI_API_KEY=...
./run.sh                                  # ^C to stop; state persists
```

First run downloads the Playwright Chromium build. The agent runs until ^C
(or `RPG_TURNS=N ./run.sh` for a bounded run) and resumes where it left off —
history, playbook, memories, tools, skills, and checkpoints all live in
`runtime/` (gitignored).

Env knobs: `RPG_MODEL` (default `gpt-5.4`), `RPG_REASONING` (default `low`),
`RPG_TURNS`, `RPG_EMULATOR_PORT`, `RPG_HEADED=1` (watch the emulator in a
real Chromium window), `RPG_EJS_DATA_URL` (self-hosted EmulatorJS data dir
instead of the pinned CDN).

## Watching it

- `RPG_HEADED=1 ./run.sh` — the emulator runs in a visible Chromium window
  with the game audio on (headless runs are muted).
- Console: one line per turn (distinct screens seen, actions, token spend)
  with `★ MILESTONE` banners and self-improvement markers
  (`[PLAYBOOK updated]`, `[NEW TOOL: map_dungeon]`, `[CLAIMED: ...]`,
  `[REWIND: ...]`).
- `runtime/playbook.md`, `runtime/memory/`, `runtime/tools/`,
  `runtime/skills/`, `runtime/todos.json` — watch the agent's own artifacts
  evolve mid-run.
- `runtime/events.jsonl` — append-only canonical log of every tool call,
  milestone, and turn summary (the agent has no tool that can edit it).
- `runtime/progress.jsonl` — objective screen-novelty milestones measured by
  the harness.
- `runtime/screenshots/` — every distinct screen the agent saw, numbered,
  ready to stitch into a GIF.

## Objective progress without RAM

The PyBoy harness reads Pokemon's RAM for objective milestones; EmulatorJS
has no supported cross-core memory API, so this harness measures **screen
novelty**: the count of pixel-distinct screens ever seen, hashed by the
sidecar. It cannot be inflated by standing still or re-walking old rooms.
The agent can also `claim_milestone` for story beats; claims are logged as
claims, separate from objective metrics.

## Fresh start

```bash
rm -rf runtime
```
