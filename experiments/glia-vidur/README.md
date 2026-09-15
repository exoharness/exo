# Glia / Vidur experiment

Two separate questions:

1. **Algorithm reproduction:** do LLQ and the paper's Figure 10 HRA scheduler
   produce the reported response-time improvement in the released simulator?
2. **Discovery reproduction:** can our single-context Glia executor discover
   a good scheduler, starting from LLQ, with a limited experiment budget?

This directory prepares both. It does not claim the paper's results have been
reproduced merely because a simulator or an agent runs.

See [VALIDATION.md](VALIDATION.md) for checks actually performed and the current
reproduction status.

## Run

From the repository root, with Docker running:

```bash
./experiments/glia-vidur/setup.sh
./experiments/glia-vidur/run.sh smoke      # plumbing only; NOT a paper result
./experiments/glia-vidur/run.sh llq        # full published trace
./experiments/glia-vidur/run.sh hra        # Figure 10 reference implementation
./experiments/glia-vidur/run.sh lor
./experiments/glia-vidur/run.sh round_robin
```

No GPU, model weights, Hugging Face token, or model API key is needed for these
simulations. The first full run fits timing predictors (27 hyperparameter
combinations × 10-fold cross-validation per operator); allow substantial CPU
setup time. Later runs reuse a cache. Runs use up to two CPU cores and 1.5 GiB
of memory. Smoke mode uses 64 requests and a smaller predictor search, with a
separate cache. Its timing results must not be compared with the paper.

Each invocation creates `.work/runs/<timestamp>-<policy>-<pid>/` containing
`console.log`, `image-id.txt`, and `result/`. The result includes:

- `manifest.json`: simulator/trace/candidate hashes, seed, package versions,
  full simulation arguments, and smoke/full label.
- `summary.json`: mean and P90 response time in seconds, queueing/execution
  time, restarts, completed request count, and elapsed CPU-run wall time.
- `simulator/<timestamp>/`: original config, per-request CSVs, scheduling logs.
- `ledger/`: the simulator's own code/config snapshot.
- `candidate.py`: exact evaluated source for custom policies.

A run fails if any request is unfinished, metrics are missing, or response
times are invalid. Failed runs retain logs but do not receive a summary score.
Compare mean response time: `100 * (1 - candidate_mean / llq_mean)` is the
percentage reduction. Both runs must have the same mode, trace, predictor
configuration, seed, and environment.

## Run our Glia executor

Build Exo (`cargo build -p exo`, with the project's Rust version), install the
repository's TypeScript dependencies, and register `o3` in the usual Exo root:

```bash
# OPENAI_API_KEY must already be set; do not put its value in command arguments.
./target/debug/exo secret set openai --env OPENAI_API_KEY
./target/debug/exo model register o3 --secret openai
./experiments/glia-vidur/run-agent.sh
```

**`run-agent.sh` makes paid model calls.** It uses the existing registered `o3`
binding, a fresh agent/conversation and workspace, and the Vidur Docker image.
`EXO_BIN` and `EXO_ROOT` select an existing binary/root if needed; use the same
root when registering the model. The sandbox has networking and tool creation
disabled; model calls happen through the host runtime.

The identical task is saved in `TASK.md`. The Researcher starts with LLQ and
runs `python /opt/experiment/evaluate.py` inside its sandbox. The wrapper allows
15 simulations, counting failures and the initial baseline, and archives each
candidate. One simulation has a 1-hour wall timeout. These are evaluation
wrapper limits, not a security boundary: audit the trial history and final code
for compliance. The agent can inspect the simulator and its bundled baseline
implementations, just as it can inspect other environment code. Our HRA
reference file is not included in the image or discovery workspace.

The executor permits 50 Researcher calls plus Supervisor reviews. **This is
not the paper's $30 optimization budget:** dollar spending is currently
recorded by Exo, not capped here. Do not launch ten runs expecting a $300 cap.
Model/provider availability and our reconstructed role prompts also prevent
bit-for-bit reproduction of the original discovery trajectory.

On completion, `run-agent.sh` prints the command to independently reevaluate
the final restored `candidate.py` in a fresh container, outside the writable
agent workspace. Verify the final code obeys the task's no-future-decode-length
constraint before interpreting its score. The source of the best trial and
its metrics remain available even if the executor hits its round limit.

## Pinned provenance and fidelity

- Paper: Hamadanian et al., _Glia_, **arXiv:2510.27176v5**, §5 and Figure 10.
  [Paper](https://arxiv.org/abs/2510.27176v5).
- Authors' archived release: [Zenodo 20077409](https://zenodo.org/records/20077409),
  `mit-nms/Engram` tag `v0`, commit
  `1fa3d52adcd3887ffead8ff4d2db299ae35b054c`. The archive is labeled Glia,
  but its README/artifact appendix describes the later Engram framework.
- Its Vidur submodule: [mit-nms/vidur_simulator](https://github.com/mit-nms/vidur_simulator),
  commit `2666cb64a622d9b8532791c6fdbe852cf3c1ae7f` (`glia_compat`).
- Configuration: the archived
  `SystemBench/vidur/env_evaluator.py`, Sarathi branch. Four A10 replicas,
  `meta-llama/Meta-Llama-3-8B`, TP=PP=1, 2240 KV blocks of 16 tokens,
  8192-token prefill chunks, batch cap 128, 1% watermark, memory margin 0.2445.
- Trace: bundled `sharegpt_7.5.csv`, SHA-256
  `135700d7ce3c6efd7e2b2de171d2e393683553b390f8d2553cd0a44a6dba6820`.
  **7,859 requests; last arrival 999.7999526730711 s.** Replay the trace unchanged
  and drain all requests, including completions after 1000 s. The nominal
  rate is 7.5 QPS; the finite sample realizes about 7.86 QPS.
- The bundled trace generator uses log-normal intervals with sigma 2 and
  seeds 192/151. Its script applies 5% prompt/decode inflation by up to 10×,
  with token caps. We use the published CSV; regenerating with another
  tokenizer, seed, or inflation rule would change the experiment.
- The paper describes Llama-3-8B-Instruct and vLLM with chunked prefill. The
  released evaluator names the base Llama-3-8B performance profile and Sarathi;
  `run_single.sh` instead chooses vLLM, while `run_all.sh` defaults to 28 QPS.
  We deliberately use the evaluator's 7.5-QPS setup. These discrepancies
  must be resolved before claiming an exact numerical reproduction.
- Python packages are pinned for this experiment; the release did not pin
  all Vidur dependencies or supply fitted timing predictors. A different
  scikit-learn version can change predictions. The CPU architecture and
  dependency versions should accompany reported results.

## Next comparison

First validate LLQ/HRA on this fixed workload. Then run independent Glia
optimization trials (the paper reports ten seeds, 90% bootstrap intervals,
$30 per optimization, and a 15-simulation SCG comparison). Repeating a
deterministic scheduler on the same CSV with different simulator seeds is
**not** ten independent discovery trials or ten different workloads.

For Exo versus Glia, preserve the same benchmark, model, initial candidate,
per-trial evaluation budget, and separate workspaces. Include Supervisor calls
in Glia's cost. Fix or report the executor tool-set differences. Keep scoring
outside both executors; do not give the published HRA solution to discovery
agents. This directory does not add a general experiment orchestration system.
