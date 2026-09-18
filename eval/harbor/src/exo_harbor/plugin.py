"""ExoSessionPlugin - prepares the shared Exo agent once per Harbor job and,
when enabled, lets it reflect on each graded trial."""

from __future__ import annotations

import json
import logging
from pathlib import Path
from typing import Any

from harbor.job import Job
from harbor.models.environment_type import EnvironmentType
from harbor.models.job.plugin import BaseJobPlugin
from harbor.models.job.result import JobResult
from harbor.models.trial.result import TrialResult
from harbor.trial.hooks import TrialHookEvent

from exo_harbor import conventions
from exo_harbor.exo import EXO_HARNESS, ExoClient
from exo_harbor.trajectory import export_trial_trajectory

logger = logging.getLogger(__name__)


class ExoSessionPlugin(BaseJobPlugin):
    def __init__(
        self,
        *,
        feedback_timeout_sec: float | str = 600,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._feedback_timeout_sec = float(feedback_timeout_sec)
        self._client: ExoClient | None = None
        self._model: str | None = None

    async def on_job_start(self, job: Job) -> None:
        # Run at the start of the job (full run): sets up the Agent for all trials to share.
        if job.config.environment.type != EnvironmentType.DOCKER:
            raise ValueError("ExoSessionPlugin must be on Docker")
        if len(job.config.agents) != 1:
            raise ValueError("only supports single agent")

        kwargs = job.config.agents[0].kwargs
        harness = kwargs.get("harness", EXO_HARNESS)
        if job.config.n_concurrent_trials != 1 and harness == EXO_HARNESS:
            # Exo self-edits, so to avoid races and enable learning from one
            # trial to the next, we require sequential trials. 
            raise ValueError(
                f"--n-concurrent {job.config.n_concurrent_trials} is not allowed "
                "with the exo harness: trials share one self-evolving agent and "
                "must run in sequence"
            )
        try:
            model = kwargs["exo_model"]
            client = ExoClient(
                exo_bin=Path(kwargs["exo_bin"]),
                exo_root=Path(kwargs["exo_root"]),
                repo_root=Path(kwargs["exo_repo_root"]),
                harness=harness,
            )
        except KeyError as error:
            raise ValueError(f"ExoAgent is missing required --ak {error.args[0]}") from error

        await client.ensure_agent(model)
        self._client = client
        self._model = model
        if conventions.parse_flag(kwargs.get("reflection")):
            job.on_trial_ended(self._reflect_on_trial)
        else:
            logger.info("reflection is off; trials will not review grader feedback")
        logger.info("Exo ready for job %s under %s", job.id, client.exo_root)

    async def _reflect_on_trial(self, event: TrialHookEvent) -> None:
        try:
            await self._reflect(event)
        except Exception:
            # Reflection runs after grading, so it cannot change this trial's
            # score, but raising here would forfeit every remaining trial.
            logger.exception(
                "trial %s reflection failed; continuing with the job",
                event.result.id,
            )

    async def _reflect(self, event: TrialHookEvent) -> None:
        """Review the grade inside the fork captured at the end of the trial.

        Harbor has verified and removed its own container by now; the fork is
        the submitted state, untouched by verification.
        """
        result = event.result
        context = result.agent_result
        metadata = context.metadata if context is not None else None
        if not metadata or not metadata.get(
            conventions.REFLECTION_SANDBOX_METADATA_KEY
        ):
            logger.info("trial %s has no Exo fork; skipping feedback", result.id)
            return

        conversation = metadata.get(conventions.CONVERSATION_METADATA_KEY)
        instruction = metadata.get(conventions.INSTRUCTION_METADATA_KEY)
        sandbox_id = metadata[conventions.REFLECTION_SANDBOX_METADATA_KEY]
        if (
            not isinstance(conversation, str)
            or not isinstance(instruction, str)
            or not isinstance(sandbox_id, str)
        ):
            raise ValueError(f"trial {result.id} has incomplete Exo metadata")
        if self._client is None or self._model is None:
            raise RuntimeError("Exo feedback hook ran before job setup")

        trial_dir = event.config.trials_dir / event.trial_name
        logger.info("trial %s reflecting in forked sandbox %s", result.id, sandbox_id)

        try:
            await self._client.select_sandbox(conversation, sandbox_id)
            feedback = build_feedback(result, trial_dir / "verifier")
            await self._client.send(
                conversation,
                f"{REFLECTION_INSTRUCTIONS}\n\nGrader feedback:\n{feedback}",
                timeout_sec=self._feedback_timeout_sec,
            )
            # Re-export so Harbor's trajectory includes the reflection turn.
            await _export_trajectory(
                self._client,
                conversation,
                str(result.id),
                instruction,
                self._model,
                trial_dir / "agent" / "trajectory.json",
            )
            logger.info("trial %s feedback complete", result.id)
        finally:
            # Reflection owns this fork, so it destroys it; nothing else will,
            # and one container per trial adds up over a long run. In a
            # finally because a failed or timed-out reflection leaks just as
            # readily.
            try:
                await self._client.terminate_sandbox(conversation, sandbox_id)
            except Exception:
                logger.exception(
                    "trial %s could not terminate reflection sandbox %s",
                    result.id,
                    sandbox_id,
                )

    async def on_job_end(self, _job_result: JobResult) -> None:
        # Required by the plugin protocol. The agent's state lives under the
        # run directory and is kept for inspection, so there is nothing to do.
        pass


async def _export_trajectory(
    client: ExoClient,
    conversation: str,
    trial_id: str,
    instruction: str,
    model: str,
    destination: Path,
) -> None:
    try:
        await export_trial_trajectory(
            client, conversation, trial_id, instruction, model, destination
        )
    except Exception:
        logger.exception("failed to export Harbor trajectory for %s", trial_id)


REFLECTION_INSTRUCTIONS = """Review your work on this completed trial and the grader feedback.
Your shell is attached to a fork of the environment you submitted, so inspect it when useful.
Determine what went well or wrong and extract general lessons that will help you solve similar tasks in the future.
Before ending this turn, persist any useful generalizable lesson in durable memory so later trial conversations can use it.
If there are any routines that appeared that may be re-usable, create or improve a tool for them.
If there is any mechanism in your own policy or implementation that could be improved to be better at this class of task, change it.
Anything you only say in your reply is a report to the evaluator; it does not persist learning.
If there is genuinely nothing worth retaining, say so explicitly.
Focus on improving future behavior; this submission has already been graded."""

# The prompt travels as one argv string to `exo conversation send`, so it is
# bounded by Linux's MAX_ARG_STRLEN (128 KiB).
MAX_ARG_STRLEN = 128 * 1024
MAX_FEEDBACK_BYTES = 48_000
MAX_SERIALIZED_FEEDBACK_BYTES = 96 * 1024

# Everything before this banner is toolchain installation, not test output.
PYTEST_BANNER = "= test session starts ="


def strip_setup_noise(text: str) -> str:
    index = text.find(PYTEST_BANNER)
    if index == -1:
        return text
    return text[text.rfind("\n", 0, index) + 1 :]


def build_feedback(result: TrialResult, verifier_dir: Path) -> str:
    """Return structured rewards, failures, and available verifier logs."""
    remaining = MAX_FEEDBACK_BYTES
    logs: dict[str, str] = {}
    if verifier_dir.is_dir():
        for path in sorted(item for item in verifier_dir.rglob("*") if item.is_file()):
            if remaining <= 0:
                break
            data = strip_setup_noise(
                path.read_bytes().decode("utf-8", errors="replace")
            ).encode("utf-8")
            kept = data[:remaining]
            text = kept.decode("utf-8", errors="replace")
            remaining -= len(kept)
            if len(kept) < len(data):
                text += f"\n[truncated {len(data) - len(kept)} bytes]"
            logs[str(path.relative_to(verifier_dir))] = text

    payload = {
        "rewards": (
            result.verifier_result.rewards
            if result.verifier_result is not None
            else None
        ),
        "exception": (
            result.exception_info.model_dump(mode="json")
            if result.exception_info is not None
            else None
        ),
        "verifier_logs": logs,
    }
    serialized = json.dumps(payload, indent=2, sort_keys=True)
    if len(serialized.encode("utf-8")) <= MAX_SERIALIZED_FEEDBACK_BYTES:
        return serialized

    dropped = sum(len(text.encode("utf-8")) for text in logs.values())
    payload["verifier_logs"] = {
        "[omitted]": f"{dropped} bytes of verifier logs did not fit the prompt"
    }
    return json.dumps(payload, indent=2, sort_keys=True)
