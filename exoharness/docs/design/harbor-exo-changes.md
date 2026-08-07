# Exo-side changes needed for the Harbor integration

Three small changes to Exo core that the Harbor evaluation integration
(`eval/harbor`, `exo/adapters/harbor`) either needs or would benefit
from. They are independent of each other and independent of the integration
itself, which is why they belong in their own commit.

Context for all three: `exoharness/docs/design/harbor-integration.md`.

| #   | Change                                 | Status for the integration        |
| --- | -------------------------------------- | --------------------------------- |
| 1   | Skip the liveness check when detaching | **Implemented in this workspace** |
| 2   | Allow snapshotting borrowed containers | Enabler; nothing blocked on it    |
| 3   | Query a conversation's active sandbox  | **Implemented in this workspace** |

---

## 1. Detaching should not require a live container

**Where:** `crates/exoharness/src/basic.rs:1648` (`detach_sandbox`) →
`active_sandbox_handle` (`basic.rs:3017`) → `create_sandbox_handle`
(`basic.rs:3046`) → `crates/exoharness/src/sandbox.rs:568` (borrowed backend
`attach()`, which calls `inspect_running_docker_container` at `sandbox.rs:578`).

**Previous behavior.** `active_sandbox_handle` serves from a live-handle cache. On a hit,
detach is a pure no-op Docker-wise and works on a dead container. On a miss it
rebuilds the handle through the borrowed backend's `attach()`, which inspects
the container and fails if it is not running.

A cache miss means the Exo process restarted since the attach.

**Why it matters.** Harbor stops (and by default deletes) the task container in
`Trial._finalize()` _before_ emitting `TrialEvent.END`, which is where the
integration detaches. So detach always runs against a dead container. It
succeeds today only because the handle is still cached — after a mid-job Exo
restart it fails, precisely in the case where the container is guaranteed gone.
Long overnight runs are where a restart is most likely.

**Wanted.** Detach should not need a working container. It has no use for one:
`BorrowedDockerSandboxHandle::detach()` only returns the stored attachment
descriptor. Either let `create_sandbox_handle` skip the liveness inspect when
rebuilding purely to detach, or short-circuit `detach_sandbox` to read the
stored attachment without materializing a handle at all.

**Implemented.** `detach_sandbox` now uses the durable attachment descriptor
without reconstructing a provider handle. A cross-process regression test
covers the case where the external resource has already been deleted.

**Consequence of not doing it.** A stale `running = true` sandbox record and a
logged error in the trial's feedback sidecar. The trial itself survives, since
the plugin's trial-end hook must swallow everything anyway.

**Open question.** The same rebuild-validates-liveness path presumably affects
`stop_sandbox` on an already-dead sandbox. Worth checking whether this is one
fix or two.

---

## 2. Allow snapshotting borrowed containers

**Where:** `crates/exoharness/src/sandbox.rs:689`.

```rust
async fn snapshot(&self) -> Result<SnapshotPayload> {
    bail!("borrowed Docker containers cannot be snapshotted")
}
```

**Today.** All three lifecycle methods on `BorrowedDockerSandboxHandle` reject,
under the blanket rule in `harbor-integration.md:222`: _"Sandbox lifecycle
operations that imply ownership must reject borrowed handles."_

**Why it should be split out.** `stop` is correctly blocked: it destroys a
container Harbor owns. `snapshot` is not the same kind of thing. The
implementation (`docker_snapshot_container`, `sandbox.rs:1943`) is
`docker commit -p` followed by `docker save` — a read. It does not stop,
modify, or replace the container. The ownership rule does not reach it; it was
swept in with `stop`.

**Restore is not an ownership problem either.** Worth stating plainly, because
the obvious assumption is wrong. `acquire_from_snapshot` (`sandbox.rs:587`)
loads the tar as an image, builds a fresh `SandboxRequest` pointing at it, and
creates a new container, returning a **`WarmSandboxHandle`** (`warm:{key}`).
Its eviction step only inspects `warm_sandboxes`, and borrowed containers never
enter that map — `attach()` builds the handle directly and inserts nothing. So
restore cannot delete or replace Harbor's container, and cannot produce a
borrowed handle. Since it also requires `idle_ttl`, which a borrowed attach has
no notion of, restoring a borrowed-origin snapshot is not a rewind at all: it
is "boot a copy of the task container as my own warm sandbox".

