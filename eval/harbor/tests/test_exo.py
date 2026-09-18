from __future__ import annotations

import unittest
from pathlib import Path
from unittest.mock import patch

from exo_harbor import conventions
from exo_harbor.exo import ExoClient, ExoCommandError


CLIENT = ExoClient(
    exo_bin=Path("/repo/target/debug/exo"),
    exo_root=Path("/runs/one/exo"),
    repo_root=Path("/repo"),
)


class ArgvTest(unittest.TestCase):

    def test_selects_the_exo_harness(self) -> None:
        # Without it, `agent create --module` is rejected outright: --module is
        # only valid with --harness typescript or exo. The flag's own help text
        # omits `exo`, so this is easy to drop by reading the docs alone.
        argv = CLIENT._argv("agent", "create")
        self.assertEqual(argv[argv.index("--harness") + 1], "exo")

    def test_harness_module_comes_from_this_repo_layout(self) -> None:
        self.assertTrue(
            (Path(__file__).parents[3] / conventions.HARNESS_MODULE).is_file(),
            f"{conventions.HARNESS_MODULE} should exist in this repo",
        )


class AttachTest(unittest.IsolatedAsyncioTestCase):
    """Attaching goes through `exo conversation sandbox attach`.

    The conversation is what makes the container usable by the trial: the
    executor runs a conversation's turns in the sandbox attached to it.
    """

    async def attach(self, output: str) -> tuple[str, tuple[str, ...]]:
        calls: list[tuple[str, ...]] = []

        async def fake_run(_self, *args: str, **_kwargs: object) -> str:
            calls.append(args)
            return output

        with patch.object(ExoClient, "_run", fake_run):
            sandbox_id = await CLIENT.attach_container(
                "trial-1", "abc123", default_workdir="/app"
            )
        return sandbox_id, calls[0]

    async def test_names_the_trial_conversation(self) -> None:
        _, argv = await self.attach(
            "attached Docker container as sandbox sandbox-1 for trial-1"
        )
        self.assertEqual(
            argv[:5],
            ("conversation", "sandbox", "attach", conventions.AGENT_SLUG, "trial-1"),
        )
        self.assertEqual(argv[argv.index("--external-id") + 1], "abc123")
        self.assertEqual(argv[argv.index("--default-workdir") + 1], "/app")

    async def test_returns_the_sandbox_id_from_the_cli_output(self) -> None:
        sandbox_id, _ = await self.attach(
            "attached Docker container as sandbox sandbox-1 for trial-1"
        )
        self.assertEqual(sandbox_id, "sandbox-1")

    async def test_unrecognised_output_is_an_error(self) -> None:
        # Silently returning garbage would only surface later, as a confusing
        # failure somewhere else in the trial.
        with self.assertRaises(ExoCommandError):
            await self.attach("something else entirely")


if __name__ == "__main__":
    unittest.main()
