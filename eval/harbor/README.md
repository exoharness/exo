# Exo agent for Harbor

Runs Exo as a Harbor external agent so it can be evaluated over Harbor's
benchmark catalog. The Exo-side adapter that handles waking Exo's
conversation and waiting on a response lives in
[`examples/exo/adapters/harbor`](../../examples/exo/adapters/harbor).

## The split

Harbor has two lifecycle planes relevant to Exo:

1. `BaseAgent`, which is constructed fresh for each trial and controls what
   happens during the trail.
2. `Plugin`, which lives for the duration of a full job and exposes hooks for
   initial agent setup, per-trial completions, and final Exo tear-down.

|                                                            | Owns                                                                    |
| ---------------------------------------------------------- | ----------------------------------------------------------------------- |
| [`plugin.py`](src/exo_harbor/plugin.py) `ExoSessionPlugin` | Exo's lifetime, the adapter, and the feedback loop. Job-scoped.         |
| [`agent.py`](src/exo_harbor/agent.py) `ExoAgent`           | One task: attach the container, send it, wait. Trial-scoped, stateless. |

Thus the end-to-end lifecycle looks like this:

```
attach_job_plugins(job)
  on_job_start(job)          start Exo, create adapter (fixed slug + socket)
  job.run()
    Trial.create()           new ExoAgent
      setup(env)             attach Harbor's container      [once per trial]
      run(instruction)       task_started -> task_complete  [once per step]
      resume(instruction)    same, for multi-step continuation
      <verifier runs>
      TrialEvent.END         detach, verification_result -> feedback_processed
    ... next trial ...
  on_job_end(result)         tear down
```

Nit: run() is called per-_step_, which is a construct that exists for some
types of tasks where there are intermediate checkpoints to specify. For most
common task-sets, such as terminal-bench, there is only one default step.

## The two channels

Harbor constructs the agent and the plugin independently and never introduces
them, so each direction needs its own path:

- **plugin → agent**: nothing is passed. Everything the agents need is either
  already an `--ak` they hold or derivable from
  [`conventions.py`](src/exo_harbor/conventions.py) — the Exo agent by a fixed
  slug (agent refs resolve by slug as well as UUID), the adapter socket by a
  path under `exo_root`. No ids have to travel.
- **agent → plugin**: the agent writes `AgentContext.metadata["exo"]`, which
  Harbor carries into `TrialResult` for the plugin to read at trial end.

The plugin recovers `exo_root` from `job.config.agents[0].kwargs` rather than
taking its own `--pk` copy, so the path is written down once.

Because nothing is handed over, `setup()` instead probes the adapter socket as
a preflight. That is what catches a run launched with `--agent` but no
`--plugin`: every convention would still resolve to something plausible, so the
job would run every task, score them, and report a result while Exo never
received a single `verification_result`. A live probe is a stronger check than
a marker file, which only proves something was written at some point and can be
stale from an earlier run in a reused `exo_root`.

## Running

```bash
pip install -e eval/harbor
pnpm install
cargo build -p exo

harbor run \
  --env docker \
  --n-concurrent 1 \
  --agent exo_harbor.agent:ExoAgent \
  --plugin exo_harbor.plugin:ExoSessionPlugin \
  --ak exo_root="$PWD/.local/run/exo" \
  --ak exo_bin="$PWD/target/debug/exo" \
  --ak exo_model=<registered-exo-model> \
  --dataset hello-world@1.0
```

Both `--agent` and `--plugin` are required. `--n-concurrent 1` is enforced:
trials share one Exo, and reflection on trial N must complete before trial N+1
starts.

## Status

Skeleton. Every module is signatures and comments; nothing is implemented.
Pinned to `harbor>=0.20,<0.21` — the `JobPlugin` protocol and the
`TrialEvent.END` payload are the parts most likely to move on a bump.
