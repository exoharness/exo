"""What Exo accumulated over a job, and when.

Two kinds of output, deliberately kept apart:

* **Inventory** — measured from Exo's own CLI and workspace. Installed tools,
  memory entries, source diffs. Deterministic, no model call. This is the
  evidence.
* **Narrative** — Exo's own account of what it learned. A model turn, so it
  costs a wake and a third adapter exchange. Useful qualitative color for a
  writeup; not proof of anything, because it is self-report.

A reader of the artifact must be able to tell which is which, so they are
separate fields and never merged into one summary.

Snapshots are taken per trial rather than once at the end. A final inventory
says what Exo ended up with; the series says *when* each piece appeared, which
is what lets a tool install be lined up against the point where the
correctness curve bends. That correlation is the self-improvement claim.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path


@dataclass(frozen=True)
class Inventory:
    """A point-in-time measurement of Exo's durable state."""

    # Installed managed tools: stable id, name, source. From `exo tools list`.
    tools: list[dict] = field(default_factory=list)
    # Memory entries. Identity + a digest, not full content — this is sampled
    # every trial and full bodies would dwarf the rest of the artifact.
    memories: list[dict] = field(default_factory=list)
    # Files changed under exo_root against the baseline commit, with line
    # counts. Exo editing its own source is the strongest self-modification
    # signal and the easiest to measure.
    source_diff_stat: dict[str, int] = field(default_factory=dict)
    # Adapters, scheduler entries, and anything else Exo can durably create.
    other: dict = field(default_factory=dict)


@dataclass(frozen=True)
class TrialSnapshot:
    """Inventory as of the end of one trial, tagged for correlation."""

    trial_id: str
    trial_name: str
    task_name: str
    # Position in the job's trial sequence. The x-axis of every curve.
    index: int
    rewards: dict[str, float] | None
    inventory: Inventory


@dataclass(frozen=True)
class JobReport:
    job_id: str
    # Every snapshot in trial order. Diffing consecutive entries gives the
    # timeline of what appeared when.
    snapshots: list[TrialSnapshot]
    final: Inventory
    # Exo's self-report. None when the narrative was skipped or failed —
    # which must not invalidate everything above it.
    narrative: str | None = None
    narrative_error: str | None = None


def capture_inventory(exo_root: Path, exo_bin: Path) -> Inventory:
    """Measure Exo's durable state. No model call, no wake.

    Cheap enough to run after every trial. Must never raise into the caller:
    a failed measurement is an empty field, not a dead job.
    """
    # TODO: shell out to `exo tools list`, read the memory store, and run a
    # diff of exo_root against the baseline commit recorded at job start.
    raise NotImplementedError


def diff_inventories(before: Inventory, after: Inventory) -> dict:
    """What appeared, changed, or vanished between two snapshots.

    The per-trial deltas are the interesting series; the absolute inventories
    are just what they are computed from.
    """
    raise NotImplementedError


def write_report(job_dir: Path, report: JobReport) -> Path:
    """Write exo-job-report.json beside Harbor's job result.

    Called BEFORE the narrative is requested, then again after. on_job_end
    exceptions are swallowed by finalize_job_plugins, so an unwritten report
    fails silently — get the measured half on disk first.
    """
    raise NotImplementedError
