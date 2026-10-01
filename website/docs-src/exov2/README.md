---
title: README
description: An open-source runtime for self-improving agents
---

# Exo

Exo is an open-source runtime for agents. Harnesses like Codex and Claude Code are world-class at wielding their respective models, but hosting them requires significant custom engineering. The key insight behind Exo is that you can separate a harness's semantics (e.g., compaction, tool calling, and planning) from the infrastructure layer, making it possible to host agents powered by the harness of your choice.

Exo supports popular harnesses like Codex, Claude Code, and Pi, as well as your own custom harness, with a common set of infrastructure primitives: durable conversations, sandboxes, credentials, and network controls. You can think of Exo as an open-source alternative to Claude Managed Agents or OpenAI's Agents API, but one that's harness, model, and vendor agnostic and self-hostable (which, in turn, lets you maintain ZDR).

Exo's architecture enables use cases like:

- Hosting agents powered by the harness of your choice, switching between them when you wish
- Implementing security best practices like credential substitution and granular network policies, so your agent can run with minimal supervision
- Automatic self-improvement by giving an agent access to its own history and code, allowing it to inspect failures, revise its prompts, tools, or harness, and test the results
