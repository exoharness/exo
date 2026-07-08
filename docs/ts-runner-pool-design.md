# TypeScript runner pool — design & semantics

Status: draft for review. Semantics only; implementation follows after sign-off.

## Problem

`TypeScriptExecutor` keeps exactly one Node runner process per harness
`module_path` and holds its mutex for the **entire turn**, model latency
included (`crates/executor/src/typescript.rs:120-138`). All turns for the same
module therefore execute strictly serially, even across unrelated
conversations and unrelated agents — the runner map is keyed by module path
alone, so every agent configured with the same harness module shares the one
serial process. The agent-scale eval measured this as the dominant throughput
ceiling and worked around it with K module-path symlinks round-robin'd by the
driver — which multiplies the map key instead of fixing the executor, and
round-robin assignment lets one slow turn idle its whole slot while other
slots queue.

The serialization is not accidental: the host↔guest stdio protocol has no
turn correlation (`Init`/`Done`/`Error`/`StreamEvent` carry no turn id), so at
most one turn may be in flight per process. This design keeps that protocol
invariant and adds concurrency by **pooling processes**, not by multiplexing
turns within a process (that is a separate, later change — see Evolution).

## Goals

- N concurrent turns for the same harness module within one exo process.
- Free assignment: a turn takes _any_ idle runner; a slow turn never blocks a
  turn that an idle runner could serve.
- Failure isolation: one failed turn costs one process, nothing else.
- Pool quiesces when idle (bursty daemons like exoclaw must not pin
  N Node processes forever).
- Minimal machinery: the simplest design that has the right semantics,
  wherever that puts the changes. (As drafted it needs no protocol, guest, or
  exoharness change — the diff lands in `crates/executor/src/typescript.rs` —
  but that's a consequence, not a constraint; if a change elsewhere makes
  this cleaner, we take it.)

## Non-goals

- Multiplexing turns over one process (option A / exec-id protocol).
- Cross-process or cross-machine work distribution (PR #113's coordinator +
  a future `claim_ready` worker fleet).
- Scheduling policy, priorities, or per-turn timeouts. Callers wait FIFO;
  pacing belongs to the layers above (conversation leases, drivers).
- Pre-spawning / warm-up.

## Design

Replace the per-module `Arc<Mutex<TypeScriptRunnerProcess>>` with a
per-module `RunnerPool`:

```
runners: Mutex<HashMap<module_path, Arc<RunnerPool>>>

RunnerPool {
    permits: Semaphore(max_size),        // concurrency budget
    idle:    Mutex<Vec<IdleRunner>>,     // LIFO stack of warm processes
}
IdleRunner { process: TypeScriptRunnerProcess, since: Instant }
```

The per-runner mutex disappears entirely: checkout transfers **ownership** of
the process to the turn (`&mut` by move), which is the exclusivity guarantee.
The pool replaces the lock rather than wrapping it.

### Turn lifecycle

```
execute_turn(module_path, turn)
  │
  ├─ pool = get-or-create RunnerPool for module_path
  ├─ permit = pool.permits.acquire().await          // FIFO wait if saturated
  ├─ runner = pool.idle.pop()                       // LIFO; prune expired
  │           else TypeScriptRunnerProcess::start() // lazy spawn
  ├─ runner.execute_turn(turn).await                // unchanged internals
  │
  ├─ Ok  ⇒ pool.idle.push(runner, now)              // check-in, warm
  └─ Err ⇒ drop(runner)                             // kill_on_drop reaps it
            (permit released on drop either way)
```

### Semantics (invariants)

1. **Bounded concurrency.** At most `max_size` runner processes per module
   path; at most one in-flight turn per process. Concurrency for a module is
   exactly `min(in-flight turns, max_size)`.
2. **Free assignment, no affinity.** Any idle runner of the module serves any
   turn. Consecutive turns of one conversation may land on different
   processes.
3. **LIFO reuse.** Checkout pops the most-recently-used runner. Load
   concentrates on few warm processes; the rest age out. Scale-down falls out
   of reuse order instead of needing a balancer.
4. **Lazy scale-up.** A process is spawned only when a turn arrives, no idle
   runner exists, and the permit budget allows. Spawn cost (node + tsx +
   module import, sub-second) is paid at the concurrency high-water mark
   only.
5. **TTL scale-down.** An idle runner unused for `IDLE_TTL` (5 min) is
   dropped. A quiescent pool shrinks to zero processes. (Behavior change vs.
   today, where the single runner lives forever; first turn after a long
   idle pays one spawn.)
6. **Failure isolation.** A turn error discards its process and only its
   process; sibling runners and queued waiters are untouched. This matches
   guest behavior — `runner.ts` exits on a failed `runTurn`, so "turn failed"
   and "process dead" are already the same event. Spawn failures are returned
   to the caller and never cached (a broken module fails each attempt fast,
   as today).
