# exo

Exo is a systems approach to recursive self improvement. In short, it's a
complete agent harnes that has support for tools, tasks, integrations (e.g.
WhatsApp, Discord or Slack), and general computer use. But most importantly it
has full visbility of both its code and runtime logs and can incrementally
improve every aspect of itself, clone itself, and even manage a lineage of
clones.

While most agents can do some form of self improvement, such as evolve their
prompts or add tools, Exo is fully recursive in that can clone or operate on any
aspect of itself, from prompts, to memory, to tooling, to the basic harness
itself. And it's architected so that this evolution can be done incrementally
and (mostly) safely. The only thing it can't muck with is an event log which
provides canonical history of what it's tried to prevent getting stuck in
recursive loops.

The goal of Exo is to be the minimal framework possible to give an agent full
ability for recursive self improvmenet. Why would you want such a thing?

- It's a great agent framework to build exactly the agent you like that's
  maximally bitter lesson aligned. Future smarter models can evolve every aspect
  of the system. And do it safely with full history.
- It'a s great framework to allow AI models to solve complex problems by
  iterating on system level properties. We've have agents learn to play games,
  cost optimize themselves, build complex systems. In each case, it required the
  agents to modify themselves heavily.

In short, we think this is the best way to take advantage of the growing power
of AI models when building long lived agents.

<!-- ![Exo playinb pokemon go](docs/images/exo_playing.gif) -->

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

A good end to end test is to have it install a tool in the sandbox and use it with the task scheduler. For example,
try the following prompt:

```
Install python3 and curl in the sandbox. You don't need sudo, just use apt-get. Once you've done that, please
schedule a task to run every minute that grabs news headlines from the BBC RSS feed. Only print new headlines you've
not printed before. Please print them here.
```

## Understanding Exo

There are only a few key components you need to know about to understand how
Exo works. You can use Exo like any agent without understanding these internals.
But having a basic idea will help you more effectively guide Exo if you want it
to evolve itself. For a deeper dive into these concepts, see
[`docs/EXO-BASICS.md`](docs/EXO-BASICS.md).

**Basic Loop** Exo runs a host-side loop that receives user messages and adapter
events, builds the model context, exposes the active tools, executes tool calls,
and records the results. This loop runs outside the sandbox.

**Sandbox** Exo uses a vanilla unbuntu sandbox where
it can install packages, run commands, and experiment. It can snapshot and
rewind that sandbox when it needs to back out changes.

**Tools and Adapters** These are the core methods for Exo to interact with the
world and with itself. Tools are functions the model can call, such as executing
a shell command with the `shell` tool. Adapters are long-running host processes
for stateful external channels such as ExoChat, IRC, WhatsApp, Signal, and
Discord.

**Canonical State** Exo stores durable conversation history, tool activity,
adapter events, artifacts, and sandbox records outside the sandbox filesystem.
This state is not rewound when the sandbox is rewound, so the agent can
reconstruct what happened across experiments, restarts, and rebuilds.

**Source Code** Exo's source code is mounted in the
sandbox at `/workspace/exo`. The agent can read and modify that code and has
tools that allow it to rebuild and restart itself and all components. This allows Exo to be able to modify every aspect of itself.

**REPL and ExoChat** The minimal setup gives you two ways to talk to Exo: the
local REPL, which is a command-line chat interface, and ExoChat, a simple
text-only web chat hosted at `https://exoharness.ai`.

## Where to Go From Here

If you have your agent up and running, there really is little else you need to understand or do other than talk to it and ask it to evolve itself in a direction that you want. It already contains basic support for services such as WhatsApp, Slack, Discord, Signal, and IRC. But it's very easy to extend it to support more things

## Shortcomings

While there are many, the most obvious is that right now there isn't a simple way for Exo to do generalized computer use of a windowed system. This is in the works and should land shortly. But in the meantime, you can get a long way by asking Exo the build such a thing for itself.

## Tweaking Prompts

There are a numebr of prompt files that Exo uses during runtime. You can edit these directly or ask Exo to.

- `examples/exoclaw/prompts/me.md`: the committed core identity and operating
  rules for the default Exoclaw agent.
- `.exo/exoclaw-profile.md`: local, git-ignored profile instructions such as
  your name and machine-specific preferences. Create or update it with
  `scripts/exo.sh setup-profile`.
- `examples/exoclaw/harness.ts`: assembles the full prompt sent each turn,
  including dynamic instructions about tools, adapters, memory, sandbox behavior,
  and self-maintenance.

After changing prompt files, ask Exo to rebuild/restart itself for them to go in
use.

## License

MIT
