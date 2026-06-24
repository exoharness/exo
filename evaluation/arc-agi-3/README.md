# ARC-AGI-3 — Exo eval (interactive reasoning)

Runs Exo on [ARC-AGI-3](https://arcprize.org/arc-agi/3) (launched March 2026), the
**interactive** reasoning benchmark. Unlike ARC-AGI-1/2 (static `input -> output`
grid puzzles — see `../arc-agi/`), v3 drops an agent into a game with **no
instructions, no stated goal, and no rules**. The agent must infer everything by
acting and observing over many steps, across 8-10 levels per game. At launch every
frontier model scored **<1%** while humans solved all environments — so expect low
scores; this is infrastructure for a very hard, contamination-resistant target.

## How it integrates

ARC-AGI-3 ships an official agent framework — [`arcprize/ARC-AGI-3-Agents`](https://github.com/arcprize/ARC-AGI-3-Agents)
— that owns the hosted game API (scorecards, frames, actions) and the play loop.
We plug in an **exo policy**, the same way Horizon plugs exo into Harbor:

- **`exo_agent/exo_arc_agent.py`** — `ExoArc(arcengine.Agent)` (registered as
  `exoarc`). The framework calls `choose_action(frames, latest_frame)` each turn;
  we render the current grid + legal actions into a prompt, run **host-side exo**
  over **one persistent conversation per game** (so exo accumulates context and can
  learn the game across steps), and parse exo's JSON reply into a `GameAction`
  (`ACTION1..7`/`RESET`; `ACTION6` carries x,y coordinates). `NOT_PLAYED`/
  `GAME_OVER` states auto-`RESET` (the API requires it).
- **`setup.sh`** clones the framework, `uv sync`s it, **symlinks our policy into
  `agents/templates/`** and registers it in `agents/__init__.py`, and ensures a
  host exo binary.
- Default exo harness: `examples/simple-coding-agent/harness-arc3.ts` — tool-less,
  pure reasoning (a game-playing system prompt; the per-step frame arrives as the
  conversation message). No shell needed; nothing outside the prompt is reachable.

## Quickstart

```bash
./setup.sh                                              # clone framework + link policy + build exo
OPENAI_API_KEY=… ARC_API_KEY=… ./run.sh                # play game ls20
OPENAI_API_KEY=… ARC_API_KEY=… ./run.sh vc33           # another public game
OPENAI_API_KEY=… ARC_API_KEY=… GAME=ft09 ./run.sh
```

- **`ARC_API_KEY` is required** — the games are hosted; get a key at
  [arcprize.org](https://arcprize.org/). Without it the framework can't fetch
  frames. `OPENAI_API_KEY` is exo's model key.
- Public games: **`ft09`, `ls20`, `vc33`** (3 private games — `sp80`, `lp85`,
  `as66` — decide the leaderboard and aren't playable here).
- Knobs (env): `MODEL` (default `gpt-5.5`), `EXO_HARNESS`, `ARC3_MAX_ACTIONS`
  (default 80), `ARC_SANDBOX`, `ARC3_REPO`.

## Status / notes

- **Scoring** is handled by the framework (levels completed / win); results print
  per game. Score on the public games is the dev signal; the official leaderboard
  uses the private games + the ARC Prize 2026 Kaggle competition.
- This is the agentic sibling of `../arc-agi/` (v1/v2 single-shot grids). The
  obvious next lever is richer per-turn scaffolding (explicit hypothesis tracking,
  letting exo request a coordinate scan) and longer action budgets.
- **Validated offline** (no `ARC_API_KEY` needed): the policy registers as
  `exoarc`, exo reasons about a synthetic frame and returns a valid parsed
  `GameAction`, and the persistent per-game conversation produces genuine
  cross-step learning (e.g. turn 1 "probe ACTION1", turn 2 "ACTION1 had no effect,
  try ACTION2"). The **live game API** path needs an `ARC_API_KEY` (not yet run).