7. **No ordering.** The pool provides no serialization or ordering
   guarantees whatsoever. Per-conversation FIFO/exclusivity live above, in
   the conversation send lock today and PR #113's coordinator leases after.
   Nothing may rely on the executor for mutual exclusion. (Nothing does
   today — the module mutex is an artifact, not a contract — but this makes
   it explicit.)
8. **FIFO waiting, no timeout.** When saturated, callers queue on the
   semaphore in arrival order. No checkout timeout at this layer: waiting on
   a saturated pool is today's mutex wait, K-wide; introducing a new failure
   mode here buys nothing.

### Harness-author contract (made explicit)

A harness module must not rely on process-local state (module-level globals,
in-memory caches) surviving across turns. Cross-turn state belongs in the
substrate (events, artifacts). This was always the de-facto contract — the
single runner process already dies on any error and the eval's symlink pool
already violated stickiness — but the pool makes it load-bearing. To be
documented in the TS harness docs alongside this change.

**Verified pool-safe:** sandbox-process reuse (`reuse_key`) resolves through
the conversation event log + substrate process status
(`typescript.rs:666-715`), not through runner memory. A turn on runner 2
reuses a sandbox process created via runner 1; the runner-local
`sandbox_processes` map only routes stdin/events for processes started during
the current checkout, and a re-established pump resumes from the recorded
cursor. Reaping an idle runner therefore orphans nothing: sandbox processes
live in the sandbox, their event pumps die with the process's channel and are
rebuilt on next use — identical to what runner death already does today.

### Idle expiry

No global reaper. Check-in arms a one-shot expiry: pushing a runner onto the
free-list assigns it a unique check-in token and spawns a task that sleeps
`IDLE_TTL`, then removes the free-list entry **with that token, if still
present** — i.e. only if the runner sat idle the whole time. If the runner
was checked out (and possibly re-checked-in) meanwhile, the token is gone and
the timer no-ops; the newer check-in armed its own timer. Dropping the
removed runner reaps the process (`kill_on_drop` SIGKILLs a quiescent Node
process — the host has never sent the protocol's `Shutdown` message and an
idle runner has nothing to flush, so graceful shutdown is not worth the
machinery).

Properties: a fully idle pool drains to zero with no periodic machinery and
no task whose lifecycle must be managed — every timer either acts once or
no-ops, and at most `max_size` timers per module sleep at any moment. Pool
map entries themselves are kept (bounded by distinct module paths).

## Configuration

Two knobs, one exposed:

| knob       | value                                   | rationale                                                                                                                                                                                                                      |
| ---------- | --------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `max_size` | env `EXO_TS_RUNNER_POOL`, default **4** | Default must be >1 or the fix is opt-in and the measured serialization persists out of the box. 4 ≈ 200-400 MB worst-case per module, only under sustained concurrency, reclaimed by TTL. Eval drivers set 16 on big machines. |
| `IDLE_TTL` | const, **300 s**                        | Long enough to ride out gaps between bursts; spawn is sub-second so the cost of expiring too eagerly is small anyway. Not worth an env var until proven otherwise.                                                             |

Plumbing: `TypeScriptExecutor::new` gains the pool size (read from env in the
`from_*` constructors), so tests can pin it.

## Interaction with PR #113 and evolution path

- **#113 (turn coordination)** changes _who calls_ `execute_turn`
  (enqueue/claim/lease above the executor); this pool changes _what happens
  inside_ it. Orthogonal; either can land first. Together: leases hand out up
  to K conversations, and K turns on the same module actually overlap instead
  of re-serializing at the runner mutex.
- **Option A (turn multiplexing)** later changes invariant 1's "one turn per
  process" to "≤ M turns per process" by adding exec-id correlation to the
  protocol. The pool structure survives intact — permits count turn slots
  instead of processes — and A shrinks the memory cost of a given
  concurrency level rather than replacing the pool. A should wait for #113's
  redelivery + resume, since a multiplexed process death aborts M turns.
- **Eval cleanup:** `--runner-pool` symlink machinery in
  `evaluation/agent-scale/driver` becomes obsolete (drive size via
  `EXO_TS_RUNNER_POOL` instead); delete the `.harness.poolN.ts` artifacts.

## Testing

- Unit (pool semantics, fake spawner): checkout beyond `max_size` blocks;
  LIFO reuse; error discards exactly one process; TTL prunes; waiter wakes on
  check-in.
- Integration: N concurrent `send`s on one module measurably overlap
  (wall-clock ≪ N × turn time with a slow mock model); sandbox `reuse_key`
  works across two different runners of one pool.

## Resolved review decisions

1. Default `max_size` = 4. Confirmed.
2. `IDLE_TTL` = 300 s const (was 60 s in draft; too short for a default).
3. No global reaper — per-check-in one-shot expiry (see Idle expiry above).
