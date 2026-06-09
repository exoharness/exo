# Exoclaw Self-Control

Exoclaw is meant to be a long-running agent that can understand and adjust the system it is operating inside.

This document describes the self-control surfaces we want Exoclaw to have: understanding its own code, creating tools, managing adapters, controlling its sandbox environment, and restarting host-side services.

## Design Principles

- Prefer explicit tools over hidden conventions. If Exoclaw can perform a durable host-side action, that action should be represented by a named tool with a clear schema.
- Keep authority scoped to the current agent and conversation unless the user explicitly asks for broader work.
- Return inspectable records after mutations: ids, names, enabled state, owner conversation ids, and any follow-up constraints the agent needs.
- Preserve history by default. Prefer disabling or checkpointing over deletion or irreversible rewrites unless the user asks for a permanent change.
- Use the existing harness APIs as the source of truth. Tools should wrap `exoharness` and executor primitives rather than parsing storage files or reimplementing persistence.

## Self Introspection

Exoclaw should always be able to inspect the code that defines its own behavior. Local startup with `examples/exoclaw/scripts/exoclaw-control` keeps sandbox support enabled by default and mounts the repository into the sandbox at `/workspace/exo`. The path can be changed with `--self-repo-mount` or `EXOCLAW_REPO`, but the invariant is the same: Exoclaw's shell tool should start with a stable view of this source tree.

`examples/exoclaw/SELF.md` is the checked-in self map. In the sandbox it is available at `/workspace/exo/examples/exoclaw/SELF.md` by default. The harness also tells Exoclaw this path each turn through `EXOCLAW_SELF_MAP`, so the agent has a compact navigation guide before it edits or explains itself.

The self map should stay concise and navigational. It should point to:

- prompt and harness assembly files,
- scheduler and adapter code,
- sandbox and service guardian tools,
- host-side scripts,
- durable local state such as `.exo`,
- common build, restart, and inspection commands.

This is intentionally not a full architecture document. It is a starting point for self-directed inspection.

## Tool Creation

Exoclaw should eventually be able to create reusable tools for itself when a user needs a capability that is too specific for the built-in surface. The intended shape is:

- The agent writes a small TypeScript tool module with a strict input schema and a handler.
- The host validates the module and installs it as an agent-owned tool.
- The tool becomes available on the next model turn, not halfway through the current call.

This keeps tool creation auditable and avoids giving arbitrary runtime authority to generated code. Generated tools should use stable platform APIs, avoid extra npm dependencies by default, and declare any required initialization such as environment variable names for API keys.

Current status: `install_agent_tool` provides the host-validated installation path for generated TypeScript tools, and `uninstall_agent_tool` removes them again.

Tools are layered: the TypeScript registry defines what the model can see and call, and a tool handler either runs in TypeScript or delegates execution to the Rust tool runtime (`crates/executor/src/harness_tool.rs`) by calling `execution.context.executeTool(...)` with the same function name. Rust-backed tools therefore always need a TypeScript definition; `registerHostTool` in `examples/exoclaw/host-tools.ts` is the standard bridge, and the sandbox tools in `examples/exoclaw/sandbox-tools.ts` are the reference example. A Rust match arm without a registered TypeScript definition is unreachable, and an agent-installed tool must never reuse the name of a Rust-backed tool.

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

The shared agent sandbox is implemented as an agent-owned named reusable sandbox. Exoclaw stores the owner conversation and sandbox name in an agent artifact, then resolves the sandbox through `create_sandbox` with that name. The harness reuses an active sandbox handle when one exists, including a handle restored from a snapshot, and recreates the sandbox when needed. Exoclaw does not need to scan raw sandbox lifecycle events to find its current environment.

The repo mount is part of the sandbox spec. Changing the mount path or mode changes the desired sandbox configuration, so the next shell use may resolve or create a sandbox matching the new spec. The default workdir is the first configured mount, so Exoclaw starts shell inspection in its own source tree when the standard mount is present.

The canonical local startup is `examples/exoclaw/scripts/exoclaw-control canonical`. It selects the Docker sandbox provider/backend for the REPL, adapters, service guardian, and scheduler runner; pulls the sandbox image when needed; ensures the repo self-map mount; configures the service guardian for Docker-backed restarts; sends IRC and Discord setup prompts; starts services; and opens the control REPL. Use `examples/exoclaw/scripts/exoclaw-control fresh --canonical` when the user wants the same shape from a clean agent/conversation state.

The sandbox control tools expose filesystem checkpointing:

- `list_sandbox_snapshots` lists snapshots for the selected sandbox scope and reports the current snapshot id when one is known.
- `snapshot_sandbox` captures a filesystem checkpoint of the selected sandbox.
- `rewind_sandbox` restarts the selected sandbox from a prior snapshot id.

The default scope is the shared agent sandbox. Use conversation scope only when the conversation was configured to use its own sandbox. Tool results return the selected scope, sandbox id, owner conversation id, and snapshot id so later actions can target the same environment explicitly.

Snapshot and rewind availability depends on the sandbox backend. Docker warm sandboxes support this flow; one-shot or unsupported backends should report clear errors from the underlying sandbox implementation. For local Exoclaw testing on macOS, the REPL wrapper can set both the conversation provider and process backend with `--sandbox-provider docker`.

Sandbox lifecycle history remains canonical and append-only. `snapshot_sandbox` records a `sandbox_snapshotted` event, and `rewind_sandbox` records a snapshot-backed `sandbox_started` event in the same active turn as the corresponding tool request and result. Rewind restores sandbox filesystem state; it does not roll back conversation events, artifacts, adapter records, scheduler records, secrets, or other host-side harness state. This lets later history readers reconstruct what happened from conversation events instead of relying only on tool-result artifacts.

