# Glia executor and Vidur experiment: handoff

## Request for the next session

Continue this work on the remote server. First get the full Vidur LLQ baseline
and published HRA reference comparison to complete with adequate memory, then
report measured results and any discrepancies from the paper. Keep the
implementation simple and opinionated. The longer-term goal is to compare Exo
and Glia executors on the same problem, model, and evaluation budget.

Repository: https://github.com/exoharness/exo

Branch: `glia-inspire`

Status recorded: 2026-09-15. No full benchmark result or paid Glia discovery run
has completed. Do not treat the passing smoke test as a reproduction result.

## Implemented

- `--harness glia` selects
  `exoharness/examples/typescript/glia-harness.ts` through the existing
  TypeScript executor support in `crates/cli/src/main.rs`.
- Single-context Researcher and Supervisor, using the same registered model.
  The Researcher gets the normal tool registry. The Supervisor gets public
  reports and task messages, with no tools, tool arguments/results, or private
  reasoning. Reviews happen every five Researcher calls and on proposed
  completion. Decisions are `continue`, `revise`, and `finish`.
- The default budget is 50 Researcher calls, with a final tool-free report.
  Usage for both roles and Supervisor feedback are saved as durable events.
  There is no multi-context search, automatic compaction, or dollar cap.
  Prompts and review cadence are our SCG-inspired implementation choices.
- Documentation: `website/docs-src/concepts/executors.md`.
- Experiment: `experiments/glia-vidur/README.md`, with setup, standalone
  scoring, baseline/reference policies, and a Glia discovery launcher.

## Experiment provenance and fidelity

- Paper: Hamadanian et al., _Glia_, arXiv:2510.27176v5,
  https://arxiv.org/abs/2510.27176v5. Figure 10 supplies the HRA reference.
- Authors' archive: https://zenodo.org/records/20077409. It points to
  `mit-nms/Engram` tag `v0`, commit
  `1fa3d52adcd3887ffead8ff4d2db299ae35b054c`. The archive is labeled Glia,
  although its README describes the later Engram framework.
- Vidur fork: https://github.com/mit-nms/vidur_simulator, pinned commit
  `2666cb64a622d9b8532791c6fdbe852cf3c1ae7f`.
- We match the released `SystemBench/vidur/env_evaluator.py` configuration:
  four simulated A10 replicas, Meta-Llama-3-8B, Sarathi, 8192-token chunks,
  2240 KV blocks, and the bundled nominal-7.5-QPS ShareGPT trace.
- The trace has 7,859 requests and SHA-256
  `135700d7ce3c6efd7e2b2de171d2e393683553b390f8d2553cd0a44a6dba6820`.
  All requests must finish; scoring rejects incomplete or invalid metrics.
- The paper describes vLLM and the Instruct model; the released evaluator
  uses Sarathi and the base-model performance profile. Other released scripts
  disagree on scheduler or QPS. These differences are documented, not resolved.
- Python dependencies are pinned in `requirements.lock`. The authors did not
  supply a fully pinned Vidur environment or fitted predictor cache. Record
  CPU architecture and package versions alongside results.
- The reference HRA file is deliberately excluded from the discovery image
  and workspace. Evaluating that reference does not show that our Glia policy
  independently discovered it.

## What passed, and what failed

- 14 Glia policy tests, 3 Python metric tests, and the Rust Glia CLI test passed.
  TypeScript typecheck, targeted lint/format checks, Rust formatting, Python
  compilation, and shell syntax checks also passed.
- The built-in LLQ smoke run completed all 64 requests. Smoke mode uses a much
  smaller predictor grid and a separate cache; its timings are not paper results.
- Exo launched the Vidur Docker sandbox and accessed the candidate/evaluator
  without calling a model. The evaluator refused a sixteenth trial as intended.
- Both full runs failed while fitting the decode timing predictor. Eleven of
  twelve timing models were fitted and cached first. Joblib reported a worker
  killed by `SIGKILL`, consistent with memory exhaustion. We did not obtain an
  OS-level OOM diagnostic, so the exact cause is not proven.
- The local Docker VM had about 2 GiB RAM; `run.sh` caps each container at
  **1536 MiB and two CPUs**. Moving to a larger machine alone does not remove
  that hardcoded cap. Increase it before retrying the full run.
- Detailed status: `experiments/glia-vidur/VALIDATION.md`.

## First steps on the remote server

1. Verify this branch and its files are present. Read the repository's
   `AGENTS.md`, then the experiment README and validation notes.
2. Check available host and Docker memory. Increase `run.sh`'s container
   memory allocation appropriately; 8 GiB is a reasonable next trial if the
   host has room, not a verified minimum. Keep the predictor grid, simulator,
   trace, and scoring unchanged while resolving resource limits.
3. Build and run from the repository root:

   ```bash
   ./experiments/glia-vidur/setup.sh
   ./experiments/glia-vidur/run.sh smoke
   ./experiments/glia-vidur/run.sh llq && ./experiments/glia-vidur/run.sh hra
   ```

   Setup needs network access and Docker. These simulations use CPU and
   bundled GPU timing profiles; they need no GPU, model weights, or API key.
   The first full run fits 27 predictor configurations with 10-fold
   cross-validation per operator. Give it time; later runs reuse its cache.

4. Inspect both `result/summary.json` files. Check that each completed all
   7,859 requests with matching workload, predictor settings, and environment.
   Calculate improvement as `100 * (1 - hra_mean / llq_mean)`. Preserve the
   manifests and original request metrics. Report actual results, even if they
   differ from the paper's roughly 40 s to under 23 s example.
5. Only after the simulator is working, proceed to a fresh Glia discovery run
   as described in the README. Build Exo and install TypeScript dependencies;
   the local build used Rust 1.95.0 (`cargo +1.95.0 build -p exo`). Register `o3`
   through Exo's normal secret/model commands. Never put API keys in this file.

   `run-agent.sh` makes paid model calls. It allows 15 evaluations including
   the baseline and failures, and 50 Researcher calls plus Supervisor reviews.
   It does **not** enforce the paper's $30 budget. The evaluation wrapper is a
   cooperative limit, not a security boundary; audit the final candidate and
   independently reevaluate it outside the agent's writable workspace.

## What Git does not transfer

- `experiments/glia-vidur/.work/` is intentionally ignored: downloaded Vidur
  source, fitted caches, images' IDs, raw run logs, and preflight state stay on
  the original machine. Setup can fetch the pinned source and regenerate caches.
- Docker images, `target/`, `node_modules/`, Exo secrets/state, and the original
  PDF in the Mac's Downloads directory are not part of this branch transfer.
- For optional recovery, the original workspace was
  `/Users/akrentsel/Documents/exo/glia-inspire`. Its full predictor cache is
  `experiments/glia-vidur/.work/cache/full`. The local runs used Linux/aarch64;
  prefer a fresh cache for an x86_64 server and document that environment.
- Local failed run directories were
  `.work/runs/20260915T220424Z-llq-65923` and
  `.work/runs/20260915T221540Z-hra-65911` under the experiment directory.

Keep this handoff and `VALIDATION.md` current as verified results arrive.
