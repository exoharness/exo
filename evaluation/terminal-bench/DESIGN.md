# Exo Evaluation — Design

How Exo is evaluated as a first-class agent in the **Harbor** framework: a real
**Terminal-Bench 2.0** score today, and **Horizon** (long-horizon learning) next
to exercise Exo's durable memory.

## Goal

Stand Exo up as a Harbor agent and produce reproducible, comparable scores:

1. **Terminal-Bench 2.0** (89 terminal/coding tasks) with gpt-5.5 — comparable to
   the published Claude Code / Codex / OpenHands numbers.
2. **Horizon** (long-horizon learning/memory) — same harness, different dataset,
   chosen to showcase Exo's agent-native memory against retrieval baselines.

## Why Harbor

Harbor (from the Terminal-Bench team) gives an open agent API, reproducible local
scoring, public datasets, and a peer leaderboard with Claude Code / Codex /
OpenHands already integrated. Exo joins the same board with no bespoke scoring.

## How Exo maps onto Harbor

**Harbor's model.** A _task_ is an instruction plus an isolated _environment_ (a
Docker container locally; Daytona/Modal at scale). Agents act on the environment
through `environment.exec(command=...)` and `environment.upload_file(...)`. An
_agent_ subclasses `BaseAgent` (`name()`, `version()`, `setup()`,
`run(instruction, environment, context)`), loaded via `--agent-import-path
<module:Class>`. Two styles ship in-tree: **installed** agents
(`BaseInstalledAgent`) install a CLI _into_ the environment and run it there via
`exec()`; **host** agents run their loop on the host and proxy each tool call into
the environment.

**The core tension.** Harbor owns the environment and expects agents to act
through `environment.exec()`. Exo runs its _own_ sandbox. We reconcile this with
an **installed agent + `local-process` sandbox**:

- Install the Exo stack _into_ the task container and run `exo` there.
- Configure the conversation with Exo's `local-process` sandbox, which means "run
  on this host" — and inside the container that host _is_ the task environment.

Net effect: **Exo's shell == the task container's shell.** No nested sandbox, and
**no changes to Exo**. (The alternative — a host agent that proxies Exo's shell
tool to `environment.exec()` — would require a new Exo sandbox backend and was
rejected as invasive.)

## ExoAgent — implementation

`evaluation/exo_agent/agent.py` defines `ExoAgent(BaseInstalledAgent)`:

- **`install(environment)`**: ensure Node 22 (install via NodeSource if absent);
  upload and unpack the slim exo bundle; symlink the `exo` binary onto `PATH`.
- **`run(instruction, environment, context)`**: register the model from the
  injected key; create an agent on the **Simple Coding Agent** harness with a
  `local-process` sandbox; mount the task working directory; deliver the task as
  the conversation's user turn via `exo conversation send`; capture the transcript.
- **Usage harvest**: after the turn, sum the per-turn token fields exo records in
  its event store (prompt/completion/cached/reasoning) for the per-task breakdown,
  and take cost straight from exo's own `usage.cost_usd` (no hardcoded price
  table). Written to `/logs/agent/exo_usage.json` for the report.

**The agent itself** is Exo's minimal **Simple Coding Agent** harness
(`exo/examples/simple-coding-agent/`): a single `shell` tool + a Responses turn
loop + an autonomy/verify system prompt. It deliberately excludes memory,
adapters, scheduler, and MCP so the score reflects the agent loop and the model.

### The bundle

A portable, slim tarball shipped into each task container, built by
`build-bundle.sh` from the exo repo:

- A **static musl** `exo` binary (`x86_64-unknown-linux-musl`) — runs on any task
  image regardless of its glibc version.
- A pruned `node_modules` (exo spawns `node --import tsx` to run the harness, so
  tsx + core deps must be present) with the heavy unused packages excluded, plus
  `typescript/`, the example harnesses, and tsconfig. ~64 MB.

Build from a base including PR #68 (removed optimistic concurrency on append);
older bases hit `turn is stale` on rapid tool-result writes.

## Running it

```bash
cd evaluation
./setup.sh                                  # install harbor CLI + build bundle + report deps
OPENAI_API_KEY=... ./run.sh                 # full 89-task suite
OPENAI_API_KEY=... ./run.sh -l 10           # quick subset
OPENAI_API_KEY=... ./run.sh -i <task-name>  # one named task
```

