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

### Terminal-Bench 2.0 — exo + gpt-5.5

**Clean run (2026-06-23, D2 prompt + D3 infra fixes) — current best:**

- **Raw: 64/89 = 72%.** Excluding 4 tasks that fail on a box-specific large-image
  Docker-storage limitation: **64/85 = 75%.**
- Remaining non-passes: 4 large-image infra (containerd overlayfs corruption on
  multi-GB CUDA images — box limit, not the agent), 6 timeouts (heavy tasks at the
  standard budget), 15 genuine wrong answers.
- Cost ~$41 (more tasks now execute than the first run; gpt-5.5).

**First run (2026-06-22\_\_06-21-32) — contaminated baseline, for contrast:**

- Raw 42/89 = 47%; only 52 executed (30 killed by infra — see D3). The jump to 72%
  is almost entirely D3 (infra) letting tasks actually run, not the D2 prompt.

### Key finding (drives next steps)

The losses are **mostly harness/scaffold, not model capability**. The dominant
failure mode is **weak, self-referential verification**: the agent confirms its
code _runs/compiles_, declares "Done," and ships without checking output against
the real success criteria. Every clean success verified against an _independent
oracle_. Secondary modes: grade-time-blind shortcuts (depending on local-only
artifacts, tearing down verified state) and one deps-not-installed case. These are
addressable in the harness.

## Next steps — outcomes (D2/D3/D4 done 2026-06-23)

**D2 — Stronger system + verification prompt. ✅ Done; net-neutral.** Rewrote the
prompt for verification-against-an-oracle, grade-time isolation, dep install,
tool-use, and a pre-finish self-check. Measured with a controlled full-suite A/B
(same 50 tasks executed under both prompts): **40 → 41 (net +1; 4 gains, 3
regressions)** — within nondeterminism noise. Kept (principled, harmless), but the
prediction that this was "the biggest lever" was wrong — D3 was. A dedicated
verify-tool / further iteration is possible future work but low expected value
given this result.

**D3 — Fix the tasks that didn't run. ✅ Done; the actual lever.** Root cause was
NOT disk — it was **Docker Hub anonymous pull-rate-limiting (21/30)** plus a
**stale `docker image prune` loop deleting images mid-run** (left over from an
earlier session; killed) and containerd layer corruption. Fixes: `docker login`
(authenticated pulls) + killed the pruner. Result: **28 of 30 previously-dead
tasks now execute.** Remaining: 4 large-image tasks that corrupt during
containerd/overlayfs extraction (box-specific storage limit; a daemon storage-
driver swap would fix it but isn't worth the restart risk) and 6 legitimate
timeouts (kept at standard budget for leaderboard comparability; `TIMEOUT_MULT`
in run.sh raises it for exploratory ceiling runs).

**D4 — Clean full re-run + leaderboard comparison. ✅ Done.** Clean number above:
**64/89 = 72% raw, 75% excl. box-storage limits.** For context, the Meta-Harness
paper's TB2 Table 7 (different base models, so not apples-to-apples): on Claude
Opus 4.6, hand-built harnesses span Claude Code 58.0 / Terminus-2 62.9 /
Terminus-KIRA 74.7 / Meta-Harness 76.4 / ForgeCode 81.8. exo's minimal shell agent
on gpt-5.5 at ~72% lands in that band — strong for a single-tool agent with no
harness search. A direct same-model comparison needs running exo on Opus/Haiku
(deferred — needs an Anthropic path in exo).

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
