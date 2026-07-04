#!/usr/bin/env python3
"""Convert an arc_runner summary JSON into a board run JSON
(evaluation/shared/board/ — see its README for the schema).

  python3 board_emit.py results/evolve50.json --system exo-evolve \
      --label "exo evolve (gpt-5.5)" --benchmark arc-agi-1

Cost is computed as token counts x shared/board/prices.json — never taken from
a provider dashboard. `usage.input_tokens` includes cached reads (OpenAI
convention; arc_runner normalizes Anthropic usage to match).
"""
from __future__ import annotations

import argparse
import datetime
import json
import os

_HERE = os.path.dirname(os.path.abspath(__file__))
_BOARD = os.path.normpath(os.path.join(_HERE, "..", "shared", "board"))


def cost_usd(usage: dict, price: dict) -> float:
    uncached = usage["input_tokens"] - usage["cached_input_tokens"]
    return (uncached * price["input_per_mtok"]
            + usage["cached_input_tokens"] * price["cached_input_per_mtok"]
            + usage["output_tokens"] * price["output_per_mtok"]) / 1_000_000


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("summary", help="arc_runner --out JSON")
    ap.add_argument("--system", required=True)
    ap.add_argument("--label", required=True)
    ap.add_argument("--benchmark", required=True, help="e.g. arc-agi-1")
    ap.add_argument("--metric", default="pass@2")
    ap.add_argument("--out-dir", default=os.path.join(_BOARD, "runs"))
    args = ap.parse_args()

    s = json.load(open(args.summary))
    prices = json.load(open(os.path.join(_BOARD, "prices.json")))
    model = s["model"]
    if model not in prices["models"]:
        raise SystemExit(f"model {model} not in prices.json — add it first")
    price = prices["models"][model]

    total = cost_usd(s["usage_total"], price)
    score = s["pass_at_2"] if args.metric == "pass@2" else s["pass_at_1"]
    run = {
        "benchmark": args.benchmark,
        "system": args.system,
        "model": model,
        "label": args.label,
        "metric": args.metric,
        "score": score,
        "n_tasks": s["n"],
        "task_set": {"data_dir": s["data_dir"], "offset": s.get("offset", 0),
                     "task_ids": [r["task"] for r in s["results"]]},
        "cost": {
            "input_tokens": s["usage_total"]["input_tokens"],
            "cached_input_tokens": s["usage_total"]["cached_input_tokens"],
            "output_tokens": s["usage_total"]["output_tokens"],
            "total_usd": round(total, 4),
            "per_task_usd": round(total / s["n"], 4),
            "prices_as_of": prices["as_of"],
        },
        "per_task": [{"task": r["task"], "solved": r["solved"],
                      "solved_pass2": r["solved_pass2"],
                      "cost_usd": round(cost_usd(r["usage"], price), 4)}
                     for r in s["results"]],
        "run_config": {"backend": s.get("backend"), "harness": s.get("harness"),
                       "evolve": s.get("evolve", False),
                       "protocol": ("continual-learning (verdict feedback)"
                                    if s.get("evolve") else "stateless")},
        "created": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    }
    os.makedirs(args.out_dir, exist_ok=True)
    out = os.path.join(args.out_dir, f"{args.benchmark}_{args.system}.json")
    json.dump(run, open(out, "w"), indent=2)
    print(f"wrote {out}  (score={score:.1%}, ${run['cost']['per_task_usd']}/task)")


if __name__ == "__main__":
    main()
