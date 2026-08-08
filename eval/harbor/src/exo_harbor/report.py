"""What Exo accumulated over a job, and when.

Measured from Exo's own CLI — installed tools, and the diff of its workspace.
No model call, so it is cheap enough to run after every trial, and cheap is the
point: a final inventory says what Exo ended up with, but the per-trial series
says *when* each piece appeared. That is what lets a tool install be lined up
against the point where the correctness curve bends, which is the
self-improvement claim.

Deliberately no self-narrative. Asking Exo what it learned would need a third
adapter exchange and would produce self-report, which is not evidence. If that
is wanted later it belongs in a separate field, clearly labelled.
"""

from __future__ import annotations

import json
import logging
import subprocess
from dataclasses import asdict, dataclass, field
from pathlib import Path

logger = logging.getLogger(__name__)

REPORT_FILENAME = "exo-job-report.json"


@dataclass(frozen=True)
class Inventory:
    """A point-in-time measurement of Exo's durable state."""

    # Installed managed tools, one raw line each from `exo tools list`.
    tools: list[str] = field(default_factory=list)
    # Files changed under the workspace against the baseline commit. Exo
    # editing its own source is the strongest self-modification signal and the
    # easiest to measure.
    changed_files: list[str] = field(default_factory=list)


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
    snapshots: list[TrialSnapshot]
    final: Inventory


async def capture_inventory(list_tools_output: str, repo_root: Path) -> Inventory:
    """Measure Exo's durable state. No model call, no wake.

    Never raises into the caller: a failed measurement is an empty field, not
    a dead job. This runs inside a trial-end hook, where an exception would
    escape trial.run() and cost the whole trial result.
    """
    tools = [line for line in list_tools_output.splitlines()[1:] if line.strip()]
    return Inventory(tools=tools, changed_files=_changed_files(repo_root))


def write_report(job_dir: Path, report: JobReport) -> Path:
    """Write exo-job-report.json beside Harbor's job result."""
    path = job_dir / REPORT_FILENAME
    path.write_text(json.dumps(asdict(report), indent=2))
    return path


def _changed_files(repo_root: Path) -> list[str]:
    try:
        result = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=repo_root,
            capture_output=True,
            text=True,
            check=False,
            timeout=30,
        )
        if result.returncode != 0:
            return []
        return [line for line in result.stdout.splitlines() if line.strip()]
    except (OSError, subprocess.SubprocessError) as error:
        logger.warning("could not read workspace diff: %s", error)
        return []
