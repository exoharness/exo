# exo

Exo is a minimal system for building agents. It separates the trusted
infrastructure needed for state, resources, and security from agent-specific
logic.

This repo contains both **exoharness**, the durable substrate for agents, and
**Exoclaw**, a full long-running personal agent built on top of it. Exoclaw is
the best place to start if you want to try Exo as a user: it can inspect and
modify its own code, restart its services, manage adapters, run scheduled work,
create tools, and snapshot or rewind its sandbox. Exoclaw supports a number of
tools and adapters including IRC, WhatsApp, Signal, and Discord.

For setup and usage, see [Exoclaw](examples/exoclaw/README.md).

![Exo architecture overview](docs/images/architecture-overview.svg)

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

![Exo architecture, detailed](docs/images/architecture-detailed.svg)

This architecture pushes the minimal infrastructure into the protected exoharness to enforce safety, while leaving all non-safety-essential components to the executor to manage and evolve at will. Decisions that affect what an agent means or does belong in the executor, or in
the agent itself. The exoharness provides the durable building blocks those
decisions run on: history, state, secrets, and sandboxing.

Because the exoharness substrate doesn't depend on the executor, an agent built on exo can:

- **Fork or rewind** at any past event, without losing secrets, sandboxes, or history.
- **Swap executors**, running the same agent via Codex, Claude Code, the Cursor SDK, or your own executor, without rebuilding state.
- **Evolve safely** to change its own policy processes, with access to inspect its own history and artifacts while the exoharness isolates secrets and compute resources to maintain safety.

For the architectural model and terminology, see
[spec/exoharness.md](./spec/exoharness.md).

### Self-update across executor swaps: what carries over

**Swap executors** means durable _state_ carries over: agents, conversations,
event log, artifacts, secrets, and sandboxes all live in the exoharness
substrate, so you can replace the policy layer without rebuilding any of it.
It does **not** mean the self-update (RSI) tooling carried by an executor like
Exoclaw comes along for free. Self-update capability splits into two
categories:

| Capability                                                                                    | Where it lives                                                                                                                                                                                                                              | Automatic when you swap executors?                                                                                                                                                                |
| --------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Durable event log (append-only history of turns, tool calls, errors, sandbox and host events) | exoharness substrate (`crates/exoharness/src/basic.rs`, `append_events*`)                                                                                                                                                                   | Yes — any executor on exoharness records into the same log.                                                                                                                                       |
| Sandbox snapshot / rewind                                                                     | exoharness substrate (`snapshot_sandbox` / `rewind_sandbox`, dispatched in `crates/executor/src/harness_tool.rs`)                                                                                                                           | Yes — backend-provided; unsupported backends report clear errors.                                                                                                                                 |
| Secrets / credentials                                                                         | exoharness substrate (`exo secret set`, secret backend selection in `crates/cli/src/main.rs`)                                                                                                                                               | Yes — stored outside the executor, survives executor swaps and restarts.                                                                                                                          |
| Code visibility + editing                                                                     | Sandbox mounts plus the `shell` tool (`typescript/harness/built-in-tools.ts`)                                                                                                                                                               | Mostly — the mount and shell are substrate features, but mounting _the executor's own repo_ is a configuration choice each harness makes (Exoclaw does it via `scripts/exo.sh` / `EXOCLAW_REPO`). |
| Self-rebuild + self-restart with state preserved                                              | Executor-specific: Exoclaw's `guardian_action` tool (`examples/exoclaw/guardian-tools.ts`) wrapping the host-side guardian script (`examples/exoclaw/scripts/exoclaw-service-guardian`), with drain markers and reboot notices under `.exo` | **No.** "Restart yourself cleanly" is inherently specific to the executor's own build and supervision; the generic substrate cannot automate it.                                                  |

The last row is the one to plan for. A custom executor (a LangChain agent, a
Claude Agent SDK app, etc.) gets the substrate rows automatically, but has no
equivalent of the Exoclaw guardian bridge today. To add self-update to a custom
executor, follow the same pattern:

1. Write a host-side guardian-style script that builds and restarts _your_
   executor's process while preserving the `.exo` state directory, using
   deferred restart markers so the in-flight turn can finish first (see the
   drain/marker flow in `examples/exoclaw/docs/SELF-CONTROL.md`, area 1).
2. Wrap it with a strict allowlist tool registered via `registerHostTool`
   (`examples/exoclaw/host-tools.ts` is the standard bridge; the schema,
   validation, and deferred handoff in `examples/exoclaw/guardian-tools.ts` are
   the reference implementation).

Everything else Exoclaw does — event-log introspection, snapshot-before-risky-
change, git as the second audit log — rides on substrate capabilities your
executor inherits by running on exoharness.

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

Register a model, then drop into a REPL:

```bash
./target/debug/exo secret set openai --env OPENAI_API_KEY
./target/debug/exo model register gpt-5.5 --secret openai
./target/debug/exo repl
```

`exo repl` reuses or creates a default agent and conversation and uses a
registered model, so you can start chatting in one command. It's a plain chat
with no shell sandbox; create a conversation explicitly when you want tools.

`--env` takes the variable name literally. Use `--value "$OPENAI_API_KEY"` if
you intentionally want the shell to expand the value.

For explicit control over agents, conversations, or a shell-enabled sandbox:

```bash
./target/debug/exo agent create --model gpt-5.5 "Sandbox Example"
./target/debug/exo conversation create sandbox-example "Local Dev"
./target/debug/exo repl --agent sandbox-example --conversation local-dev
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
  --model gpt-5.5
```

The `examples/typescript` directory also contains Codex, Claude Code, Cursor,
and recursive-language-model harness experiments.

For the coding-agent setup commands, see
[docs/coding-agent-harnesses.md](./docs/coding-agent-harnesses.md).

## Exoclaw Long-Running Harness

Exoclaw is a long-running claw-type agent built on exoharness. It supports
scheduled tasks, and a full adapter system including support for WhatsApp,
Signal, and IRC. See [examples/exoclaw/README.md](./examples/exoclaw/README.md)
for setup, operation, and debugging commands.

## Repository Layout

- `crates`: Rust workspace for the CLI, exoharness substrate, and executors.
- `typescript`: TypeScript harness runtime, model-runtime helpers, and
  adapter-specific support code.
- `examples/typescript`: runnable TypeScript harness examples.
- `examples/exoclaw`: long-running TypeScript harness example with scheduled
  task and adapter support.
- `containers`: sandbox images used by the coding-agent harness examples.
- `spec`: core architecture and terminology.
- `docs`: design notes for in-progress directions.
- `scripts`: development and live e2e utilities.

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
