# Exoclaw Harness

Exoclaw is the long-running agent harness example. It builds on the minimal
TypeScript harness turn loop, but opts into heavier runtime features:

- scheduled sandbox tasks
- long-running adapters for external applications
- live conversation wake-ups
- sticky agent sandbox policy
- optional `sandboxScope: "conversation"` conversation-scoped shell sandboxes
- optional `sandboxMode: "conversation"` scheduled tasks
- optional `sandboxMode: "task_fresh"` task-owned sandboxes

Use Exoclaw when the agent should keep working over time. Use
`examples/typescript/basic-harness.ts` for a minimal TypeScript harness without
scheduler tools.

For a deeper explanation of the adapter runtime, IRC example, and scheduler
cooperation, see [adapter-architecture.md](./adapter-architecture.md).

Start an Exoclaw REPL with the default agent-scoped sandbox:

```bash
scripts/exoclaw-repl --pull-sandbox
```

The script also starts local scheduler and adapter runner processes by default.
Use `--no-scheduler` or `--no-adapters` when you only want the interactive REPL.
For repeatable setup tests, pass an initial prompt file; the script sends it to
the conversation before dropping into the REPL:

```bash
scripts/exoclaw-repl fresh --pull-sandbox \
  --initial-prompt-file examples/exoclaw/prompts/irc-test.md
```

Edit the nick and channel in `examples/exoclaw/prompts/irc-test.md` before using
it on a real IRC network.

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

And adapter tools:

- `create_adapter`
- `list_adapters`
- `disable_adapter`
- `delete_adapter`
- `send_adapter_message`
- `install_agent_adapter`
- `build_agent_adapter`

`disable_adapter` stops future adapter wake-ups while preserving the adapter
record and event history. `delete_adapter` removes the adapter record and stored
events.

## Adapters

Adapters are host-owned long-running runtimes for external applications. They
are intentionally separate from scheduled sandbox commands: adapters own sockets,
reconnect behavior, inbound message parsing, event history, and conversation
wake-ups. Agents configure adapters with tools, and the local adapter runner
started by `scripts/exoclaw-repl` keeps them connected.

The first built-in adapter is IRC. It runs as a host-supervised Node.js worker
that connects to a configured server/channel, responds to `PING`, parses
`PRIVMSG`, and wakes the conversation when the trigger policy matches. The
recommended trigger is `mention`, which only wakes the conversation when a
channel message mentions the adapter nick. `all_messages` is available for
quieter channels.

Exoclaw also includes an experimental WhatsApp adapter using Baileys. The Rust
adapter runtime owns supervision, durable events, conversation wakeups, and
outbox draining; workers own protocol-specific sockets and parsing. When first
run, the WhatsApp worker emits a QR pairing event into adapter history and logs;
after pairing, Baileys auth state is stored under
`.exo/adapters/whatsapp/<adapter-id>/auth` by default.

Example IRC adapter tool arguments:

```json
{
  "name": "libera-exo",
  "source": "built_in",
  "config": {
    "type": "irc",
    "server": "irc.libera.chat",
    "port": 6697,
    "tls": true,
    "nick": "exo-bot",
    "username": "exo-bot",
    "realname": "Exoclaw Bot",
    "channel": "#example",
    "passwordSecretId": null,
    "trigger": "mention"
  }
}
```

Example WhatsApp adapter tool arguments:

```json
{
  "name": "whatsapp-dev",
  "source": "built_in",
  "config": {
    "type": "whatsapp",
    "authDir": null,
    "trigger": "all_messages",
    "allowedChats": null,
    "workerCommand": null
  }
}
```

Inbound adapter messages do not automatically send model output back to the
external service. The agent must explicitly call `send_adapter_message`, which
keeps external side effects visible in tool history. WhatsApp sends require the
`target` chat id from the inbound wakeup. IRC sends go to the configured channel;
using the inbound channel target is fine, but the worker does not require it.

If an IRC or WhatsApp user asks Exoclaw to schedule recurring work and expects
future results in the originating app, the agent should put that routing
instruction in the task's `reportPrompt`, including the `adapterId` and target
from the wakeup. Scheduler wakeups are normal Exoclaw turns, so they can call
`send_adapter_message` when the `reportPrompt` says to post the result back.

Adapters use the same source model as tools:

- `built_in`: shipped with Exoclaw, starting with IRC and experimental WhatsApp.
- `library`: registered from reusable module metadata.
- `agent`: installed by the agent with `install_agent_adapter`, then validated
  with `build_agent_adapter`.

The host runtime runs built-in IRC and WhatsApp adapters through the same generic
worker bridge. Module-backed library and agent adapters are persisted and
build-validated so the source model is in place; richer module execution can be
layered on the same registry/runtime boundary.

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
