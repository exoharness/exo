# Changes Outside `examples/exoclaw`

This summarizes the current `adapters` branch relative to `origin/main`,
excluding `examples/exoclaw/**`. It is intended as a PR cleanup guide.

## High-Level Summary

The branch touches 38 files outside `examples/exoclaw/**`.

Most changes fall into these areas:

- adapter runtime and adapter management tools in `crates/executor/src/adapter`
- CLI entry points for adapters
- scheduler persistence and runtime support in `crates/executor`
- sandbox scoping and wakeup plumbing for long-running agents
- TypeScript harness support for adapter tools and turn-loop reuse
- repo-level docs, manifests, and dependency updates

There is also one untracked local file, `exospooky.sh`, which is not part of the
branch diff.

## CLI Surface

Files:

- `crates/cli/src/adapters.rs`
- `crates/cli/src/main.rs`
- `crates/cli/src/tui.rs`

Description:

- Adds adapter CLI commands for creating, listing, disabling, deleting, sending
  through, and supervising adapters.
- Wires the `exoclaw` harness kind into CLI command handling and TypeScript
  harness configuration.
- Updates TUI behavior and error handling to align with upstream changes merged
  into this branch.

PR cleanup notes:

- The Exoclaw scheduler runner has been moved under
  `examples/exoclaw/scheduler-runner`, so the shared CLI no longer exposes an
  `exo exoclaw ...` subcommand.
- Review whether the generic CLI should expose adapter management broadly or
  whether some commands should stay example-only.

## Adapter Runtime

Files:

- `crates/executor/src/adapter/mod.rs`
- `crates/executor/src/adapter/runtime.rs`
- `crates/executor/src/adapter/store.rs`
- `crates/executor/src/adapter/tools.rs`
- `crates/executor/src/adapter/types.rs`
- `crates/executor/src/adapter/worker.rs`
- `crates/executor/src/lib.rs`
- `typescript/harness/adapter-tools.ts`
- `typescript/harness/index.ts`

Description:

- Adds the executor-side adapter subsystem: persisted adapter records, event
  records, worker supervision, inbound wakeups, outbound sends, and host tool
  execution.
- Adds model-facing TypeScript adapter tools through `@exo/harness`.
- Simplifies adapter configuration around the current worker adapters rather
  than preserving unused module-adapter abstractions.
- Avoids writing adapter artifacts outside active turns, which prevents stale
  turn failures when adapters wake conversations.
- Ensures worker child processes are cleaned up more reliably when the adapter
  runner exits or restarts.

PR cleanup notes:

- The adapter runtime is the largest non-example feature in the branch. If the
  PR goal is "Exoclaw only", this is still hard to avoid because the runtime is
  implemented in shared executor code.
- `typescript/harness/adapter-tools.ts` makes adapter tools available from the
  shared TypeScript harness package. Confirm that this is desired for non-
  Exoclaw harnesses.

## Scheduler Runtime

Files:

- `crates/executor/src/scheduler_runtime.rs`
- `crates/executor/src/scheduler_store.rs`
- `crates/executor/src/scheduler_types.rs`
- `crates/executor/src/lib.rs`

Description:

- Adds scheduler data types, on-disk task storage, run history, due-task
  selection, and wakeup reporting.
- Supports recurring sandbox tasks with task status, output capture, run
  records, and scheduler-driven conversation wakeups.
- Exposes scheduler APIs through the executor crate for use by the Exoclaw CLI
  and Exoclaw tools.

PR cleanup notes:

- The scheduler is primarily used by Exoclaw. The TypeScript model-facing tools
  have already been moved under `examples/exoclaw`, but the host runtime remains
  in `crates/executor`.
- If this should be more example-scoped, the main question is whether executor
  should keep generic scheduling primitives while Exoclaw owns tool definitions
  and CLI wiring.

## Sandbox And Wakeup Plumbing

Files:

- `crates/executor/src/agent_sandbox.rs`
- `crates/executor/src/conversation_sandbox.rs`
- `crates/executor/src/conversation_wakeup.rs`
- `crates/executor/src/executor_types.rs`
- `crates/executor/src/harness_executor.rs`
- `crates/executor/src/harness_tool.rs`
- `crates/executor/src/typescript.rs`
- `crates/exoharness/src/sandbox.rs`

Description:

- Adds or adjusts sandbox scoping so Exoclaw can use a long-lived agent sandbox,
  conversation sandboxes, and task-owned sandboxes.
