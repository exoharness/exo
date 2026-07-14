# Task: supervise and improve the RPG player evaluation

You own `evaluation/rpg-player/` in this repo: a small self-improving agent
that plays Phantasy Star (Sega Master System) through an EmulatorJS sidecar.
Your job is NOT to play the game — the player agent does that. Your job is
to be its engineer: keep it running, watch how it plays, and improve its
harness, prompts, and tools so it learns faster. The player agent cannot
modify its own harness; you can. That asymmetry is the whole point.

Read these first (in the repo mount):

- `evaluation/rpg-player/README.md` — how to run it
- `evaluation/rpg-player/HOW-IT-WORKS.md` — the turn loop, what persists,
  the known weaknesses and improvement levers
- `evaluation/rpg-player/PLAN.md` — architecture background

## Setup (one-time)

Your sandbox mounts the repo read-write at `/workspace/exo`, but do not run
`pnpm install` there — the host's `node_modules` is for a different OS.
Work in a container-local copy instead:

1. Run `bash /workspace/exo/evaluation/rpg-player/supervisor/sandbox-setup.sh`.
   It copies the repo to `~/rpg` (excluding node_modules/state), installs
   Node 22 + pnpm if missing, installs dependencies and Playwright's Linux
   Chromium with system deps, and copies the ROM from the mounted repo.
   If it fails, read it and fix the problem yourself — you have root.
2. Confirm `OPENAI_API_KEY` is set in your environment (the player agent
   needs it to call its model). If it is missing, stop and ask.

## Operating loop

Repeat until told to stop:

1. **Sync**: `rsync -a --delete --exclude runtime --exclude roms \
/workspace/exo/evaluation/rpg-player/ ~/rpg/evaluation/rpg-player/`
   (picks up any code you edited in the mount; preserves the run state).
2. **Run a chunk**: `cd ~/rpg/evaluation/rpg-player && RPG_TURNS=25 ./run.sh`.
   Headless is expected; the run resumes from its persisted state.
3. **Study the run**:
   - `runtime/history.json` — turn summaries; where is it wasting turns?
   - `runtime/progress.jsonl` + console output — is screen novelty growing?
   - `runtime/playbook.md`, `runtime/memory/` — is what it "learned"
     specific, correct, actually consulted? Is the turn-1 knowledge dump
     good, or thin/hallucinated?
   - `runtime/memory/harness_wishlist.md` — capabilities it says it needs
     but cannot build. Treat this as your feature backlog.
4. **Improve ONE thing at a time**, editing the code in `/workspace/exo`
   (so the human sees your changes in their working tree), then re-sync and
   re-run. Judge each change by turns-to-milestone and screen novelty, not
   by vibes. Candidate improvements, roughly in expected-value order:
   - prompt wording (bootstrap directive, system.md, reflection audits)
   - budget constants in `agent/run.ts` / `agent/context.ts`
   - a RAM probe in `emulator/server.ts` reading player x/y/map-id from the
     WASM heap via `EJS_emulator.gameManager` so the player gets objective
     position sense (the wishlist will likely ask for this)
   - a pre-installed dungeon-mapping tool
5. **Keep a log**: append every experiment (what you changed, why, and the
   before/after metrics) to
   `/workspace/exo/evaluation/rpg-player/supervisor/LOG.md`.

## Rules

- Never edit or delete `runtime/events.jsonl`, `runtime/progress.jsonl`, or
  anything under `runtime/checkpoints/` — canonical history is sacred here,
  same as in exo proper.
- Do not commit; leave changes in the working tree for the human to review.
- Do not put the ROM or any copyrighted game content in the repo.
- One variable per experiment. If a change makes things worse, revert it
  and log that too — negative results count.
- If the player agent crashes, diagnose from its output and the sidecar log
  before restarting; fix the cause, not the symptom.

Report back after each chunk: turns played, milestones, what you changed,
what you'll try next.
