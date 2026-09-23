---
title: Sandboxes
description: Isolated execution with pluggable local and remote backends.
---

# Sandboxes

Agents benefit from being able to write and run commands in secure
environments. The exoharness runs sandboxes with pluggable backends and
gives agents lifecycle control — `create`, `start`, `stop`, `snapshot` —
plus arbitrary command execution inside them.

## Backends

| Backend | Type | Notes |
|:--------|:-----|:------|
| `docker` | Local | Container; the default |
| `apple-container` | Local | Container (macOS) |
| `local-process` | Local | No isolation — commands run on the host |
| `daytona` | Remote | [daytona.io](https://www.daytona.io) |
| `e2b` | Remote | [e2b.dev](https://e2b.dev) |
| `runta` | Remote | [Runta](https://runta.com) |
| `sprites` | Remote | [sprites.dev](https://sprites.dev) |
| `vercel` | Remote | [Vercel Sandbox](https://vercel.com/docs/vercel-sandbox) |
| `aws-agentcore` | Remote | [Amazon Bedrock AgentCore](https://aws.amazon.com/bedrock/agentcore/) |

Select a provider with `--provider`; local providers need no credentials:

```bash
exo agent create --model <model> --provider docker "My Agent"
```

**Remote** backends are configured as bindings and need an API key:

```bash
exo secret set <name> --env <PROVIDER_API_KEY>
exo provider configure --provider <name> --secret <name>
```

Remote backends run the sandbox on hosted infrastructure instead of your
machine, so it keeps running even when your local process stops.

### Runta

Store your Runta token and configure the provider:

```bash
exo secret set runta --env RUNTA_TOKEN
exo provider configure --provider runta --secret runta
exo agent create --model <model> --provider runta "My Agent"
```

Runta uses the platform's default Runtime Image unless you set
`--default-image <runtime-image-id>` when configuring the provider. These are
Runta Runtime Image ids, not Docker image names. Use `--api-url` to select a
Runta API endpoint other than `https://api.runta.com`.

Exo maps CPU and memory settings to Runta resource requests. Stopping pauses
the runtime, preserving memory and disk for the next start. An idle timeout
configures automatic suspension and wakeup. Snapshots are Runta checkpoints;
restoring creates a separate runtime and requires access to the original
checkpoint in the same Runta account.

Host bind mounts, durable filesystem mounts, external attachments, and Exo's
restricted-network and credential-injection policies are not supported by this
backend. Commands run with Runta's default egress policy.

## Scope

A sandbox can be scoped to a **conversation** (each conversation gets its
own) or to an **agent** (shared across the agent's conversations) — set at
creation with `conversation create --sandbox-scope <agent|conversation>`.
Long-running personal agents like the canonical exo agent typically use an
agent-scoped sandbox so installed tools persist across conversations.

## Lifecycle

Sandboxes move through create → start/run → snapshot/stop, or through
**attach / detach** when Exo borrows an externally created environment
(for example a Docker container started by another system). Attached
sandboxes must be detached, not stopped. For the full state machine,
candidate selection during a turn, and how agent vs conversation scope
behaves over time, see [Lifecycles → Sandbox](./lifecycles#sandbox-lifecycle).

## Snapshots

Snapshotting a sandbox writes a snapshot id to the event log, tying
filesystem state to conversation history. That's what lets
[time travel](./time-travel) restore not just what was said, but the
environment the agent was working in.

## Secrets in sandboxes

Secrets can be securely mounted into sandboxes so programs inside can use
them **without the LLM being able to view or expose them** — see
[Bindings & Secrets](./bindings-and-secrets).
