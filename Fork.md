# Agent Fork Design

## Goal

Add a model-visible `fork` tool that lets an Exo agent create a child agent to
work independently, while preserving a shared canonical lineage between parent
and child. The parent should also be able to stop a child safely if the child is
misbehaving, obsolete, or no longer needed.

The important design point is that this should be an **agent fork**, not just a
conversation fork. Exo already has conversation-level fork support; agent fork
needs to create a new agent record, its own conversations and runtime state, and
a shared family history that both parent and child can inspect.

## Existing Building Blocks

Current code already has useful pieces:

- `ExoHarness::new_agent` and `delete_agent` create and remove agents.
- `AgentHandle::new_conversation` creates conversations under an agent.
- `ConversationHandle::fork` copies a conversation's history into a new
  conversation and records `conversation_forked`.
- Agent config is stored as the agent artifact `config/executor.json`.
- Conversation config is stored as the conversation artifact
  `config/executor.json`.
- Exoclaw host tools are exposed by TypeScript definitions and delegated to Rust
  through `examples/exoclaw/host-tools.ts` and
  `crates/executor/src/harness_tool.rs`.
- The current canonical event log is conversation-scoped. It is good for turn
  history, but it is not enough by itself for shared parent/child agent lineage.

## Recommended Model

Introduce an **agent family ledger**. A family is the durable lineage shared by a
root agent and every child forked from it.

Each agent keeps its normal private runtime state:

- agent config
- conversations
- artifacts and memory
- adapters
- scheduler tasks
- sandbox records

The family ledger is shared:

- parent and child can both read it
- every fork is appended to it
- every kill/termination is appended to it
- later child activity summaries can be appended to it

This gives us shared canonical state without forcing parent and child to share a
single conversation log or live sandbox.

## Data Model

Add first-class fork metadata instead of relying only on ad hoc custom events.

Suggested records:

```rust
struct AgentFamilyRecord {
    id: Uuid7,
    root_agent_id: AgentId,
    created_at: DateTimeUtc,
}

struct AgentForkRecord {
    family_id: Uuid7,
    agent_id: AgentId,
    parent_agent_id: Option<AgentId>,
    created_by_conversation_id: Option<ConversationId>,
    created_by_turn_id: Option<TurnId>,
    generation: u32,
    status: ForkStatus,
    purpose: Option<String>,
}

enum ForkStatus {
    Active,
    Terminating,
    Killed,
}

struct AgentFamilyEvent {
    id: Uuid7,
    family_id: Uuid7,
    actor_agent_id: AgentId,
    target_agent_id: Option<AgentId>,
    created_at: DateTimeUtc,
    data: AgentFamilyEventData,
}

enum AgentFamilyEventData {
    AgentForked {
        parent_agent_id: AgentId,
        child_agent_id: AgentId,
        child_slug: String,
        child_name: String,
        purpose: Option<String>,
        source_conversation_id: Option<ConversationId>,
        source_turn_id: Option<TurnId>,
    },
    AgentKillRequested {
        parent_agent_id: AgentId,
        child_agent_id: AgentId,
        reason: String,
    },
    AgentKilled {
        child_agent_id: AgentId,
        reason: String,
    },
    ForkMessageSent {
        from_agent_id: AgentId,
        to_agent_id: AgentId,
        to_conversation_id: ConversationId,
        message: String,
        expects_reply: bool,
    },
}
```

Storage can live alongside existing harness storage, for example:

```text
.exo/.tribe/<root-agent-slug>/
  tribe.json
  events/<event-id>.json
  agents/<agent-id>.json
  root/
    agent.json
    repo/
    state/
    children/
      fork-001-adapters/
        agent.json
        manage
        repo/
        state/
        children/
          fork-001-discord/
            agent.json
            manage
            repo/
            state/
            children/
      fork-002-scheduler/
        agent.json
        manage
        repo/
        state/
        children/
```

This `.exo/.tribe` tree is intentionally human-inspectable. The directory shape
mirrors lineage, so a parent can see its children and grandchildren as a
subtree. The machine-readable records remain the source of truth; directory
names are for readability and can be derived from slugs plus ids if needed to
avoid collisions.

Each child gets its own directory under its parent. A child directory should
contain:

```text
agent.json    # id, slug, display name, parent id, generation, status, purpose
manage        # child-local management script
repo/         # child source worktree
state/        # child-local metadata/cache if needed
children/     # this child's children
```

