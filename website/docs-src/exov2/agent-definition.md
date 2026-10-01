---
title: Agent definition
description: Define an agent's harness, instructions, tools, and resources, then run it locally or with a provider.
---

# Agent definition

Exo allows you to build agents on top of the exoharness that you can self-host or run in provider managed infrastructure. It supports popular harnesses
like Codex, Claude Code, Pi, and custom harnesses, while letting you define the agent's behavior, environment, code access, MCP configuration, and more.

## Quickstart

Define an agent in a Markdown file. This is the support agent used in the [tutorial](./tutorial):

```markdown
---
name: support-analyst
harness: codex
model:
  name: gpt-6-sol
  credential: openai
---

For each support ticket, determine whether it describes a technical issue.
If it does, try to reproduce it, explain what you found, and suggest a workaround.
```

With `OPENAI_API_KEY` set, save the credential, create the agent, and run it:

```bash
exo vault secret create global --preset openai
exo agent create support-analyst --file support-analyst.md
exo agent run --agent support-analyst \
  --prompt "Triage this ticket: CSV uploads return HTTP 500 after 30 seconds."
```

To serve the agent over HTTP, run:

```bash
exo serve --agent support-analyst --bind 127.0.0.1:8080
```

In another terminal, use the HTTP API. Creating a thread starts a new
conversation; posting to that thread starts a turn:

```bash
BASE=http://127.0.0.1:8080/exo
AGENT_ID=$(curl -fsS "$BASE/agent" | jq -r '.agents[0].id')
THREAD_ID=$(curl -fsS -X POST "$BASE/agent/$AGENT_ID/thread" \
  -H 'Content-Type: application/json' -d '{}' | jq -r '.thread.id')
curl -fsS -X POST "$BASE/agent/$AGENT_ID/thread/$THREAD_ID/turn" \
  -H 'Content-Type: application/json' \
  -d '{"input":{"role":"user","content":"Triage this ticket: CSV uploads return HTTP 500 after 30 seconds."}}'
curl -N "$BASE/agent/$AGENT_ID/thread/$THREAD_ID/event/watch"
```

The turn request returns a receipt; the event stream shows the agent's response.
Post to the same `/turn` URL for another turn, or create another thread to start
a separate conversation. This example uses `jq` to read the IDs from the API.

There’s a lot more you can configure, including auth, sandboxes, policies, and more. We’ll get into all of that in the sections that follow.

## Core components

An **agent** is a configurable bundle of a harness, model, instructions, and tools. A **thread** is an instance of that agent, containing its conversation history and execution state.

A **session** represents a client interacting with an agent within a thread and can span multiple **turns**. A turn begins with submitted input and includes the agent’s work in response. A thread can continue across multiple sessions.

Each thread uses an **environment** (sandbox and network configuration) and has access to selected **vaults** (named collections of secrets). Credential bindings determine where those secrets can be used.

### Agent

An **agent** is defined by:

- **Instructions:** free-form text that gets included in the system prompt
- **Harness:** implements an interface that can accept messages and produce events, with its own configuration (eg. model)
- **MCP servers:** tools and data exposed through MCP
- **Adapters**: integrations with chat providers like Slack, WhatsApp, and Discord
- **Custom tools:** additional tools executed by a worker running in the environment (if registered) or the client.
- **Resources:** give the agent access to data (volumes, Github, and others to follow) in an efficient way. Each thread gets its own isolated copy, so edits are localized to the thread.

This configuration is just data that is commonly embedded in the frontmatter of a markdown file.

#### Planned additions

- [ ] Skills in agent definitions
- [ ] Memory store

<!-- TODO: Reconcile planned additions with GitHub issues before publishing. -->

### Environment

Agents benefit from a sandboxed **environment** they can work in to read files, write/run code, and accumulate scratch work. Exo uses SmolVM by default. It also supports Apple Containers on macOS, Docker, Firecracker (via Lima on macOS), and sandbox providers including AWS, Daytona, E2B, Sprites, and Vercel.

Environments can also configure credential substitution rules: agents get placeholder credentials like `OPENAI_API_KEY=exo_egress_d81f72ff9a6b4ff2bd4e1bae63c207b0`, which get transparently substituted outside of the sandbox.

- [ ] Support remote outbound connections, so that you can have a local environment that receives commands from a remote agent server

### Vault

A **vault** is a named collection of secrets, such as API keys and OAuth credentials, stored encrypted at rest. Each secret has a name and can be associated with an MCP server or HTTP endpoint. Threads inherit the global vault, vaults attached to their agent, and any explicitly attached vaults. When multiple vaults contain a matching credential, the more specifically attached vault takes precedence.

## Providers

A **provider** executes an agent on top of an **exoharness** which manages thread state, environments, vaults, and integrations. Exo comes with a built-in provider that you can run locally, as well as an http-based client that works with remote providers (like Braintrust).

To use a provider, run

```bash
exo provider create braintrust --url https://api.braintrust.dev/exo
exo provider switch braintrust
```

Commands like `exo agent create` will automatically run against the provider.

## Future roadmap

- [ ] Scheduled deployments (eg CMA).
