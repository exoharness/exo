#!/usr/bin/env python3
"""Convert Exo's SREGym runs into ATIF trajectories and a Harbor job.

SREGym's own postprocess only knows the built-in agents' session formats, so
Exo runs finish without a `trajectory.json`. This writes one into each native
run directory and mirrors the batch as a Harbor job for `harbor view`.

Usage:
    python eval/sregym/postprocess.py <SREGym results/<batch> dir> [--job-name NAME]
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import sys
import uuid
from pathlib import Path

from harbor.models.agent.context import AgentContext
from harbor.models.job.config import JobConfig
from harbor.models.job.result import JobResult, JobStats
from harbor.models.task.id import LocalTaskId
from harbor.models.trial.config import AgentConfig, TaskConfig, TrialConfig
from harbor.models.trial.result import AgentInfo, ModelInfo, TrialResult
from harbor.models.verifier.result import VerifierResult
from pydantic import BaseModel, ConfigDict, Field

from exo_harbor.trajectory import (
    ConversationEvents,
    MessagesData,
    UserMessage,
    build_trajectory,
)

AGENT = "exo"
DATASET = "sregym"
RESULTS_FILE = f"{AGENT}_ALL_results.csv"
SREGYM_ATIF_VERSION = "ATIF-v1.7"


class ResultRow(BaseModel):
    """The columns of SREGym's results CSV that the Harbor view needs."""

    model_config = ConfigDict(populate_by_name=True)

    problem_id: str
    attempt: int
    run_status: str
    diagnosis_success: bool | None = Field(None, alias="Diagnosis.success")
    diagnosis_score: float | None = Field(None, alias="Diagnosis.composite_score")
    diagnosis_submission: str | None = Field(None, alias="Diagnosis.submission")
    diagnosis_reasoning: str | None = Field(None, alias="Diagnosis.reasoning")
    mitigation_success: bool | None = Field(None, alias="Mitigation.success")
    mitigation_reason: str | None = Field(None, alias="Mitigation.reason")
    mitigation_detail: str | None = Field(None, alias="Mitigation.detail")
    mitigation_failure_class: str | None = Field(None, alias="Mitigation.failure_class")
    ttl: float | None = Field(None, alias="TTL")
    ttm: float | None = Field(None, alias="TTM")


def read_rows(batch: Path) -> list[ResultRow]:
    with (batch / RESULTS_FILE).open(newline="") as handle:
        return [
            ResultRow.model_validate({key: value for key, value in row.items() if value})
            for row in csv.DictReader(handle)
        ]


def rewards(row: ResultRow) -> dict[str, float]:
    stages = {
        "diagnosis": row.diagnosis_success,
        "mitigation": row.mitigation_success,
    }
    graded = {name: float(success) for name, success in stages.items() if success is not None}
    graded["reward"] = float(bool(graded) and all(graded.values()))
    if row.diagnosis_score is not None:
        graded["diagnosis_score"] = row.diagnosis_score
    return graded


def verdict(row: ResultRow) -> str:
    lines = [f"problem: {row.problem_id} (attempt {row.attempt}, {row.run_status})"]
    if row.diagnosis_success is not None:
        lines += [
            "",
            f"== Diagnosis: {'PASS' if row.diagnosis_success else 'FAIL'} "
            f"(score {row.diagnosis_score}, TTL {row.ttl}s)",
            "",
            "Submission:",
            row.diagnosis_submission or "",
            "",
            "Judge:",
            row.diagnosis_reasoning or "",
        ]
    if row.mitigation_success is not None:
        lines += [
            "",
            f"== Mitigation: {'PASS' if row.mitigation_success else 'FAIL'} (TTM {row.ttm}s)",
            f"reason: {row.mitigation_reason}",
            f"failure class: {row.mitigation_failure_class}",
            f"detail: {row.mitigation_detail}",
        ]
    return "\n".join(lines) + "\n"


def write_json(path: Path, model: BaseModel) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(model.model_dump_json(indent=2) + "\n")


