# ARC-AGI — Exo eval

Runs Exo on [ARC-AGI](https://arcprize.org/) (François Chollet's Abstraction and
Reasoning Corpus). Each task gives a few `input -> output` grid demonstrations
that share one hidden transformation rule; the solver must produce the output
grid for held-out test input(s). Scored by **exact grid match**.

Unlike a perception or knowledge test, ARC is pure fluid reasoning — a clean
probe of whether the model can infer and apply a novel rule from a handful of
examples. It complements our other evals: Terminal-Bench/Horizon (agentic
tool-use) and clbench (continual learning).

## How it integrates

ARC has no agent framework or CLI to plug into (unlike harbor/clbench) — it's just
JSON task files plus an exact-match scoring convention. So `arc_runner.py` is the
harness:

- For each task it builds a prompt (the train pairs + the test input grid(s)),
  drives **host-side exo** (secret/model/agent/conversation/send — the same
  pattern as the clbench `ExoSystem`), parses the predicted grid(s) from exo's
  final message, and compares to the **withheld** true outputs.
- The public eval JSONs include the test answers (for self-scoring). The runner
  strips them from the prompt and keeps them only for scoring — and the default
  harness has **no tools**, so the agent can't read the on-disk answer files
  either. (Lesson carried over from clbench's local-process answer-key leak.)
- Default harness: `examples/simple-coding-agent/harness-arc.ts` — pure reasoning,
  no shell. Point `EXO_HARNESS` at a shell harness (+ `ARC_SANDBOX=docker`) to try
  an agentic program-synthesis approach instead.

## Quickstart

```bash
./setup.sh                                  # clone ARC-AGI v1+v2 datasets + build exo
OPENAI_API_KEY=… ./run.sh                   # 10 ARC-AGI-1 evaluation tasks, pass@1
OPENAI_API_KEY=… ARC_N=50 ./run.sh          # more tasks
OPENAI_API_KEY=… ARC_VERSION=2 ./run.sh     # ARC-AGI-2
OPENAI_API_KEY=… ARC_SPLIT=training ./run.sh
```

Knobs (env): `ARC_VERSION` (1|2), `ARC_SPLIT` (evaluation|training), `ARC_N`
(task count), `MODEL` (default `gpt-5.5`), `EXO_HARNESS`, `ARC_SANDBOX`. Results
land in `results/latest.json` (gitignored).

## Evolve mode (`--evolve`)

```bash
OPENAI_API_KEY=… ARC_VERSION=2 ARC_N=25 ./run.sh --evolve --out results/evolve25.json
```

A continual-learning protocol (clbench-style), not the official leaderboard
protocol: ONE persistent agent solves the whole sequence, keeping its memory
(`remember`/`forget`), self-authored tools (`install_agent_tool`), and
agent-scoped docker sandbox across tasks (each task is a fresh conversation).
Harness: `examples/simple-coding-agent/harness-arc-evolve.ts` — it pushes the
program-synthesis loop (write the transform in the sandbox, verify against ALL
train pairs, then apply to test) plus a persistent grid library in the sandbox.

- **Task data**: the prompt embeds the task JSON with test outputs stripped;
  the shell is docker-sandboxed with no host mount, so on-disk answer keys are
  unreachable. Known hole: agent-authored TS tools execute host-side — the
  prompt forbids dataset lookup, and the kept root makes auditing easy (read
  the agent-tools sources + memory after the run).
- **Feedback** (`--feedback verdict|none`): after scoring, the agent gets one
  SOLVED/FAILED turn (no answer content) to update its memory/library/tools.
- **Timeouts**: `--task-timeout` (default 1200 s) per solve turn; a timeout
  scores as a miss and the run continues.
- **Metrics**: pass@1 and pass@2 (official ARC metric; the harness may return a
  second candidate as `outputs2`).
- The exo root persists at `results/evolve-roots/<out-name>/` — inspect what the
  agent built (memory artifact, agent-tools, sandbox library). Serial only.

## Status / notes

- **Metric:** this runner reports **pass@1** (one prediction per test input, exact
  match; a task counts only if _all_ its test outputs match). The official ARC
  Prize metric is **pass@2** — straightforward to add (ask for two candidate
  grids, count a hit if either matches).
- Datasets: ARC-AGI-1 (400 train / 400 public eval) and ARC-AGI-2 (1000 / 120).
  The private test sets used for the prize are not public.
- Single-shot reasoning is a floor; the agentic shell/program-synthesis path
  (write a transform, verify it on the train pairs, apply to test) is the obvious
  next lever and is what tends to score highest on ARC.
