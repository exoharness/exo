---
title: Task Scheduler
description: Run recurring work in a sandbox on a schedule, with each run waking the conversation.
---

# Task Scheduler

Run recurring work in a sandbox on a schedule, with each run waking the conversation.

The **task scheduler** runs recurring work in a sandbox on a schedule —
independent of whether anyone is chatting. It's how a long-running agent
does things like "check the BBC headlines every hour" or "run the test
suite nightly."

Like [tools](./tools), the scheduler is executor-level, not an
exoharness primitive. The agent manages tasks through scheduler tools; a
separate **scheduler runner** process owns timing and execution and is
started as one of the [canonical agent's](./canonical-agent) services.

## Managing tasks

The agent has four tools:

- `schedule_sandbox_task` — create a recurring task
- `list_scheduled_tasks` — see active tasks
- `cancel_scheduled_task` — disable a task but keep its history
- `delete_scheduled_task` — remove a task entirely

## What a task is

Each task records:

- **schedule** — `@every 10m`, `@every 1h`, or a simple cron interval like
  `*/30 * * * *`
- **command** — the argv to run (e.g. `["bash", "-lc", "curl -fsSL …"]`)
- **setupCommand** — optional argv run before each run (install deps, etc.)
- **sandboxMode** — where it runs (see below)
- **reportPrompt** — how to summarize each completed run back to the user
- **maxOutputBytes** — how much output to retain before truncating

### Sandbox mode

| Mode | Runs in |
|:-----|:--------|
| `agent` | The shared, persistent agent [sandbox](./sandboxes) (default) |
| `conversation` | This conversation's sandbox |
| `task_fresh` | A separate sandbox created for the task and reused across its runs |

## How a run reports back

When a run finishes, the scheduler stores its output as an
[artifact](./data-model) and **wakes the conversation** — it starts a
new turn carrying a compact result guided by the task's `reportPrompt`. So
scheduled work shows up in the conversation as if the agent had just done
it, and the durable record (last run, next run, latest result) lives in the
task record.

::: info
  Registering a task writes it to the scheduler's store immediately, but a
  task only *runs* if the scheduler runner process is active. The canonical
  agent's setup starts it; a bare CLI agent has no runner.
:::
