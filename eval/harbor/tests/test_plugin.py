from __future__ import annotations

import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

from harbor.models.environment_type import EnvironmentType

from exo_harbor.plugin import ExoSessionPlugin


def job(
    *,
    environment: EnvironmentType = EnvironmentType.DOCKER,
    n_concurrent: int = 1,
    kwargs: dict[str, str] | None = None,
) -> SimpleNamespace:
    if kwargs is None:
        kwargs = {
            "exo_model": "gpt-5.5",
            "exo_bin": "/repo/target/debug/exo",
            "exo_root": "/runs/one/exo",
            "exo_repo_root": "/repo",
        }
    return SimpleNamespace(
        id="job-1",
        config=SimpleNamespace(
            environment=SimpleNamespace(type=environment),
            n_concurrent_trials=n_concurrent,
            agents=[SimpleNamespace(kwargs=kwargs)],
        ),
    )


class ExoSessionPluginTest(unittest.IsolatedAsyncioTestCase):
    async def test_creates_the_shared_agent_once_per_job(self) -> None:
        plugin = ExoSessionPlugin()
        with patch("exo_harbor.plugin.ExoClient.ensure_agent", AsyncMock()) as ensure:
            await plugin.on_job_start(job())
        ensure.assert_awaited_once_with("gpt-5.5")

    async def test_exo_refuses_concurrent_trials(self) -> None:
        # Exo trials share one self-evolving agent and each is meant to see
        # what earlier ones learned; in parallel that ordering is gone.
        with self.assertRaises(ValueError):
            await ExoSessionPlugin().on_job_start(job(n_concurrent=2))

    async def test_stateless_harnesses_may_run_concurrently(self) -> None:
        for harness in ("basic", "pi"):
            with self.subTest(harness=harness):
                kwargs = {
                    "exo_model": "gpt-5.5",
                    "exo_bin": "/repo/target/debug/exo",
                    "exo_root": "/runs/one/exo",
                    "exo_repo_root": "/repo",
                    "harness": harness,
                }
                with patch("exo_harbor.plugin.ExoClient.ensure_agent", AsyncMock()):
                    await ExoSessionPlugin().on_job_start(
                        job(n_concurrent=4, kwargs=kwargs)
                    )

    async def test_a_missing_agent_kwarg_names_the_flag(self) -> None:
        with self.assertRaises(ValueError) as raised:
            await ExoSessionPlugin().on_job_start(
                job(kwargs={"exo_model": "gpt-5.5"})
            )
        self.assertIn("--ak exo_bin", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
