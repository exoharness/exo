---
title: Sandboxes
parent: Concepts
nav_order: 4
---

# Sandboxes

Agents benefit from being able to write and run commands in secure
environments. The exoharness runs sandboxes with pluggable backends and
gives agents lifecycle control — `create`, `start`, `stop`, `snapshot` —
plus arbitrary command execution inside them.

## Backends

Local backends (selected with `--sandbox-backend` / `EXO_SANDBOX_BACKEND`):

| Backend | Isolation |
|:--------|:----------|
| `docker` | Container |
| `apple-container` | Container (macOS) |
| `local-process` | None — commands run on the host |

Remote providers, configured as bindings with
`exo provider configure --provider <name> --secret <secret>`:

- `daytona`
- `e2b`
- `vercel`
- `sprites`
- `aws-agentcore`

Remote providers let the sandbox outlive the machine running the executor,
which matters for long-running agents.

## Scope

A sandbox can be scoped to a **conversation** (each conversation gets its
own) or to an **agent** (shared across the agent's conversations) — set at
creation with `conversation create --sandbox-scope <agent|conversation>`.
Long-running personal agents like Exoclaw typically use an agent-scoped
sandbox so installed tools persist across conversations.

## Snapshots

Snapshotting a sandbox writes a snapshot id to the event log, tying
filesystem state to conversation history. That's what lets
[time travel]({% link concepts/time-travel.md %}) restore not just what
was said, but the environment the agent was working in.

## Secrets in sandboxes

Secrets can be securely mounted into sandboxes so programs inside can use
them **without the LLM being able to view or expose them** — see
[Bindings & Secrets]({% link concepts/bindings-and-secrets.md %}).
