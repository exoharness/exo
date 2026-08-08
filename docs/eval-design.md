# Evaluating Exo

This document specifies how we capture and measure the benefit Exo's architecture provides. It defines what we measure, proposes the benchmark structure, and discusses how to integrate evaluation into Exo itself.

## Overview

The ultimate goal is to provide supportive evidence for Exo's tagline:

> Exo is an agent + harness architecture that is fully recursive, able to safely edit all aspects of itself at runtime to get better at your tasks.

Doing so requires sharpening and formalizing our claims. The claims at stake are "safely edit" and "get better."

### Safety

Safety, in the context of Exo, refers to (a) runtime safety, i.e. Exo does not break while running in a way that requires human intervention, and (b) state safety, i.e. Exo does not lose converstaion history, secrets, or snapshots.

We expect to report both to be 0 in our evaluations – these are promises we make by Exo's design, so if violations of either (a) or (b) are detected, we have a flaw in the architecture that we must resolve.

We will measure "safety" in all self-improvement evals with the expectation of reporting perfect safety, but will additionally run dedicated "safety" fuzzing evals that drive Exo on an infinite stream of tasks to probe for safety failure cases.

### Self-improvement

Self-improvement comprises two orthogonal measurable axes:

- **Correctness** – given a hard sequence of tasks, does Exo improve it's completion rate (for binary correctness) or maximize its reward (for continuous correctness) as it continues to handle tasks.
- **Efficiency** – given a surmountable sequence of tasks, can Exo _decrease_ its resource usage while maintain its completion rate/reward.

Note: "resources" most obviously refers to API costs (proxied by token usage), but could just as well refer to other resource usage such as memory or compute, number of tool calls, runtime, etc. A unique benefit of Exo's design is the convo log provides cost annotations for all messages, which can be filled with tool cost calls, runtimes, etc as well. i.e. kicking off a 'compile_rust_binary' tool call may come with an associated monetary cost, and Exo can consider that in self-reflection.

For all tasks, fair correctness evaluation poses two challenges:

1. How an observer (us) is to measure correctness of completed tasks – provided by benchmarking packages in the form of a metric or grader module, often on the resulting trace.
2. How Exo itself should understand correctness of completed tasks at runtime – required to be fair to Exo, so it understands what it should be improving upon.

Addressing (2) requires providing a notion of correctness to Exo so it can direct improvement towards that. This can be done in natural language, i.e. "get the code running", or via a provided evaluator that allows Exo to know how it is doing.

## Baseline

The space of agents we compete with is roughly Pi, OpenClaw, Hermes, Claude Code, Codex.

Note the differing value props:

- **Pi**: simplest core agent loop to enable hacking
- **OpenClaw/Hermes**: (human-driven) extensibility to the real world, market of plugins
- **Claude Code/Codex**: out-of-the-box tuned for coding work (Ferarri pre-tuned by Anthropic/OpenAI employees)

By contrast, we don't claim to be the simplest, more manually-extensible, or best-tuned agent. We claim to be fully-self-improvable in a safe way.

As a result, it is FULLY expected that Exo out-of-the-box on a single task will have worse correctness than Claude Code/Codex. The benefit of Exo shows up in running over time.

We will also need to provide ablations showing which components of Exo's architecture contribute to its performance. These include:

- Disabling tool installation
- Disabling harness mounting + rebuilding

## Continual reproducibility

As Exo is in the very early stages of development, we are iterating quickly on its design. It's important that we make the interface to pinned re-running evals as simple as possible:

> ./eval.sh [--benchmarks all/<specify>] [--fast]

## Benchmarks

Continual learning benchmarks are best suited to showing "performance improvement over time." However, the majority of the main-stream top-line agent benchmarks (TerminalBench, ARC-AGI, etc) are structured to evaluate task performance, rather than task performance improvement over time.

We thus proceed with two classes of benchmarks: native continual-learning, and adapted general benchmarks.

