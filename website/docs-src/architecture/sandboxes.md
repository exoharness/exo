---
title: Sandboxes
description: Sandbox ownership, backends, filesystems, and snapshots.
---

# Sandboxes

An **environment** defines the image, provider, mounts, and network policy.
A **sandbox** is an instance of that configuration. Managed agents normally
use a sandbox scoped to their [thread](data-model#thread-session-turn), so files survive across sessions
without being shared with another thread.

## Lifecycle and ownership

The exoharness manages sandbox lifecycle; the harness decides which
commands to run. A sandbox can outlive the client session that started it.

| Operation | Effect |
|:----------|:-------|
| Create | Provision a sandbox from an image, mounts, provider, and network policy |
| Start | Ensure it is running, optionally restoring a supported snapshot |
| Run | Execute a command inside it |
| Snapshot | Capture state and record a snapshot id in the event log |
| Stop | Stop a sandbox created by Exo |
| Attach | Register an externally created environment, currently an existing Docker container |
| Detach | Release a borrowed environment without stopping it |

Attached sandboxes are borrowed: detach them when done instead of stopping
them. The API also supports agent-scoped sandboxes shared across threads;
the managed-agent default is a separate sandbox per thread.

## Backends

| Backend | Type | Notes |
|:--------|:-----|:------|
| `smolvm` | Local | MicroVM; the default |
| `firecracker` | Local | MicroVM; VM snapshots including memory, with restrictions below |
| `docker` | Local | Container |
| `apple_container` | Local | Container on macOS |
| `local_process` | Local | Commands run on the host without isolation |
| `daytona` | Hosted | Daytona sandbox |
| `e2b` | Hosted | E2B sandbox |
| `sprites` | Hosted | Sprites sandbox |
| `vercel` | Hosted | Vercel Sandbox |
| `aws_agentcore` | Hosted | Amazon Bedrock AgentCore |

Codex, Claude Code, and Pi default to digest-pinned devbox images containing
the harness, Python, and Node.js/TypeScript. Custom harnesses default to
`ubuntu:24.04`; choose an image containing any dependencies they need.
See [Environments](../agent-definition#environments).

Support varies by backend. Credential substitution and proxy-based network
controls currently require SmolVM or Firecracker. Builds include SmolVM
by default; Firecracker and AWS AgentCore require the `firecracker` and
`aws-agentcore` Cargo features respectively.

## Snapshots

Snapshotting writes a snapshot id to the event log. Filesystem snapshots
capture files; full VM snapshots can also preserve memory and running
processes. The backend determines which formats it can capture and restore.

Filesystem restoration requires a corresponding snapshot. The event log
records tool calls and their results, but isn't a copy of every file those
commands read or wrote. [Forking a thread](data-model#forking-history) does not
automatically restore its sandbox to the fork event.

Firecracker supports full VM snapshots, but currently rejects ordinary
snapshot capture with an egress proxy or mounted resource/durable
filesystems. SmolVM full snapshots and forks are live copy-on-write branches
of the running machine; they include memory and processes and do not survive a
host reboot. SmolVM filesystem snapshots stop and restart the machine and
reject an egress proxy. A sandbox
using credential substitution therefore cannot currently use these
snapshot paths. Snapshot support doesn't imply that every sandbox
configuration can be restored.

## Credentials and host mounts

Sandbox processes can use placeholder credentials. The runtime substitutes
the real value only for requests to destinations permitted by the secret's
policy. See [Vaults](vaults).

Host mounts expose actual files on the runtime host. Read-only mounts
prevent writes, but still allow the agent to read their contents. A mount
containing a raw API key doesn't provide credential substitution's isolation.
