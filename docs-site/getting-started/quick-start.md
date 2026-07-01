---
title: Quick Start
parent: Getting Started
nav_order: 2
---

# Quick Start

Register a model, then drop into a REPL:

```bash
./target/debug/exo secret set openai --env OPENAI_API_KEY
./target/debug/exo model register gpt-5.5 --secret openai
./target/debug/exo repl
```

`exo repl` reuses or creates a default agent and conversation, so you can start
chatting in one command. It's a plain chat with no shell sandbox; create a
conversation explicitly when you want tools.

{: .note }
`--env` takes the variable name literally. Use `--value "$OPENAI_API_KEY"` if
you intentionally want the shell to expand the value.

For explicit control over agents, conversations, or a shell-enabled sandbox:

```bash
./target/debug/exo agent create --model gpt-5.5 "Sandbox Example"
./target/debug/exo conversation create sandbox-example "Local Dev"
./target/debug/exo repl --agent sandbox-example --conversation local-dev
```

The CLI stores state under `.exo` by default. Pass `--root <path>` to use a
different state directory.
