# Exoclaw Self-Control

Exoclaw is meant to be a long-running agent that can understand and adjust the system it is operating inside.

This document describes the self-control surfaces we want Exoclaw to have: creating tools, managing adapters, and controlling its sandbox environment.

## Design Principles

- Prefer explicit tools over hidden conventions. If Exoclaw can perform a durable host-side action, that action should be represented by a named tool with a clear schema.
- Keep authority scoped to the current agent and conversation unless the user explicitly asks for broader work.
- Return inspectable records after mutations: ids, names, enabled state, owner conversation ids, and any follow-up constraints the agent needs.
- Preserve history by default. Prefer disabling or checkpointing over deletion or irreversible rewrites unless the user asks for a permanent change.
- Use the existing harness APIs as the source of truth. Tools should wrap `exoharness` and executor primitives rather than parsing storage files or reimplementing persistence.

## Tool Creation

Exoclaw should eventually be able to create reusable tools for itself when a user needs a capability that is too specific for the built-in surface. The intended shape is:

- The agent writes a small TypeScript tool module with a strict input schema and a handler.
- The host validates the module and installs it as an agent-owned tool.
- The tool becomes available on the next model turn, not halfway through the current call.

This keeps tool creation auditable and avoids giving arbitrary runtime authority to generated code. Generated tools should use stable platform APIs, avoid extra npm dependencies by default, and declare any required initialization such as environment variable names for API keys.

Current status: `install_agent_tool` provides the host-validated installation path for generated TypeScript tools.

## Adapter Control

Adapters are how Exoclaw connects conversations to external systems such as chat apps. The control model is:

- `create_adapter` creates an enabled adapter record for the current conversation. The adapter supervisor notices enabled adapters and starts their separate worker processes.
- `list_adapters` shows the adapter ids, types, configuration summaries, and enabled state.
- `disable_adapter` marks an adapter disabled. The running worker observes that state and exits, which stops external wakeups while preserving adapter history and configuration.
- `delete_adapter` removes the adapter record and its stored state when the user asks for permanent cleanup.
- `send_adapter_message` sends an intentional outbound reply to an external target.

There is not currently a separate `start_adapter`, `stop_adapter`, or `restart_adapter` tool. Starting is modeled as creating an enabled adapter, and stopping is modeled as disabling it. A future resume/restart surface should probably be explicit, for example `enable_adapter` or `restart_adapter`, so Exoclaw can bring a disabled adapter back without recreating it.

The important safety boundary is that inbound adapter messages wake the conversation, but model text is not automatically posted back externally. Exoclaw should call `send_adapter_message` only when the conversation context makes an external reply appropriate.

## Sandbox Environment Control

Exoclaw's shell tool runs inside a sandbox. By default, Exoclaw conversations use `sandboxScope: "agent"`, so shell commands share one persistent agent sandbox across conversations. A conversation can opt into `sandboxScope: "conversation"` when it needs an isolated sandbox for that conversation.

The shared agent sandbox is implemented as an agent-owned named reusable sandbox. Exoclaw stores the owner conversation and sandbox name in an agent artifact, then reacquires the sandbox through `create_sandbox` with that name. The harness handles reuse when the named sandbox is still running and recreates it when needed, so Exoclaw does not need to scan raw sandbox lifecycle events to find its current environment.

The sandbox control tools expose filesystem checkpointing:

- `list_sandbox_snapshots` lists snapshots for the selected sandbox scope and reports the current snapshot id when one is known.
- `snapshot_sandbox` captures a filesystem checkpoint of the selected sandbox.
- `rewind_sandbox` restarts the selected sandbox from a prior snapshot id.

The default scope is the shared agent sandbox. Use conversation scope only when the conversation was configured to use its own sandbox. Tool results return the selected scope, sandbox id, owner conversation id, and snapshot id so later actions can target the same environment explicitly.

Snapshot and rewind availability depends on the sandbox backend. Docker warm sandboxes support this flow; one-shot or unsupported backends should report clear errors from the underlying sandbox implementation. For local Exoclaw testing on macOS, the repl wrapper can set both the conversation provider and process backend with `--sandbox-provider docker`.

Sandbox lifecycle history remains canonical. `snapshot_sandbox` records a `sandbox_snapshotted` event, and `rewind_sandbox` records a snapshot-backed `sandbox_started` event in the same active turn as the corresponding tool request and result. This lets later history readers reconstruct what happened from conversation events instead of relying only on tool-result artifacts.

`list_sandbox_snapshots` reads Exoclaw's known snapshot registry, stored as an agent-owned artifact at `config/exoclaw-sandbox-snapshots.json`. That registry is useful for self-control because it lists snapshots created or selected through these tools and tracks the current known snapshot. It is not intended to be a complete inventory of every backend snapshot ever created outside Exoclaw.

## Prompting Exoclaw

Prompts should teach the agent the same model:

- Know what tools exist and inspect state before changing it.
- Prefer reversible operations where possible.
- Include ids returned by inspection tools in follow-up actions.
- Explain destructive actions before taking them.
- Use sandbox snapshots before risky filesystem experiments.

The goal is not for Exoclaw to have unlimited self-modification. The goal is for it to have enough structured visibility and control to maintain its own working environment with user-directed, auditable actions.

## Prompts

Self-control behavior is taught in a few different prompt surfaces:

- `examples/exoclaw/prompts/me.md` is the durable Exoclaw identity prompt. Put broad behavioral rules here, such as when to inspect state first, when to prefer reversible operations, and how to think about external side effects.
- `examples/exoclaw/harness.ts` assembles the Exoclaw developer prompt for each turn. It describes the available self-control surfaces at a high level: scheduled tasks, adapters, sandbox snapshots, and the default sandbox scope.
- `examples/exoclaw/sandbox-tools.ts`, `examples/exoclaw/scheduler-tools.ts`, and `typescript/harness/adapter-tools.ts` provide model-visible tool descriptions and JSON schemas. These are the most direct prompts for when and how Exoclaw should call each self-control tool.
- `typescript/harness/built-in-tools.ts` describes built-in tools such as `shell` and `install_agent_tool`, including the generated-tool installation contract.
- `examples/exoclaw/adapters/*/setup-prompt.md` files guide Exoclaw through creating specific adapters. These are setup-time prompts, not always-on behavioral rules.
- `crates/executor/src/adapter/runtime.rs` creates adapter wakeup prompts for inbound external messages. Those prompts tell Exoclaw when a response should go back through `send_adapter_message`.
- `crates/executor/src/scheduler_runtime.rs` creates scheduled-task wakeup prompts from each task's `reportPrompt`. Use those prompts to preserve adapter targets or other reporting instructions across future runs.
- `.exo/exoclaw-profile.md` or `EXOCLAW_LOCAL_PROMPT_FILE` can add local user-specific instructions without changing the checked-in Exoclaw prompts.
