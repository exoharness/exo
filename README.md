# exo

Exo is a minimal system for building agents. It separates the trusted
infrastructure needed for state, resources, and security from agent-specific
logic.

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

Most agent systems conflate trusted infrastructure with agent specific
implementations (such as prompts and memory compaction), which makes reuse,
recovery, isolation, and self-modification hard. We view the two concerns
roughly as follows:

- system resources: the infrastructure needed to serve an agent, such as durable history,
  artifacts, credentials, sandboxes, and tool execution
- agent specific implementations: the policy that makes the agent behave a
  particular way, such as prompt assembly, model calls, memory, compaction,
  approvals, and tool choice

Coupling these two things carries the usual cost of weak systems boundaries:
they become hard to evolve independently, hard to isolate, and hard to recover
to known-good states.

The problem is especially acute with AI agents because the agent-specific layer
is often exactly the part you want to make programmable. You may want an agent
to change how it compacts context, resumes from history, delegates work, exposes
tools, builds memory, or configures its environment. If that logic lives in the
same layer that owns persistence, secrets, and sandboxing, the agent has to
muck around with the layer responsible for keeping it safe.

`exo` makes that boundary explicit. An agent harness is composed of an
**exoharness** and an **executor**:

- The **exoharness** is the trusted, non-semantic foundation. It manages durable
  state and privileged resources: agents, conversations, sessions, turns,
  append-only events, artifacts, bindings, secrets, and sandbox handles.
- The **executor** owns agent semantics. It decides how to assemble prompts,
  call models, expose tools, manage memory and compaction, ask for approvals,
  and drive the turn loop.

This structure lets us:

- Build infrastructure primitives as first-class components, not as incidental
  harness internals, so they can survive and improve as harness design evolves.
- Reuse those primitives across agent runtimes without rebuilding state,
  secrets, sandboxes, and history each time. The same exoharness can back Codex,
  Claude Code, Cursor sdk, a recursive-language-model executor, and custom harnesses.
- Expose the substrate to agents when useful, so they can inspect their own
  history, manage artifacts, control sandboxes, and evolve their implementation
  without owning the layer responsible for recovery.

Decisions that affect what an agent means or does belong in the executor, or in
the agent itself. The exoharness provides the durable building blocks those
decisions run on: history, state, secrets, and sandboxing.

For the architectural model and terminology, see
[spec/exoharness.md](./spec/exoharness.md).

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

Models are registered through explicit bindings:

```bash
./target/debug/exo secret set openai --env OPENAI_API_KEY
./target/debug/exo model register gpt-5.4 --secret openai
```

Create an agent and start a conversation:

```bash
./target/debug/exo agent create --model gpt-5.4 "Sandbox Example"
./target/debug/exo conversation create sandbox-example "Local Dev"
./target/debug/exo chat repl sandbox-example local-dev
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
./target/debug/exo --harness typescript agent create "TS Basic" \
  --module examples/typescript/basic-harness.ts \
  --model gpt-5.4
```

The `examples/typescript` directory also contains Codex, Claude Code, Cursor,
and recursive-language-model harness experiments.

## Repository Layout

- `crates`: Rust workspace for the CLI, exoharness substrate, and executors.
- `typescript`: TypeScript harness runtime, model-runtime helpers, and
  adapter-specific support code.
- `examples/typescript`: runnable TypeScript harness examples.
- `containers`: sandbox images used by the coding-agent harness examples.
- `spec`: core architecture and terminology.
- `docs`: design notes for in-progress directions.
- `scripts`: development and live e2e utilities.

## Development

```bash
pnpm check
cargo test --workspace --all-targets
```

The repository includes a pre-commit hook installer:

```bash
pnpm prepare
```

## License

MIT
