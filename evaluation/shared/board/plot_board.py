#!/usr/bin/env python3
"""Render cost-vs-score board charts from runs/*.json (see README.md).

x = cost per task (USD, log scale), y = score %. One labeled point per
(system, model) run; one chart per benchmark.
"""
from __future__ import annotations

import argparse
import glob
import json
import os

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.ticker import FuncFormatter, LogLocator, NullFormatter, PercentFormatter

# Reference categorical palette (validated; assign in FIXED slot order, never cycle).
_SLOTS = ["#2a78d6", "#1baf7a", "#eda100", "#008300", "#4a3aa7", "#e34948",
          "#e87ba4", "#eb6834"]
# Stable slot assignment for known system families so colors follow the entity
# across charts and re-renders.
_SYSTEM_SLOT = {
    "exo-evolve": 0, "exo": 0, "exo-baseline": 4,
    "gpt-5.5-direct": 2, "openai-direct": 2,
    "opus-4.8-direct": 7, "anthropic-direct": 7,
    "openclaw": 1, "hermes": 5,
}
_INK = "#0b0b0b"
_MUTED = "#898781"
_GRID = "#e1e0d9"
_BASE = "#c3c2b7"
_SURFACE = "#fcfcfb"


def load_runs(runs_dir: str) -> list[dict]:
    runs = []
    for fp in sorted(glob.glob(os.path.join(runs_dir, "*.json"))):
        try:
            runs.append(json.load(open(fp)))
        except Exception as e:
            print(f"skipping {fp}: {e}")
    return runs


def color_for(run: dict, fallback_index: int) -> str:
    slot = _SYSTEM_SLOT.get(run.get("system", ""), fallback_index % len(_SLOTS))
    return _SLOTS[slot]


def plot_benchmark(benchmark: str, runs: list[dict], out: str) -> None:
    fig, ax = plt.subplots(figsize=(8, 5.5), dpi=150)
    fig.patch.set_facecolor(_SURFACE)
    ax.set_facecolor(_SURFACE)

    # legend ordered by score (desc) — beats direct labels once points cluster
    runs = sorted(runs, key=lambda r: -r["score"])
    metrics = {r.get("metric", "score") for r in runs}
    order = {r.get("system", ""): i for i, r in enumerate(
        sorted(runs, key=lambda r: r.get("system", "")))}
    for r in runs:
        x = r["cost"]["per_task_usd"]
        y = r["score"] * 100
        c = color_for(r, order[r.get("system", "")])
        ax.scatter([x], [y], s=90, color=c, zorder=3,
                   edgecolors=_SURFACE, linewidths=2,
                   label=f"{r.get('label', r.get('system', '?'))} — "
                         f"{r['score']:.0%}, ${r['cost']['per_task_usd']:.2f}/task")
    ax.legend(frameon=False, fontsize=8.5, loc="lower left", labelcolor=_INK)

    ax.set_xscale("log")
    ax.set_xlabel("Cost per task (USD, log scale)", color=_MUTED, fontsize=10)
    metric = metrics.pop() if len(metrics) == 1 else "score"
    ax.set_ylabel(f"Score ({metric}, %)", color=_MUTED, fontsize=10)
    ax.set_ylim(bottom=0)
    ax.margins(x=0.18)
    ax.yaxis.set_major_formatter(PercentFormatter(decimals=0))
    dollars = FuncFormatter(lambda v, _: f"${v:g}" if v >= 0.01 else f"${v:.3f}")
    ax.xaxis.set_major_locator(LogLocator(base=10, subs=(1.0, 2.0, 5.0)))
    ax.xaxis.set_major_formatter(dollars)
    ax.xaxis.set_minor_formatter(NullFormatter())

    ax.grid(True, which="major", color=_GRID, linewidth=0.6, zorder=0)
    ax.tick_params(colors=_MUTED, labelsize=9)
    for side in ("top", "right"):
        ax.spines[side].set_visible(False)
    for side in ("left", "bottom"):
        ax.spines[side].set_color(_BASE)

    n = {r.get("n_tasks") for r in runs}
    subtitle = f"{len(runs)} systems, {n.pop() if len(n) == 1 else 'varying'} tasks each"
    ax.set_title(f"{benchmark} — cost vs. score", color=_INK, fontsize=12,
                 loc="left", pad=14)
    ax.text(0, 1.015, subtitle, transform=ax.transAxes, color=_MUTED, fontsize=9)

    fig.tight_layout()
    fig.savefig(out, facecolor=_SURFACE)
    plt.close(fig)
    print(f"wrote {out}")


def main() -> None:
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description="Cost-vs-score board charts.")
    ap.add_argument("--runs-dir", default=os.path.join(here, "runs"))
    ap.add_argument("--benchmark", help="only this benchmark")
    ap.add_argument("--out", help="output path (single-benchmark only)")
    args = ap.parse_args()

    runs = load_runs(args.runs_dir)
    if not runs:
        raise SystemExit(f"no run JSONs in {args.runs_dir}")

    by_bench: dict[str, list[dict]] = {}
    for r in runs:
        by_bench.setdefault(r.get("benchmark", "unknown"), []).append(r)

    if args.benchmark:
        by_bench = {args.benchmark: by_bench.get(args.benchmark, [])}
        if not by_bench[args.benchmark]:
            raise SystemExit(f"no runs for benchmark {args.benchmark}")

    for bench, bruns in by_bench.items():
        out = args.out if (args.out and len(by_bench) == 1) else os.path.join(
            here, f"board_{bench}.svg")
        plot_benchmark(bench, bruns, out)


if __name__ == "__main__":
    main()
