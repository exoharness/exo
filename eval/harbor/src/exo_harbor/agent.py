"""ExoAgent — the per-task RPC stub.

This class is *not* the agent. Harbor constructs a fresh instance for every
trial (``Trial.__init__`` -> ``_init_agent()``) and throws it away when the
trial finishes, so it can hold no cross-task state. The real agent is the Exo
process, whose lifetime belongs to ExoSessionPlugin.

Where this sits in Harbor's trial lifecycle:

    Trial.create()                    <- new ExoAgent instance
    trial.run()
      _prepare()
        _setup_agent_environment()    <- docker compose up --wait
        run_healthcheck()
        setup(environment)            <- HERE: attach the container   [1/trial]
      _run()
        run(instruction, ...)         <- HERE: work the task      [1/step]
        _run_verifier()               <- the grade appears here
        _stop_agent_environment()
      _finalize() -> TrialEvent.END   <- plugin detaches + sends feedback

Two consequences worth holding onto:

* ``setup()`` is per *trial*, ``run()`` is per *step*. For a multi-step task
  Harbor calls ``run()`` then ``resume()`` against the same environment. So
  attach belongs in ``setup()`` and detach belongs in the plugin's trial-end
  hook — putting detach in ``run()``'s ``finally`` breaks step 2.
* ``run()`` returns *before* the verifier executes. This class can never see a
  reward. That is the plugin's job.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any, override

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from exo_harbor import conventions
from exo_harbor.docker import resolve_main_container
from exo_harbor.exo import ExoClient
from exo_harbor.protocol import TaskStarted, send_task_started

logger = logging.getLogger(__name__)


class ExoAgent(BaseAgent):
    """Attaches Harbor's task container to an Exo conversation and waits."""

    # Can be enabled for multi-step tasks after resume() support is added.
    SUPPORTS_RESUME = False
    SUPPORTS_ATIF = False
    SUPPORTS_WINDOWS = False

    def __init__(
        self,
        *args: Any,
        # Set by --ak. The plugin reads these same values back off job.config.agents[0].kwargs.
        exo_root: str | Path,
        exo_bin: str | Path,
        exo_repo_root: str | Path,
        exo_model: str,
        conversation_mode: str = "per_task",
        task_timeout_sec: float | str = 1800,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._exo_root = Path(exo_root)
        self._task_timeout_sec = float(task_timeout_sec)
        self._conversation_mode = conversation_mode
        self._client = ExoClient(
            exo_bin=Path(exo_bin),
            exo_root=Path(exo_root),
            repo_root=Path(exo_repo_root),
            model=exo_model,
        )

        self._conversation: str | None = None
        self._sandbox_id: str | None = None
        self._container_id: str | None = None

    @staticmethod
    @override
    def name() -> str:
        return "exo"

    @override
    def version(self) -> str | None:
        # TODO: report the Exo build under test rather than this package's
        # version — the eval result needs to identify the agent, and "0.1.0"
        # identifies the wrapper.
        return None

    # ----------------------------------------------------------------------
    # Per-trial setup
    # ----------------------------------------------------------------------
    @override
    async def setup(self, environment: BaseEnvironment) -> None:
        """Bind this trial's container to an Exo conversation.

        Runs once per trial, after Harbor has started and health-checked the
        environment. Everything expensive happened in on_job_start.
        """
        socket_path = conventions.socket_path(self._exo_root)
        if not socket_path.exists():
            raise RuntimeError(
                f"no Harbor adapter socket at {socket_path}. Pass "
                "--plugin exo_harbor.plugin:ExoSessionPlugin alongside --agent."
            )

        self._conversation = (
            conventions.SHARED_CONVERSATION_SLUG
            if self._conversation_mode == "shared"
            else conventions.conversation_slug(self.session_id or "trial")
        )
        await self._client.ensure_conversation(self._conversation)

        container = resolve_main_container(environment.session_id)
        self._container_id = container.container_id
        self._sandbox_id = await self._client.attach_sandbox(
            self._conversation, container.container_id
        )
        logger.info(
            "attached container %s as sandbox %s on %s",
            container.container_id[:12],
            self._sandbox_id,
            self._conversation,
        )

    # ----------------------------------------------------------------------
    # Per-step execution
    # ----------------------------------------------------------------------

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Hand Exo the task and block until it says it is done."""
        if self._conversation is None or self._sandbox_id is None:
            raise RuntimeError("ExoAgent.run called before setup")

        self._populate_metadata(context)

        response = await send_task_started(
            conventions.socket_path(self._exo_root),
            TaskStarted(
                trial_id=str(self.context_id),
                task_name=self.session_id or "task",
                instruction=instruction,
                conversation_id=self._conversation,
                sandbox_id=self._sandbox_id,
            ),
            timeout_sec=self._task_timeout_sec,
        )

        # Assert Exo is still in the container Harbor is about to grade. This can
        # fail if Exo chooses to snapshot + rollback during runtime. TODO to handle
        # this case cleanly, currently fail loudly.
        await self._client.verify_sandbox_unchanged(
            self._conversation, self._sandbox_id
        )

        self._populate_metadata(context, summary=response.summary)

    def _populate_metadata(
        self, context: AgentContext, summary: str | None = None
    ) -> None:
        """Write the agent -> plugin handoff into Harbor's own data flow.

        Harbor carries AgentContext into TrialResult, which the plugin reads at
        TrialEvent.END. This is how the plugin learns which conversation to
        send the grade to and which sandbox to release.
        """
        context.metadata = {
            **(context.metadata or {}),
            "exo": {
                "conversation": self._conversation,
                "sandbox_id": self._sandbox_id,
                "container_id": self._container_id,
                "summary": summary,
            },
        }
