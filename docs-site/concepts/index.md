---
title: Concepts
nav_order: 3
has_children: true
permalink: /concepts/
---

# Concepts

Exo's core idea is a separation of concerns most agent systems conflate:
the **trusted infrastructure** an agent runs on, and the **semantics** of
how the agent thinks and acts.

![Exo architecture overview]({{ site.baseurl }}/assets/images/architecture-overview.svg)

Four terms cover the whole model:

| Term | What it is |
|:-----|:-----------|
| **exoharness** | The durable, trusted substrate. Owns conversations, the append-only event log, artifacts, bindings, secrets, and sandboxes. Stateful; never loses your agent. |
| **executor** | The policy layer. Owns prompt assembly, model calls, tool dispatch, memory/compaction, approvals — every *semantic* decision. Ephemeral and swappable. |
| **harness** | An executor running on top of an exoharness. This is what you actually use. |
| **agent** | The high-level behavior an application implements: instructions, security policies, tools, and shared configuration. |

The rule of thumb for what goes where: any decision that affects what an
agent *means* or *does* belongs in the executor (or the agent itself). The
exoharness provides the durable building blocks those decisions run on —
history, state, secrets, and sandboxing.

Read on:

- [Exoharness & Executor]({% link concepts/exoharness-and-executor.md %}) —
  why the split exists and what it buys you.
- [Data Model]({% link concepts/data-model.md %}) — agents, conversations,
  sessions, turns, events, and artifacts.
- [Time Travel]({% link concepts/time-travel.md %}) — fork and rewind from
  any point in the event log.
- [Sandboxes]({% link concepts/sandboxes.md %}) — isolated execution with
  pluggable local and remote backends.
- [Bindings & Secrets]({% link concepts/bindings-and-secrets.md %}) — how
  credentials stay out of the model's reach.
- [Executors & Harnesses]({% link concepts/executors.md %}) — the built-in
  executor runtimes, from `basic` to Codex, Claude Code, and Cursor.
- [Adapters]({% link concepts/adapters.md %}) — long-running connections to
  external channels like IRC, WhatsApp, Signal, and Discord.

The canonical reference is
[`spec/exoharness.md`](https://github.com/ankrgyl/exo/blob/main/spec/exoharness.md);
these pages are a guided tour of the same material.
