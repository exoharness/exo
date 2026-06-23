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

## Learning status (read this)

On _this_ exo branch there is **no durable memory**, so the only "learning" is the
previous turn's feedback fed into the next prompt (basic in-context continuity).
Exo's agent-native durable memory lives on another branch; it plugs into
`observe()` / `_init()` later. **This folder is the eval _support_** — the plumbing
that makes Exo's continual learning measurable here — not the learning itself, so
expect modest Gain until memory is wired in.

## Quickstart

```bash
./setup.sh                                   # clone + env + symlink + exo binary
OPENAI_API_KEY=… ./run.sh                    # exploitable_poker, quick_test schedule
OPENAI_API_KEY=… ./run.sh sales_prediction   # another task
```

Requires uv, Docker, Python 3.13 (clbench), and `OPENAI_API_KEY`. The clbench
clone lives as a sibling of the exo repo (override with `CLBENCH_REPO`).
