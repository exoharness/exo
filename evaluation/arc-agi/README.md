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
