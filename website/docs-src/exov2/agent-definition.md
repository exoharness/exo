---
title: Agent definition
description: TODO
---

# Agent definition

Exo allows you to build agents on top of the exoharness that you can self-host or run in provider managed infrastructure. It supports popular harnesses
like Codex, Claude Code, Pi, and custom harnesses, while letting you define the agent's behavior, environment, code access, MCP configuration, and more.

## Quickstart

Define the agent in a markdown file:

```python
---
name: support-analyst
harness: codex
config:
  model: gpt-5.6-sol
mcp_servers:
  - type: url
    name: helpdesk
    url: https://helpdesk.example.com/mcp
---

You investigate support tickets and identify recurring product problems.
Read the available evidence, cite the tickets behind each finding, and
write your report to a file.
```
(TODO: Remove in favor of tutorial below, or use a better example, or make it the same as the Tutorial V1)

Then, create the agent and you can start to use it!

```
exo agent create support-analyst --file support-analyst.md
exo chat --agent support-analyst

> Can you triage ticket #124?
→ Calling helpdesk.load_ticket(ticket_id: 124)

Ticket #124 is urgent. The customer is unable to access...
```

To serve the agent so you can connect to it remotely over HTTP, run

```bash
exo serve support-analyst
```

and then interact with it

```bash
curl -N http://127.0.0.1:8080/turn \
  -H 'Content-Type: application/json' \
  -d '{
    "input": [
      {"role": "user", "content": "What should we fix first?"}
    ]
  }'
```
(TODO: This needs to be updated with the agent id? or name?)

There’s a lot more you can configure, including auth, sandboxes, policies, and more. We’ll get into all of that in the sections that follow.

## Core components

An **agent** is a configurable bundle of a harness, model, instructions, and tools. A **thread** is an instance of that agent, containing its conversation history and execution state.

A **session** represents a client interacting with an agent within a thread and can span multiple **turns**. A turn begins with submitted input and includes the agent’s work in response. A thread can continue across multiple sessions.

Each thread uses an **environment** (sandbox and network configuration) and has access to selected **vaults** (named collections of secrets). Credential bindings determine where those secrets can be used.

### Agent

An **agent** is defined by:

- **Instructions:** free-form text that gets included in the system prompt
- **Harness:** implements an interface that can accept messages and produce events, with its own configuration (eg. model)
- **MCP: a set of** MCP servers
- **Skills:** agent skills
- **Adapters**: integrations with chat providers like Slack, WhatsApp, and Discord
- **Custom tools:** additional tools executed by a worker running in the environment (if registered) or the client.
- **Resources:** give the agent access to data (volumes, Github, and others to follow) in an efficient way. Each thread gets its own isolated copy, so edits are localized to the thread.

This configuration is just data that is commonly embedded in the frontmatter of a markdown file.

#### TODO

- [ ]  Skillls
- [ ]  Custom tools
- [ ]  Adapters
- [ ]  Memory store

(TODO: we should reconcile this with github issues and link to an issue for each)

### Environment

Agents benefit from a sandboxed **environment** they can work in to read files, write/run code, and accumulate scratch work. By default, Exo uses Apple Containers on Mac OS X and Docker on Linux. It also supports Firecracker (via Lima on Mac OS X) and sandbox providers including AWS, Daytona, E2B, Sprites, and Vercel.

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
