from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from uuid import uuid4

from unittest.mock import AsyncMock, patch

from exo_harbor.agent import ExoAgent, get_harbor_docker_container_id


def build_agent(logs_dir: Path = Path("/tmp/logs")) -> ExoAgent:
    """Construct ExoAgent the way Harbor's AgentFactory does."""
    return ExoAgent(
        logs_dir=logs_dir,
        exo_root="/runs/one/exo",
        exo_bin="/repo/target/debug/exo",
        exo_repo_root="/repo",
        exo_model="gpt-5.5",
    )


class ExoAgentTest(unittest.TestCase):

    def test_unique_convo_per_trial(self) -> None:
        # Ensure each trial gets its own conversation slug.
        agent = build_agent()
        with self.assertRaises(AssertionError):
            agent._conversation

        first = uuid4()
        agent.context_id = first
        self.assertEqual(agent._conversation, f"trial-{first}")

        second = uuid4()
        agent.context_id = second
        self.assertEqual(agent._conversation, f"trial-{second}")

class CodingAgentHarnessTest(unittest.IsolatedAsyncioTestCase):
    """The pi, claude-code and codex harnesses run their agent inside the task
    container, which Harbor's images do not ship, so setup has to install it
    there first."""

    def build(self, harness: str, **extra: object) -> ExoAgent:
        agent = ExoAgent(
            logs_dir=Path("/tmp/logs"),
            exo_root="/runs/one/exo",
            exo_bin="/repo/target/debug/exo",
            exo_repo_root="/repo",
            exo_model="gpt-5.5",
            harness=harness,
            **extra,
        )
        agent.context_id = uuid4()
        agent._client = AsyncMock()
        agent._client.attach_container.return_value = "sandbox-1"
        return agent

    async def test_pi_installs_into_the_container_as_root(self) -> None:
        agent = self.build("pi")
        environment = SimpleNamespace(
            session_id="session-1",
            exec=AsyncMock(return_value=SimpleNamespace(return_code=0, stdout="0.1.0")),
        )
        with patch(
            "exo_harbor.agent.get_harbor_docker_container_id", return_value="abc123"
        ):
            await agent.setup(environment)
        environment.exec.assert_awaited_once()
        self.assertEqual(environment.exec.await_args.kwargs["user"], "root")
        self.assertIn("pi --version", environment.exec.await_args.kwargs["command"])

    async def test_claude_code_installs_into_the_container_as_root(self) -> None:
        agent = self.build("claude-code")
        environment = SimpleNamespace(
            session_id="session-1",
            exec=AsyncMock(
                return_value=SimpleNamespace(return_code=0, stdout="2.1.0 (Claude Code)")
            ),
        )
        with patch(
            "exo_harbor.agent.get_harbor_docker_container_id", return_value="abc123"
        ):
            await agent.setup(environment)
        environment.exec.assert_awaited_once()
        self.assertEqual(environment.exec.await_args.kwargs["user"], "root")
        command = environment.exec.await_args.kwargs["command"]
        # The harness spawns this exact path with HOME=/home/exo.
        self.assertIn("/usr/local/bin/claude-code", command)
        self.assertIn("/home/exo/.claude", command)
        self.assertIn("claude-code --version", command)

    async def test_codex_installs_into_the_container_as_root(self) -> None:
        agent = self.build("codex")
        environment = SimpleNamespace(
            session_id="session-1",
            exec=AsyncMock(
                return_value=SimpleNamespace(return_code=0, stdout="codex-cli 0.50.0")
            ),
        )
        with patch(
            "exo_harbor.agent.get_harbor_docker_container_id", return_value="abc123"
        ):
            await agent.setup(environment)
        environment.exec.assert_awaited_once()
        self.assertEqual(environment.exec.await_args.kwargs["user"], "root")
        self.assertIn("codex --version", environment.exec.await_args.kwargs["command"])

    async def test_the_gateway_certificate_is_installed_after_the_agent(self) -> None:
        with tempfile.NamedTemporaryFile("w", suffix=".crt", delete=False) as handle:
            handle.write("-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----\n")
        agent = self.build("codex", gateway_ca=handle.name)
        environment = SimpleNamespace(
            session_id="session-1",
            exec=AsyncMock(return_value=SimpleNamespace(return_code=0, stdout="ok")),
        )
        with patch(
            "exo_harbor.agent.get_harbor_docker_container_id", return_value="abc123"
        ):
            await agent.setup(environment)
        commands = [call.kwargs["command"] for call in environment.exec.await_args_list]
        self.assertEqual(len(commands), 2)
        self.assertIn("codex --version", commands[0])
        self.assertIn("/usr/local/share/ca-certificates/exo-gateway.crt", commands[1])
        self.assertIn("-----BEGIN CERTIFICATE-----", commands[1])
        self.assertIn("update-ca-certificates", commands[1])
        # The harness learns the in-sandbox path through the exo environment.
        self.assertEqual(
            agent._client.sandbox_ca_path, "/usr/local/share/ca-certificates/exo-gateway.crt"
        ) if not isinstance(agent._client, AsyncMock) else None

    async def test_a_failed_install_fails_setup(self) -> None:
        agent = self.build("claude-code")
        environment = SimpleNamespace(
            session_id="session-1",
            exec=AsyncMock(
                return_value=SimpleNamespace(return_code=1, stdout="", stderr="npm ERR!")
            ),
        )
        with patch(
            "exo_harbor.agent.get_harbor_docker_container_id", return_value="abc123"
        ):
            with self.assertRaisesRegex(RuntimeError, "installing claude-code"):
                await agent.setup(environment)
        agent._client.attach_container.assert_not_awaited()


