# Fork Implementation Summary

This document summarizes the changes on the `agent-fork` branch that made
Exoclaw's first end-to-end fork flow work. It focuses on the fork-specific
implementation and the follow-up fixes discovered while testing with Discord,
Docker, and child sandboxes.

## Architecture Overview

The fork implementation adds a lightweight agent family model on top of the
existing Exo harness:

- A parent can create a child Exo agent with its own conversation.
- Parent and child share a family ledger so they can see lineage, status, and
  internal messages.
- Each child gets a directory under `.exo/.tribe`, making the family tree
  inspectable from the host filesystem.
- Each child gets its own source checkout mounted into its sandbox at
  `/workspace/exo`.
- Parent and child coordinate through `send_fork_message`, which records a
  ledger event and starts a detached `exo conversation send` turn for the
  target.
- `kill_fork` marks a fork subtree as killed and optionally deletes the child
  agents and lineage directories.

The current implementation is intentionally conservative:

- Children get fresh sandbox state.
- Children do not inherit adapters or scheduled tasks.
- Memory is copied by default, but can be disabled per fork.
- Runtime kill enforcement is not yet hard-enforced by Rust; killed status is
  currently enforced by fork tools and lineage prompts.

## Runtime Flow

### Creating A Child

When an agent calls `fork`, Exoclaw:

1. Loads or creates the family ledger.
2. Chooses a child slug, display name, lineage node path, and source root.
3. Creates a new Exo agent record.
4. Writes Rust-compatible executor config artifacts for the child agent and
   child conversation.
5. Rewrites the child's conversation mount so `/workspace/exo` points at the
   child's source checkout instead of the parent's checkout.
6. Copies the parent's memory artifact unless `inheritMemory` is false.
7. Adds a `fork_birth` custom event to the child's conversation history.
8. Adds the child to the shared family ledger and materializes `.exo/.tribe`
   files.
9. Creates the child's source clone.
10. Writes a child-local `manage` script.
11. Saves the family ledger.
12. If an `initialPrompt` was provided, delivers it as a detached child turn.

### Source Isolation

The initial implementation used `git worktree add`, but testing showed that
linked worktrees are not usable from inside a container. A worktree's `.git` file
points back into the parent repo's `.git/worktrees/...`, which is not mounted
into the child's sandbox.

The branch now creates each child source tree as a standalone local clone:

```sh
git clone --local <parent sourceRoot> <child sourceRoot>
git -C <child sourceRoot> checkout -b fork/<child-slug>
```

This gives the child a real `.git` directory inside its mounted source tree,
while still being cheap because local clones hardlink git objects when possible.
The parent can later inspect or integrate child work with:

```sh
git fetch <child sourceRoot> fork/<child-slug>
```

### Messaging

`send_fork_message` records an append-style family event, then starts a detached:

```sh
exo conversation send <target-agent> <conversation> <prompt>
```

The target's stdout/stderr goes to the target node's
`state/fork-messages.log`, which became the main debugging surface during
testing.

Two important fixes came out of end-to-end testing:

- The TypeScript harness protocol for `list_conversations` needed to match the
  Rust request and response shapes.
- Detached message delivery must pass the conversation's sandbox backend
  (`EXO_SANDBOX_BACKEND=docker`, etc.) so the spawned CLI can run the target
  conversation with the same sandbox provider as the parent session.

## File-By-File Summary

### `examples/exoclaw/fork-tools.ts`

This is the core implementation file. It adds and registers five Exoclaw tools:

- `fork`
- `kill_fork`
- `list_forks`
- `list_fork_events`
- `send_fork_message`

Major responsibilities:

- Defines the family data model:
  - `FamilyRef`
  - `FamilyAgentRecord`
  - `FamilyEvent`
  - `FamilyStore`