def export_trial(
    row: ResultRow, *, batch: Path, job_dir: Path, job_id: uuid.UUID
) -> TrialResult:
    run_dir = batch / AGENT / row.problem_id / f"run_{row.attempt}"
    page = ConversationEvents.model_validate_json(
        (run_dir / "exo-trajectory.json").read_text()
    )
    events = page.events
    if not events:
        raise ValueError(f"{run_dir} has no Exo events")
    instruction = next(
        message.content
        for event in events
        if isinstance(event.data, MessagesData)
        for message in event.data.messages
        if isinstance(message, UserMessage)
    )
    usages = [
        event.data.usage
        for event in events
        if isinstance(event.data, MessagesData) and event.data.usage is not None
    ]
    model_name = usages[0].model if usages else None
    conversation = json.loads(
        (run_dir / "exo-self-improvements.json").read_text()
    )["conversation"]
    trial_name = f"{row.problem_id}__run_{row.attempt}"
    trajectory = build_trajectory(
        events=events,
        trial_id=trial_name,
        turn_ids=list(dict.fromkeys(event.turn_id for event in events)),
        instruction=instruction,
        model_name=model_name or "unknown",
        conversation=conversation,
        started_at=events[0].created_at,
    )
    document = trajectory.to_json_dict()
    trial_dir = job_dir / trial_name
    (trial_dir / "agent").mkdir(parents=True, exist_ok=True)
    (trial_dir / "agent" / "trajectory.json").write_text(json.dumps(document, indent=2) + "\n")
    # SREGym's own postprocess writes this file for the agents it knows. Its
    # vendored ATIF model stops at v1.7; v1.8 only added audio content.
    document["schema_version"] = SREGYM_ATIF_VERSION
    (run_dir / "trajectory.json").write_text(json.dumps(document, indent=2) + "\n")
    (trial_dir / "verifier").mkdir(parents=True, exist_ok=True)
    (trial_dir / "verifier" / "test-stdout.txt").write_text(verdict(row))
    scores = rewards(row)
    (trial_dir / "verifier" / "reward.json").write_text(json.dumps(scores, indent=2) + "\n")

    config = TrialConfig(
        task=TaskConfig(name=row.problem_id, source=DATASET),
        trial_name=trial_name,
        trials_dir=job_dir,
        agent=AgentConfig(name=AGENT, model_name=model_name),
        job_id=job_id,
    )
    write_json(trial_dir / "config.json", config)
    result = TrialResult(
        task_name=row.problem_id,
        trial_name=trial_name,
        trial_uri=run_dir.resolve().as_uri(),
        task_id=LocalTaskId(path=Path(row.problem_id)),
        source=DATASET,
        task_checksum=row.problem_id,
        config=config,
        agent_info=AgentInfo(
            name=AGENT,
            version="unknown",
            model_info=ModelInfo(name=model_name) if model_name else None,
        ),
        agent_result=AgentContext(
            n_input_tokens=sum(usage.prompt_tokens for usage in usages),
            n_cache_tokens=sum(usage.prompt_cached_tokens for usage in usages),
            n_output_tokens=sum(usage.completion_tokens for usage in usages),
            cost_usd=sum(usage.cost_usd for usage in usages),
        ),
        verifier_result=VerifierResult(rewards=scores),
        started_at=dt.datetime.fromisoformat(events[0].created_at),
        finished_at=dt.datetime.fromisoformat(events[-1].created_at),
    )
    write_json(trial_dir / "result.json", result)
    return result


def export_batch(batch: Path, *, jobs_dir: Path, job_name: str) -> Path:
    rows = read_rows(batch)
    job_dir = jobs_dir / job_name
    job_id = uuid.uuid4()
    results = [
        export_trial(row, batch=batch, job_dir=job_dir, job_id=job_id)
        for row in rows
    ]
    agents = list({result.config.agent.model_name: result.config.agent for result in results}.values())
    write_json(
        job_dir / "config.json",
        JobConfig(job_name=job_name, jobs_dir=jobs_dir, agents=agents),
    )
    started = min((result.started_at for result in results if result.started_at), default=None)
    finished = max((result.finished_at for result in results if result.finished_at), default=None)
    write_json(
        job_dir / "result.json",
        JobResult(
            id=job_id,
            started_at=started or dt.datetime.now(dt.UTC),
            updated_at=finished,
            finished_at=finished,
            n_total_trials=len(results),
            stats=JobStats.from_trial_results(results, n_total_trials=len(results)),
        ),
    )
    return job_dir


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("batch", type=Path, help="SREGym results/<batch> directory")
    parser.add_argument("--jobs-dir", type=Path)
    parser.add_argument("--job-name")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    batch = args.batch.resolve()
    jobs_dir = (args.jobs_dir or repo / ".local/sregym-evals/harbor-jobs").resolve()
    job_dir = export_batch(batch, jobs_dir=jobs_dir, job_name=args.job_name or f"sregym-{batch.name}")
    print(f"Harbor job: {job_dir}")
    print(f"View with: harbor view {jobs_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
