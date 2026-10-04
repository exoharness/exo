---
title: Exo
description: An open-source runtime for self-improving agents
---

# Exo

Exo is an open-source runtime for agents. Harnesses like Codex and Claude Code are world-class at wielding their respective models,
but hosting them requires significant custom engineering.

Exo allows you to define an agent with a markdown file and host it behind an API with durable conversations, sandboxes, credentials,
and network controls. You can think of Exo as an open-source alternative to Claude Managed Agents or OpenAI's Agents API, but one
that's harness, model, and vendor agnostic and self-hostable (enabling ZDR).

## Installation

### TODO

## Quickstart

Define an agent in a markdown file like `support-analyst.md`.

```markdown
---
name: support-analyst
harness: codex
model:
  name: gpt-6.1-sol
  credential: openai
---

For each support ticket, determine whether it describes a technical
issue. If it does, try to reproduce it, explain what you found, and
suggest a workaround.
```

Then, set `OPENAI_API_KEY` in your environment and wire the credential to exo:

```bash
exo vault secret create global --preset openai
```

Now, you're ready to run your agent:

```bash
exo agent run --agent-file support-analyst.md \
    --prompt "Triage: CSV uploads return HTTP 500 after 30 seconds"
```

If you omit `--prompt ...` you'll be dropped into a REPL instead. There's a lot more you can configure,
including the environment the agent runs in, access to MCPs, code, and hosting the agent. These topics
are covered in the [docs](/docs/).

## Architecture

The key insight behind Exo is that you can separate a harness's semantics (e.g., compaction, tool calling, and planning) from the
infrastructure layer, making it possible to host agents powered by the harness of your choice. Exo supports Codex, Claude Code,
and Pi out of the box, and you can extend it to plug in your own. This architecture enables:

- Hosting agents powered by the harness of your choice, switching between them when you wish.
- Credential substitution and granular network policies, so your agent can run with minimal supervision and still access protected resources.
- Automatic self-improvement by letting an agent safely access its own history and code, allowing it to inspect failures, revise its prompts/tools/harness, and test results.

## Comparisons

Let's start with [OpenAI’s Agents API](https://developers.openai.com/api/docs/guides/agents-api/overview) and [Claude Managed Agents](https://platform.claude.com/docs/en/managed-agents/overview).
These products are great because they are extremely easy to use and you can count on the fact that they will wield their labs' models the best. On the other hand, they
lock you into the ecosystem and store your conversational history data, deepening your reliance on the models themselves. Exo aspires to a similar developer experience,
while allowing you to switch between models and harnesses without changing a line of code. In addition to model neutrality, this approach gives you a clean pathway to explore
post training models without having to rewrite your agent.

[Pi](//github.com/earendil-works/pi) and more recently [Pi Durable](https://github.com/earendil-works/pi/tree/main/packages/durable) are more conceptually aligned with Exo. Pi is open source
and offers tools for durable execution and easy self-hosting. The primary difference is that Pi includes the agent loop as well. The default is a minimal,
model-agnostic implementation, which you can extend to eg. optimize how context is managed. Exo, on the other hand, has zero opinions about how to manage
context or run LLMs and delegates this to the harness. Practically speaking, if you _want_ to write code and customize your agent, Pi is a great fit. If you
prefer to delegate this to a preexisting harness, and focus on the other aspects of agent building (the environment the agent operates in, its tools, application
experience, etc.) then Exo could be a better fit. You can also run Pi (and potentially in the future, Pi Durable) directly on top of Exo, and take advantage of features
like Vaults, Environments, and Adapters.
