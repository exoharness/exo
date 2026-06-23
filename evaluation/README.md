# evaluation — Exo on agent benchmarks

Harnesses for evaluating Exo on external agent benchmarks, organized one folder
per benchmark with shared bits in `shared/`.

| Path                                 | What                                                                                                                                                                         |
| ------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [`EVAL_PLAN.md`](EVAL_PLAN.md)       | Cross-benchmark plan: the continual-learning thesis, selected reference benchmarks, and a survey of options. Start here.                                                     |
| [`terminal-bench/`](terminal-bench/) | **Terminal-Bench 2.0** (Harbor): Exo as an _installed_ agent (runs inside the task sandbox). `setup.sh` → `run.sh`; see its `README.md` + `DESIGN.md`.                       |
| [`horizon/`](horizon/)               | **Horizon** (orinlabs, Harbor): Exo as a _host-side_ agent (runs on the host, shell proxied into the no-internet sandbox via exo's `proxy` provider). `setup.sh` → `run.sh`. |
| `shared/`                            | Files used across benchmarks (`requirements.txt` for the report scripts).                                                                                                    |

## Layout conventions

- Each benchmark folder is self-contained: its own `exo_agent/` package (imported
  by Harbor as `exo_agent.<module>:<Class>` with `PYTHONPATH` = that folder), its
  own `setup.sh` (one-time: deps + build) and `run.sh` (run + report).
- The two benchmarks use different Exo integration modes — **installed** (Terminal-
  Bench) vs **host-side proxy** (Horizon) — because Terminal-Bench sandboxes allow
  the model host while Horizon's are network-isolated and run no agent code.
- Generated artifacts (`exo-bundle.tar.gz`, `.cache/`, `.venv/`, `jobs/`,
  `reports/`) live under each benchmark folder and are gitignored.

## Quickstart

```bash
# Terminal-Bench 2.0
cd terminal-bench && ./setup.sh && OPENAI_API_KEY=… ./run.sh -l 5

# Horizon
cd horizon && ./setup.sh && OPENAI_API_KEY=… ./run.sh 01-example-catering-vendor
```