The real risk is **divergence**, not ownership: Exo continues in the copy while
Harbor still grades the original, with nothing surfacing the mismatch. That is
exactly what change 3 detects, and detects easily, since the handle id shifts
from `borrowed-docker:{container_id}` to `warm:{key}`.

So restore does not need blocking. It needs detection plus a policy for what
divergence means — error the trial (what the guard does today), sync the copy
back into Harbor's container before `task_complete`, or make Harbor grade the
copy (still blocked on Compose labels being immutable after create).

**What it unlocks.** A snapshot taken just before `task_complete` is the
evidence artifact the reflection phase wants — the container's full final
state, frozen as an image, still explorable after Harbor has torn the original
down. Better than hand-picking Harbor artifact paths, and safe from test
contamination: the verifier uploads `tests/` into the container only at
verification time, so a snapshot taken during the task phase cannot contain
them.

**Caveats to handle.**

- `docker_snapshot_container` already passes `-p`, so the container is paused
  for the duration of the commit. Brief, but visible to a task with a running
  service or timing-sensitive tests. Decide whether borrowed snapshots should
  drop the pause.
- Each snapshot is a full image plus a saved tar. Per-trial across a few
  hundred trials this is serious disk. Ownership of cleanup needs an answer
  before enabling it.

**Open question.** Is `snapshot` ever called in Exo today other than as the
first half of a rewind? If not, the current bail is a useful early error rather
than an over-restriction, and this is only worth doing alongside a concrete
consumer.

---

## 3. Query a conversation's active sandbox

**Where:** `crates/cli/src/main.rs:648` (`ConversationSandboxCommands`, handler
at `:1734`) and `crates/exoharness/src/protocol.rs` (`Request`).

**Previous behavior.** `ConversationSandboxCommands` had `Attach`, `Detach`,
and `Run`, but no read command.

**Wanted.**

```bash
exo conversation sandbox status <agent> <conversation>
```

returning the conversation's active sandbox id plus its attachment descriptor
(provider and external id), so a caller can check both Exo's bookkeeping and
the container it actually resolves to.

**The resolution logic already exists.** `attached_conversation_sandbox` in
`crates/executor/src/conversation_sandbox.rs:74` already replays the event log,
collecting `SandboxCreated` / `SandboxAttached` as candidates and retiring
anything hit by `SandboxStopped` / `SandboxDetached`. This is mostly a matter
of surfacing it, not writing it.

**Why it is blocking.** The Harbor integration needs to assert at task
completion that Exo is still executing in the container Harbor is about to
grade. `CreateSandbox` is not blocked for a conversation, and resolution falls
back to the most recent created-or-attached handle — so Exo can create a fresh
sandbox mid-task, do all its work there, and leave the borrowed container
untouched. Harbor then grades an empty container and records a plausible zero
with no error anywhere. Silently wrong results are the worst failure mode an
eval can have.

The check must run before `ExoAgent.run()` returns, since Harbor starts the
verifier the instant it does. Call sites are stubbed and waiting:

- `eval/harbor/src/exo_harbor/exo.py` — `ExoClient.verify_sandbox_unchanged`
- `eval/harbor/src/exo_harbor/agent.py:209` — the call in `_work()`

**Implemented.** `exo conversation sandbox status <agent> <conversation>
--json` exposes the active attached sandbox id through the existing event-log
resolution. `ExoClient.verify_sandbox_unchanged` compares that id with the
borrowed sandbox before returning control to Harbor's verifier and raises
`SandboxDriftError` on a mismatch. The current consumer needs the stable Exo
sandbox id, so the command does not yet return the full attachment descriptor.

**Rejected alternative.** Probing through `exo conversation sandbox run` for a
host-written nonce needs no Exo change, but it is a heuristic and would miss a
snapshot-restore copy, which carries the nonce in its filesystem. Not worth it
when the real command is small.

**Note.** This is a read of Exo's own state, so it belongs on the exoharness
HTTP transport too (`docs/exoharness-http.md`). The Harbor integration is
CLI-only by decision, but a `Request` variant is the more general surface and
the CLI command should be a thin wrapper over it.