`run.sh` runs `harbor run -d terminal-bench@2.0 --agent-import-path
exo_agent.agent:ExoAgent -m openai/gpt-5.5`, then generates a report under
`reports/<timestamp>/`. The OpenAI key must be completion/Responses-capable (a
read-only key 401s). See `README.md` for prerequisites.

## Status

| Phase                       | State                          |
| --------------------------- | ------------------------------ |
| Harbor + TB2.0 run locally  | ✅ done                        |
| ExoAgent integration        | ✅ done — runs end-to-end      |
| Full Terminal-Bench 2.0 run | ✅ done — 89-task suite scored |
| Horizon                     | ⬜ later                       |

### Terminal-Bench 2.0 — exo + gpt-5.5 (run `2026-06-22__06-21-32`)

- **Raw: 42/89 = 47%.** **On the 59 tasks that actually executed: 42/59 = 71%.**
- 30 tasks failed to _local infra_ (disk exhaustion from per-task Docker images +
  Docker network exhaustion mid-run) — not the agent; excluded from the 71%.
- 6 timeouts (heavy tasks: model training, OS install); 11 genuine wrong answers.
- Cost **$27.03** (~$0.52/task captured; high prompt-cache hit rate).
- `run.sh` writes a per-run report under `reports/<timestamp>/` (scoreboard,
  per-task CSV, graphs); these outputs are gitignored, not committed.

### Key finding (drives next steps)

The losses are **mostly harness/scaffold, not model capability**. The dominant
failure mode is **weak, self-referential verification**: the agent confirms its
code _runs/compiles_, declares "Done," and ships without checking output against
the real success criteria. Every clean success verified against an _independent
oracle_. Secondary modes: grade-time-blind shortcuts (depending on local-only
artifacts, tearing down verified state) and one deps-not-installed case. These are
addressable in the harness.

## Next steps

**D2 — Stronger system + verification prompt (biggest score lever).** Target the
dominant failure mode:

- _Verification discipline_: validate output against the task's actual success
  criteria / an independent oracle before finishing — run provided tests/checkers,
  diff against references, sanity-check numeric results. "It compiles" / "it ran"
  is not done; contradicting evidence is a signal to keep going.
- _Grade-time isolation awareness_: the solution is re-graded in a fresh, isolated
  environment — don't depend on local-only artifacts, don't tear down verified
  state, keep required services persistent.
- Consider a structured self-check step or a dedicated verify tool over prose alone.
- Re-bundle and re-run a subset to measure the lift before a full re-run.

**D3 — Fix the tasks that didn't run.**

- Infra failures (30): the real root cause — verified from the original job dir —
  was **Docker Hub anonymous pull-rate-limiting (21/30)**, clustered in the last
  third of the run as cumulative image pulls crossed the anonymous cap; plus ~8
  containerd layer-corruption knock-ons and 2 one-offs. (Earlier "disk full +
  network limit" theory was wrong; disk wasn't the cause.) **Fix: `docker login`**
  — authenticated pulls comfortably exceed the ~89 images a run needs (verified a
  previously-rate-limited image now pulls clean). Optionally pre-pull all task
  images once before a run. Keep the 450 G for the cached image set.
- Timeouts (7): raise the per-task time/turn budget, or accept that a few heavy
  tasks (model training, OS install, video) are out of scope. Decide per task.

**D4 — Clean full re-run + leaderboard comparison.** After D2–D3, re-run the full
suite flake-free for a legitimate suite-wide number, and compare to the published
Claude Code / Codex / OpenHands Terminal-Bench 2.0 results.

**Horizon (later).** Once TB2.0 is solid, run Horizon — same harness, different
`-d` dataset. Exo's agent-native durable memory (the agent decides what to
remember; it's injected each turn) is a genuine contrast to retrieval baselines
like `trace_rag`. Open design choice: how the ~30M-token history reaches Exo —
loaded into its memory store, or read from the environment by the agent. This
choice defines the Exo-vs-RAG story. Watch-outs: history scale (start with the
smallest split) and fully-async `run()` so parallel runs don't block.

## Open decisions

- **Timeouts (D3):** raise the per-task time budget, or leave heavy tasks out of scope?
- **Horizon memory feeding:** load history into Exo's memory store, or let the
  agent read the trace from the environment?
