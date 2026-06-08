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
- `examples/exoclaw/scripts/exoclaw-control`: local startup script for REPL, scheduler, adapters, sandbox, and repo mount.
- `examples/exoclaw/sandbox-tools.ts`: sandbox snapshot and rewind tool definitions.
- `examples/exoclaw/scheduler-tools.ts`: scheduled task tool definitions.
- `examples/exoclaw/scheduler-runner/`: host scheduler runner binary.
- `examples/exoclaw/adapters/`: adapter setup prompts and worker implementations.
- `typescript/harness/adapter-tools.ts`: model-visible adapter tool definitions.
- `crates/executor/src/adapter/`: Rust adapter runtime and supervision.
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
examples/exoclaw/scripts/exoclaw-control canonical
examples/exoclaw/scripts/exoclaw-service-guardian status
examples/exoclaw/scripts/exoclaw-service-guardian build
examples/exoclaw/scripts/exoclaw-service-guardian restart-all --build
examples/exoclaw/scripts/exoclaw-control --control
```

Inside the Exoclaw sandbox, inspect the mounted code with:

```bash
cd /workspace/exo
pwd
ls examples/exoclaw
```

## Maintenance Rules

- Prefer `guardian_action` for host-side build, status, logs, and service restarts.
- Use the `shell` tool for sandbox-local inspection and experiments.
- Use `snapshot_sandbox` before risky filesystem changes.
- Use `guardian_action restart_all` after code changes that require host services
  to pick up a new build. Restart actions are deferred briefly so the current
  turn can finish before services stop; check status/logs after they come back.
- In control mode, service guardian builds write `.exo/exoclaw-control.restart`;
  the `exoclaw-control --control` wrapper restarts only the child `exo repl` and
  keeps the user's terminal open.
- Preserve `.exo` state unless the user explicitly asks to delete state.
