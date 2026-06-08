# Exoclaw Harness

Exoclaw is a persistent agent built on Exo designed to be helpful wherever
there is a task to do from a computer. It supports task scheduling, a full
sandbox where it can install its own tools and integrations, and right now
supports IRC, WhatsApp, Signal, and Discord out of the box.

Exoclaw includes a helper script to start up all the subservices (task
scheduling and adapters). To get started, simply run:

```bash
examples/exoclaw/scripts/exoclaw-control canonical
```

This canonical local setup uses the Docker sandbox backend, mounts this repo
into the sandbox at `/workspace/exo`, configures the scheduler and service
guardian for Docker-backed work, starts scheduler and adapter services, sends
IRC and Discord setup prompts, and drops into a control REPL where you can chat
with Exoclaw.

For a fresh canonical setup that deletes existing agents/conversations first:

```bash
examples/exoclaw/scripts/exoclaw-control fresh --canonical
```

Or for a minimal start without adapter setup:
`examples/exoclaw/scripts/exoclaw-control --pull-sandbox`

## Self Introspection

Exoclaw starts with sandbox shell support by default. The startup script mounts
this repository into the sandbox at `/workspace/exo` and makes that path
available to the harness as `EXOCLAW_REPO`. The self map lives at:

```text
/workspace/exo/examples/exoclaw/SELF.md
```

The checked-in source for that map is `examples/exoclaw/SELF.md`. It points
Exoclaw to the harness, prompts, adapter runtime, scheduler, sandbox tools, and
service guardian. Use `--self-repo-mount <path>` or `EXOCLAW_REPO` to choose a
different sandbox mount path.

## Service Guardian

`examples/exoclaw/scripts/exoclaw-service-guardian` is a host-side helper for
self-maintenance. It owns build and service-control actions that should happen
outside the agent's sandbox, while preserving `.exo` state such as adapter
pairing data, conversations, artifacts, and sandbox records.

Common commands:

```bash
examples/exoclaw/scripts/exoclaw-service-guardian status
examples/exoclaw/scripts/exoclaw-service-guardian build
examples/exoclaw/scripts/exoclaw-service-guardian restart-adapters
examples/exoclaw/scripts/exoclaw-service-guardian restart-scheduler
examples/exoclaw/scripts/exoclaw-service-guardian restart-all --build
```

Save local launch settings for later restarts with:

```bash
examples/exoclaw/scripts/exoclaw-service-guardian configure --sandbox-backend docker
```

The service guardian manages only the scheduler and adapter runners. Start or
reconnect an interactive REPL with `examples/exoclaw/scripts/exoclaw-control`.

Exoclaw can call the same host-side surface through the `guardian_action` tool.
That tool exposes only allowlisted actions such as `status`, `build`,
`restart_adapters`, `restart_scheduler`, `restart_all`, and `logs`.
Restart actions are handed off to a detached guardian process after a short
delay so the current agent turn can finish before services stop. Detached
restart output is written to `.exo/exoclaw-service-guardian-actions.log`.

When `examples/exoclaw/scripts/exoclaw-control --control` is running, it also acts
as the foreground REPL supervisor. Guardian builds write
`.exo/exoclaw-control.restart`; the control wrapper sees that marker, restarts only
the child `exo repl`, and keeps your terminal open.

## Setting up the identity

`examples/exoclaw/prompts/me.md` is the committed, generic Exoclaw identity
prompt. It's best to keep this high level, and not specific to any given
instance or the local deployment environment.

Use `.exo/exoclaw-profile.md` for local instructions instead. The harness loads
it as an additional developer prompt when it exists, and `.exo` is ignored by
git. To create it interactively:

```bash
examples/exoclaw/scripts/exoclaw-control setup-profile
```

The script asks for the user's name and any extra local instructions. To use a
different local prompt path, set `EXOCLAW_LOCAL_PROMPT_FILE` or pass
`--local-prompt-file <path>`.

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

`disable_adapter` stops future adapter wake-ups while preserving the adapter
record and event history. `delete_adapter` removes the adapter record and stored
events.

## Adapters

Adapters are host-owned long-running runtimes for external applications. They
are intentionally separate from scheduled sandbox commands: adapters own sockets,
reconnect behavior, inbound message parsing, event history, and conversation
wake-ups. Agents configure adapters with tools, and the local adapter runner
started by `examples/exoclaw/scripts/exoclaw-control` keeps them connected.

Exoclaw ships with IRC, WhatsApp, Signal, and Discord adapters. The canonical
local setup turns on IRC and Discord:

```bash
examples/exoclaw/scripts/exoclaw-control canonical
```

To send every setup prompt before opening the REPL:

```bash
examples/exoclaw/scripts/exoclaw-control --setup-all
```

For a fresh control agent with a local profile prompt and all adapters:

```bash
PATH="/opt/homebrew/opt/openjdk/bin:$PATH" \
  examples/exoclaw/scripts/exoclaw-control fresh \
  --agent spooky \
  --agent-name Spooky \
  --conversation dev \
  --setup-profile \
  --setup-all
```

This will:

- Prompt you for any needed adapter configuration (such as nicknames, channel/server info for IRC, or pairing for WhatsApp/Signal).
- Write adapter configuration to `.exo/adapters/`.
- Start the background adapter runner so the agent can receive and react to external messages in real time.

The adapter runner starts by default. Use `--no-adapters` to skip it, or
`--adapters` to force it on when an environment override disabled it.

You can list configured adapters with:

```bash
target/debug/exo --harness exoclaw adapters list
```

See the sections below for more details on individual adapter configuration.

### IRC

The first built-in adapter is IRC. It runs as a host-supervised Node.js worker
that connects to a configured server/channel, responds to `PING`, parses
`PRIVMSG`, and wakes the conversation when the trigger policy matches. The
recommended trigger is `mention`, which only wakes the conversation when a
channel message mentions the adapter nick. `all_messages` is available for
quieter channels.

### WhatsApp

Exoclaw also includes an experimental WhatsApp adapter using Baileys. The Rust
adapter runtime owns supervision, durable events, conversation wakeups, and
outbox draining; workers own protocol-specific sockets and parsing. When first
run, the WhatsApp worker emits a QR pairing event into adapter history and logs;
after pairing, Baileys auth state is stored under
`.exo/adapters/whatsapp/<adapter-id>/auth` by default.

### Signal

The Signal adapter uses `signal-cli` as a linked device. If its `account` config
is null, the worker starts `signal-cli link`, logs a QR code for the phone's
linked-device flow, discovers the linked account with `signal-cli listAccounts`,
then runs `signal-cli -a <account> jsonRpc`. Outbound DM targets should be
Signal usernames with the `u:` prefix, such as `u:example.01`, unless an inbound
wakeup provides a more precise target.

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
examples/exoclaw/scripts/exoclaw-control --pull-sandbox
```

If you want a conversation to have its own sandbox, use `sandboxScope: "conversation"`:

```bash
examples/exoclaw/scripts/exoclaw-control --conversation isolated-dev --sandbox-scope conversation
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
