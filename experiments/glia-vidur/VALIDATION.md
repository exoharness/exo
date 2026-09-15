# Local validation — 2026-09-15

Environment: Linux/aarch64 in Docker on macOS; Python 3.11.15. Source,
dependencies, workload, and configuration are pinned as described in README.md.

## Completed checks

- Built the experiment image from the pinned authors' Vidur commit.
- The built-in LLQ smoke simulation completed all 64 requests and wrote its
  manifest, request metrics, ledger, and summary. This uses a smaller timing
  predictor search and is **not a paper result**.
- Exo created a Glia agent and conversation, launched the Docker sandbox,
  imported Vidur, and accessed the mounted candidate and evaluator. This
  preflight did not invoke a model.
- The discovery evaluator refused evaluation 16 before starting Vidur when
  its fifteen trial slots were already occupied.
- Glia policy tests: 14 passed. Metric-validation tests: 3 passed.
- Glia CLI selection test, TypeScript typecheck, targeted lint/format checks,
  Rust formatting, Python compilation, and shell syntax checks passed.

The 512 MiB and 1 GiB containers used in additional custom-policy smoke checks
were killed during prediction-table loading. The supplied runner uses 1.5 GiB;
the initial built-in smoke check passed with that limit. Avoid concurrent
simulations on a Docker VM with only 2 GiB allocated.

## Full comparison

Both full runs failed during timing-predictor fitting, before producing a
full-workload response-time result. LLQ fitted and cached eleven of twelve
models, then a joblib worker was killed with `SIGKILL` while fitting the decode
model. HRA reused the cache and failed at the same stage. This is consistent
with memory exhaustion under the 1.5 GiB container limit; the worker traceback
alone does not establish the kill's cause. The smoke check did not establish
that this memory limit was sufficient for the full predictor grid.

Logs:

- LLQ: `.work/runs/20260915T220424Z-llq-65923/console.log`
- HRA: `.work/runs/20260915T221540Z-hra-65911/console.log`

Neither run wrote `result/summary.json`. No paper improvement is established.
The next attempt should provide more memory to both the Docker VM and the
container, retaining the existing cache and published predictor grid.

Raw local artifacts are under `.work/` (gitignored). No paid Glia discovery
run has been performed by these checks. A simulator result from the reference
HRA implementation would validate an algorithm; it would not establish that
our Glia executor independently discovered it.
