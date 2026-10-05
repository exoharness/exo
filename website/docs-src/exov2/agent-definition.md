---
title: Agent definition
description: Define an agent's harness, instructions, tools, and resources, then run it locally or with a provider.
---

# Agent definition

An **agent** is a configurable bundle of system instructions, [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) servers, custom tools, resources (like
git repositories), and defaults (harness, model).

A **thread** is an instance of that agent, containing its conversation history, execution state, filesystem state, and configuration
(harness, model, environment, vaults).

A **session** represents a client’s interaction with an agent within a thread and can span multiple **turns**. A turn begins
with submitted input and includes the agent’s work in response. A thread or even a turn can continue across multiple sessions.

Each thread runs in an **environment** (sandbox and network configuration) and has access to selected **vaults** (named collections
of secrets). The environment can restrict which hosts and ports the sandbox can reach, and each secret can restrict which origins
or URLs it can be used with.

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

To serve your agent over HTTP, run:

```bash
exo serve --bind 127.0.0.1:8080
```

In another terminal, send a turn to the agent by its slug:

```bash
curl -N http://127.0.0.1:8080/exo/support-analyst/turn \
  -H 'Content-Type: application/json' \
  -d '{"input":"Triage this ticket: CSV uploads return HTTP 500 after 30 seconds."}'
```

There’s a lot more you can configure, including auth, sandboxes, policies, and more. We’ll get into all of that in the sections that follow.

## Future roadmap

- [ ] Scheduled deployments (eg CMA).
