# Simple Coding Agent

A deliberately minimal Exo harness used to benchmark Exo's **core coding-agent
ability** (e.g. on Harbor / Terminal-Bench 2.0). It strips Exo down to "an LLM in
a terminal with a shell" so the score reflects the agent loop + the model, not
assistant features.

## What it is

`harness.ts` default-exports `defineHarness({ runTurn })`. On each turn it runs
Exo's standard **Responses turn loop** (`runResponsesHarnessTurn`) with:

- **Tools: a single `shell` tool** (the `shell` built-in via
  `registerBuiltInTools(tools, ctx, ["shell"])`). Nothing else — no memory,
  adapters, scheduler, MCP, or file/browser tools. The agent does _all_ work
  (reading, editing, building, running, testing) by issuing shell commands.
- **System prompt** (`AGENT_SYSTEM_PROMPT`, injected as a `developer` message):
  frames it as an autonomous engineer — no user interaction, don't stop early,
  explore first, make progress via the shell, and **verify output before
  finishing**. The task itself arrives as the conversation's _user_ turn.
- **Model:** whatever the agent is configured with (e.g. `gpt-5.5`).

## Execution model (how the shell runs)

The shell tool executes in the conversation's sandbox. For benchmarking we use a
**`local-process` sandbox**, so the shell runs **directly in the host/container
where Exo is invoked** — i.e. inside the benchmark task's container. The agent's
shell working directory is the task working directory (set via the conversation
mount). Net effect: _Exo's shell == the task container's shell_, no nested sandbox.

## How it's invoked

Created/driven via the Exo CLI:

```bash
exo --root <root> --secret-backend file \
  agent create --slug agent --model gpt-5.5 \
  --harness /path/to/examples/simple-coding-agent/harness.ts \
  --sandbox-provider local-process "Simple Coding Agent"
exo --root <root> conversation create agent c
# mount the task dir so the shell operates there:
exo --root <root> conversation mount add agent c <taskdir> <taskdir>
# deliver the task as the user turn:
exo --root <root> conversation send agent c "<task instructions>"
```

In the Harbor benchmark this is wrapped by `harbor-bench/exo_agent/agent.py`
(`ExoAgent`), which installs Exo into each task container and runs the above.

## Scope / non-goals

- Not the exoclaw/Scarab assistant — no persona, channels, memory, or scheduling.
- Single-tool by design. Adding tools (e.g. a structured verify/test tool) is a
  deliberate future change, tracked in `harbor-bench/DESIGN.md` (step D2).
