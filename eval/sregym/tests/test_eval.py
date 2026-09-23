from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).resolve().parents[1] / "eval.py"
SPEC = importlib.util.spec_from_file_location("sregym_eval", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
sregym_eval = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sregym_eval)


class EvalTests(unittest.TestCase):
    def test_agent_registration_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            registry = root / "agents.yaml"
            registry.write_text("agents:\n  - name: codex\n")

            sregym_eval.ensure_agent_registration(root)
            sregym_eval.ensure_agent_registration(root)

            self.assertEqual(registry.read_text().count("  - name: exo\n"), 1)

    def test_provider_exemption_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            source = root / "sregym/service/provider_endpoints.py"
            source.parent.mkdir(parents=True)
            source.write_text(f"    {sregym_eval.PASSIVE_AGENTS}\n        return ()\n")

            sregym_eval.ensure_provider_exemption(root)
            sregym_eval.ensure_provider_exemption(root)

            self.assertEqual(
                source.read_text(),
                f"    {sregym_eval.PATCHED_PASSIVE_AGENTS}\n        return ()\n",
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
        )

        self.assertIn("shop, telemetry", instruction)
        self.assertIn("host.docker.internal:8123/submit", instruction)
        self.assertIn('\"stage\":\"diagnosis\"', instruction)
        self.assertIn('\"stage\":\"mitigation\"', instruction)

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