- Stores the family ledger as a root-agent artifact at `fork/family.json`.
- Stores a child-to-family pointer at `fork/family-ref.json`.
- Materializes human-readable lineage files under `.exo/.tribe`.
- Creates child agents and child conversations.
- Copies the parent memory artifact by default.
- Writes child executor configs in the Rust serializer's expected snake_case
  shape.
- Rewrites the child conversation's self-repo mount from the parent's source
  root to the child's source root.
- Adds `fork_birth` events to child conversation history.
- Creates standalone child source clones under each tribe node's `repo/`.
- Writes per-child `manage` scripts.
- Delivers initial prompts and fork messages via detached `exo conversation
send` processes.
- Records fork messages in the family ledger.
- Resolves target conversations by id or slug.
- Injects fork lineage awareness into every turn via `forkInstruction`.
- Implements kill cascade across a subtree.
- Supports hard deletion of descendant agents and their `.exo/.tribe` nodes.
- Rolls back failed forks so retries do not leak orphaned `fork-tester-1` style
  agents.

Important implementation details:

- `fork` only supports `sandbox: "fresh"` and `adapters: "none"` for now.
- Child adapter records are not copied from the parent.
- Child scheduled tasks are not copied from the parent.
- `createSourceClone` replaced the earlier worktree implementation so git works
  inside Docker-mounted child sandboxes.
- `deliverForkMessage` sets `EXO_SANDBOX_BACKEND` for the spawned CLI, preferring
  `context.conversationConfig.sandboxProvider` over `context.agentConfig` because
  the conversation config is the effective sandbox config.
- `rollbackFork` removes the partially materialized tribe node and deletes the
  child agent record if any step after `newAgent` fails.
- `kill_fork deleteState: true` deletes descendants' Exo agent records and then
  removes the subtree's lineage directory, including the child source clones.

Known limitations in this file:

- The family ledger is read-modify-write with no compare-and-swap, so concurrent
  writes can lose updates.
- `send_fork_message` is fire-and-forget; the sender receives a pid/log path but
  no confirmed delivery result.
- Killed status is not enforced by the Rust executor, so raw host sends,
  scheduler wakeups, or adapter wakeups can still start a killed child unless
  another layer blocks them.
- Hard-delete cleanup does not yet disable/delete adapters created by a child.

### `examples/exoclaw/harness.ts`

This wires the fork implementation into Exoclaw.

Changes:

- Imports `registerForkTools` and `forkInstruction`.
- Registers fork tools alongside scheduler, adapter, introspection, sandbox,
  guardian, and memory tools.
- Updates Exoclaw's developer instructions to tell the agent it can:
  - create child agents with `fork`
  - inspect them with `list_forks` and `list_fork_events`
  - stop descendants with `kill_fork`
  - coordinate with parent/child agents using `send_fork_message`
- Injects `forkInstruction(context)` every turn, after local profile instructions
  and before memory.

The lineage instruction is what makes a child consistently aware that it is a
fork, who its parent is, what generation it is, what its purpose is, and where
its source tree lives.

### `typescript/harness/runner.ts`

This file needed protocol fixes because fork message delivery became the first
real user of `agent.listConversations()`.

Changes:

- Updated `RawExoRequest` so `list_conversations` includes the required Rust
  `request` object:

  ```ts
  {
    type: "list_conversations";
    agent_id: string;
    request: { cursor?: string | null; limit?: number | null };
  }
  ```

- Updated `RawExoResponse` so the `conversations` response matches Rust's
  `Response::Conversations { result: ... }` shape:

  ```ts
  {
    type: "conversations";
    result: {
      conversations: RawConversationHandleInfo[];
      next_cursor?: string | null;
    };
  }
  ```

- Updated `createAgent(...).listConversations()` to send `request: {}` and read
  `payload.result.conversations`.

Why it mattered:

- Without the request field, Rust rejected the protocol message with
  `missing field request`.
- Without the result wrapper, TypeScript crashed with
  `Cannot read properties of undefined (reading 'map')`.
- Both failures broke detached child replies because `send_fork_message` resolves
  conversation slugs by listing conversations.

