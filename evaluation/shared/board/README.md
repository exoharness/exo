# Board — cost-vs-score leaderboard plots (shared across benchmarks)

One scatter per benchmark, ARC-Prize-leaderboard style: **x = cost per task
(USD, log)**, **y = score %**. Each point is one (system, model) run. Works for
any of our benchmarks (arc-agi, terminal-bench, continual-learning-bench) — the
per-benchmark eval emits a small run JSON here and `plot_board.py` does the rest.

## Layout

```
evaluation/shared/board/
  prices.json        # pinned $/Mtok — cost is ALWAYS tokens x these prices
  plot_board.py      # runs/*.json -> board_<benchmark>.svg
  runs/              # one JSON per (benchmark, system, model) run — committed
```

## Run JSON schema

```jsonc
{
  "benchmark": "arc-agi-1",            // groups runs onto one chart
  "system": "exo-evolve",              // machine name
  "model": "gpt-5.5",
  "label": "exo evolve (gpt-5.5)",     // what the chart prints
  "metric": "pass@2",                  // y-axis meaning for this benchmark
  "score": 0.42,                       // 0..1
  "n_tasks": 50,
  "task_set": {"version": 1, "split": "evaluation", "offset": 0,
                "task_ids": ["..."]},  // exact subset, for reproducibility
  "cost": {
    "input_tokens": 123456, "cached_input_tokens": 0, "output_tokens": 7890,
    "total_usd": 1.23, "per_task_usd": 0.025,
    "prices_as_of": "2026-07-01"       // must match prices.json
  },
  "per_task": [{"task": "...", "solved": true, "cost_usd": 0.02}],  // optional
  "run_config": {},                    // free-form: harness, flags, protocol notes
  "created": "2026-07-01T21:00:00Z"
}
```

Rules:
- **Cost = token counts × `prices.json`.** Never copy a dollar figure from a
  provider dashboard; different systems meter differently.
- Same `task_set` across every run on one chart, or the chart is meaningless.
- Note protocol differences in `label`/`run_config` (e.g. exo-evolve uses a
  continual-learning protocol with verdict feedback; the others are stateless).

## Plot

```bash
python3 plot_board.py                 # all benchmarks found in runs/
python3 plot_board.py --benchmark arc-agi-1 --out board.svg
```
