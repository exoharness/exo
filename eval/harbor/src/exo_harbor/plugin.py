"""ExoSessionPlugin - prepares the shared Exo agent once per Harbor job."""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

from harbor.job import Job
from harbor.models.environment_type import EnvironmentType
from harbor.models.job.plugin import BaseJobPlugin
from harbor.models.job.result import JobResult

from exo_harbor.exo import EXO_HARNESS, ExoClient

logger = logging.getLogger(__name__)


class ExoSessionPlugin(BaseJobPlugin):
    def __init__(self, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self._client: ExoClient | None = None

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
        logger.info("Exo ready for job %s under %s", job.id, client.exo_root)

    async def on_job_end(self, _job_result: JobResult) -> None:
        # Required by the plugin protocol. The agent's state lives under the
        # run directory and is kept for inspection, so there is nothing to do.
        pass