`list_sandbox_snapshots` reads Exoclaw's known snapshot registry, stored as an agent-owned artifact at `config/exoclaw-sandbox-snapshots.json`. That registry is useful for self-control because it lists snapshots created or selected through these tools and tracks the current known snapshot. It is not intended to be a complete inventory of every backend snapshot ever created outside Exoclaw.

## Host Supervisors

Some self-control actions must happen outside the sandbox because they affect the host process that is running Exoclaw. Exoclaw uses two cooperating supervisors for this:

- `examples/exoclaw/scripts/exoclaw-service-guardian` is the host service supervisor. It builds Exoclaw, shows service status, prints scheduler and adapter logs, and restarts scheduler or adapter runners while preserving `.exo` state.
- `examples/exoclaw/scripts/exoclaw-control --control` is the terminal supervisor. It keeps the user's terminal open, streams service logs, runs the interactive `exo repl` as a child process, and can restart only that child after a rebuild.

The model-visible `guardian_action` tool wraps the script with a strict allowlist. It exposes actions such as `status`, `build`, `restart_adapters`, `restart_scheduler`, `restart_all`, and `logs`; it does not accept arbitrary shell commands. Exoclaw should use `guardian_action` for host-side maintenance instead of manually killing processes.

Restart actions are deferred briefly and handed off to a detached service guardian process. That lets the current model turn finish and report that the restart was scheduled before the adapter or scheduler runner is stopped. The detached output goes to `.exo/exoclaw-service-guardian-actions.log`; after services come back, Exoclaw can use `guardian_action` with `status` or `logs` to inspect the result.

Reboots are announced through the adapters. Because restart actions are deferred, the agent can post a "going down" message with `send_adapter_message` in the same turn that requests the restart. When the guardian restarts the adapter runner it also writes `.exo/exoclaw-reboot-notice.json`; the fresh runner claims the notice and sends one wakeup per adapter conversation telling the agent services are back, so it can announce its return. The announcement reply queues in the durable adapter outbox and delivers as soon as the worker reconnects, so the wakeup does not need to wait for the external connection. Stale notices (older than 15 minutes) are discarded instead of announced.

Service restarts drain instead of killing blindly. The guardian writes a restart marker (`.exo/exoclaw-adapters.restart` or `.exo/exoclaw-scheduler.restart`); the runner claims the marker by deleting it, finishes in-flight wakeup turns or scheduler passes, and exits on its own so the guardian can start the new build. A runner that never claims the marker (an old build, or one wedged mid-task) is stopped with the process-tree kill after a short wait. Adapter workers themselves run in their own process groups, so stopping a worker also terminates the `pnpm`/`tsx`/`node` children that hold the external connection.

Builds request a control REPL refresh by writing `.exo/exoclaw-control.restart`. The control wrapper notices this marker, stops the current `exo repl` child, removes the marker, and starts a fresh child. This picks up rebuilt `exoharness`, `executor`, and TypeScript harness code without closing the user's terminal. If no control wrapper is running, the marker remains pending and `guardian status` reports it.

## Prompting Exoclaw

Prompts should teach the agent the same model:

- Know what tools exist and inspect state before changing it.
- Prefer reversible operations where possible.
- Include ids returned by inspection tools in follow-up actions.
- Explain destructive actions before taking them.
- Start self-maintenance work by reading `/workspace/exo/examples/exoclaw/SELF.md` unless the user points to a more specific file.
- Use sandbox snapshots before risky filesystem experiments.
- Use `guardian_action` for host builds, logs, and service restarts.
- Treat self-restart as a supervisor handoff: the service guardian owns scheduler/adapters, while the control REPL wrapper owns the interactive REPL child.

The goal is not for Exoclaw to have unlimited self-modification. The goal is for it to have enough structured visibility and control to maintain its own working environment with user-directed, auditable actions.

## Prompts

Self-control behavior is taught in a few different prompt surfaces:

- `examples/exoclaw/prompts/me.md` is the durable Exoclaw identity prompt. Put broad behavioral rules here, such as when to inspect state first, when to prefer reversible operations, and how to think about external side effects.
- `examples/exoclaw/harness.ts` assembles the Exoclaw developer prompt for each turn. It describes the available self-control surfaces at a high level: scheduled tasks, adapters, sandbox snapshots, guardian actions, the self map, and the default sandbox scope.
- `examples/exoclaw/SELF.md` is the compact self map for navigating Exoclaw's own code from the sandbox.
- `examples/exoclaw/sandbox-tools.ts`, `examples/exoclaw/scheduler-tools.ts`, `examples/exoclaw/guardian-tools.ts`, and `typescript/harness/adapter-tools.ts` provide model-visible tool descriptions and JSON schemas. These are the most direct prompts for when and how Exoclaw should call each self-control tool.
- `typescript/harness/built-in-tools.ts` describes built-in tools such as `shell`, `install_agent_tool`, and `uninstall_agent_tool`, including the generated-tool installation contract.
- `examples/exoclaw/adapters/*/setup-prompt.md` files guide Exoclaw through creating specific adapters. These are setup-time prompts, not always-on behavioral rules.
- `crates/executor/src/adapter/runtime.rs` creates adapter wakeup prompts for inbound external messages. Those prompts tell Exoclaw when a response should go back through `send_adapter_message`.
- `crates/executor/src/scheduler_runtime.rs` creates scheduled-task wakeup prompts from each task's `reportPrompt`. Use those prompts to preserve adapter targets or other reporting instructions across future runs.
- `.exo/exoclaw-profile.md` or `EXOCLAW_LOCAL_PROMPT_FILE` can add local user-specific instructions without changing the checked-in Exoclaw prompts.
