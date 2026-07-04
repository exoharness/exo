# Evaluation reports

One directory per experiment write-up, shareable as-is:

```
evaluation/reports/
  YYYY-MM-DD-<slug>/
    report.md          # date, description, protocol, results, caveats
    *.svg / *.png      # charts (committed — they're the deliverable)
    data/              # optional: small result JSONs backing the numbers
```

Conventions:
- `report.md` starts with a metadata block (date, benchmark, model, commits,
  raw-results paths) so every number is traceable to a run.
- Charts follow `evaluation/shared/board/plot_board.py` styling; cost figures
  are always token counts × `evaluation/shared/board/prices.json`.
- State caveats (contamination, protocol differences, small n) in the report —
  a shared report without them is a foot-gun.

Index:

| Date | Report | Headline |
|---|---|---|
| 2026-07-01 | [arc-agi-2-self-evolution](2026-07-01-arc-agi-2-self-evolution/report.md) | Self-evolving exo 64% vs stateless 44% pass@1 on ARC-AGI-2 (25 tasks, gpt-5.5) |
| 2026-07-02 | [arc-agi-1-board](2026-07-02-arc-agi-1-board/report.md) | Six-system cost-vs-score board: exo evolve 92% tops; all systems 86–92%; Hermes cheapest at $0.03/task |
| 2026-07-02 | [clbench-exo-vs-openclaw](2026-07-02-clbench-exo-vs-openclaw/report.md) | Both systems show real continual-learning gain; exo edges 3/4 tasks, OpenClaw wins codebase_adaptation |
| 2026-07-02 | [selfimprove-vs-memory](2026-07-02-selfimprove-vs-memory/report.md) | Self-improve's drag is context overhead (+14-44% input tokens for never-used tool machinery), not bad tools |
