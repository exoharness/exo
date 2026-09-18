from __future__ import annotations

import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

from exo_harbor import conventions
from exo_harbor.plugin import ExoSessionPlugin, build_feedback, strip_setup_noise


def trial_event(metadata: dict[str, object] | None, tmp_path: Path):
    return SimpleNamespace(
        result=SimpleNamespace(
            id="trial-1",
            agent_result=SimpleNamespace(metadata=metadata),
            verifier_result=SimpleNamespace(rewards={"reward": 1.0}),
            exception_info=None,
        ),
        config=SimpleNamespace(trials_dir=tmp_path),
        trial_name="trial-1",
    )


FORKED = {
    conventions.CONVERSATION_METADATA_KEY: "trial-abc",
    conventions.REFLECTION_SANDBOX_METADATA_KEY: "sandbox-fork",
    conventions.INSTRUCTION_METADATA_KEY: "do the thing",
}


def ready_plugin(client: AsyncMock) -> ExoSessionPlugin:
    plugin = ExoSessionPlugin()
    plugin._client = client
    plugin._model = "gpt-5.5"
    return plugin


class ReflectionTest(unittest.IsolatedAsyncioTestCase):
    async def test_reflects_inside_the_fork_in_the_trials_conversation(self) -> None:
        client = AsyncMock()
        with patch("exo_harbor.plugin.export_trial_trajectory", AsyncMock()):
            await ready_plugin(client)._reflect_on_trial(
                trial_event(FORKED, Path("/tmp/trials"))
            )

        client.select_sandbox.assert_awaited_once_with("trial-abc", "sandbox-fork")
        # Same conversation as the trial, or the agent reflects with no memory
        # of what it did.
        conversation, prompt = client.send.await_args.args
        self.assertEqual(conversation, "trial-abc")
        self.assertIn("Grader feedback:", prompt)

    async def test_the_fork_is_destroyed_even_when_reflection_fails(self) -> None:
        # One leaked container per trial adds up over a long run, and a
        # reflection that times out is exactly where it would leak.
        client = AsyncMock()
        client.send.side_effect = TimeoutError("reflection timed out")
        with patch("exo_harbor.plugin.export_trial_trajectory", AsyncMock()):
            await ready_plugin(client)._reflect_on_trial(
                trial_event(FORKED, Path("/tmp/trials"))
            )
        client.terminate_sandbox.assert_awaited_once_with("trial-abc", "sandbox-fork")

    async def test_a_failed_reflection_does_not_abort_the_job(self) -> None:
        client = AsyncMock()
        client.select_sandbox.side_effect = OSError(7, "Argument list too long")
        with patch("exo_harbor.plugin.logger.exception") as log_exception:
            await ready_plugin(client)._reflect_on_trial(
                trial_event(FORKED, Path("/tmp/trials"))
            )
        log_exception.assert_called_once()

    async def test_a_trial_without_a_fork_skips_reflection(self) -> None:
        client = AsyncMock()
        await ready_plugin(client)._reflect_on_trial(trial_event({}, Path("/tmp/trials")))
        client.send.assert_not_awaited()


class FeedbackTest(unittest.TestCase):
    def test_includes_rewards_exception_and_verifier_output(self) -> None:
        with TemporaryDirectory() as directory:
            verifier_dir = Path(directory)
            (verifier_dir / "test-stdout.txt").write_text("one test failed")
            result = SimpleNamespace(
                verifier_result=SimpleNamespace(rewards={"reward": 0.5}),
                exception_info=SimpleNamespace(
                    model_dump=lambda **_kwargs: {"message": "verification failed"}
                ),
            )
            feedback = json.loads(build_feedback(result, verifier_dir))

        self.assertEqual(feedback["rewards"], {"reward": 0.5})
        self.assertEqual(feedback["exception"]["message"], "verification failed")
        self.assertEqual(feedback["verifier_logs"]["test-stdout.txt"], "one test failed")

    def test_oversized_logs_are_dropped_so_the_prompt_fits_argv(self) -> None:
        # A single argv string is capped at 128 KiB by the kernel; some
        # verifiers print more than that.
        from exo_harbor.plugin import MAX_ARG_STRLEN

        with TemporaryDirectory() as directory:
            verifier_dir = Path(directory)
            (verifier_dir / "test-stdout.txt").write_text("x" * 400_000)
            result = SimpleNamespace(
                verifier_result=SimpleNamespace(rewards={"reward": 0.0}),
                exception_info=None,
            )
            feedback = build_feedback(result, verifier_dir)

        self.assertLess(len(feedback.encode("utf-8")), MAX_ARG_STRLEN)
        self.assertEqual(json.loads(feedback)["rewards"], {"reward": 0.0})

    def test_setup_noise_before_the_pytest_banner_is_dropped(self) -> None:
        text = (
            "Get:1 http://archive.ubuntu.com/ubuntu noble InRelease [126 kB]\n"
            "============================= test session starts ====\n"
            "collected 1 item\n"
        )
        self.assertEqual(
            strip_setup_noise(text),
            "============================= test session starts ====\ncollected 1 item\n",
        )


if __name__ == "__main__":
    unittest.main()
