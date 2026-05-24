# Exoclaw Harness

Exoclaw is the long-running agent harness example. It builds on the minimal
TypeScript harness turn loop, but opts into heavier runtime features:

- scheduled sandbox tasks
- live conversation wake-ups
- sticky agent sandbox policy
- optional `sandboxScope: "conversation"` conversation-scoped shell sandboxes
- optional `sandboxMode: "conversation"` scheduled tasks
- optional `sandboxMode: "task_fresh"` task-owned sandboxes

Use Exoclaw when the agent should keep working over time. Use
`examples/typescript/basic-harness.ts` for a minimal TypeScript harness without
scheduler tools.

Start an Exoclaw REPL with the default agent-scoped sandbox:

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

Exoclaw conversations default to `sandboxScope: "agent"`. The `shell` tool uses
the sticky agent sandbox, so packages installed through the Exoclaw REPL, such as
`curl` or `python3`, are available to scheduled task runs and future
conversations for the same agent while that warm sandbox is still alive. Normal
REPL exits leave the warm sandbox running so the next Exoclaw process can
reattach to it.

Because exoclaw defaults to agent scope, you don't need to specify anything from
the cli. The following command will create a REPL with the agent and a
persistent sandbox that will be durable across conversations

```bash
scripts/exoclaw-repl --pull-sandbox
```

If you want a conversation to have its own sandbox, use `sandboxScope: "conversation"`:

```bash
scripts/exoclaw-repl --conversation isolated-dev --sandbox-scope conversation
exo --harness exoclaw conversation update exoclaw-agent isolated-dev --sandbox-scope conversation
```

Scheduled tasks also default to `sandboxMode: "agent"`. A task can explicitly use
`sandboxMode: "conversation"` to run in the current conversation's sandbox, or
`sandboxMode: "task_fresh"` to create a separate task-owned sandbox.

Important limitation: the current sandbox filesystem is not durable across warm
container death. Exoclaw stores a durable pointer to the agent's sandbox, but
package installs made interactively live in the running warm container. If the
host restarts or the container backend cleans up the warm container, a later
scheduled task may recreate the sandbox from the base image and lose packages
installed with commands like `apt-get install python3`. Stale warm containers are
eligible for orphan cleanup after roughly 24 hours.

For reliable scheduled tasks, prefer one of these:

- Use a sandbox image that already contains required dependencies.
- Include a `setupCommand` that installs required packages before the task runs.
- Keep task code/data on mounted storage instead of relying on mutated container
  filesystem state.

The task-owned sandbox starts from the configured image and mounts. It is reused
across the task's runs and stopped when the task is cancelled.

The current scope model is Exoclaw-specific policy on top of conversation-owned
exoharness sandbox records. The default mental model is agent-scoped, while
conversation and task scopes remain available for isolation.
