# SREGym eval over self-evolving Exo

This recipe evaluates one durable Exo agent across SREGym's native live
Kubernetes incidents.

The runner clones a pinned SREGym revision with submodules under
`.local/sregym-evals/upstream/`, adds a passive `exo` agent registration, and
starts SREGym normally. For each incident, Exo attaches to SREGym's isolated
agent container. SREGym still owns the cluster, fault injection, network policy,
agent container, grading, timeout, and cleanup.

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

The run-scoped Exo state is retained under `.local/sregym-evals/<run>/exo`, so
the exact memory, artifacts, conversations, and tool state can be inspected
afterward. A new invocation starts with a fresh Exo agent by default.
