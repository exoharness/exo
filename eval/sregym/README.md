# SREGym eval over self-evolving Exo

This recipe evaluates one durable Exo agent across SREGym's native live
Kubernetes incidents.

The runner clones a pinned SREGym revision with submodules under
`.local/sregym-evals/upstream/`, applies `sregym.patch`, and starts SREGym
normally. The patch registers a passive `exo` agent, exempts it from provider
egress rules and from SREGym's credential forwarding (Exo calls its model from
the host, so the agent container gets neither API keys nor Codex auth), lets
the runner add bind mounts to the agent container, and adds an optional review
hold used by `--reflection`.

Each run gets its own git worktree of this repository's committed `HEAD` at
`<run>/exo-source`, built with `pnpm install` and `cargo build`. As in a
regular `exo.sh` launch, that tree is mounted read-write at `/workspace/exo`
inside the container Exo works in, so Exo can inspect and edit its own code
without touching the checkout the runner was started from (uncommitted changes
there are not part of the run). `rebuild_and_restart_exo` runs the service
guardian in the worktree with `EXO_ROOT` set to the run's Exo state; a
successful build replaces the worktree's `target/debug/exo`, which the next
turn picks up, and a failed build leaves the previous binary in place. Exo's
TypeScript harness loads fresh every turn, so a harness edit is live at once.
The runner stops any guardian-started scheduler and adapters when the
evaluation ends, and removes the worktree after a completed run (the policy
repository keeps its final source). Files Exo writes through the mount are
owned by root on the host. For each incident, Exo attaches to SREGym's isolated
agent container. SREGym still owns the cluster, fault injection, network policy,
agent container, grading, timeout, and cleanup.

Nothing in Exo validates a source edit before it is live, and a harness that
fails to load fails every later turn, including the one Exo would need to
repair it. So after any trial that changed the worktree, the runner sends a
tool-free probe turn ("reply ok") in a separate `health-*` conversation
attached to a throwaway `alpine` container. If the turn completes, the change
is committed in the worktree as the new known-good state; if it fails, the
worktree is reset to the previous one (and rebuilt), and the policy
repository gets a commit saying so. Every check is appended to
`<run>/source-checks.json`. This is the runner's backstop, not part of Exo.

Each incident gets a fresh conversation, while memory, skills, and installed
tools remain attached to the same run-scoped Exo agent. Trials are deliberately
sequential so later incidents can benefit from earlier self-improvements.

## Requirements

- Linux with at least 8 vCPU, 16 GB RAM, and 100 GB disk for SREGym-Lite
- Python 3.12 (`python3.12`), Docker, Helm 4+, kubectl, Kind, uv, Cargo, Node.js, and pnpm
- `OPENAI_API_KEY`, or another key selected with `--api-key-env`

Create the pinned checkout by starting the first evaluation. If SREGym reports
that no cluster exists, initialize Kind from that checkout and rerun:

```bash
cd eval/sregym
./eval.sh --problem network_policy_block
bash ../../.local/sregym-evals/upstream/SREGym/kind/setup_kind_cluster.sh
./eval.sh --problem network_policy_block
```

Run one incident first:

```bash
./eval.sh --problem network_policy_block --profile svelte
```

Run the leaderboard-compatible 21-problem suite:

```bash
./eval.sh --suite sregym-lite --profile full
```

Use `--provider-model` when Exo's local model name differs from the provider
model ID. `--judge-model` independently selects the SREGym diagnosis judge.

## Control arm without self-modification

`--exo-profile memory-only` runs Exo's `memory-only` profile: memory stays
writable and installed skills stay usable, but there is no `install_skill`,
`manage_tool`, `install_agent_tool`, or `rebuild_and_restart_exo`, and the
source tree is mounted read-only. The reflection prompt drops its sentences
about tools, skills, and code changes. Compare it against the default
`practical` profile to measure what self-modification adds.

## Reflection

Each task prompt ends with a short brief telling Exo it may modify itself
(inspect and change its code, build tools, add skills, remember facts; on
`memory-only`, remember only) where its own observations suggest it would
help, that the right answer comes first and speed and cost second, and that
what it builds persists across incidents.

