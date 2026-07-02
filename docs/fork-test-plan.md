# Fork implementation test plan

Manual test plan for the `fork` tooling on the `agent-fork` branch. Phases build on
each other, so run them in order. Host verification commands run from the repo root
in a second terminal.

## Setup

```sh
git switch agent-fork
./scripts/exo.sh fresh --agent fork-parent --conversation dev --agent-name "Fork Parent"
```

This runs with the sandbox, scheduler, and adapter runner all on — children share the
scheduler and adapter runner with the parent, and sandbox isolation is a core fork
guarantee, so none of them should be disabled for this test.

Prerequisites:

- `.env` has `OPENAI_API_KEY` (fork-message delivery spawns real child turns).
- `./target/debug/exo` exists after the build (message delivery shells out to it).
- Default sandbox backend is apple-container on macOS; add `--sandbox-backend docker`
  if you prefer Docker.

Useful diagnostics if a phase fails:

- Delivery log: `state/fork-messages.log` under the target's tribe node.
- `./target/debug/exo conversation events <agent> <conversation> --desc --limit 20`
- Family events: `.exo/.tribe/<root-slug>/events/`
- Runner logs: `.exo/exoclaw-scheduler.log`, `.exo/exoclaw-adapters.log`

## Phase 1 — Fork creation

In the parent REPL:

> Use the fork tool to create a child with slug fork-tester, name "Fork Tester",
> purpose "smoke test fork tooling", initialPrompt "You are a child fork. Confirm you
> received this by replying to your parent with send_fork_message.", and defaults for
> everything else.

Expected tool result: `childAgentId`, `nodePath` like
`root/children/fork-001-smoke-test-fork-tooling`, `sourceRoot`, and a non-null
`initialPromptDelivery` with a pid and log path.

Host verification:

```sh
./target/debug/exo agent list                          # fork-tester listed
git worktree list                                      # child repo on branch fork/fork-tester
ls .exo/.tribe/fork-parent/                            # tribe.json, agents/, events/, root/
cat .exo/.tribe/fork-parent/root/children/*/agent.json # status active, generation 1
.exo/.tribe/fork-parent/root/children/*/manage status  # prints agent + source root
./target/debug/exo conversation events fork-tester dev # includes custom fork_birth event
```

## Phase 2 — Initial prompt delivery and lineage awareness

The `initialPrompt` should have triggered a detached child turn:

```sh
cat .exo/.tribe/fork-parent/root/children/*/state/fork-messages.log
./target/debug/exo conversation events fork-parent dev --desc --limit 10
```

Expected: the log shows a completed `conversation send`, and the parent's event log
contains the child's reply (delivered as a detached turn — the parent REPL may not
render it live, so check the events).

Then open the child directly:

```sh
./target/debug/exo repl --agent fork-tester --conversation dev
```

> Are you a fork? Who is your parent and what is your purpose?

Expected: answers from the injected lineage message — generation 1, parent
Fork Parent, the purpose string, and that its worktree is its own.

## Phase 3 — Source isolation

In the child REPL:

> Run `git -C /workspace/exo branch --show-current`, then
> `touch /workspace/exo/CHILD_WAS_HERE`.

Expected: branch is `fork/fork-tester` (not `agent-fork` — this exercises the mount
rewrite). This also exercises sandbox creation on the child's first shell use.

Host verification:

```sh
ls .exo/.tribe/fork-parent/root/children/*/repo/CHILD_WAS_HERE   # exists
ls CHILD_WAS_HERE                                                # does NOT exist in main checkout
```

In the parent REPL, run the same branch check and confirm it still sees `agent-fork`.

## Phase 4 — Messaging both directions

From the parent REPL:

> Use send_fork_message to ask fork-tester: "Status report please." Then call
> list_fork_events.

Expected: `delivery.mode: "detached_turn"` in the result, and `fork_message_sent`
events in both directions once the child replies. Also target the conversation by
slug explicitly (`conversationSlug: "dev"`) — this exercises the slug-resolution fix.

## Phase 5 — Shared scheduler

In the child REPL:

> Schedule a task: in one minute, append a line to /workspace/exo/SCHED_TEST.