So lineage is visible directly from the tree:

```text
root -> fork-001-adapters -> fork-001-discord
root -> fork-002-scheduler
```

Avoid a single flat global folder for all children. A nested subtree is easier
for humans to inspect and matches the actual parent/child relationship.

The `agents/<agent-id>.json` index is still useful for fast lookup by id, but it
should point back to the node path in the lineage tree, for example:

```text
agents/<child-agent-id>.json -> { nodePath: "root/children/fork-001-adapters" }
```

For the first implementation, this can be file-backed like the existing basic
harness storage. If we later add a database-backed harness, these records become
normal tables while `.exo/.tribe` can remain a materialized view/worktree
location.

## Tool API

### `fork`

Creates a child agent.

Suggested schema:

```json
{
  "slug": "optional child agent slug",
  "name": "optional child display name",
  "purpose": "why the child is being created",
  "initialPrompt": "optional first instruction to send to the child",
  "conversationSlug": "optional initial child conversation slug",
  "conversationName": "optional initial child conversation name",
  "inheritMemory": true,
  "sandbox": "fresh",
  "adapters": "none"
}
```

Recommended defaults:

- `slug`: derived from purpose when present, otherwise `fork-001`,
  `fork-002`, etc.
- `name`: derived from parent name plus a readable label, e.g.
  `Exo / Adapters` or `Exo / Fork 1`
- `nodePath`: nested under the parent's `children/`, e.g.
  `root/children/fork-001-adapters`
- `conversationSlug`: `dev`
- `conversationName`: `Dev`
- `inheritMemory`: `true`
- `sandbox`: `fresh`
- `adapters`: `none`

Naming should be practical, not lore-heavy. Avoid defaults like "son of Exo".
Store lineage in metadata and the directory tree; keep display names readable
and filesystem names stable.

Return value:

```json
{
  "ok": true,
  "familyId": "...",
  "parentAgentId": "...",
  "childAgentId": "...",
  "childSlug": "...",
  "childName": "...",
  "conversationId": "...",
  "conversationSlug": "...",
  "status": "active"
}
```

### `kill_fork`

Stops a child agent controlled by the caller.

Suggested schema:

```json
{
  "childAgentId": "agent id or slug",
  "reason": "why the child should be stopped",
  "deleteState": false
}
```

Recommended default behavior is **terminate, do not delete**:

- mark the child `Terminating`
- stop child adapter runner work for that child
- disable or delete child adapters
- cancel child scheduled tasks
- stop child sandboxes
- mark the child `Killed`
- append `AgentKillRequested` and `AgentKilled` to the family ledger

`deleteState: true` should be a separate, explicit destructive option. Most of
the time we want killed children to remain inspectable.

Current limitation: Exo has `delete_agent`, but it does not yet have a
first-class primitive to cancel an in-flight model turn for a child. The first
implementation should prevent future child work and stop supervised host
services; graceful active-turn cancellation should be added in the runtime
enforcement phase.

### `list_forks`

Not strictly required for the first PR, but strongly recommended. The parent
needs an easy way to see active children.

Return:

```json
{
  "ok": true,
  "familyId": "...",
  "forks": [
    {
      "agentId": "...",
      "slug": "...",
      "name": "...",
      "parentAgentId": "...",
      "status": "active",
      "purpose": "..."
    }
  ]
}
```

### `list_fork_events`

Also recommended. This is the tool that makes the shared canonical state visible
to both parent and child.

### `send_fork_message`

Sends an internal coordination message between family members. This should use
Exo's conversation wakeup path, not IRC or any external adapter by default.

Suggested schema:

```json
{
  "targetAgentId": "child or parent agent id/slug",
  "message": "message to send",
  "conversationSlug": "optional target conversation slug",
  "expectsReply": true
}
```

Default behavior:

- resolve `targetAgentId` inside the same family
- choose the target's default fork conversation if `conversationSlug` is null
- append `ForkMessageSent` to the family ledger
- send a wakeup to the target conversation with the sender, family id, message,
  and whether a reply is expected

Return:

```json
{
  "ok": true,
  "familyId": "...",
  "fromAgentId": "...",
  "targetAgentId": "...",
  "targetConversationId": "...",
  "eventId": "..."
}
```

Use this for parent/child coordination. External shared channels such as IRC,
Discord, or ExoChat should be optional, human-visible adapters, not required
internal control plumbing.