### `scripts/exo.sh`

This file adds host-side cleanup for fork experiments.

Changes:

- `stop-all` now calls `prune_orphan_fork_state`.
- `fresh` and `delall` call `cleanup_fork_state` through
  `delete_all_agents_and_conversations`.

`prune_orphan_fork_state`:

- Runs `git worktree prune`.
- Deletes orphaned `fork/*` branches that are not checked out by any registered
  worktree.
- Preserves live state and is safe for `stop-all`.

`cleanup_fork_state`:

- Removes registered worktrees under `.exo/.tribe` from earlier worktree-based
  fork attempts.
- Calls `prune_orphan_fork_state`.
- Removes `.exo/.tribe`.
- Is intentionally only used when all agents/conversations are being deleted.

Why worktree cleanup still exists:

- The current fork implementation uses standalone local clones, but early testing
  created real git worktrees. The script keeps cleanup for stale worktree debris
  so existing developer checkouts can recover cleanly.

### `Fork.md`

This is the design document for the fork feature.

It documents:

- Goals and non-goals.
- The family ledger model.
- The `.exo/.tribe` filesystem layout.
- Root/child lineage.
- Tool APIs:
  - `fork`
  - `kill_fork`
  - `list_forks`
  - `list_fork_events`
  - `send_fork_message`
- Fork semantics:
  - config
  - memory
  - conversation
  - source
  - sandbox
  - adapters
  - scheduler
  - parent/child communication
- Permissions and safety.
- Implementation phases and future work.

Follow-up edits from testing:

- Changed source isolation guidance from "git worktree or clone" to standalone
  local clones.
- Documented why linked worktrees fail inside container sandboxes.
- Documented that parents can integrate child work with `git fetch <sourceRoot>
fork/<child-slug>`.

### `docs/fork-test-plan.md`

This is the manual end-to-end test plan used to validate fork behavior.

It covers:

- Initial setup with sandbox, scheduler, and adapter runner enabled.
- Fork creation.
- Initial prompt delivery.
- Lineage awareness.
- Source isolation.
- Bidirectional fork messaging.
- Shared scheduler behavior.
- Child-owned adapter behavior.
- Grandchildren.
- Permission checks.
- Cascade soft kill.
- Hard delete.
- Rollback on failed fork.

Follow-up edits from testing:

- Updated source isolation checks to expect standalone clones rather than git
  worktrees.
- Updated hard-delete checks now that deleting the tribe node deletes child
  clones.
- Updated rollback testing to use a non-empty clone destination instead of a
  branch collision.

## Debugging Lessons From The First End-To-End Run

The end-to-end run surfaced several important implementation details:

- TypeScript harness changes are cached by a long-lived runner process. Restart
  the REPL/adapter runner after editing harness code.
- Detached child turns can fail silently from the sender's perspective; always
  inspect `state/fork-messages.log` under the target tribe node.
- The effective sandbox provider can live on the conversation config, not the
  agent config.
- Docker-backed child turns need the spawned CLI to receive
  `EXO_SANDBOX_BACKEND=docker`.
- A linked git worktree is not enough for sandbox source isolation because the
  `.git` pointer target is outside the mount.

## Current Known Follow-Ups

The implementation now works end to end for trusted local experiments, but a few
items remain before this should be treated as a hard isolation/runtime feature:

- Move fork message delivery into the Rust host layer so the sender can observe
  delivery success/failure instead of relying on detached process logs.
- Add Rust-side runtime kill enforcement so killed agents cannot be started by
  raw CLI sends, scheduler wakeups, or adapter wakeups.
- Make the family ledger append-only or versioned with compare-and-swap semantics
  to avoid lost updates from concurrent writers.
- Disable or delete child-created adapters when `kill_fork` hard-deletes a
  subtree.
- Sweep stale Apple Container sandboxes in `scripts/exo.sh`, not just stale
  Docker containers.
