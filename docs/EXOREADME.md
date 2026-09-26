# exo

Exo is a minimal system for building agents. It separates the trusted
infrastructure needed for state, resources, and security from agent-specific
logic.

This repo contains both **exoharness**, the durable substrate for agents, and
**Exo**, a full long-running personal agent built on top of it. Exo is
the best place to start if you want to try Exo as a user: it can inspect and
modify its own code, restart its services, manage adapters, run scheduled work,
create tools, and snapshot or rewind its sandbox. Exo supports a number of
tools and adapters including IRC, WhatsApp, Signal, and Discord.

For setup and usage, see [Exo](../exo/README.md).

![Exo architecture overview](images/architecture-overview.svg)

The goal is to provide a small, durable kernel for agents: minimal enough to
stay independent of any particular agent design, but complete enough to support
agents of arbitrary complexity. That includes agents that safely evolve their
own implementations, such as tools, compute environments, and memory systems.

Because the trusted substrate is separate from the agent code changing above it,
agents can fork, rewind, or return to known-good states without losing critical
state such as secrets, config, or history.

This directory contains the exoharness. And everything you need to build your
own agent from scratch, or to back Codex, Claude Code, or the Cursor SDK with
durable sessions you can stop, resume, and rewind across runs.

## The Why and What of Exo

Enabling powerful autonomy requires agents to be (1) **adaptable**, i.e. be able to adapt their policies, tools, and architecture to a target domain, and (2) **trustworthy**, i.e. be durable across crashes, isolated from one another, and recoverable to known-good states. Existing harnesses struggle to provide both: most agent systems conflate trusted infrastructure with agent-specific implementations (such as prompts and memory compaction), which makes reuse, recovery, isolation, and self-modification hard.

The `exo` agent harness instead decouples trusted infrastructure from agent-specific implementations into two halves:

1. The **exoharness** as the durable substrate that owns identity (agents, conversations, turns), history (event log), artifacts, secrets, and sandbox management. It is trusted and stateful.
2. The **executor** as the policy layer that owns prompt assembly, model calling, tool dispatch, memory compaction, approvals, etc., i.e. all _semantic_ decisions about how the agent behaves. It is ephemeral, swappable, and can be killed without losing the agent.

![Exo architecture, detailed](images/architecture-detailed.svg)

This architecture pushes the minimal infrastructure into the protected exoharness to enforce safety, while leaving all non-safety-essential components to the executor to manage and evolve at will. Decisions that affect what an agent means or does belong in the executor, or in
the agent itself. The exoharness provides the durable building blocks those
decisions run on: history, state, secrets, and sandboxing.

Because the exoharness substrate doesn't depend on the executor, an agent built on exo can:

- **Fork or rewind** at any past event, without losing secrets, sandboxes, or history.
- **Swap executors**, running the same agent via Codex, Claude Code, the Cursor SDK, or your own executor, without rebuilding state.
- **Evolve safely** to change its own policy processes, with access to inspect its own history and artifacts while the exoharness isolates secrets and compute resources to maintain safety.

For the architectural model and terminology, see
[exoharness/docs/spec.md](../exoharness/docs/spec.md).

## Status

This repository is early software. The Rust crates, CLI, TypeScript harness
runtime, and example coding-agent harnesses are useful for experimentation, but
the public API should still be treated as unstable.

## Quick Start

Install Rust and pnpm, then build the CLI:

```bash
cargo build -p exo
./target/debug/exo --help
```

Register a model, create an agent, then start chatting:

```bash
./target/debug/exo vault secret create global openai --token-env OPENAI_API_KEY
./target/debug/exo model create gpt-5.5 --secret openai
cat > assistant.md <<'EOF'
---
name: "assistant"
harness: basic
config:
  model: gpt-5.5
---
Help the user with their task.
EOF
./target/debug/exo agent create assistant --file assistant.md
./target/debug/exo agent run --agent assistant
```

`exo agent run --agent assistant` starts a new saved thread. Pass `--thread <slug>`
to resume an existing thread. `exo agent run --agent-file agent.md` creates or
updates a saved agent from the file, then starts a saved thread; add `--thread
<slug>` to resume one. The agent slug combines the filename with a hash of its
canonical absolute path, so rerunning the same file reuses the agent without a
local association file. Moving the file creates a different agent. Each run
replaces the saved definition, including fields removed from the file. Agents
and history remain available until explicitly deleted.

`--token-env` takes the environment variable name; the CLI reads its value.

For explicit control over agents, conversations, or a shell-enabled sandbox:

```bash
cat > sandbox-example.md <<'EOF'
---
name: "Sandbox Example"
harness: basic
config:
  model: gpt-5.5
---
Help the user with their task.
EOF
./target/debug/exo agent create sandbox-example --file sandbox-example.md
./target/debug/exo thread create sandbox-example "Local Dev"
./target/debug/exo agent run --agent sandbox-example --thread local-dev
```

The CLI stores state under `.exo` by default. Pass `--root <path>` to use a
different state directory.

## TypeScript Harnesses

TypeScript harnesses can own the turn loop while Rust owns durable exoharness
state. Install Node dependencies once:

```bash
pnpm install
```

Then create an agent backed by a TypeScript harness module:

```bash
cat > ts-basic.md <<'EOF'
---
name: "TS Basic"
harness: exoharness/examples/typescript/basic-harness.ts
config:
  model: gpt-5.5
---
Help the user with their task.
EOF
./target/debug/exo agent create ts-basic --file ts-basic.md
```

The `exoharness/examples/typescript` directory also contains Codex, Claude Code, Cursor,
and recursive-language-model harness experiments.

For the coding-agent setup commands, see
[exoharness/docs/coding-agent-harnesses.md](../exoharness/docs/coding-agent-harnesses.md).

## Exo Long-Running Harness

Exo is a long-running claw-type agent built on exoharness. It supports
scheduled tasks, and a full adapter system including support for WhatsApp,
Signal, and IRC. See [exo/README.md](../exo/README.md)
for setup, operation, and debugging commands.

## Repository Layout

- `crates`: Rust crates for the CLI, exoharness substrate, and
  executors.
- `exoharness/typescript`: TypeScript harness runtime, model-runtime helpers, and
  adapter-specific support code.
- `exoharness/examples/typescript`: runnable TypeScript harness examples.
- `exoharness/examples/gameboy-agent`: example sidecar-backed agent.
- `exo`: canonical long-running Exo agent with scheduled
  task and adapter support.
- `exoharness/containers`: sandbox images used by the coding-agent harness
  examples.
- `exoharness/docs/spec.md`: core architecture and terminology.
- `exo/docs`: canonical Exo behavior and design documentation.
- `exoharness/docs`: reusable platform documentation and design notes.
- `docs/images`: shared README and project imagery.
- `exoharness/scripts`: live exoharness e2e utilities.
- `exo/scripts`: Exo service, adapter, and setup utilities.
- `scripts`: repository development hooks.

## Development

```bash
pnpm check
cargo test --workspace --all-targets
```

The repository includes a Git hook installer:

```bash
pnpm prepare
```

The installed pre-commit hook formats staged Rust files with `rustfmt` and
runs the TypeScript checks. The pre-push hook runs:

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

## License

MIT