- Adds conversation wakeup support used by adapters and scheduled tasks.
- Extends harness tool execution and TypeScript harness requests to carry the
  context needed for long-running agent workflows.
- Updates sandbox cleanup behavior for named containers after merging upstream
  changes.

PR cleanup notes:

- These changes are cross-cutting and likely need the closest review because
  they affect shared executor behavior, not just Exoclaw.
- Confirm which sandbox modes are truly needed for the first adapter PR. Some
  scheduler/task-owned sandbox pieces may be separable if the PR needs to shrink.

## Shared TypeScript Harness Examples

Files:

- `examples/typescript/basic-harness.ts`
- `examples/typescript/turn-loop.ts`
- `examples/typescript/tools/irc.manifest.json`
- `examples/typescript/tools/uppercase.manifest.json`
- `typescript/harness/runner.ts`

Description:

- Extracts or expands reusable TypeScript turn-loop behavior.
- Updates example harnesses and manifests used by the TypeScript harness.
- Adjusts the TypeScript harness runner protocol in support of the branch's
  harness changes.

PR cleanup notes:

- Review whether the new `examples/typescript` files are required for Exoclaw or
  should be split into a separate TypeScript harness cleanup PR.
- The turn-loop changes are relevant to stale-turn fixes and may be important
  even if the example manifests are not.

## Core Harness And Tests

Files:

- `crates/executor/src/basic.rs`
- `crates/executor/src/basic_tests.rs`
- `crates/executor/src/harness_basic_tests.rs`
- `crates/exoharness/src/basic.rs`
- `crates/exoharness/src/basic_tests.rs`

Description:

- Updates basic harness behavior and tests around turn/event handling.
- Fixes stale-turn related behavior by ensuring events are appended through the
  active turn handle where appropriate.
- Incorporates clippy and test adjustments from earlier merge work.

PR cleanup notes:

- Keep the stale-turn fixes if adapters remain in scope.
- Separate purely mechanical clippy/test updates if they distract from adapter
  review.

## Repo Metadata And Documentation

Files:

- `Cargo.toml`
- `README.md`
- `adapter-arch.md`
- `package.json`
- `pnpm-lock.yaml`

Description:

- Adds repo-level documentation for adapter architecture and updates README
  guidance.
- Updates Rust and TypeScript dependency metadata needed by the branch.
- Adds package changes associated with TypeScript harness work and adapter
  dependencies.

PR cleanup notes:

- `adapter-arch.md` is useful review context, but consider whether it belongs at
  the repo root or under `examples/exoclaw`.
- Dependency changes should be checked for anything that was only needed during
  experimentation.

## Full File List

```text
M  Cargo.toml
M  README.md
A  adapter-arch.md
A  crates/cli/src/adapters.rs
M  crates/cli/src/main.rs
M  crates/cli/src/tui.rs
A  crates/executor/src/adapter/mod.rs
A  crates/executor/src/adapter/runtime.rs
A  crates/executor/src/adapter/store.rs
A  crates/executor/src/adapter/tools.rs
A  crates/executor/src/adapter/types.rs
A  crates/executor/src/adapter/worker.rs
A  crates/executor/src/agent_sandbox.rs
M  crates/executor/src/basic.rs
M  crates/executor/src/basic_tests.rs
A  crates/executor/src/conversation_sandbox.rs
A  crates/executor/src/conversation_wakeup.rs
M  crates/executor/src/executor_types.rs
M  crates/executor/src/harness_basic_tests.rs
M  crates/executor/src/harness_executor.rs
M  crates/executor/src/harness_tool.rs
M  crates/executor/src/lib.rs
A  crates/executor/src/scheduler_runtime.rs
A  crates/executor/src/scheduler_store.rs
A  crates/executor/src/scheduler_types.rs
M  crates/executor/src/typescript.rs
M  crates/exoharness/src/basic.rs
M  crates/exoharness/src/basic_tests.rs
M  crates/exoharness/src/sandbox.rs
M  examples/typescript/basic-harness.ts
A  examples/typescript/tools/irc.manifest.json
A  examples/typescript/tools/uppercase.manifest.json
A  examples/typescript/turn-loop.ts
M  package.json
M  pnpm-lock.yaml
A  typescript/harness/adapter-tools.ts
M  typescript/harness/index.ts
M  typescript/harness/runner.ts
```
