from __future__ import annotations

import subprocess
import sys
import unittest

import tempfile
from pathlib import Path
from unittest.mock import patch

from exo_harbor.gateway import (
    GatewayError,
    free_port,
    gateway_executable,
    is_anthropic_model,
    needs_gateway,
    wait_for_health,
)


class GatewayTest(unittest.TestCase):
    def test_only_claude_models_skip_the_gateway(self) -> None:
        self.assertTrue(is_anthropic_model("claude-sonnet-4-6"))
        self.assertTrue(is_anthropic_model("Claude-Opus-5"))
        self.assertFalse(is_anthropic_model("gpt-5.5"))
        self.assertFalse(is_anthropic_model("openai/gpt-5.5"))

    def test_only_a_vendor_cli_on_the_other_vendors_model_needs_a_gateway(self) -> None:
        self.assertTrue(needs_gateway("claude-code", "gpt-5.5"))
        self.assertFalse(needs_gateway("claude-code", "claude-sonnet-4-6"))
        self.assertTrue(needs_gateway("codex", "claude-sonnet-4-6"))
        self.assertFalse(needs_gateway("codex", "gpt-5.5"))
        for harness in ("exo", "basic", "pi"):
            self.assertFalse(needs_gateway(harness, "claude-sonnet-4-6"))
            self.assertFalse(needs_gateway(harness, "gpt-5.5"))

    def test_an_installed_gateway_is_reused_without_installing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            venv = Path(directory)
            (venv / "bin").mkdir()
            (venv / "bin" / "litellm").write_text("#!/bin/sh\n")
            with patch("exo_harbor.gateway.subprocess.run") as run:
                self.assertEqual(gateway_executable(venv), venv / "bin" / "litellm")
            run.assert_not_called()

    def test_a_missing_gateway_is_installed_into_its_own_venv(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            venv = Path(directory) / "gateway"
            with patch("exo_harbor.gateway.subprocess.run") as run:
                with self.assertRaisesRegex(GatewayError, "still missing"):
                    gateway_executable(venv)
            commands = [call.args[0] for call in run.call_args_list]
            self.assertEqual(commands[0][1:], ["-m", "venv", str(venv)])
            self.assertEqual(commands[1][0], str(venv / "bin" / "pip"))
            self.assertIn("litellm[proxy]==1.101.0", commands[1])

    def test_free_port_is_usable(self) -> None:
        port = free_port()
        self.assertGreater(port, 0)
        self.assertLess(port, 65536)

    def test_a_gateway_that_dies_during_startup_is_reported(self) -> None:
        process = subprocess.Popen([sys.executable, "-c", "raise SystemExit(3)"])
        process.wait()
        with self.assertRaisesRegex(GatewayError, "exited with code 3"):
            wait_for_health("http://127.0.0.1:9/health/liveliness", 5.0, process)

    def test_an_unreachable_gateway_times_out(self) -> None:
        process = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
        try:
            with self.assertRaisesRegex(GatewayError, "did not become healthy"):
                # Port 9 (discard) refuses connections immediately.
                wait_for_health("http://127.0.0.1:9/health/liveliness", 1.0, process)
        finally:
            process.kill()
            process.wait()


if __name__ == "__main__":
    unittest.main()