### Native continual-learning settings

Native continual-learning benchmarks capture improvement-over-time by design:

- **CL-Bench (Continual Learning Bench)** — Contains six stateful task families (codebase adaptation, SQL exploration over a reviews database, demand forecasting, spectrum monitoring, cohort studies, exploitable poker), each with 20–90 sequential instances sharing exploitable latent structure. Its headline metric, **gain**, is the score difference between running the system stateful (with history) and stateless (fresh per instance), normalized by learning headroom. Published results show basic in-context learning beating dedicated memory systems (best gain ~25%). https://continual-learning-bench.com/

Others to investigate: LifelongAgentBench (https://caixd-220529.github.io/LifelongAgentBench/), SkillLearnBench (https://github.com/cxcscmu/SkillLearnBench), SelfEvolvingAgent Gym (https://arxiv.org/abs/2606.17546)

### Repurposed general-task benchmarks

We can take general-task benchmarks that are sufficiently large and show improvement over time by running them as a sequence. The best fits are effectively infinite (procedurally generated or continuously refreshed), since a fixed small set both caps the curve length and invites contamination.

- **Endless Terminals** — a fully autonomous pipeline that procedurally generates containerized terminal tasks with completion tests (~3,255 released tasks, unbounded in principle) (https://arxiv.org/abs/2601.16443). Propose running Exo on this for continual safety fuzzing.
- **TerminalWorld** — ~1,530 validated real-world terminal tasks; same treatment as Endless Terminals, human-validated but finite.
- **Terminal-Bench 2.0** — frequently reported-on benchmark but too small to show continual learning as the tasks are all quite different. We can measure Exo on TerminalBench after running for some time on Endless Terminals to have a number to report.

Additionally, SWE Bench Live looks promising: https://github.com/microsoft/swe-bench-live.

The ideal correctness result looks like:

```text
  correctness
      ^
  1.0 |                                    _____...  Exo
      |                            ____---
      |                       __--
      | ~~~~~~~~~~~~~~~~~~/~~~~~~~~~~~~~~~~~~~~~~~  hand-tuned agent (Claude Code)
      |               __--
      |           __--
      |      __--
      | ____-
      +-------------------------------------------->  task #
```

Exo starts below the tuned static agent, crosses it as accumulated tools and harness edits compound, and flattens near the benchmark ceiling.

Likewise, for resource use:

```text
  cost per task
      ^
      | ____
      |     \__   ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~  hand-tuned agent (Claude Code)
      | ~~~~~~~\~~
      |         \____
      |              \_____
      |                    \______________________  Exo
      +-------------------------------------------->  task #
```

Cost starts comparable (or higher, since self-reflection isn't free), then drops as Exo replaces expensive model-driven exploration with cheap installed tools — while the correctness curve above holds.

### Cross-domain transfer

Alternatively, we can group benchmarks by domain — e.g. terminal tasks, coding tasks, data-analysis tasks — and run Exo through a sequence of benchmark families A, B, C (tasks A_1, A_2, ..., B_1, B_2, ...). Then ideally can show that tools and harness improvements picked up in A improve performance on B from its first task, compared to running Exo on B cold. This measures whether self-improvement generalizes or is task-overfit.

## Protecting Exo's Alignment

Exo can only improve on qualities it knows to consider, whether evaluated quantitatively or qualitatively. Today, this is handled fully implicitly (in-context) and without any alignment guarantees, i.e. Exo is free to interpret goals set in context as it wishes and evaluate them as it wishes.

Open questions here: considering how to elevate "evaluations" to an exoharness primitive that is protected in the exoharness from deletion or unfair changes the same way convo history, artifacts, and secrets are protected.

The evaluator is a capsule that functions as an explicit reward signal over state – artifacts, convo history, tool results that inspect container state, etc. This capsule can be provided by a human or written by the agent, but should be kept safe and versioned by the Exoharness to prevent cheating.
