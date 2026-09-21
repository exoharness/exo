# Harbor evals over Exoharness

Runs exoharness executors as a Harbor external agent to enable valuating over Harbor's
benchmark catalog.

`ExoSessionPlugin` creates the shared Exo agent once per job. `ExoAgent`
drives each trial: it attaches Harbor's task container to a fresh
conversation, sends the task as one turn, and exports the trajectory.

Each trial gets its own conversation, so no earlier trial's context leaks into
the next one. Learnings appear in the agent-scoped durable state: memory, installed
tools, and Exo's own source code changes. The exo harness therefore runs trials
in sequence: `--n-concurrent` must stay at 1 so each trial sees what earlier
trials learned. The basic and pi harnesses carry no agent state, so they may
run with `--n-concurrent` above 1.

Use `--harness=exo` (the default) for Exo's tools and memory,
`--harness=basic` for a shell-only control, or `--harness=pi`,
`--harness=claude-code` or `--harness=codex` to drive that coding agent through
Exo. Those three run inside the task container, so their arms install Node and
the agent there at the start of each trial.

## How a trial runs in Harbor's container

Harbor builds and owns the task container. The agent looks the container up by its Compose labels and attaches it to the trial's conversation with `exo conversation sandbox attach`. The executor runs every turn of a conversation in its attached sandbox, so nothing else is needed to make the task run there. Harbor removes the container after grading; Exo only borrows it and never stops or deletes it.

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
```

To test self-evolution across trials, run the bundled dataset. It checks that
custom tools persist across trials and exercises restart and timeout handling.

```bash
./eval.sh --dataset=self-evolution-smoke-test
```

SWE-bench Lite is not in Harbor's registry. Generate its 300 tasks once with
[`datasets/swebench-lite/generate.py`](datasets/swebench-lite/generate.py),
then run them like any other dataset:

```bash
./eval.sh --dataset=swebench-lite --n-tasks=3
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

Claude Code only speaks the Anthropic Messages API and Codex only the OpenAI
Responses API, so `--harness=claude-code` on an OpenAI model, or
`--harness=codex` on an Anthropic one, needs a gateway that serves the model in
the CLI's format. With no `--base-url`, the eval runs one itself for the length
of the job: a LiteLLM proxy on the host, forwarding to the provider with the
key named by `--api-key-env`. It serves TLS with a certificate generated for
the Docker bridge address, because Harbor's egress sidecar proxies plain HTTP
and drops a response whose headers take over about 15 seconds (routine for a
slow model on a large prompt), while it passes TLS through untouched. Each
trial installs the certificate into its task container at setup. The key
variable has to be the provider's own
(`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, ...), since LiteLLM reads it by name:

```bash
./eval.sh --dataset=terminal-bench-easy --harness=claude-code \
  --model=gpt-5.5 --api-key-env=OPENAI_API_KEY
./eval.sh --dataset=terminal-bench-easy --harness=codex \
  --model=claude-sonnet-4-6 --api-key-env=ANTHROPIC_API_KEY
```

The same gateway serves a plain exo agent outside the eval. Run it, register
the model against the URL it prints, and create the agent as usual. A plain exo
Docker sandbox has no egress proxy, so `--no-tls` avoids having to trust a
certificate there:

```bash
OPENAI_API_KEY=... .venv/bin/python -m exo_harbor.gateway --no-tls gpt-5.5
```

`eval.sh` sets the environment up with [uv](https://docs.astral.sh/uv/), which
has to be installed. pip cannot install `litellm[proxy]` next to harbor because
litellm pins `rich<14` for its admin CLI alone; `overrides.txt` lifts the pin
for uv, and https://github.com/BerriAI/litellm/pull/42322 asks upstream to.

A `--base-url` skips the local gateway and points Claude Code at a hosted one
instead. OpenRouter serves its whole catalog in Anthropic format, and the
OpenAI-style `/v1` URL exo's other harnesses take works here too:

```bash
./eval.sh --dataset=terminal-bench-easy --harness=claude-code \
  --model=openai/gpt-5.5 --api-key-env=OPENROUTER_API_KEY \
  --base-url=https://openrouter.ai/api/v1
```