class SetupTest(unittest.IsolatedAsyncioTestCase):
    async def test_attaches_harbors_container_to_this_trials_conversation(self) -> None:
        agent = build_agent()
        agent.context_id = uuid4()
        agent._client = AsyncMock()
        agent._client.attach_container.return_value = "sandbox-1"

        with patch(
            "exo_harbor.agent.get_harbor_docker_container_id", return_value="abc123"
        ) as lookup:
            await agent.setup(SimpleNamespace(session_id="session-1"))

        lookup.assert_called_once_with("session-1")
        self.assertEqual(agent._container_id, "abc123")
        self.assertEqual(agent._sandbox_id, "sandbox-1")
        # The conversation must exist before anything is attached to it.
        agent._client.ensure_conversation.assert_awaited_once_with(
            f"trial-{agent.context_id}"
        )
        agent._client.attach_container.assert_awaited_once_with(
            f"trial-{agent.context_id}", "abc123"
        )


class RunTest(unittest.IsolatedAsyncioTestCase):
    def build_ready_agent(self) -> ExoAgent:
        agent = build_agent()
        agent.context_id = uuid4()
        agent._container_id = "abc123"
        agent._sandbox_id = "sandbox-1"
        agent._client = AsyncMock()
        return agent

    async def test_sends_the_instruction_and_records_the_conversation(self) -> None:
        agent = self.build_ready_agent()
        context = SimpleNamespace(metadata={"existing": "kept"})

        with patch("exo_harbor.agent.export_trial_trajectory", AsyncMock()) as export:
            await agent.run("do the thing", SimpleNamespace(), context)

        agent._client.send.assert_awaited_once_with(
            f"trial-{agent.context_id}", "do the thing", timeout_sec=None
        )
        self.assertEqual(
            context.metadata,
            {
                "existing": "kept",
                "exo_conversation_id": f"trial-{agent.context_id}",
            },
        )
        export.assert_awaited_once()

    async def test_a_timeout_still_exports_the_partial_trajectory(self) -> None:
        # The trial is over, but what Exo did up to the timeout is still worth
        # keeping, and Harbor must still see the timeout as the failure.
        agent = self.build_ready_agent()
        agent._client.send.side_effect = TimeoutError("task timeout")
        context = SimpleNamespace(metadata=None)

        with patch("exo_harbor.agent.export_trial_trajectory", AsyncMock()) as export:
            with self.assertRaises(TimeoutError):
                await agent.run("do the thing", SimpleNamespace(), context)

        self.assertEqual(
            context.metadata["exo_conversation_id"], f"trial-{agent.context_id}"
        )
        export.assert_awaited_once()

    async def test_a_failed_trajectory_export_does_not_fail_the_trial(self) -> None:
        agent = self.build_ready_agent()
        context = SimpleNamespace(metadata=None)

        with patch(
            "exo_harbor.agent.export_trial_trajectory",
            AsyncMock(side_effect=ValueError("no events")),
        ):
            await agent.run("do the thing", SimpleNamespace(), context)

        self.assertEqual(
            context.metadata["exo_conversation_id"], f"trial-{agent.context_id}"
        )

if __name__ == "__main__":
    unittest.main()


class ExoClientEnvironmentTest(unittest.TestCase):
    def test_the_sandbox_ca_path_reaches_exo_as_an_environment_variable(self) -> None:
        from exo_harbor.exo import ExoClient

        plain = ExoClient(exo_bin=Path("/x"), exo_root=Path("/r"), repo_root=Path("/repo"))
        self.assertNotIn("EXO_SANDBOX_CA_CERTS", plain._environment())
        with_ca = ExoClient(
            exo_bin=Path("/x"), exo_root=Path("/r"), repo_root=Path("/repo"),
            sandbox_ca_path="/usr/local/share/ca-certificates/exo-gateway.crt",
        )
        self.assertEqual(
            with_ca._environment()["EXO_SANDBOX_CA_CERTS"],
            "/usr/local/share/ca-certificates/exo-gateway.crt",
        )
