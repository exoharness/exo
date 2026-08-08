"""ExoSessionPlugin — owner of the Exo process for the whole job.

Harbor exposes two lifecycle planes, and Exo needs both:

    trial plane (BaseAgent)     one task, object destroyed after, sees the
                                environment and the instruction, never sees
                                the reward
    job plane   (JobPlugin)     spans every trial, sees each TrialResult
                                including verifier_result, never sees the
                                environment

Exo's whole claim is cross-trial improvement, which the trial plane
structurally cannot hold. So this plugin owns Exo's lifetime and the feedback
loop, and ExoAgent is reduced to a per-task stub.

    attach_job_plugins(job)
      on_job_start(job)         start Exo, create the adapter, register hooks
      job.run()
        ... trial 1 ...
        TrialEvent.END          detach, send the grade, wait for reflection
        ... trial 2 ...
      on_job_end(job_result)    write the report, tear down

Requires --n-concurrent 1. Trials share one Exo, and reflection on trial N
must finish before trial N+1 begins or the learning signal interleaves.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

from harbor.job import Job
from harbor.models.environment_type import EnvironmentType
from harbor.models.job.plugin import BaseJobPlugin
from harbor.models.job.result import JobResult
from harbor.models.trial.paths import TrialPaths
from harbor.models.trial.result import TrialResult
from harbor.trial.hooks import TrialHookEvent

from exo_harbor import conventions
from exo_harbor.exo import ExoClient
from exo_harbor.protocol import (
    VerificationException,
    VerificationResult,
    send_verification_result,
)
from exo_harbor.report import (
    Inventory,
    JobReport,
    TrialSnapshot,
    capture_inventory,
    write_report,
)

logger = logging.getLogger(__name__)


class ExoSessionPlugin(BaseJobPlugin):
    """Runs one Exo across every trial in the job and feeds it the grades."""

    def __init__(
        self,
        *,
        # Set by --pk. Note exo_root is absent: it is read off the agent's own
        # kwargs in on_job_start so it stays a single source of truth.
        feedback_timeout_sec: float | str = 900,
        adapter_start_timeout_sec: float | str = 90,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._feedback_timeout_sec = float(feedback_timeout_sec)
        self._adapter_start_timeout_sec = float(adapter_start_timeout_sec)

        self._job_id: str | None = None
        self._job_dir: Path | None = None
        self._client: ExoClient | None = None
        self._snapshots: list[TrialSnapshot] = []

    # ----------------------------------------------------------------------
    # Job start
    # ----------------------------------------------------------------------

    async def on_job_start(self, job: Job) -> None:
        """Bring Exo up before the first container is built.

        Validation happens at the top so a misconfigured run dies in a second
        rather than after Docker spends a minute on the first image.
        """
        if job.config.environment.type != EnvironmentType.DOCKER:
            raise ValueError(
                "ExoSessionPlugin resolves task containers by Compose label on "
                "the host, so it requires --env docker"
            )
        if job.config.n_concurrent_trials != 1:
            # Not a plumbing limit — the adapter keys exchanges by trial id and
            # would cope. The claim under test is "at task N, Exo has learned
            # from tasks 1..N-1". Run trials concurrently and trial 20 begins
            # before reflection on 18 and 19, so task number stops meaning
            # accumulated learning and every chart loses its x-axis.
            #
            # Binds the stateful arm only: a stateless control run, or any
            # baseline agent, has no ordering to preserve and can go wide.
            raise ValueError("ExoSessionPlugin requires --n-concurrent 1")
        if len(job.config.agents) != 1:
            raise ValueError("ExoSessionPlugin requires exactly one --agent")

        # Read the agent's --ak values rather than taking --pk copies, so
        # exo_root and friends are written down once.
        kwargs = job.config.agents[0].kwargs
        try:
            self._client = ExoClient(
                exo_bin=Path(kwargs["exo_bin"]),
                exo_root=Path(kwargs["exo_root"]),
                repo_root=Path(kwargs["exo_repo_root"]),
                model=kwargs["exo_model"],
            )
        except KeyError as error:
            raise ValueError(
                f"ExoAgent is missing required --ak {error.args[0]}"
            ) from error

        socket_path = conventions.socket_path(self._client.exo_root)
        await self._client.ensure_agent()
        await self._client.ensure_harbor_adapter(socket_path)
        # Waiting here is what makes the agents' preflight meaningful: they
        # check the same socket to detect a --plugin-less run.
        await self._client.ensure_adapter_runner(
            socket_path, timeout_sec=self._adapter_start_timeout_sec
        )

        self._job_id = str(job.id)
        self._job_dir = job.job_dir
        job.on_trial_ended(self._on_trial_ended)
        logger.info("Exo ready for job %s on %s", job.id, socket_path)

    # ----------------------------------------------------------------------
    # Per-trial feedback
    # ----------------------------------------------------------------------

    async def _on_trial_ended(self, event: TrialHookEvent) -> None:
        """Close the loop: release the container, deliver the grade, wait.

        Fires from Trial._finalize(), so the verifier has already run and the
        environment is already stopped. Detaching here rather than in the agent
        is what makes multi-step tasks work — the agent's run() is per-step and
        must not tear down an attachment later steps still need.

        TOTAL BY CONSTRUCTION. Trial._emit does not guard hook calls, and
        _finalize() runs inside a finally before `return self.result`, so an
        exception raised here escapes trial.run() and the trial produces no
        result at all — not a failed one, none.
        """
        try:
            await self._deliver_feedback(event)
        except Exception:
            logger.exception(
                "feedback failed for trial %s; continuing", event.trial_name
            )

        try:
            self._snapshots.append(await self._snapshot(event))
        except Exception:
            logger.exception("inventory snapshot failed for %s", event.trial_name)

    async def _deliver_feedback(self, event: TrialHookEvent) -> None:
        assert self._client is not None
        metadata = _exo_metadata(event.result)
        if metadata is None:
            # The agent never got far enough to record its handoff, so there is
            # no conversation to send a grade to and nothing attached.
            logger.warning("trial %s has no Exo metadata", event.trial_name)
            return

        conversation = metadata["conversation"]
        sandbox_id = metadata.get("sandbox_id")
        if sandbox_id:
            try:
                await self._client.detach_sandbox(conversation, sandbox_id)
            except Exception as error:
                # Feedback still matters if cleanup bookkeeping fails. Keep
                # the trial result and surface the stale record in the log.
                logger.warning("detach failed for %s: %s", event.trial_name, error)

        paths = TrialPaths(trial_dir=self._job_dir / event.trial_name)
        await send_verification_result(
            conventions.socket_path(self._client.exo_root),
            VerificationResult(
                trial_id=str(event.trial_id),
                task_name=event.task_name,
                conversation_id=conversation,
                rewards=_rewards(event.result),
                verifier_stdout=_read(paths.test_stdout_path),
                verifier_stderr=_read(paths.test_stderr_path),
                exception=_exception(event.result),
            ),
            timeout_sec=self._feedback_timeout_sec,
        )

    async def _snapshot(self, event: TrialHookEvent) -> TrialSnapshot:
        assert self._client is not None
        inventory = await capture_inventory(
            await self._client.list_tools(), self._client.repo_root
        )
        return TrialSnapshot(
            trial_id=str(event.trial_id),
            trial_name=event.trial_name,
            task_name=event.task_name,
            index=len(self._snapshots),
            rewards=_rewards(event.result),
            inventory=inventory,
        )

    # ----------------------------------------------------------------------
    # Job end
    # ----------------------------------------------------------------------

    async def on_job_end(self, job_result: JobResult) -> None:
        """Write the accumulation report.

        finalize_job_plugins SWALLOWS exceptions from this method, so anything
        that fails here fails invisibly. Get the measured data on disk first
        and do nothing clever after it.

        Nothing is torn down: each exo CLI call is its own process, and the
        adapter runner is left alive deliberately. The durable state under
        exo_root — the tools, memory, and source edits Exo accumulated — is the
        real artifact of the run and must survive regardless.
        """
        if self._client is None or self._job_dir is None:
            return
        try:
            final = await capture_inventory(
                await self._client.list_tools(), self._client.repo_root
            )
        except Exception:
            logger.exception("final inventory failed")
            final = Inventory()

        path = write_report(
            self._job_dir,
            JobReport(
                job_id=self._job_id or str(job_result.id),
                snapshots=self._snapshots,
                final=final,
            ),
        )
        logger.info("wrote %s", path)


def _exo_metadata(result: TrialResult) -> dict[str, Any] | None:
    """Recover the agent's handoff from Harbor's own data flow.

    Multi-step trials record one context per step; take the last that carries
    it, since earlier steps may predate the attach.
    """
    contexts = [result.agent_result] if result.agent_result is not None else []
    contexts.extend(
        step.agent_result
        for step in (result.step_results or [])
        if step.agent_result is not None
    )
    for context in reversed(contexts):
        metadata = (context.metadata or {}).get("exo")
        if isinstance(metadata, dict) and metadata.get("conversation"):
            return metadata
    return None


def _rewards(result: TrialResult) -> dict[str, float] | None:
    if result.verifier_result is not None:
        return result.verifier_result.rewards
    rewards: dict[str, float] = {}
    for step in result.step_results or []:
        if step.verifier_result is None:
            continue
        for name, value in (step.verifier_result.rewards or {}).items():
            rewards[f"{step.step_name}.{name}"] = value
    return rewards or None


def _exception(result: TrialResult) -> VerificationException | None:
    """Infrastructure failure, kept distinct from scoring zero.

    Exo has to be able to tell "I failed the task" from "the harness broke", or
    it will learn from noise.
    """
    info = result.exception_info
    if info is None:
        return None
    return VerificationException(
        type=info.exception_type,
        message=info.exception_message,
        traceback=info.exception_traceback,
    )


def _read(path: Path) -> str | None:
    try:
        return path.read_text()
    except OSError:
        return None
