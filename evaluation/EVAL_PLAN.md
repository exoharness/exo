# Exo Continual-Learning Evaluation — task selection

Purpose of this doc: **select the classes of tasks and the specific reference
benchmarks** for evaluating exo, and fix how we position **Meta-Harness as a
baseline**.

## Thesis: continual learning

exo's differentiator is **continual learning** — durable agent memory
(remember/forget, per-turn injection) that accumulates across turns, tasks, and
sessions and reshapes behavior _online_. The evaluation must demonstrate that: not
"is exo a good agent once," but "does exo get better as it accumulates experience."

This is why **Meta-Harness is only a loose baseline.** Meta-Harness is an _offline,
one-time outer-loop search_ over harness source code: a separate proposer (Claude
Code) explores ~60 candidate harnesses, picks the best, and freezes it. There is no
continual learning at inference — the discovered harness is static. So Meta-Harness
is a _non-continual_ point of comparison. We report it where our task classes
overlap theirs (it's a strong, recent number to stand next to), but the real
contrast we're drawing is **continual self-adaptation vs. a static/searched
harness** — different axis, so we don't over-index on beating their number.

## Selected task classes (the decision)

Three classes, chosen so each exercises exo's continual learning in a distinct way,
with at least two directly comparable to Meta-Harness:

| #   | Class                           | Reference benchmark(s)                                           | Continual-learning angle                                                                                    | Meta-Harness comparable?                           |
| --- | ------------------------------- | ---------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- | -------------------------------------------------- |
| 1   | Long-horizon continual learning | **Horizon** (orinlabs/horizon)                                   | the purpose-built benchmark: acquire learnings from a long first-person history, apply in live environments | No (they didn't run it)                            |
| 2   | Online text classification      | **USPTO-50k, Symptom2Disease, LawBench** (+ 9-dataset OOD suite) | labeled examples arrive one at a time; memory accumulates; held-out accuracy rises with the stream          | **Yes** (their §4.1; ACE/MCE + their search)       |
| 3   | Agentic coding                  | **TerminalBench-2** (89 tasks)                                   | capability anchor; continual angle = memory carried across tasks/attempts lifts pass rate                   | **Yes** (their §4.3; Terminus-KIRA + their search) |

**Not selected: retrieval-augmented math reasoning** (their §4.2). It's the
least-continual of their three (a static retrieval policy over a fixed corpus),
the heaviest to build (535K-problem corpus + airtight decontamination + 5-model
pass@1), and adds little to a continual-learning thesis. Cut from v1; revisit only
if we want full coverage of their paper.

## Class 1 — Long-horizon continual learning (Horizon)

**The headline continual-learning result.** [orinlabs/horizon](https://github.com/orinlabs/horizon):
a learning benchmark for extremely long-horizon agents, packaged as Harbor tasks.
Agents must acquire learnings from a large first-person history (~30M tokens) and
apply them in real environments. Reference agents: `trace_rag` (RAG over history)
and `hermes`.

- **Why it's the centerpiece**: it directly measures the thing exo is built for —
  carrying learnings across a long horizon. exo's _agent-native_ memory (the agent
  decides what to remember; injected each turn) is a genuine contrast to
  `trace_rag`'s retrieval-over-history.
- **Metric**: Horizon's own task success; learning curve as history accrues.
- **Meta-Harness comparison**: none — they didn't run it. This class stands on the
  continual-learning thesis alone.
- **Build**: same Harbor plumbing, new dataset (`orinlabs/horizon-public`, 3 public
  example tasks). Key design choice = how the history reaches exo (load into its
  memory store vs. read from env). Watch-outs: 30M-token scale, async `run()`.

**Implementation findings (2026-06-22).**

- _Trace data_: tasks download a prior-session trace at image-build time. The base
  image's `horizon-download-trace` defaults to the **private** HF dataset
  `orinlabs/horizon-example-traces` → 401. The **public** traces are at
  `orinlabs/horizon-1-example-traces` (no token). Fix: set
  `HORIZON_TRACE_BASE_URL` to the public dataset (patched into the cloned tasks'
  Dockerfiles). Verified: the environment builds and `trace.jsonl` lands in
  `/workdir`. See [[reference_horizon_trace_url]].
- _Agent architecture_: Horizon is a **host-side-agent** benchmark — the sandbox is
  `allow_internet=false` and runs no agent code (only execs shell the host sends).
  Our installed `ExoAgent` (exo runs _in_ the sandbox, calls the model from there)
  does **not** fit; local Docker supports only NO_NETWORK/PUBLIC, not per-host
  allowlist. **Chosen path: a host-side exo agent.** Design:
  - exo runs on the host (has internet → model calls work).
  - A new exo **"proxy" sandbox provider** (`ManagedSandboxBackend`) forwards each
    `exec(SandboxCommand)` to a configured host HTTP endpoint, mirroring the
    existing Daytona provider.
  - A Python `ExoHostAgent(BaseAgent)` serves that endpoint, backed by harbor's
    `environment.exec()`, and drives exo (Simple Coding Agent harness) for the turn.
  - Net: exo's shell runs _in_ the no-internet sandbox via `environment.exec`, while
    the model call stays on the networked host. Reusable for any no-internet bench.
  - `run-horizon.sh` exists (smoke driver); will switch to the host agent once built.

## Class 2 — Online text classification

**Setup (Meta-Harness §4.1).** The model receives labeled examples one at a time,
updates memory, and is evaluated on a held-out test set — _this is a continual/
online-learning protocol by construction_, which is why it's the cleanest overlap
with our thesis. Datasets: **USPTO-50k** (180 classes), **Symptom2Disease** (22),
**LawBench/Law** (215); OOD generalization on **9 datasets** (SciCite, FiNER-139,
Amazon Reviews, Financial PhraseBank, GoEmotions, Banking77, AG News, SciTail,
TweetEval-Hate).

- **Metric**: held-out accuracy **and** context tokens (accuracy-vs-context Pareto),
  plus the **learning curve** over the stream (accuracy as a function of examples
  seen) — the curve is our continual-learning signal, beyond their single number.
- **Baselines**: zero/few-shot (8/16/32/all), ACE, MCE, and Meta-Harness (their
  best 48.6% / 11.4K context). exo's native memory plays the role their discovered
  harnesses (draft-verification, label-primed query) had to be _searched_ for.
- **exo mechanism**: durable memory accumulates labeled examples; the harness builds
  each prompt from memory.
- **Build**: an exo classification harness + the online streaming driver (feed,
  update memory, evaluate held-out) + Pareto/learning-curve logging.

## Class 3 — Agentic coding (TerminalBench-2)

**Setup (Meta-Harness §4.3).** TB2, 89 tasks. We already run this. Capability anchor

- a continual angle.

* **Metric**: pass rate; leaderboard rank. Continual signal: pass rate _with memory
  persisted across tasks/attempts_ vs. memory reset per task.
* **Baselines**: the TB2 leaderboard agents + Meta-Harness's discovered harness
  (76.4% Opus / 37.6% Haiku).
* **exo mechanism**: the Simple Coding Agent + online memory. Two cheap wins to
  adopt: **environment bootstrap** (one shell snapshot before turn 1 — the paper's
  validated TB2 improvement, +7/89 dependency-heavy tasks) and **verification
  discipline** (our trace analysis' top failure mode; the D2 work).
* **Build**: mostly done — env-bootstrap, verification prompt/tool (D2), clean full
  run on fixed infra (D3).

## Rigor (continual-learning protocol)

The continual setup creates one failure mode the paper's static setup doesn't:
**memory leaking held-out answers.** Non-negotiables:

- **Strict adaptation/eval separation per class**: exo adapts on the task stream;
  the held-out set is clean and never enters memory. State this protocol explicitly
  for each class so it isn't conflated with offline tuning.
- **Decontamination + leakage audits** (esp. Class 1 OOD, Class 3 task strings).
- **Report the learning curve**, not just the final point — that's what shows
  continual learning rather than a good static config.
- Lightweight validation before expensive eval; navigable JSON logging; eval
  automated outside the agent (already partly in place).

## Sequencing

1. **TB2 (Class 3) first** — exo is ready; headline agentic number. Fix infra (D3) →
   harness optimization incl. env-bootstrap + verification (D2) → clean full run +
   leaderboard comparison.
2. **Horizon (Class 1) second** — the centerpiece continual-learning result; same
   Harbor plumbing, new dataset; design the history→memory path.
3. **Text classification (Class 2) third** — cleanest continual protocol and a
   direct Meta-Harness comparison; moderate build.
4. Model config (eventually frozen Opus 4.6 / Haiku 4.5 for a directly-comparable
   leaderboard number) deferred until the protocols are proven on a convenient model.

## Open decisions

- Confirm the continual-learning framing and the per-class adaptation/eval protocol.
- Horizon history→memory design (load into memory store vs. read from env) — the
  core of the exo-vs-RAG comparison.
- When to switch to Opus 4.6 / Haiku 4.5 (needs an Anthropic base-model path in exo).

**Resolved:** all three classes are in scope (Class 2 kept — cheapest
Meta-Harness-comparable win and the cleanest continual protocol).
