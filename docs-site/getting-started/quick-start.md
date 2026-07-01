---
title: Quick Start
parent: Getting Started
nav_order: 2
---

# Quick Start

Three commands take you from a fresh build to a chat with an agent: store a
secret, register a model binding that uses it, and start the REPL.

## 1. Store a secret

```bash
./target/debug/exo secret set openai --env OPENAI_API_KEY
```

This stores your API key in exo's secret store (file-backed by default,
Apple Keychain also supported via `--secret-backend`).

{: .note }
`--env` takes the variable *name* literally and reads it at use time. Use
`--value "$OPENAI_API_KEY"` if you intentionally want the shell to expand
the value and store it.

## 2. Register a model

```bash
./target/debug/exo model register gpt-5.5 --secret openai
```

This writes a *model binding*: a named model plus the secret it
authenticates with. Use `--base-url` to target any OpenAI-compatible
endpoint. Model names starting with `claude` use the Anthropic API.

## 3. Start the REPL

```bash
./target/debug/exo repl
```

`exo repl` reuses or creates a default agent and conversation and uses the
first registered model (override with `--model`), so you can start chatting
in one command. It's a plain chat with no shell sandbox; see
[A Sandboxed Conversation]({% link getting-started/sandboxed-conversation.md %})
when you want tools.

## Where state lives

The CLI stores everything — agents, conversations, the event log, secrets —
under `.exo` in the current directory by default. Pass `--root <path>` to
use a different state directory.

Because all conversation state is durable and owned by the exoharness, you
can quit the REPL and resume the same conversation later:

```bash
./target/debug/exo repl --agent repl --conversation <slug>
```

Use `./target/debug/exo conversation list` to find the slug, and
`conversation events <slug>` to inspect the raw event log.
