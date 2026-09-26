---
title: Using the CLI Directly
description: Store a secret, configure an agent, and start chatting.
---

# Using the CLI Directly

The [setup script](./installation) does all of this for you. Use this
page when you've [installed just the CLI](./installation#installing-just-the-cli)
and want a bare agent — none of the canonical agent's tools or adapters — or want to understand the
primitives the setup script drives.

## 1. Store a secret

```bash
exo vault secret create global openai --token-env OPENAI_API_KEY --http-origin https://api.openai.com
```

This stores your API key in the global vault, encrypted using the configured secret backend.

::: info
  `--token-env` takes the environment variable name; the CLI reads its value.
:::

## 2. Configure an agent and chat

The spec selects the upstream model and a secret from its selected vaults.
Set `config.base_url` for a custom endpoint. Model names starting with `claude`
use the Anthropic API.

```bash
cat > assistant.md <<'EOF'
---
name: "assistant"
harness: basic
config:
  model: gpt-5.5
  credential: openai
---
Help the user with their task.
EOF
exo agent create assistant --file assistant.md
exo agent run --agent assistant
```

Each `exo agent run --agent assistant` invocation starts a new saved thread.
Use `--agent-file agent.md` to create or update a saved agent from a Markdown file
and start a saved thread. Rerunning the same file reuses the agent; add
`--thread <slug>` to resume a thread.
See [A Sandboxed Conversation](./sandboxed-conversation) to configure a sandbox.

Chat runs inline, keeping your terminal scrollback available. Use `exo agent run --agent assistant --tui`
to opt into the full-screen interface.

While a turn runs, a spinner shows whether the agent is waiting for the model,
thinking, or running a tool. Each response ends with time to first token, duration,
token throughput, token counts, and cost. Totals include earlier turns in the
conversation. Usage and cost are shown as unavailable when the harness or pricing
table does not supply them. Press Ctrl+C to interrupt a turn and return to the prompt.

Messages from other clients appear when you submit your next line; press Enter on
an empty line to check for updates.

## Where state lives

The CLI stores everything — agents, conversations, the event log, secrets —
under `.exo` in the current directory by default. Pass `--root <path>` to
use a different state directory.

Because all conversation state is durable and owned by the exoharness, you
can quit the REPL and resume the same conversation later:

```bash
exo agent run --agent assistant --thread <slug>
```

Use `exo thread list assistant` to find the slug, and
`exo thread events assistant <slug>` to inspect the raw event log.
