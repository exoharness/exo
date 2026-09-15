---
title: Executors & Harnesses
description: The built-in executor runtimes, from basic to Codex, Claude Code, and Cursor.
---

# Executors & Harnesses

A harness is an executor running on the exoharness. The `exo` CLI selects
one with `--harness`:

| Harness | What it is |
|:--------|:-----------|
| `basic` | Built-in Rust executor: straightforward prompt → model → tools loop |
| `rlm` | Recursive-language-model experiment |
| `glia` | Single-context Researcher + Supervisor policy for empirical systems optimization |
| `typescript` | Runs a TypeScript harness module that owns the turn loop, while Rust owns durable state |
| `codex` | Backs OpenAI Codex with durable exoharness sessions |
| `claude-code` | Backs Claude Code with durable exoharness sessions |
| `cursor` | Backs the Cursor SDK with durable exoharness sessions |
| `<module.ts>` | Any TypeScript module path implementing the harness interface |

## The executor loop

Whatever the runtime, the canonical loop is the same:

1. `beginTurn(...)` — durably accept the user input, get a turn handle
2. Read or derive prompt history from events
3. Call the model
4. Append messages and tool requests through the turn handle
5. Execute tools and append results through the turn handle
6. `finish()`

Everything in steps 2–5 is executor policy: which slice of history to
send, which model to call, which tools to expose, when to compact. The
exoharness can even be exposed *to the model* — e.g. a tool for querying
the agent's own history — but that exposure is still configured by the
executor.

## TypeScript harnesses

The `typescript` harness runs a module that owns the turn loop:

```bash
exo --harness typescript agent create "TS Basic" \
  --module exoharness/examples/typescript/basic-harness.ts \
  --model gpt-5.5
```

This is the main extension point for building your own agent — see the
[Tutorials](../tutorials/index) section.

## Glia: supervised systems research

`glia` runs a TypeScript policy inspired by Single-Context Glia (SCG),
described in sections 4 and 4.1 of *Glia: A Human-Inspired AI for Automated
Systems Design and Optimization* (Hamadanian et al., arXiv:2510.27176v5).

```bash
exo --harness glia agent create "Glia" --slug glia \
  --model <registered-model> --max-tool-round-trips 49
exo repl --agent glia
```

Provide the task in the conversation: the objective, constraints, baseline,
benchmark command, and where the code and metrics live in the sandbox.
Configure the sandbox image and mounts for your project as usual. The harness
does not bundle the paper's simulator or workload. Both roles use the agent's
registered model binding; the paper's experiments used o3, but the policy does
not require a particular provider or model.

The Researcher owns code inspection, implementation, instrumentation, and
experiments through the normal shell and configured tools. Its instructions
emphasize measured baselines, explicit hypotheses, detailed metric analysis,
and preserving the best measured design and reproduction artifacts.

The Supervisor receives user task messages, public Researcher reports, and
prior reviews. It receives no tools, tool arguments, raw tool results, or
reasoning blocks. It can ask questions, recall findings, encourage promising
work, redirect stalled exploration, and challenge premature completion. Its
instructions prohibit introducing new designs. This behavioral constraint is
prompt-based; tool isolation is enforced by the executor.

### Policy and limits

- Review every five Researcher calls, and whenever the Researcher responds
  without a tool call (a proposed final answer).
- Supervisor decisions are `continue`, `revise`, and `finish`. Feedback is
  included in the next Researcher prompt. Only a proposed final answer can
  receive `finish`.
- Use the shared agent budget: `maxToolRoundTrips + 1` Researcher calls per
  turn, defaulting to 50 calls when unset. The last call is reserved for a tool-free report.
  Every Researcher call counts, including a rejected proposed final answer.
  Reviews add model calls and cost, with at most one review per Researcher call.
- If the budget expires without approval, record and display that completion
  was not approved. Model failures, malformed tool arguments, and invalid
  Supervisor JSON fail the turn explicitly.

The paper specifies the roles and intervention behavior but does not provide
an exact review cadence or machine-readable decision protocol. These prompts,
cadence, and round limits are implementation choices. This is an SCG-inspired
executor, not a reproduction of the paper's benchmark results. A round limit
does not enforce a dollar, wall-clock, token-context, or simulation budget.
There is no automatic compaction or best-of-N multi-context search.

Glia has one fixed policy and no separate configuration API. Both roles use
one model binding; the review cadence and role prompts live in the executor.
Use the standard agent settings for model, output tokens, tools, sandbox, and
round budget.

For a future Exo-versus-Glia evaluation, use the same task, initial code,
benchmark, and model, with separate sandboxes. Account for both Glia roles in
the total cost. Equal round limits do not imply equal compute budgets. Exo
currently exposes its profile tools while Glia uses the default tool registry;
control or report that difference when comparing results. Benchmark scoring
belongs outside either executor.

Researcher messages, tool requests/results, and both roles' usage are durable
events. `glia_run_started`, `glia_supervisor_review`, and `glia_run_finished`
custom events record policy settings, reviews, and termination reasons.
Later turns reconstruct the research history and Supervisor feedback from
those events. A later turn gets a new round budget; this does not automatically
resume interrupted tool execution. Streaming shows Researcher text and normal
tool activity; Supervisor reviews are available in the event log.

## Coding-agent harnesses

The `codex`, `claude-code`, and `cursor` harnesses treat exoharness events
as the canonical conversation state and run the native agent runtimes
inside exoharness-managed sandboxes. The payoff: sessions you can stop,
resume, fork, and rewind across runs, regardless of which coding agent is
driving.
