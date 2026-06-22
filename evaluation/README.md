# evaluation — Exo on Terminal-Bench 2.0

This folder lives inside the exo repo and runs Exo as an agent on
[Harbor](https://github.com/harbor-framework/harbor)'s **Terminal-Bench 2.0**
(89 terminal/coding tasks), producing a scored report.

Exo is wrapped as a Harbor _installed agent_ (`exo_agent/agent.py`): a slim static
exo binary + pruned `node_modules` is shipped into each task container and driven
with `exo conversation send`, using a `local-process` sandbox so Exo's shell **is**
the task container's shell. The agent it runs is Exo's minimal **Simple Coding
Agent** harness (single `shell` tool + a verify-before-finish system prompt); the
harness source lives in the exo repo at `examples/simple-coding-agent/`.

## Prerequisites

- The **exo repo** at a base including PR #68 (this folder is inside it; the
  bundle is built from one level up — override with `EXO_REPO=/path/to/exo`).
- **Rust** + the `x86_64-unknown-linux-musl` target and `musl-tools` (builds a
  portable static exo binary that runs on any task image regardless of glibc).
- **pnpm** (exo's JS deps), **uv** (for the harbor CLI), **Docker**, **Python 3**.
- An **`OPENAI_API_KEY`**.

## Quickstart

```bash
# 1. One-time setup: install harbor CLI, build the exo bundle, install report deps.
./setup.sh

# 2. Run the benchmark (writes jobs/<ts>/ and a report under reports/<ts>/).
OPENAI_API_KEY=sk-... ./run.sh              # full 89-task suite (~2h, ~$27 @ gpt-5.5)
OPENAI_API_KEY=sk-... ./run.sh -l 5         # smoke test: first 5 tasks
OPENAI_API_KEY=sk-... ./run.sh -i mailman   # one named task
```

`run.sh` forwards extra args to `harbor run` (`-l/--n-tasks`, `-t/--task`,
`-i/--include-task-name`, …). Tune with `MODEL=` and `N_CONCURRENT=` env vars.

## What's here

| Path                  | What                                                                       |
| --------------------- | -------------------------------------------------------------------------- |
| `exo_agent/agent.py`  | `ExoAgent` — the Harbor installed-agent wrapper for Exo.                   |
| `build-bundle.sh`     | Builds the slim static exo bundle (`exo-bundle.tar.gz`) from the exo repo. |
| `setup.sh` / `run.sh` | One-time setup; run the suite + report.                                    |
| `gen_report.py`       | Scoreboard + per-task report from a `jobs/<ts>/` run → `reports/<ts>/`.    |
| `ran_graphs.py`       | Extra graphs over executed-only tasks (calls/cost/reward).                 |
| `requirements.txt`    | Python deps for the report scripts.                                        |
| `DESIGN.md`           | Design, status, results, and planned next steps.                           |

## Reports (manual)

`run.sh` calls `gen_report.py` automatically. To regenerate by hand:

```bash
.venv/bin/python gen_report.py [jobs/<ts>]     # default: latest jobs/ dir
.venv/bin/python ran_graphs.py <report-ts>     # executed-only graphs
```

## Not committed (regenerated locally)

`.gitignore` excludes the downloads/outputs: `exo-bundle.tar.gz` (built by
`setup.sh`), the upstream `harbor/` reference clone (not needed — the CLI pulls
the dataset from its registry), per-run `jobs/`, and generated `reports/`. The exo
bundle is rebuilt from your exo checkout; everything else regenerates per run.

## Results to date

Full TB2.0 run (gpt-5.5): **47% raw** (42/89), **71% on executed tasks** (42/59),
~$27. The rest were infra failures (since fixed — disk) and a handful of timeouts.
See `DESIGN.md`.
