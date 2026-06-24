#!/usr/bin/env python3
"""Build a Continual Learning Bench report from clbench result traces.

Reads the latest live/ trace (baseline.json = stateless, run_*.json = exo+memory)
for each task under <clbench>/results/, computes Gain (= memory reward - stateless
baseline), and writes REPORT.md + graphs into reports/<timestamp>/.

Usage: gen_report.py [clbench_repo]   (default: ../../../clbench from this file)
"""
import glob
import json
import os
import sys
import datetime as dt

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
# clbench clone is a sibling of the exo repo: evaluation/continual-learning-bench
# is 3 levels below the exo repo root, whose parent holds the clone.
CLBENCH = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(HERE))), "clbench")
# Only the tasks measured under Docker isolation (agent's shell cannot reach the
# host's clbench checkout / ground-truth files). database_exploration,
# codebase_adaptation, and exploitable_poker need honest isolated re-measurement
# (their prior numbers came from a host-exposed shell and are NOT trustworthy).
TASKS = ["blind_spectrum_monitoring", "sales_prediction", "cohort_studies"]


def latest_live(task):
    dirs = sorted(glob.glob(os.path.join(CLBENCH, "results", task, "live", "*")))
    return dirs[-1] if dirs else None


def rewards_list(path):
    """All instance rewards in order. Gain = total memory reward - total baseline
    reward (clbench's definition); some tasks reuse instance_index, so we must NOT
    key by it — just sum every outcome."""
    if not path or not os.path.exists(path):
        return []
    d = json.load(open(path))
    return [o["reward"] for o in d.get("instance_outcomes", [])
            if isinstance(o.get("reward"), (int, float))]


def main():
    ts = dt.datetime.now().strftime("%Y-%m-%d__%H-%M-%S")
    outdir = os.path.join(HERE, "reports", ts)
    os.makedirs(outdir, exist_ok=True)

    rows = []
    for task in TASKS:
        live = latest_live(task)
        if not live:
            continue
        b = rewards_list(os.path.join(live, "baseline.json"))
        runs = sorted(glob.glob(os.path.join(live, "run_*.json")))
        m = rewards_list(runs[0]) if runs else []
        if not b and not m:
            continue
        rows.append({
            "task": task, "n": len(m),
            "baseline_total": sum(b), "memory_total": sum(m),
            "gain": sum(m) - sum(b),
            "b": b, "m": m,
        })

    rows.sort(key=lambda r: r["gain"], reverse=True)

    # --- Graph 1: Gain per task (bar) ---
    fig, ax = plt.subplots(figsize=(8, 4.5))
    names = [r["task"] for r in rows]
    gains = [r["gain"] for r in rows]
    colors = ["#2a9d8f" if g > 0 else "#e76f51" for g in gains]
    ax.barh(names, gains, color=colors)
    ax.axvline(0, color="#333", lw=0.8)
    ax.set_xlabel("Gain (exo+memory total reward − stateless baseline)")
    ax.set_title("Continual Learning Bench — Gain by task (exo+gpt-5.5, memory)")
    for y, g in enumerate(gains):
        ax.text(g, y, f" {g:+.2f}", va="center", fontsize=9)
    fig.tight_layout(); fig.savefig(os.path.join(outdir, "gain_by_task.png"), dpi=130); plt.close(fig)

    # --- Graph 2: per-task cumulative reward, baseline vs memory ---
    n = len(rows)
    fig, axes = plt.subplots(1, n, figsize=(4 * n, 3.6), squeeze=False)
    for ax, r in zip(axes[0], rows):
        cb = [sum(r["b"][:k]) for k in range(1, len(r["b"]) + 1)]
        cm = [sum(r["m"][:k]) for k in range(1, len(r["m"]) + 1)]
        ax.plot(range(1, len(cb) + 1), cb, "--o", ms=3, color="#888", label="stateless baseline")
        ax.plot(range(1, len(cm) + 1), cm, "-o", ms=3, color="#2a9d8f", label="exo + memory")
        ax.set_title(f"{r['task']}\nGain {r['gain']:+.2f}", fontsize=9)
        ax.set_xlabel("instance"); ax.set_ylabel("cumulative reward")
        ax.legend(fontsize=7)
    fig.suptitle("Cumulative reward over instances — memory vs stateless", y=1.03)
    fig.tight_layout(); fig.savefig(os.path.join(outdir, "cumulative_reward.png"), dpi=130, bbox_inches="tight"); plt.close(fig)

    # --- REPORT.md ---
    md = ["# Continual Learning Bench — Exo (gpt-5.5) results\n",
          f"_Generated {ts}. Gain = exo+memory total reward − stateless-reset baseline "
          "(clbench runs the baseline automatically). `default` schedule, 1 run each._\n",
          "## Gain by task\n",
          "| Task | Instances | Baseline reward | exo+memory reward | **Gain** |",
          "|------|-----------|-----------------|-------------------|----------|"]
    for r in rows:
        md.append(f"| {r['task']} | {r['n']} | {r['baseline_total']:.2f} | "
                  f"{r['memory_total']:.2f} | **{r['gain']:+.2f}** |")
    md += ["\n![gain](gain_by_task.png)\n",
           "## Learning curves (cumulative reward over instances)\n",
           "Memory pulling above the dashed baseline = lessons compounding across instances.\n",
           "![curves](cumulative_reward.png)\n",
           "> exo+gpt-5.5 with the memory-enabled Simple Coding Agent harness "
           "(remember/forget + per-turn injection). The reference leaderboard uses "
           "claude-opus-4-6, so this is exo's own column, not a same-model comparison.\n"]
    open(os.path.join(outdir, "REPORT.md"), "w").write("\n".join(md))
    print(f"report written: {outdir}/REPORT.md")
    for r in rows:
        print(f"  {r['task']:28} gain {r['gain']:+.2f}")


if __name__ == "__main__":
    main()
