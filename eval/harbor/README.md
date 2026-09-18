# Harbor evals over Exoharness

Runs exoharness executors as a Harbor external agent to enable valuating over Harbor's
benchmark catalog.

`ExoSessionPlugin` creates the shared Exo agent once per job and, with
`--reflection`, reviews each graded trial. `ExoAgent` drives each trial: it
attaches Harbor's task container to a fresh conversation, sends the task as one
turn, exports the trajectory, and captures the submitted state when reflection
is enabled.

Each trial gets its own conversation, so no earlier trial's context leaks into
the next one. Learnings appear in the agent-scoped durable state: memory, installed
tools, and Exo's own source code changes. The exo harness therefore runs trials
in sequence: `--n-concurrent` must stay at 1 so each trial sees what earlier
trials learned. The basic and pi harnesses carry no agent state, so they may
run with `--n-concurrent` above 1.

Use `--harness=exo` (the default) for Exo's tools and memory,
`--harness=basic` for a shell-only control, or `--harness=pi` to drive the
Pi coding agent through Exo. Pi runs inside the task container, so that arm
installs Node and pi there at the start of each trial.

## How a trial runs in Harbor's container

Harbor builds and owns the task container. The agent looks the container up by
its Compose labels, attaches it to the trial's conversation with
`exo sandbox attach`, and selects it with `exo sandbox select`. Attaching only
registers the container as a sandbox the conversation has; selection is what
makes the conversation's turns run there, and it is recorded as a
`sandbox_selected` event so the log says which sandbox every turn used. Harbor
removes the container after grading; Exo only borrows it and never stops or
deletes it.

Sandbox commands go through `exo sandbox <verb> --agent <agent> --conversation
<trial>`, which prints a bare id. Naming the conversation as the owner is what
makes the sandbox usable by the trial: sandbox records live under their owner,
and a conversation cannot address one its agent owns.

## Reflection

Reflection is opt-in (`--reflection`) and only meaningful with the exo harness.
Harbor grades a trial and removes its container before any hook can run, so
before the trial returns the agent forks the attached container into a new
Exo-owned sandbox with `exo sandbox fork`. After grading, the plugin selects
that fork in the trial's conversation, sends the grader feedback as one more
turn, re-exports the trajectory, and terminates the fork. Verification cannot
change the state being reviewed, and each trial's fork is cleaned up whether
reflection succeeds or fails.

Forking an attached Docker container reads its state through the borrowed
handle (`docker commit`) and restores it into an owned target. Ownership stays
unchanged: Exo may run, snapshot, or fork an attachment, but it may only
detach it, never stop or terminate it.

Trials that fail to fork skip reflection but keep their trajectory.

## Running

Requires Python 3.12+, Docker, Cargo, Node.js, pnpm, and `OPENAI_API_KEY`.
The wrapper installs the Python package into `.venv` and builds the Exo binary.

```bash
cd eval/harbor
./eval.sh --dataset=terminal-bench
```

It defaults to GPT-5.5, all tasks in the dataset, and one attempt. Flags can
limit or override those defaults:

```bash
./eval.sh --dataset=terminal-bench --model=gpt-5.5 --n-tasks=10 --n-attempts=2
```

Select particular tasks by repeating `--include-task-name`:

```bash
./eval.sh --dataset=terminal-bench \
  --include-task-name=path-tracing \
  --include-task-name=gpt2-codegolf
```

The equivalent TOML field is `include_task_names = ["path-tracing",
"gpt2-codegolf"]`.

For a quick end-to-end check using the bundled tiny task:

```bash
./eval.sh --dataset=smoke-test --n-tasks=1
# Also exercise fork, feedback, and reflection cleanup:
./eval.sh --dataset=smoke-test --n-tasks=1 --reflection
```

To test self-evolution across trials, run the bundled dataset. It checks that
custom tools persist across trials and exercises restart and timeout handling.

```bash
./eval.sh --dataset=self-evolution-smoke-test
```

For a short real benchmark run, `terminal-bench-easy` selects three Terminal
Bench 2 tasks marked easy: `fix-git`, `prove-plus-comm`, and
`cobol-modernization`.

```bash
./eval.sh --dataset=terminal-bench-easy
```

A local dataset directory that contains a `task_order.json` (a JSON list of
task directory names in run order) is treated as an _ordered_ dataset: the
runner passes its tasks to Harbor individually, in that order, instead of as
a `--path` dataset, because Harbor discovers `--path` tasks with
`Path.iterdir()` and the filesystem does not guarantee that order. Ordered
datasets are how continual-learning sequences run — each episode assumes the
agent has seen the previous ones. `--n-tasks=N` takes the first N episodes of
the sequence, and `--n-attempts=2` replays the whole sequence a second time.
For example, with the Continual Learning Bench database-exploration port:

```bash
./eval.sh --dataset=clbench-database-exploration \
  --dataset-path=/path/to/continual-learning-bench/harbor/datasets/database-exploration
```

For repeatable runs, copy [`eval.example.toml`](eval.example.toml), edit it,
and run:

```bash
./eval.sh --config=my-eval.toml
```

For another provider, set `--api-key-env`, `--base-url`, and optionally
`--provider-model` when the upstream model ID differs from the local `--model`
name. These flags also work as `api_key_env`, `base_url`, and `provider_model`
in the TOML config.
