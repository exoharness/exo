# Exoclaw Harness

Exoclaw is the long-running agent harness example. It builds on the minimal
TypeScript harness turn loop, but opts into heavier runtime features:

- scheduled sandbox tasks
- live conversation wake-ups
- sticky conversation sandbox policy
- optional `sandboxMode: "task_fresh"` task-owned sandboxes

Use Exoclaw when the agent should keep working over time. Use
`examples/typescript/basic-harness.ts` for a minimal TypeScript harness without
scheduler tools.

## Tools

Exoclaw includes the normal minimal tools:

- `shell`
- `install_agent_tool` when agent tool creation is enabled
- configured library tools

It also adds scheduler tools:

- `schedule_sandbox_task`
- `list_scheduled_tasks`
- `cancel_scheduled_task`
- `delete_scheduled_task`

`cancel_scheduled_task` disables a task and preserves its record/history.
`delete_scheduled_task` removes the task record and stored run history.

## Sandbox Modes

Scheduled tasks default to `sandboxMode: "conversation"`. This uses the sticky
conversation sandbox, so packages installed through the REPL, such as `curl` or
`python3`, are available to scheduled task runs while that warm sandbox is still
alive.

Important limitation: the current sandbox filesystem is not durable across warm
container death. Exoclaw stores a durable conversation sandbox record, but package
installs made interactively live in the running warm container. If the REPL exits,
the host restarts, or the container backend cleans up the warm container, a later
scheduled task may recreate the sandbox from the base image and lose packages
installed with commands like `apt-get install python3`.

For reliable scheduled tasks, prefer one of these:

- Use a sandbox image that already contains required dependencies.
- Include a `setupCommand` that installs required packages before the task runs.
- Keep task code/data on mounted storage instead of relying on mutated container
  filesystem state.

Use `sandboxMode: "task_fresh"` when a task should have a separate fresh sandbox.
That sandbox starts from the configured image and mounts. It is reused across the
task's runs and stopped when the task is cancelled.

The right long-term scope is still open. Conversation-scoped sandboxes are useful
for making one conversation's setup visible to its scheduled tasks, but
agent-scoped sandboxes may be more intuitive for long-running agents that manage
multiple conversations. This should likely become configurable, with an explicit
durability model rather than relying on warm container lifetime.
