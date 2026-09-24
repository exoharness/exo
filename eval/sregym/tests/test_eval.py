from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(MODULE_DIR))
MODULE_PATH = MODULE_DIR / "eval.py"
SPEC = importlib.util.spec_from_file_location("sregym_eval", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
sregym_eval = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sregym_eval)


class EvalTests(unittest.TestCase):
    def test_sregym_patch_applies_once(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            git = ["git", "-c", "user.name=t", "-c", "user.email=t@t", "-C", str(root)]
            subprocess.run([*git, "init", "-q"], check=True)
            (root / "agents.yaml").write_text("agents: []\n")
            subprocess.run([*git, "add", "."], check=True)
            subprocess.run([*git, "commit", "-q", "-m", "base"], check=True)
            (root / "agents.yaml").write_text("agents:\n  - name: exo\n")
            patch = root / "exo.patch"
            patch.write_text(
                subprocess.run([*git, "diff"], check=True, capture_output=True, text=True).stdout
            )
            subprocess.run([*git, "checkout", "--", "agents.yaml"], check=True)

            sregym_eval.ensure_sregym_patch(root, patch)
            sregym_eval.ensure_sregym_patch(root, patch)

            self.assertEqual((root / "agents.yaml").read_text(), "agents:\n  - name: exo\n")

    def test_real_patch_covers_every_sregym_change(self) -> None:
        patch = sregym_eval.SREGYM_PATCH.read_text()
        for path in (
            "agents.yaml",
            "sregym/service/provider_endpoints.py",
            "sregym/conductor/conductor.py",
            "sregym/conductor/conductor_api.py",
            "sregym/service/container_runner.py",
            "sregym/agent_launcher.py",
            "main.py",
        ):
            self.assertIn(f"+++ b/{path}", patch)

    def test_reflection_rejects_repeated_attempts(self) -> None:
        with self.assertRaises(SystemExit):
            sregym_eval.parse_args(["--reflection", "--n-attempts", "2"])
        args = sregym_eval.parse_args(["--reflection"])
        self.assertTrue(args.reflection)

    def test_grade_summary_reads_stage_outcomes(self) -> None:
        summary = sregym_eval.grade_summary(
            {"Diagnosis": {"success": True}, "Mitigation": {"success": False}, "TTL": 3.0}
        )
        self.assertEqual(summary, "diagnosis PASS, mitigation fail")
        self.assertEqual(sregym_eval.grade_summary(None), "ungraded")

    def test_build_reflection_embeds_grader_feedback(self) -> None:
        feedback = {"Diagnosis": {"success": False, "reasoning": "Blamed search"}}
        reflection = sregym_eval.build_reflection(feedback, self_modification=True)
        self.assertIn("Blamed search", reflection)
        self.assertIn("faster or more cheaply", reflection)
        self.assertIn("list_conversation_events", reflection)
        self.assertIn("rebuild_and_restart_exo", reflection)
        self.assertIn("If nothing is worth keeping, say so and stop.", reflection)

        memory_only = sregym_eval.build_reflection(feedback, self_modification=False)
        self.assertIn("Blamed search", memory_only)
        self.assertIn("remember it now", memory_only)
        self.assertNotIn("rebuild_and_restart_exo", memory_only)
        self.assertNotIn("skill", memory_only)

    def test_resume_requires_the_run_directory(self) -> None:
        with self.assertRaises(SystemExit):
            sregym_eval.parse_args(["--resume", "results.csv"])
        args = sregym_eval.parse_args(["--resume", "results.csv", "--run-dir", "run"])
        self.assertEqual(args.run_dir, Path("run"))

    def test_run_manifest_records_arguments_and_appends_on_resume(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            repo = root / "repo"
            repo.mkdir()
            git = ["git", "-c", "user.name=t", "-c", "user.email=t@t", "-C", str(repo)]
            (repo / "f").write_text("x")
            subprocess.run([*git, "init", "-q"], check=True)
            subprocess.run([*git, "add", "."], check=True)
            subprocess.run([*git, "commit", "-q", "-m", "base"], check=True)
            run_dir = root / "run"
            run_dir.mkdir()

            args = sregym_eval.parse_args(["--exo-profile", "memory-only", "--reflection"])
            sregym_eval.write_run_manifest(run_dir, args=args, repo=repo, command=["uv", "run"])
            sregym_eval.write_run_manifest(run_dir, args=args, repo=repo, command=["uv", "run", "--resume"])

            entries = json.loads((run_dir / "run.json").read_text())
            self.assertEqual(len(entries), 2)
            self.assertEqual(entries[0]["arguments"]["exo_profile"], "memory-only")
            self.assertTrue(entries[0]["arguments"]["reflection"])
            self.assertEqual(entries[0]["sregym_ref"], sregym_eval.SREGYM_REF)
            self.assertIn("faster or more cheaply", entries[0]["reflection_instructions"]["opening"])
            self.assertIn("top goal", entries[0]["task_brief"]["self_modification"])
            self.assertEqual(entries[1]["sregym_command"][-1], "--resume")

    def test_exo_profile_defaults_to_practical(self) -> None:
        self.assertEqual(sregym_eval.parse_args([]).exo_profile, "practical")
        self.assertEqual(
            sregym_eval.parse_args(["--exo-profile", "memory-only"]).exo_profile, "memory-only"
        )

    def test_build_instruction_names_stages_and_namespaces(self) -> None:
        instruction = sregym_eval.build_instruction(
            {
                "app_name": "shop",
                "namespace": "default",
                "namespaces": ["shop", "telemetry"],
                "descriptions": "The checkout path is degraded.",
            },
            api_port=8123,
            stages=["diagnosis", "mitigation"],
            self_modification=True,
        )

        self.assertIn("shop, telemetry", instruction)
        self.assertIn("host.docker.internal:8123/submit", instruction)
        self.assertIn('\"stage\":\"diagnosis\"', instruction)
        self.assertIn('\"stage\":\"mitigation\"', instruction)
        self.assertIn("rebuild_and_restart_exo", instruction)
        self.assertIn("top goal is to get the right answer", instruction)

        memory_only = sregym_eval.build_instruction(
            {"app_name": "shop", "namespace": "default", "descriptions": ""},
            api_port=8123,
            stages=["diagnosis"],
            self_modification=False,
        )
        self.assertIn("remember", memory_only)
        self.assertNotIn("rebuild_and_restart_exo", memory_only)
        self.assertNotIn("install_skill", memory_only)

    def test_improvement_actions_extracts_only_mutations(self) -> None:
        events = {
            "events": [
                {
                    "created_at": "2026-01-01T00:00:00Z",
                    "data": {
                        "type": "tool_requested",
                        "tool_call_id": "call-1",
                        "request": {
                            "function_name": "remember",
                            "arguments": {"text": "Check events before logs"},
                        },
                    },
                },
                {
                    "created_at": "2026-01-01T00:01:00Z",
                    "data": {
                        "type": "tool_requested",
                        "request": {
                            "function_name": "shell",
                            "arguments": {"command": "kubectl get pods"},
                        },
                    },
                },
                {
                    "created_at": "2026-01-01T00:00:01Z",
                    "data": {
                        "type": "tool_result",
                        "tool_call_id": "call-1",
                        "result": {"ok": True, "value": {"id": "memory-1"}},
                    },
                },
            ]
        }

        self.assertEqual(
            sregym_eval.improvement_actions(events),
            [
                {
                    "timestamp": "2026-01-01T00:00:00Z",
                    "tool_call_id": "call-1",
                    "tool": "remember",
                    "arguments": {"text": "Check events before logs"},
                    "succeeded": True,
                    "result": {"ok": True, "value": {"id": "memory-1"}},
                }
            ],
        )

    def test_sregym_command_keeps_native_staged_runner(self) -> None:
        args = sregym_eval.parse_args([])
        args.problem = "network_policy_block"
        args.suite = None
        command = sregym_eval.sregym_command(args)

        self.assertEqual(command[:3], ["uv", "run", "main.py"])
        self.assertEqual(command[command.index("--agent") + 1], "exo")
        self.assertEqual(
            command[command.index("--problem") + 1], "network_policy_block"
        )
        self.assertNotIn("--use-external-harness", command)


if __name__ == "__main__":
    unittest.main()
