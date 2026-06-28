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
curl -fsSL https://raw.githubusercontent.com/61cygni/exo/main/public/setup.sh -o setup.sh
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

## Using Exo

The setup script above creates a minima Exo agent you can chat with directly on
the command line or via ExoChat. Once it's running you can have it start to build
itself for whatever use you have in mind. For example, try telling it to do the following:

```
Install python3 and curl in the sandbox. You don't need sudo, just use apt-get. Once you've done that, please
schedule a task to run every minute that grabs news headlines from the BBC RSS feed. Only print new headlines you've
not printed before. Please print them here.
```

## Architecture

TODO

## License

MIT
