# Exoclaw Self Map

This file helps Exoclaw inspect and maintain its own code. In a normal local
startup, the repository is mounted in the sandbox at:

```text
/workspace/exo
```

Use this map before changing Exoclaw itself.

## Important Paths

- `examples/exoclaw/harness.ts`: assembles Exoclaw's prompt and tool registry.
- `examples/exoclaw/prompts/me.md`: durable identity and operating rules.
- `examples/exoclaw/guardian-tools.ts`: model-visible host maintenance tool.
- `examples/exoclaw/scripts/exoclaw-service-guardian`: host-side build and service control.
- `./exo.sh`: local startup script for REPL, scheduler, adapters, sandbox, and repo mount.
- `examples/exoclaw/sandbox-tools.ts`: sandbox snapshot and rewind tool definitions.
- `examples/exoclaw/introspection-tools.ts`: `list_adapter_events` and `list_conversation_events` introspection tools.
- `examples/exoclaw/host-tools.ts`: `registerHostTool` helper that bridges TypeScript tool definitions to Rust execution.
- `examples/exoclaw/scheduler-tools.ts`: scheduled task tool definitions.
- `examples/exoclaw/scheduler-runner/`: host scheduler runner binary.
- `examples/exoclaw/adapters/`: adapter setup prompts and worker implementations.
- `examples/exoclaw/adapters/agent-cli/`: shell entry point adapter; `exo-cli` sends prompts plus the user's working directory over a unix socket, and the message tells you which sandbox path (under the `/agent-cli` mount by default) to `cd` into.
- `typescript/harness/adapter-tools.ts`: model-visible adapter tool definitions.
- `crates/executor/src/adapter/`: Rust adapter runtime and supervision.
- `crates/executor/src/harness_tool.rs`: Rust tool execution runtime (`execute_tool` match arms).
- `crates/executor/src/agent_sandbox.rs`: shared agent sandbox selection.
- `crates/executor/src/conversation_sandbox.rs`: conversation sandbox selection.
- `crates/exoharness/`: durable harness API, conversation state, events, artifacts, and sandbox lifecycle.

## Local State

- `.exo/` contains local harness state, adapter config, pairing data, artifacts,
  pid files, logs, and service guardian config. It is intentionally ignored by
  git.
- `.env` contains local secrets and environment configuration. It is ignored by
  git.
- Do not commit personal profile data from `.exo/exoclaw-profile.md`.

## Common Commands

From the repository root on the host:

```bash
./exo.sh
examples/exoclaw/scripts/exoclaw-service-guardian status
examples/exoclaw/scripts/exoclaw-service-guardian build
examples/exoclaw/scripts/exoclaw-service-guardian restart-all --build
./exo.sh --control
```

Inside the Exoclaw sandbox, inspect the mounted code with:

```bash
cd /workspace/exo
pwd
ls examples/exoclaw
```

## Tool Architecture

Tools have two layers, and both matter:

1. **Definition (TypeScript)**: the model only sees tools registered in the
   TypeScript registry each turn (`examples/exoclaw/harness.ts`). Sources are
   built-in tools, library tool modules, and agent-installed tools in
   `.exo/agent-tools/` (managed with `install_agent_tool` /
   `uninstall_agent_tool`).
2. **Execution (TypeScript or Rust)**: a tool's handler can run entirely in
   TypeScript, or delegate to the Rust runtime via
   `execution.context.executeTool(...)`, which dispatches on the function name
   in `crates/executor/src/harness_tool.rs`.

### Adding a Rust-backed tool

A Rust match arm alone is invisible to the model; it always needs a TypeScript
definition that delegates to it:

1. Implement the tool logic as a match arm in `execute_tool` in
   `crates/executor/src/harness_tool.rs` (see `list_sandbox_snapshots` for a
   full example).
2. Register a TypeScript definition with the same name using
   `registerHostTool` from `examples/exoclaw/host-tools.ts`, wired into the
   registry in `examples/exoclaw/harness.ts` (see `sandbox-tools.ts` for the
   pattern).
3. Rebuild and restart with `guardian_action restart_all` (with build) so both
   the Rust binary and the harness pick up the change.
4. Never create an agent-installed tool with the same name as a Rust-backed
   tool; the registry conflict makes calls ambiguous.

## Diagnosing Adapters and Restarts

When an adapter seems quiet, broken, or recently restarted, diagnose it from
inside the conversation before touching host services:

1. `list_adapters` shows each adapter's `enabled` state plus health fields
   `last_connected_at_ms` and `last_error`.
2. `list_adapter_events` returns the adapter's recent telemetry newest first:
   `connected`, `disconnected`, `inbound`, `outbound`, `error`, and
   `lifecycle` records. Filter with `eventType` and `sinceMs` to narrow in
   (for example `eventType: "error"` after a reboot, or `sinceMs` set to the
   restart time).
3. `list_conversation_events` reads the canonical conversation event log,
   which host components also write to. `host_reboot` records a planned host
   restart with its reason; `adapter_runner_started` records every adapter
   runner start (without a preceding `host_reboot` it implies a crash or
   manual restart); `adapter_runner_draining` records a graceful shutdown
   beginning. This is the immutable history of what happened to you — use it
   to answer "was I restarted, when, and why?".
4. Only escalate to `guardian_action` logs or restarts once the event history
   shows a host-side problem (worker crash loops, repeated disconnects, send
   failures).

## Maintenance Rules

- Prefer `guardian_action` for host-side build, status, logs, and service restarts.
- Use the `shell` tool for sandbox-local inspection and experiments.
- Use `snapshot_sandbox` before risky filesystem changes.
- Use `guardian_action restart_all` after code changes that require host services
  to pick up a new build. Restart actions are deferred briefly so the current
  turn can finish before services stop; check status/logs after they come back.
- In control mode, service guardian builds write `.exo/exo-control.restart`;
  the `./exo.sh --control` wrapper restarts only the child `exo repl` and
  keeps the user's terminal open.
- Service restarts drain gracefully: the guardian writes
  `.exo/exoclaw-adapters.restart` / `.exo/exoclaw-scheduler.restart`, the
  runner claims the marker, finishes in-flight work, and exits so the guardian
  can start the new build. Runners that do not claim the marker are killed
  after a short wait.
- Adapter restarts also write `.exo/exoclaw-reboot-notice.json`; the fresh
  adapter runner claims it and wakes the adapter conversations so you can
  announce externally that you are back. Announce planned downtime with
  `send_adapter_message` before requesting the restart.
- Preserve `.exo` state unless the user explicitly asks to delete state.
