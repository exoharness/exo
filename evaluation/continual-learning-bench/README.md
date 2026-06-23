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
  exo on the host (Simple Coding Agent harness, `local-process` sandbox, a
  per-instance temp `--root`/workspace) with the task prompt + a JSON-schema
  instruction, then parses exo's final assistant message into the query's Pydantic
  `response_schema` (one repair retry). `observe()` stashes the task's feedback
  into the next prompt. Modeled on clbench's in-tree `codex` system.
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

### Results to date (gpt-5.5, `default` schedule, 1 run; Gain = memory − stateless baseline)

| Task                        | Gain       | Notes                                                    |
| --------------------------- | ---------- | -------------------------------------------------------- |
| `sales_prediction`          | **+1.37**  | clear positive — memory helps                            |
| `cohort_studies`            | **+0.02**  | positive (task `r_max` ≈ 0.16, so ~12% of max)           |
| `database_exploration`      | _running_  | full default (subsetting clashes with its fixed variant) |
| `blind_spectrum_monitoring` | _running_  | full default                                             |
| `exploitable_poker`         | ~0 / noisy | luck-dominated on few hands — not a clean discriminator  |

Positive Gain on the deterministic learners confirms the continual-learning loop
works end to end. (gpt-5.5; the reference leaderboard uses claude-opus-4-6, so this
is exo's own column, not a same-model comparison.)

## Quickstart

```bash
./setup.sh                                   # clone + env + symlink + exo binary
OPENAI_API_KEY=… ./run.sh                    # exploitable_poker, quick_test schedule
OPENAI_API_KEY=… ./run.sh sales_prediction   # another task
```

Requires uv, Docker, Python 3.13 (clbench), and `OPENAI_API_KEY`. The clbench
clone lives as a sibling of the exo repo (override with `CLBENCH_REPO`).
