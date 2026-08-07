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


logger = logging.getLogger(__name__)


class ExoAgent(BaseAgent):
    """Attaches Harbor's task container to an Exo conversation and waits."""

    # Multi-step tasks call resume() for continuation steps.
    SUPPORTS_RESUME = True
    SUPPORTS_ATIF = False
    SUPPORTS_WINDOWS = False

    def __init__(
        self,
        *args: Any,
        # Set by --ak. exo_root is the only one the plugin also needs; it
        # reads it back off job.config.agents[...].kwargs so the value is
        # never written down twice.
        exo_root: str | Path,
        task_timeout_sec: float | str = 1800,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._exo_root = Path(exo_root)
        self._task_timeout_sec = float(task_timeout_sec)

        # Resolved in setup(), valid for this trial only.
        self._conversation_id: str | None = None
        self._sandbox_id: str | None = None
        # Harbor's Docker container id. Kept alongside the Exo sandbox id so
        # the completion check can compare both — Exo's bookkeeping and the
        # container it actually resolves to.
        self._container_id: str | None = None

    @staticmethod
    @override
    def name() -> str:
        return "exo"

    @override
    def version(self) -> str | None:
        # TODO: report the Exo build the session is running, not this
        # package's version — the eval result needs to identify the agent
        # under test.
        raise NotImplementedError

    # ----------------------------------------------------------------------
    # Per-trial setup
    # ----------------------------------------------------------------------

    @override
    async def setup(self, environment: BaseEnvironment) -> None:
        """Bind this trial's container to an Exo conversation.

        Runs once per trial, after Harbor has started and health-checked the
        environment. Everything expensive already happened in on_job_start,
        so this should be fast.
        """
        # 1. Confirm the adapter is actually listening.
        #
        #    This is the preflight that catches a run launched with --agent and
        #    no --plugin. Without it every convention below still resolves to
        #    something plausible, so the job runs all its tasks, scores them,
        #    and reports a result that measures nothing — Exo never receives a
        #    single verification_result.
        #
        #    A live probe beats a marker file: it proves the adapter is up now,
        #    rather than that something wrote a file at some point which may be
        #    left over from a previous run in a reused exo_root.
        # TODO: if not await probe(socket_path(self._exo_root), timeout_sec=...):
        #           raise RuntimeError("no Harbor adapter; pass --plugin ...")

        # 2. Pick this trial's conversation. per_task creates a fresh one;
        #    shared reuses the job-wide one. Neither uses the conversation
        #    that owns the adapter. The Exo agent is addressed by its fixed
        #    slug (conventions.AGENT_SLUG) — agent refs resolve by slug as
        #    well as UUID, so no generated id has to reach this class.
        # TODO: self._conversation_id = client.ensure_conversation(...)

        # 3. Resolve Harbor's running `main` container by Compose label and
        #    borrow it. Held until the plugin detaches at trial end.
        # TODO: container = resolve_main_container(environment.session_id)
        # TODO: self._sandbox_id = client.attach_sandbox(...)
        raise NotImplementedError

    # ----------------------------------------------------------------------
    # Per-step execution
    # ----------------------------------------------------------------------

    # Harbor's run/resume split is about the agent's *own session*, not about
    # the environment — the container is attached per trial and is identical
    # for both. Harbor's wording: resume "the agent's native session from the
    # previous step instead of starting a fresh conversation on each step".
    #
    # Exo's native session is the conversation, so:
    #
    #   run()     step gets a fresh conversation
    #   resume()  step continues the previous step's conversation
    #
    # Gated on --resume-trajectory, which defaults OFF. Default multi-step
    # behavior is (fresh, fresh, fresh, ...) — resume() is never called.
    #
    # OPEN: this collides with conversation_mode, which already decides
    # conversation reuse at the *trial* level. The matrix of
    # {per_task, shared} x {fresh-per-step, resume-per-step} needs a defined
    # answer before either flag can be trusted in a result.
    #
    # OPEN: if a step really gets a fresh conversation, the borrowed container
    # has to be attached to *that* conversation, so attach cannot live wholly
    # in setup() as it does below. Attach is Exo-side bookkeeping with no
    # Docker call, so re-attaching per step is cheap — but then the plugin's
    # trial-end detach has more than one attachment to release.

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Work a task (or a step) in a fresh conversation."""
        await self._work(instruction, context, resumed=False)

    @override
    async def resume(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Work a step in the previous step's conversation, turns intact."""
        await self._work(instruction, context, resumed=True)

    async def _work(
        self,
        instruction: str,
        context: AgentContext,
        *,
        resumed: bool,
    ) -> None:
        # Populate metadata FIRST. It is the only channel back to the plugin,
        # and the plugin needs it to detach and deliver feedback even when
        # this step times out or raises.
        self._populate_metadata(context)

        # Send task_started and block on task_complete.
        #
        # Completion is the explicit adapter message, never the presence of a
        # final assistant turn — Exo decides when the container is ready to be
        # graded. The failure mode that buys us: Exo stops without sending it
        # and we burn to the agent timeout. If that shows up in practice the
        # answer is a nudge-and-rewake here, not treating text as completion.
        #
        # No detach in a finally block. Detach is the plugin's, at trial end.
        # TODO: response = await send_task_started(...)
        # TODO: record response.summary on the context

        # Assert Exo is still in the container Harbor is about to grade.
        #
        # This must run HERE — after task_complete, before returning — because
        # Harbor starts the verifier the instant run() returns. Checking any
        # later means the wrong container has already been graded and the
        # score written.
        #
        # Guards a failure that is possible today: CreateSandbox is not
        # blocked for a conversation, and sandbox resolution falls back to the
        # most recent created-or-attached handle, so Exo can quietly relocate
        # mid-task and leave the borrowed container untouched. Harbor would
        # then grade an empty container and record a plausible zero with no
        # error anywhere.
        #
        # Raising propagates as a Harbor agent error, so the trial carries
        # exception_info instead of a reward — void, not failed. Keeping those
        # apart matters: a silently voided task read as a genuine zero would
        # bend the learning curve downward for a reason that has nothing to do
        # with Exo's competence.
        #
        # TODO: BLOCKED — needs `exo conversation sandbox status`, which does
        # not exist yet. See ExoClient.verify_sandbox_unchanged.
        # TODO: client.verify_sandbox_unchanged(
        #     agent_id=..., conversation_id=self._conversation_id,
        #     sandbox_id=self._sandbox_id, container_id=self._container_id,
        # )
        raise NotImplementedError

    def _populate_metadata(self, context: AgentContext) -> None:
        """Write the agent -> plugin handoff into Harbor's own data flow.

        Harbor carries AgentContext into TrialResult, which the plugin reads
        at TrialEvent.END. This is how the plugin learns which conversation to
        send the grade to and which sandbox to release.
        """
        # TODO: context.metadata["exo"] = {
        #     "conversation_id": ..., "sandbox_id": ...,
        #     "socket_path": str(...), "agent_id": ...,
        # }
        raise NotImplementedError
