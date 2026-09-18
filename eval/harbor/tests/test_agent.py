from __future__ import annotations

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

class PiHarnessTest(unittest.IsolatedAsyncioTestCase):
    """The pi harness runs `pi` inside the task container, which Harbor's
    images do not ship, so setup has to install it there first."""

    def build(self, harness: str) -> ExoAgent:
        agent = ExoAgent(
            logs_dir=Path("/tmp/logs"),
            exo_root="/runs/one/exo",
            exo_bin="/repo/target/debug/exo",
            exo_repo_root="/repo",
            exo_model="gpt-5.5",
            harness=harness,
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
        # Attaching alone leaves the container unused, and the trial would be
        # graded on a sandbox Harbor never sees.
        agent._client.select_sandbox.assert_awaited_once_with(
            f"trial-{agent.context_id}", "sandbox-1"
        )


class ReflectionFlagTest(unittest.IsolatedAsyncioTestCase):
    """Without reflection nothing uses the fork, so do not create one.

    Harbor passes agent kwargs as strings, so the flag has to survive "false"
    rather than being read as a truthy value.
    """

    def build(self, reflection: object) -> ExoAgent:
        agent = ExoAgent(
            logs_dir=Path("/tmp/logs"),
            exo_root="/runs/one/exo",
            exo_bin="/repo/target/debug/exo",
            exo_repo_root="/repo",
            exo_model="gpt-5.5",
            reflection=reflection,
        )
        agent.context_id = uuid4()
        agent._container_id = "abc123"
        agent._sandbox_id = "sandbox-1"
        agent._client = AsyncMock()
        agent._client.fork_sandbox.return_value = "sandbox-fork"
        return agent

    async def test_the_string_false_does_not_enable_reflection(self) -> None:
        agent = self.build("false")
        context = SimpleNamespace(metadata=None)
        with patch("exo_harbor.agent.export_trial_trajectory", AsyncMock()):
            await agent.run("do the thing", SimpleNamespace(), context)
        agent._client.fork_sandbox.assert_not_awaited()
        self.assertIsNone(context.metadata["exo_reflection_sandbox_id"])

    async def test_reflection_forks_the_submitted_sandbox(self) -> None:
        agent = self.build("true")
        context = SimpleNamespace(metadata=None)
        with patch("exo_harbor.agent.export_trial_trajectory", AsyncMock()):
            await agent.run("do the thing", SimpleNamespace(), context)
        agent._client.fork_sandbox.assert_awaited_once_with(
            f"trial-{agent.context_id}", "sandbox-1"
        )
        self.assertEqual(context.metadata["exo_reflection_sandbox_id"], "sandbox-fork")

    async def test_a_timeout_still_forks(self) -> None:
        # The trial is over, but the container is alive until the verifier
        # finishes -- this is the last chance to capture it for reflection.
        agent = self.build("true")
        agent._client.send.side_effect = TimeoutError("task timeout")
        context = SimpleNamespace(metadata=None)
        with patch("exo_harbor.agent.export_trial_trajectory", AsyncMock()):
            with self.assertRaises(TimeoutError):
                await agent.run("do the thing", SimpleNamespace(), context)
        agent._client.fork_sandbox.assert_awaited_once()
        self.assertEqual(context.metadata["exo_reflection_sandbox_id"], "sandbox-fork")

    async def test_a_failed_fork_does_not_mask_the_real_error(self) -> None:
        # Raising from the finally would replace the timeout and Harbor would
        # record the wrong reason for the failure.
        agent = self.build("true")
        agent._client.send.side_effect = TimeoutError("task timeout")
        agent._client.fork_sandbox.side_effect = RuntimeError("no sandbox")
        context = SimpleNamespace(metadata=None)
        with patch("exo_harbor.agent.export_trial_trajectory", AsyncMock()):
            with self.assertRaises(TimeoutError):
                await agent.run("do the thing", SimpleNamespace(), context)
        # No fork id means the plugin skips reflection.
        self.assertIsNone(context.metadata["exo_reflection_sandbox_id"])


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
                "exo_reflection_sandbox_id": None,
                "exo_instruction": "do the thing",
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