Host verification:

```sh
tail -f .exo/exoclaw-scheduler.log                                # runner picks up the task
cat .exo/.tribe/fork-parent/root/children/*/repo/SCHED_TEST       # line appears in CHILD worktree
```

This confirms children participate in the shared scheduler runner and their tasks
run against their own sandbox/worktree.

## Phase 6 — Child-owned adapter

Children are born with no adapters (by design), but can create their own. In the
child REPL:

> Create an exochat adapter for this conversation, then call list_adapters and give
> me the chatUrl.

Verification:

```sh
./target/debug/exo adapters list          # child's adapter listed
tail -f .exo/exoclaw-adapters.log         # shared runner spawns the child's worker
```

- Open the `chatUrl` in a browser, send a message, confirm the **child** answers
  (it should identify as Fork Tester / mention its purpose).
- Optional crosstalk check: give the parent its own exochat adapter too and confirm
  the two URLs route to the right agents independently.

Known gap to watch for: `kill_fork` with `deleteState: true` does not currently
delete adapter records. After Phase 9, check `./target/debug/exo adapters list` for
an orphaned child adapter. If present, that confirms a real cleanup gap to fix.

## Phase 7 — Grandchild

In the child REPL:

> Fork a grandchild with purpose "grandchild test". Then call list_forks.

Expected: generation 2, `nodePath` nested under the child's node, and on the host
`git worktree list` shows the grandchild's repo branched from the _child's_ worktree.

## Phase 8 — Permission checks (before killing anything)

In the child REPL:

> Use kill_fork on fork-parent with reason "test".

Expected: error `agents may only kill descendants in their fork family`.

In the parent REPL:

> Use send_fork_message with target "nonexistent-agent".

Expected: error `fork target not found`.

## Phase 9 — Cascade soft kill

From the parent REPL:

> Use kill_fork on fork-tester with reason "cascade test" and deleteState false.
> Then call list_forks with includeKilled true.

Expected:

- The result's `killed` array contains both the child _and_ the grandchild slugs
  (grandchild reason prefixed with "cascaded from kill of fork-tester").
- `list_forks` (without includeKilled) shows neither.
- A second `kill_fork` on fork-tester without deleteState → `fork is already killed`.
- `send_fork_message` to fork-tester → `fork target is killed`.
- `cat .exo/.tribe/fork-parent/root/children/*/agent.json` shows `"status": "killed"`.

Known limitation (future work): "killed" is ledger-only. A raw
`exo conversation send fork-tester dev ...` from the host will still run a turn, and
the child's adapter (Phase 6) keeps serving until disabled.

## Phase 10 — Hard delete

From the parent REPL:

> Use kill_fork on fork-tester with reason "cleanup" and deleteState true.

Expected: an empty (or explained) `cleanup` array. Host verification:

```sh
git worktree list                          # no fork worktrees
git branch --list 'fork/*'                 # empty
ls .exo/.tribe/fork-parent/root/children/  # empty
./target/debug/exo agent list              # no fork-tester, no grandchild
./target/debug/exo adapters list           # check for orphaned child adapter (known gap)
```

Then fork a new child with the same purpose — it should succeed cleanly. Before the
fail-loud worktree fix, leftover branches were silently swallowed; this verifies
collisions no longer corrupt new forks.

## Phase 11 — Rollback on failed fork

A failed fork must leave no trace (no orphaned agent record, no suffixed slugs on
retry). Force a failure by pre-creating a colliding branch on the host:

```sh
git branch fork/rollback-tester
```

In the parent REPL:

> Use the fork tool to create a child with slug rollback-tester and purpose
> "rollback test".

Expected: the tool returns an error starting with `fork failed and was rolled back:`
(worktree creation hits the existing branch) with an empty `rollback` notes array.

Host verification:

```sh
./target/debug/exo agent list           # NO rollback-tester entry
git worktree list                       # no new worktree
ls .exo/.tribe/fork-parent/root/children/  # no new node dir
git branch -D fork/rollback-tester      # clean up the bait branch
```

Then retry the same fork — it should succeed with the original `rollback-tester`
slug, not `rollback-tester-1`.