With `--reflection`, SREGym holds each graded incident's cluster instead of
tearing it down at once. The runner reads the grades from SREGym's API and
sends them to Exo as one more turn in the incident's conversation, with the
fault still live for inspection. The reflection prompt is deliberately light:
it presents the grades, notes the environment is still up, and says to act
only if something would help get future incidents right or solve them faster
or more cheaply, and to say so if nothing is worth keeping. An earlier, more
directive wording produced a new tool and skill after nearly every incident.
The runner releases the hold afterwards and SREGym tears down. Submissions are
already closed during review, so the grades cannot change. The prompt text a
run used is recorded in its `run.json`.

Reflection shows Exo the answer, so it refuses `--n-attempts` above 1.
`--reflection-timeout` bounds the reflection turn (default 900s); SREGym's own
hold deadline is two minutes longer. Time spent in review does not count
against SREGym's agent timeout.

```bash
./eval.sh --suite sregym-lite --profile full --reflection
```

## Learn, then test on unseen incidents

`--test-suite` (or `--test-problem`) adds a second phase: after the first
selection finishes, the same agent, with everything it built, runs the test
selection with reflection off, so no grades are revealed there. What it
learned is measured on incidents it has never seen. The patch adds the
`sregym-lite-transfer` problem set for this: 20 incidents from the same fault
families as SREGym-Lite but held out of it, first the same fault injected into
a different application, then each lite problem's closest relatives.

```bash
./eval.sh --suite sregym-lite --test-suite sregym-lite-transfer \
  --profile full --reflection --model gpt-5.6-sol --judge-model gpt-5
```

Each phase is its own SREGym invocation and results batch, mirrored as
`harbor-jobs/<run>-learn` and `<run>-test`. Policy commits carry the phase in
their subject. To resume an interrupted test phase, pass `--resume-phase
test` with the usual `--resume <csv> --run-dir <run>`.

## Outputs

SREGym writes its standard CSV and run artifacts beneath the pinned checkout's
`results/` directory. Every native run directory also receives:

- `exo-trajectory.json`: Exo's canonical messages and tool events.
- `exo-self-improvements.json`: durable mutation actions such as remembered
  facts, installed tools, installed skills, and self-rebuild requests.

After the run, `postprocess.py` converts each `exo-trajectory.json` into an
ATIF `trajectory.json` beside it, where SREGym's own trace postprocess writes
trajectories for the agents it knows. It also mirrors the batch as a Harbor job
under `.local/sregym-evals/harbor-jobs/<run>`, with SREGym's diagnosis and
mitigation grades as rewards and the judge's reasoning as verifier output:

```bash
eval/sregym/.venv/bin/harbor view .local/sregym-evals/harbor-jobs
```

To convert an earlier batch, pass its SREGym results directory:

```bash
eval/sregym/.venv/bin/python eval/sregym/postprocess.py \
  .local/sregym-evals/upstream/SREGym/results/<batch>
```

Exo's policy has three homes: this source tree (which Exo edits through the
`/workspace/exo` mount), agent-built tools under `.exo/agent-tools`,
`.exo/tools`, and `.exo/tool-sources` in that tree, and memory and skills
stored as versioned artifacts in the run's Exo root. Each run keeps a git
repository at `.local/sregym-evals/<run>/policy` that brings them together:
`source/` (tracked and untracked files), `tools/`, and `agent/` (the latest
memory and skill artifacts). Its first commit is the policy as the run began;
every graded incident adds a commit named after the trial and its grades, and
the run ends with a final commit, so `git log -p` in that directory shows what
each reflection changed.

A run starts with no inherited tools: the runner removes the `.exo` tool
directories first (the previous run's copies live in its policy repository).
Files the agent container wrote as root are reclaimed with a throwaway
`alpine` container before they are removed or copied.

The run-scoped Exo state is retained under `.local/sregym-evals/<run>/exo`, so
the exact memory, artifacts, conversations, and tool state can be inspected
afterward. A new invocation starts with a fresh Exo agent by default.
