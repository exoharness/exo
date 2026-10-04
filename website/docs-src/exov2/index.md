---
title: Exo
description: An open source runtime for self-improving agents
---

# Exo

Exo is an open source runtime for agents. Harnesses like Codex and Claude Code are world-class at wielding their respective models,
but hosting them requires significant custom engineering.

Exo allows you to define an agent with a markdown file and host it behind an API with durable conversations, sandboxes, credentials,
and network controls. You can think of Exo as an open source alternative to Claude Managed Agents or OpenAI's Agents API, but one
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
are covered in the [docs](../index.md).

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
lock you into their ecosystem and store your conversational history data, deepening your reliance on the models themselves. Exo aspires to a similar developer experience,
while allowing you to switch between models and harnesses without changing your application code. In addition to model neutrality, this approach gives you a clean pathway to explore
post training models without having to rewrite your agent.

[Pi](https://github.com/earendil-works/pi) is open source and can run anywhere, but includes its own minimal, extensible agent loop. Exo, on the other hand, has no opinions
about how to manage context or run LLMs and delegates this to the harness. Practically speaking, if you _want_ to write code to customize your agent, Pi is a great fit.
Like Codex and Claude Code, Pi has first-class support in Exo. [Pi Durable](https://github.com/earendil-works/pi/tree/main/packages/durable) adds Exo-like capabilities
including persistent conversations and resumable tasks to Pi. From an architecture standpoint, the primary difference between Pi Durable and Exo is that Exo is harness
agnostic. There are real trade-offs to that, for example, there is a lot of code in Exo that models the event lifecycle of Codex and Claude Code that Pi does not need to have.
On the other hand, Exo is fundamentally resilient to the co-evolution of models and their proprietary harnesses, and it allows you to use labs' harnesses directly to build
custom applications.

[Omnigent](https://omnigent.ai/) is probably the most similar to Exo, as it's open, model-agnostic, and stateful. Exo is a bit more focused on building custom agents than
running eg. as a personal coding agent replacement, although technically speaking, most use cases work in both Exo and Omnigent. For example, Exo
allows you to create per-user vaults and attach them to arbitrary threads, whereas Omnigent associates a thread's credentials with its owner.
