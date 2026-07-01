---
title: Home
layout: home
nav_order: 1
description: "Exo is a minimal, durable substrate for building AI agents."
permalink: /
---

# exo
{: .fs-9 }

A minimal, durable substrate for building AI agents — separating trusted
infrastructure (state, history, secrets, sandboxes) from agent-specific policy.
{: .fs-6 .fw-300 }

[Get started]({% link getting-started/index.md %}){: .btn .btn-primary .fs-5 .mb-4 .mb-md-0 .mr-2 }
[View on GitHub](https://github.com/ankrgyl/exo){: .btn .fs-5 .mb-4 .mb-md-0 }

---

Exo splits an agent into two halves:

- **exoharness** — the durable, trusted substrate. Owns identity (agents,
  conversations, turns), the append-only event log, artifacts, secrets, and
  sandbox lifecycle.
- **executor** — the ephemeral, swappable policy layer. Owns prompt assembly,
  model calls, tool dispatch, memory, and the turn loop.

Because the substrate doesn't depend on the executor, agents built on exo can
**fork or rewind** to any past event, **swap executors** (Codex, Claude Code,
Cursor SDK, or your own), and **evolve safely** — modifying their own tools,
prompts, and harness code without losing critical state.

{: .warning }
Exo is early software. The public API should be treated as unstable.
