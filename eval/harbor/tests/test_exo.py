import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

from exo_harbor.exo import ExoClient, SandboxDriftError


class ExoClientTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self.client = ExoClient(
            exo_bin=Path("/opt/exo/bin/exo"),
            exo_root=Path("/tmp/exo-root"),
            repo_root=Path("/src/exo"),
            model="model",
        )

    async def test_verify_sandbox_unchanged_accepts_attached_sandbox(self) -> None:
        run = AsyncMock(return_value='{"sandbox_id":"sandbox-1"}')
        with patch.object(ExoClient, "run", run):
            await self.client.verify_sandbox_unchanged("conversation", "sandbox-1")

        run.assert_awaited_once_with(
            "conversation",
            "sandbox",
            "status",
            "harbor-eval",
            "conversation",
            "--json",
        )

    async def test_verify_sandbox_unchanged_rejects_drift(self) -> None:
        with (
            patch.object(
                ExoClient,
                "run",
                AsyncMock(return_value='{"sandbox_id":"sandbox-2"}'),
            ),
            self.assertRaisesRegex(SandboxDriftError, "sandbox-2"),
        ):
            await self.client.verify_sandbox_unchanged("conversation", "sandbox-1")

    async def test_verify_sandbox_unchanged_rejects_no_attachment(self) -> None:
        with (
            patch.object(
                ExoClient,
                "run",
                AsyncMock(return_value='{"sandbox_id":null}'),
            ),
            self.assertRaisesRegex(SandboxDriftError, "None"),
        ):
            await self.client.verify_sandbox_unchanged("conversation", "sandbox-1")

    async def test_read_conversation_events_filters_one_turn(self) -> None:
        run = AsyncMock(return_value='{"events":[],"cursor":null}')
        with patch.object(ExoClient, "run", run):
            result = await self.client.read_conversation_events(
                "conversation",
                types=["messages", "tool_result"],
                turn_id="turn-1",
                limit=10_000,
            )

        self.assertEqual(result, '{"events":[],"cursor":null}')
        run.assert_awaited_once_with(
            "conversation",
            "events",
            "harbor-eval",
            "conversation",
            "--type",
            "messages",
            "--type",
            "tool_result",
            "--turn-id",
            "turn-1",
            "--limit",
            "10000",
        )


if __name__ == "__main__":
    unittest.main()
