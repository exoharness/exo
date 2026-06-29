# exo

Exo is a systems approach to recursive self improvement. In short, it's a
complete agent harnes that has support for tools, tasks, adapters (e.g.
WhatsApp, Discord or Slack), full computer use, and state management (snapshot,
clone, migrate, rewind). But most importantly it has full visbility of both its
code and runtime logs and can incrementally improve every aspect of itself.

While most agents can do some form of self improvement, such as evolve their
prompts or add tools, Exo is fully recursive in that can clone or operate on any
aspect of itself, from prompts, to memory, to tooling, to the basic harness
itself. And it's architected so that this evolution can be done incrementally
and (mostly) safely. The only thing it can't muck with is an event log which
provides canonical history.

The goal is to give an agent maximum power anbd flexibility to improve itself.
Or customize itself for whatever purpose. For example an Exo agent can cost optimize
itself, build custom tools, or even evolve itself to learn to play a game:

![Exo playinb pokemon go](docs/images/exo_playing.gif)

## Quick Start

Exo was designed to be incredibly simple to use. With just a few commands you
should have a fully functional agent who can do standard agent tasks (computer
use, research, coding etc.) but can also extent itself as needed.

To use Exo as an agent, you'll ned an OpenAI API key. If you have that, simply do the following:

```
curl -fsSL https://raw.githubusercontent.com/ankrgyl/exo/main/public/setup.sh -o setup.sh
bash setup.sh
```

_Note that Exo requires git, cargo, pnpm, and Docker_

It'll ask for the API key and your name and your agent's name. Once you enter
these, the setup will start the agent. It will also print an URL of the form.

```
https://exoharness.ai/chat?role=user&c=...#k=...
```

This is a minimal remote chat interface to your agent you can access from anywhere.
Open that URL in your browser or on your phone.

When complete, the script will drop you to a prompt you can use to talk to your
agent locally.

A good end to end test is to have it install a tool in the sanbox and use it with the task scheduler. For example,
try the following prompt:

```
Install python3 and curl in the sandbox. You don't need sudo, just use apt-get. Once you've done that, please
schedule a task to run every minute that grabs news headlines from the BBC RSS feed. Only print new headlines you've
not printed before. Please print them here.
```

## Exo Basics

### Key Concepts

#### Exo source code

In the canonical Exoclaw setup, the running source tree is mounted into the
agent sandbox at `/workspace/exo`. This lets the agent inspect its own harness,
prompts, tools, adapters, scheduler, and startup scripts, and propose or make
changes to them.

#### Canonical state

Exo stores conversation history, tool activity, host lifecycle events, adapter
events, artifacts, and sandbox records outside the sandbox filesystem. That
durable history is not rewound when the sandbox is rewound, so the agent can
reconstruct what happened across restarts, rebuilds, and experiments.

#### Sandbox

Canonical Exoclaw conversations use a shared agent-scoped sandbox by default.
The agent can run shell commands there, install tools, inspect snapshots, create
new snapshots, and rewind the sandbox when it needs to back out risky changes.

#### Guardian

The guardian is a host-side control surface for maintenance that should happen
outside the sandbox. The agent can call it through `guardian_action` to build
Exo, inspect service status and logs, and restart the scheduler or adapter
runners while preserving `.exo` state.

#### Tools

Tools are functions the model can call during a turn. Core tools expose shell
access and agent-created tool installation; Exoclaw adds tools for adapters,
scheduling, sandbox snapshots, memory, introspection, and guardian maintenance.
Tool definitions are registered each model round, so the agent sees the current
tool list as part of the model request.

#### Adapters

Adapters are long-running host processes that connect an agent conversation to
external surfaces such as ExoChat, IRC, WhatsApp, Signal, Discord, or a local
shell CLI. They own protocol sockets, reconnect behavior, inbound event history,
conversation wakeups, and outbound sends. The canonical setup starts ExoChat by
default and prints a browser URL for it.

#### Scheduler

Exo also includes a task scheduling process that manages recurring sandbox work
(for example, once an hour). The agent can create, list, cancel, and delete
scheduled tasks, and each completed run can wake the conversation with a compact
result.

#### Memory

Exoclaw includes `remember` and `forget` tools for durable agent memory. Saved
memory is stored outside the sandbox and injected back into future turns across
conversations.

### Tools

Exo has the following minimal set of tools to control and interact with its environme and to evolve itself.

#### Core

- host control : `shell`
- Tool management :`install_agent_tool`, `uninstall_agent_tool`

#### Agent

- Adapter tools: `create_adapter`, `list_adapters`, `disable_adapter`,
  `delete_adapter`, `send_adapter_message`.
- Adapter/event introspection: `list_adapter_events`,
  `list_conversation_events`.
- Scheduler tools: `schedule_sandbox_task`, `list_scheduled_tasks`,
  `cancel_scheduled_task`, `delete_scheduled_task`.
- Sandbox tools: `list_sandbox_snapshots`, `snapshot_sandbox`,
  `rewind_sandbox`.
- Self-maintenance: `guardian_action`.
- Memory: `remember`, `forget`.

### Adapters

Supported adapters:

- `exochat`: a hosted, text-only browser chat at `https://exoharness.ai`.
- `irc`: an IRC channel adapter for lightweight text chat.
- `whatsapp`: a WhatsApp linked-device adapter using Baileys.
- `signal`: a Signal linked-device adapter using `signal-cli`.
- `discord`: a Discord bot adapter with message and attachment support.
- `agent-cli`: a local shell adapter for sending prompts from any directory.

## Minimum Viable Exo

When you run the startup.sh script, it creates what we believe to be a minimum viable Exo agent that has just enough functionality to evolve itself to whatever you want. It has all the core and agent tools. And `exochat` as the single adapter so you can chat with your agent from anywhere on the Internet. It also has a vanilla ubuntu sandbox.

## License

MIT