## Fork Semantics

### Agent Config

The child should start with a copy of the parent's agent config:

- harness type
- TypeScript module path
- model
- sandbox image/provider
- networking
- max tool round trips
- tool creation settings
- Braintrust config

This is a snapshot at fork time. Later prompt/tool evolution by the child should
not silently mutate the parent.

### Memory

Default should be `inheritMemory: true`, implemented as a copy of the parent's
memory artifact at fork time. This gives the child useful context without making
parent and child write concurrently to the same memory artifact.

If we want truly shared memory later, that should be a separate explicit feature
with conflict handling.

### Conversations

Create a new initial child conversation. Do not reuse the parent's current
conversation id.

Seed the child conversation with:

- an `AgentForked` family event reference
- a short system/developer message explaining that this agent is a child fork
- the user-provided `initialPrompt` if present

We should not copy the full parent conversation history by default. The shared
family ledger provides lineage. If we want history transfer, add an explicit
`copyConversationHistory` option later.

### Source Code

Each child should get its own mutable source checkout by default. The safest
implementation is a git worktree or clone created from the parent's current
commit/branch, for example:

```text
.exo/.tribe/<root-agent-slug>/children/<child-agent-slug>/repo
```

That child repo should be mounted into the child's sandbox at `/workspace/exo`,
so from the child's perspective it still has "its own source code" in the same
canonical location.

This matters because source edits are part of identity and evolution. If parent
and child share one live checkout, a child can accidentally break the parent
while experimenting. Separate source worktrees let children evolve independently,
produce diffs, and later ask the parent to merge successful changes.

Important runtime implication: giving a child its own source tree is not enough
unless the host can run that child from that source tree. The fork design should
therefore record a per-agent `sourceRoot` and teach the guardian/runner how to
build and restart services for the selected agent source. Until that exists,
child source isolation is useful for experimentation and diffs, but the running
host process may still be using the root checkout.

Recommended first implementation:

- create a child git worktree at fork time
- place it under the child node in `.exo/.tribe`
- store the child `sourceRoot` in the fork record or agent config
- mount that `sourceRoot` into the child sandbox at `/workspace/exo`
- keep parent and child source changes isolated until an explicit merge/review

### Sandbox

Each child should also get its own sandbox by default. Do not share the parent's
live sandbox. Sharing would let a child break the parent's environment or mutate
dependencies out from under the parent.

Recommended first implementation:

- child gets a fresh agent-scoped sandbox using the same sandbox image/provider
- child gets its own source worktree mounted at `/workspace/exo`

Future option:

- `sandbox: "snapshot"` could snapshot the parent sandbox and start the child
  from that snapshot, when the selected backend supports it cleanly.

### Child Management Script

Each child directory should include a small host-side management script. The
parent can inspect and invoke this script to maintain the child without giving
each child its own independent guardian process.

Suggested path:

```text
.exo/.tribe/<root-agent-slug>/root/children/<child-node>/manage
```

Suggested commands:

```bash
./manage status
./manage stop
./manage build
./manage start
./manage restart
./manage logs
```

Responsibilities:

- `status`: show child agent id, status, pid files, adapter/scheduler state, and
  source root
- `stop`: stop child-specific services, adapters, scheduled work, and live
  sandbox processes where possible
- `build`: build from the child's `repo/`
- `start`: start child services using the child's `sourceRoot`, agent id, and
  conversation defaults
- `restart`: `stop`, `build`, then `start`
- `logs`: show child-local service logs

This script should be generated at fork time from a checked-in template. It
should mostly delegate to the shared guardian/control machinery with the child
agent id and child source root as explicit arguments. That keeps one host-level
guardian model while still giving the parent an obvious per-child control point:

```text
parent shell/tool -> .exo/.tribe/.../<child>/manage restart
```

The script should not be the canonical source of truth. It should read
`agent.json` and the family ledger to discover child metadata, then call the
normal host-side APIs.

### Adapters

Do not clone adapters by default. Duplicating external adapters can cause
duplicate replies, duplicate Discord bots, duplicate WhatsApp sessions, or
confusing ExoChat links.

For the first implementation:

- child starts with no external adapters
- parent can explicitly create an ExoChat adapter for the child if needed

Future option:

- `adapters: "exochat"` can create a fresh child ExoChat URL
- `adapters: "copy-disabled"` can copy adapter configs but keep them disabled

### Scheduler

