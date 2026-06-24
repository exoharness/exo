# Continual Learning Bench — Exo system

Runs Exo on the [Continual Learning Bench](https://continual-learning-bench.com/)
([repo](https://github.com/pgasawa/continual-learning-bench), arXiv:2606.05661) —
a benchmark that measures how much an agent **improves from past interactions**
(its headline **Gain** metric = stateful performance minus a stateless-reset
baseline). This is the most on-thesis benchmark for Exo's continual learning; see
`../EVAL_PLAN.md`.

## How it integrates

Unlike Terminal-Bench/Horizon (Harbor), clbench is its own framework. The
pluggable agent is a **"system"**: a `ContinualLearningSystem` subclass that
implements `respond(query) → Response`, `reset()`, `name`, registered with
`@register_system`. Systems are discovered from `clbench/src/systems/`.

- **`system/`** — `ExoSystem` (registered as `exo`). On each `respond`, it runs
  exo on the host with a **Docker sandbox** (isolated `ubuntu:24.04` container, **no
  host mount**) so the agent's shell cannot reach this clbench checkout's
  ground-truth files — the task prompt is the only data channel. (We started on
  `local-process` and caught the agent reading the host answer keys; see "Isolation"
  below.) It runs the memory harness on the task prompt + a JSON-schema instruction,
  then parses exo's final assistant message into the query's Pydantic
  `response_schema` (one repair retry). `observe()` stashes the task's feedback into
  the next prompt and, at an instance boundary, drops the conversation so the next
  instance starts fresh while the agent's durable memory persists. `get_run_artifacts()`
  exports that memory into the trace so we can see _what_ it learned. Modeled on
  clbench's in-tree `codex` system.
- **`setup.sh`** — clones clbench, `uv sync`s it, **symlinks `system/` →
  `clbench/src/systems/exo/`** (so the registry finds it), and ensures a host exo
  binary. **`run.sh`** — `clbench run <task> --schedule quick_test --system exo`.

## Learning status

This branch is rebased onto `exoclaw-self-control` (agent memory, merged #70), and
the default harness is the **memory-enabled** `harness-memory.ts` (shell +
remember/forget + per-turn memory injection). So exo genuinely learns across a
run's instances: the agent (and its durable memory) persists, with a fresh
conversation per instance — lessons carry via memory, not a bloated transcript.
Set `EXO_HARNESS=.../harness.ts` for a memory-free control.

### Isolation (read this before trusting any number)

The sandbox **must** be Docker, not `local-process`. With `local-process` exo's
shell is the _host_ shell, so the agent discovered and read this repo's checked-out
ground-truth files (e.g. `data/sales_prediction/sales_lifecycle_panel.jsonl`,
`data/cohort_studies/.../ground_truth.json`) and remembered the paths — turning
"Gain" into "value of finding the answer key." All pre-isolation numbers (a
`blind_spectrum_monitoring` +18.49, `database_exploration` +4.27, `sales_prediction`
+1.37) are **contaminated and void**. The numbers below are from the Docker sandbox
(host filesystem invisible); the agent must reason from the prompt alone.

### Results (gpt-5.5, `default` schedule, 1 run, **Docker-isolated**; Gain = memory − stateless baseline)

| Task                        | Gain       | Baseline | Memory (honest, no host access)                                   |
| --------------------------- | ---------- | -------- | ----------------------------------------------------------------- |
| `blind_spectrum_monitoring` | **+22.12** | +19.76   | memory **>2×** reward — persistent channel/occupancy map per scan |
| `sales_prediction`          | **+2.14**  | +6.47    | "anchor on realized actuals; raise fast-movers we underpredict"   |
| `cohort_studies`            | **+0.07**  | −0.64    | epidemiological conventions (reward scale is tiny here)           |
| `database_exploration`      | pending    | —        | needs isolated re-measure (prior +4.27 was contaminated)          |
| `exploitable_poker`         | noisy      | —        | luck-dominated on few hands — not a clean discriminator           |
| `codebase_adaptation`       | not run    | —        | agentic — needs clbench's task container, not a bare box          |

**Honest Gain is positive on every prediction task — and _higher_ than the
contaminated runs**, because real learning (tracking dormant channels, correcting a
forecast bias) beats clumsy answer-key hunting. `blind_spectrum_monitoring` is the
showcase: it's engineered so a single scan is insufficient, and exo's memory builds
the cross-scan occupancy map that more than doubles reward. The agentic tasks
(`database_exploration`, `codebase_adaptation`) genuinely need the task's own data,
which lived on the host — under isolation they have nothing to work with, so they
need running inside clbench's **task** container (separate work). The reference
leaderboard uses claude-opus-4-6, so this is exo's own column, not a same-model
comparison. Next: `--runs 3` for variance bars + the task-container path for the
agentic tasks.

## Quickstart

```bash
./setup.sh                                   # clone + env + symlink + exo binary
OPENAI_API_KEY=… ./run.sh                    # exploitable_poker, quick_test schedule
OPENAI_API_KEY=… ./run.sh sales_prediction   # another task
```

Requires uv, Docker, Python 3.13 (clbench), and `OPENAI_API_KEY`. The clbench
clone lives as a sibling of the exo repo (override with `CLBENCH_REPO`).
