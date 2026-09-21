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
    generate_certificate,
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

    def test_the_gateway_command_lives_beside_the_interpreter(self) -> None:
        with patch("exo_harbor.gateway.Path.is_file", return_value=True):
            self.assertEqual(gateway_executable().name, "litellm")
        with patch("exo_harbor.gateway.Path.is_file", return_value=False):
            with self.assertRaisesRegex(GatewayError, "litellm[^ ]* is missing"):
                gateway_executable()

    def test_the_certificate_names_the_docker_host_address(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            cert, key = generate_certificate("172.17.0.1", Path(directory))
            self.assertTrue(cert.is_file() and key.is_file())
            text = subprocess.run(
                ["openssl", "x509", "-in", str(cert), "-noout", "-text"],
                capture_output=True, text=True, check=True,
            ).stdout
            self.assertIn("IP Address:172.17.0.1", text)

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