Do not copy scheduled tasks by default. A fork that inherits scheduled tasks
could accidentally duplicate side effects.

If child scheduled work is needed, the parent should include it in
`initialPrompt` or create it explicitly after fork.

### Parent/Child Communication

Parent and child agents should not need a shared IRC channel or any external
adapter to coordinate. Coordination should happen through the internal
`send_fork_message` tool and the shared family ledger.

Recommended flow:

1. Parent calls `fork` with a purpose and optional `initialPrompt`.
2. Parent later calls `send_fork_message` to ask for status or assign follow-up
   work.
3. Child replies with `send_fork_message` to the parent.
4. Both directions append durable `ForkMessageSent` events to the family ledger.

This gives parent and child chat-like behavior, but keeps it private,
inspectable, and independent of external services.

## Permissions and Safety

Rules:

- an agent may kill only descendants in its family
- a child may request self-termination
- a child may not kill its parent or sibling
- hard deletion requires an explicit `deleteState: true`
- all fork and kill actions append to the family ledger before side effects

This prevents a runaway child from erasing the parent while still allowing the
parent to recover.

## Implementation Plan

### PR 1: Core Fork Registry and Tools

1. Add family/fork records to `crates/exoharness`.
2. Add storage helpers to the basic harness:
   - create or get current agent family
   - append family events
   - list family agents
   - update fork status
   - maintain the `.exo/.tribe` lineage subtree
3. Add Rust tool execution in `crates/executor/src/harness_tool.rs`:
   - `fork`
   - `kill_fork`
   - optionally `list_forks`
   - optionally `list_fork_events`
   - optionally `send_fork_message`
4. Add TypeScript tool definitions under `examples/exoclaw/`, likely a new
   `fork-tools.ts`, registered from `examples/exoclaw/harness.ts`.
5. Make `fork` create:
   - child agent
   - copied child agent config
   - initial child conversation
   - child node under `.exo/.tribe`
   - child `manage` script from a checked-in template
   - child source worktree and `sourceRoot`
   - fresh child sandbox using that `sourceRoot` mount
   - copied memory artifact when requested
   - family ledger `AgentForked` event
6. Make `kill_fork` terminate without deleting by default.
7. Make `send_fork_message` wake the target conversation and append a family
   ledger event.
8. Add tests for:
   - child creation
   - duplicate slug rejection
   - family event visibility from parent and child
   - parent can send child a fork message
   - child can reply to parent
   - parent can kill child
   - child cannot kill parent

### PR 2: Runtime Enforcement

1. Refuse new turns for agents marked `Killed`.
2. Ensure scheduler runner skips killed agents.
3. Ensure adapter runner skips killed agents.
4. Stop active child sandboxes on kill.
5. Make guardian/build/restart paths source-root aware.
6. Teach shared guardian/control paths to accept child agent id and `sourceRoot`.
7. Have generated child `manage` scripts delegate to those shared paths.
8. Surface killed status clearly in CLI/listing tools.

### PR 3: UX and Control

1. Add CLI commands:
   - `exo agent fork <parent> ...`
   - `exo agent forks <agent>`
   - `exo agent kill-fork <parent> <child>`
2. Add better REPL rendering for fork events.
3. Add better REPL rendering for fork messages.
4. Add prompt guidance so agents know when to fork, how to supervise children,
   and how to coordinate with `send_fork_message`.

## Open Questions

- Should child agents share secrets and bindings by default, or only inherit the
  model binding needed to run?
- Should a child inherit `.exo/exoclaw-profile.md` local user instructions?
- Should child ExoChat links be created by default, or only on request?
- Should a killed child be restartable, or is restart a new fork?
- Do we want a hard cap on active child agents per parent?
- Should `send_fork_message` require direct ancestry, or can siblings message
  each other with parent permission?
- Should successful child source changes merge back through normal git PR/review,
  or should Exo have an explicit `merge_fork` tool later?

## Recommendation

Implement `fork`, `kill_fork`, `list_forks`, `list_fork_events`, and
`send_fork_message` together. The shared family ledger is the key piece: it gives
parent and child a common canonical history while preserving separate
conversations, sandboxes, adapters, and scheduled work. Keep the first version
conservative: copy config and memory, start a fresh sandbox, do not copy adapters
or scheduled tasks, give each child its own source worktree mounted at
`/workspace/exo`, coordinate through internal fork messages, and make killing a
child non-destructive by default.
