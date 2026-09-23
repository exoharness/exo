from __future__ import annotations

import csv
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

from harbor.models.job.result import JobResult
from harbor.models.trajectories.trajectory import Trajectory
from harbor.models.trial.result import TrialResult


MODULE_PATH = Path(__file__).resolve().parents[1] / "postprocess.py"
SPEC = importlib.util.spec_from_file_location("sregym_postprocess", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
postprocess = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(postprocess)


def event(index: int, data: dict) -> dict:
    return {
        "id": f"event-{index}",
        "session_id": "session",
        "turn_id": "turn",
        "created_at": f"2026-01-01T00:00:0{index}Z",
        "data": data,
    }


EVENTS = {
    "events": [
        event(0, {
            "type": "messages",
            "messages": [{"role": "user", "content": "Fix the incident."}],
        }),
        event(1, {
            "type": "messages",
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_call",
                    "tool_call_id": "call-1",
                    "tool_name": "shell",
                    "arguments": {"type": "valid", "value": {"command": "kubectl get netpol -A"}},
                }],
            }],
            "usage": {
                "model": "gpt-5.5",
                "prompt_tokens": 100,
                "completion_tokens": 10,
                "prompt_cached_tokens": 40,
                "cost_usd": 0.5,
            },
        }),
        event(2, {
            "type": "tool_result",
            "tool_call_id": "call-1",
            "result": {
                "ok": True,
                "preview": "deny-all",
                "source": "sandbox",
                "toolName": "shell",
                "truncated": False,
            },
        }),
    ]
}


class PostprocessTests(unittest.TestCase):
    def test_rewards_require_every_graded_stage(self) -> None:
        passed = postprocess.ResultRow(
            problem_id="p",
            attempt=1,
            run_status="complete",
            diagnosis_success=True,
            mitigation_success=False,
        )
        diagnosis_only = passed.model_copy(update={"mitigation_success": None})

        self.assertEqual(postprocess.rewards(passed)["reward"], 0.0)
        self.assertEqual(postprocess.rewards(diagnosis_only)["reward"], 1.0)
        self.assertNotIn("mitigation", postprocess.rewards(diagnosis_only))

    def test_export_batch_writes_atif_and_harbor_job(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            batch = root / "results/0101_0000"
            run_dir = batch / "exo/network_policy_block/run_1"
            run_dir.mkdir(parents=True)
            (run_dir / "exo-trajectory.json").write_text(json.dumps(EVENTS))
            (run_dir / "exo-self-improvements.json").write_text(
                json.dumps({"conversation": "trial-1", "actions": []})
            )
            with (batch / postprocess.RESULTS_FILE).open("w", newline="") as handle:
                writer = csv.DictWriter(handle, fieldnames=[
                    "problem_id", "attempt", "run_status",
                    "Diagnosis.success", "Mitigation.success", "Mitigation.reason",
                ])
                writer.writeheader()
                writer.writerow({
                    "problem_id": "network_policy_block",
                    "attempt": "1",
                    "run_status": "complete",
                    "Diagnosis.success": "True",
                    "Mitigation.success": "True",
                    "Mitigation.reason": "",
                })

            job_dir = postprocess.export_batch(
                batch, jobs_dir=root / "jobs", job_name="job"
            )

            trial_dir = job_dir / "network_policy_block__run_1"
            trajectory = Trajectory.model_validate_json(
                (run_dir / "trajectory.json").read_text()
            )
            harbor_copy = json.loads((trial_dir / "agent/trajectory.json").read_text())
            self.assertEqual(harbor_copy["schema_version"], "ATIF-v1.8")
            self.assertEqual(trajectory.schema_version, postprocess.SREGYM_ATIF_VERSION)
            self.assertEqual(harbor_copy["steps"], trajectory.to_json_dict()["steps"])
            self.assertEqual(trajectory.steps[0].message, "Fix the incident.")
            self.assertEqual(
                trajectory.steps[1].observation.results[0].content, "deny-all"
            )
            trial = TrialResult.model_validate_json((trial_dir / "result.json").read_text())
            self.assertEqual(trial.verifier_result.rewards["reward"], 1.0)
            self.assertEqual(trial.agent_info.model_info.name, "gpt-5.5")
            job = JobResult.model_validate_json((job_dir / "result.json").read_text())
            self.assertEqual(job.stats.n_completed_trials, 1)
            self.assertIn("Mitigation: PASS", (trial_dir / "verifier/test-stdout.txt").read_text())


if __name__ == "__main__":
    unittest.main()
