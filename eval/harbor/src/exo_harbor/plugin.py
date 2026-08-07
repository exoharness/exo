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
      on_job_end(job_result)    tear down

Requires --n-concurrent 1. Trials share one Exo, and reflection on trial N
must finish before trial N+1 begins or the learning signal interleaves.
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

from harbor.job import Job
from harbor.models.job.plugin import BaseJobPlugin
from harbor.models.job.result import JobResult
from harbor.trial.hooks import TrialHookEvent


logger = logging.getLogger(__name__)


class ExoSessionPlugin(BaseJobPlugin):
    """Runs one Exo across every trial in the job and feeds it the grades."""

    def __init__(
        self,
        *,
        # Set by --pk. Note exo_root is absent: it is read off the agent's
        # own kwargs in on_job_start so it stays a single source of truth.
        feedback_timeout_sec: float | str = 900,
        adapter_start_timeout_sec: float | str = 90,
        keep_exo_running: bool | str = False,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._feedback_timeout_sec = float(feedback_timeout_sec)
        self._adapter_start_timeout_sec = float(adapter_start_timeout_sec)
        self._keep_exo_running = bool(keep_exo_running)

        self._job_dir: Path | None = None

    # ----------------------------------------------------------------------
    # Job start
    # ----------------------------------------------------------------------

    async def on_job_start(self, job: Job) -> None:
        """Bring Exo up before the first container is built.

        Validation happens here, at the top, so a misconfigured run dies in a
        second rather than after Docker spends a minute on the first image.
        """
        # 1. Reject configurations the integration cannot honor:
        #      - job.config.environment.type must be DOCKER (container
        #        resolution is by Compose label on the host)
        #      - job.config.n_concurrent_trials must be 1
        #      - exactly one agent config, and it must be ExoAgent
        #
        #    On concurrency: the transport would cope fine — the adapter keys
        #    pending exchanges by trial id, so they cannot collide. The reason
        #    is the experiment, not the plumbing. The claim under test is
        #    "at task N, Exo has learned from tasks 1..N-1"; run trials
        #    concurrently and trial 20 begins before reflection on 18 and 19
        #    has happened, so task number stops corresponding to accumulated
        #    learning and the x-axis of every chart in docs/eval-design.md
        #    loses its meaning.
        #
        #    Secondary, and only an implementation issue: shared conversation
        #    mode has exactly one conversation to interleave into, and all
        #    modes share one tool registry, memory store, and source tree —
        #    concurrent installs or a rebuild_and_restart_exo mid-job would
        #    race.
        #
        #    Note this constraint binds the STATEFUL arm only. A stateless
        #    control run (needed for CL-Bench-style gain) and any baseline
        #    agent have no ordering to preserve and can run concurrently.
        # TODO

        # 2. Read the agent's --ak kwargs off job.config.agents[0].kwargs to
        #    recover exo_root/exo_bin/exo_model. Reading rather than
        #    duplicating via --pk keeps one source of truth and lets us
        #    validate the agent's config while we are here.
        # TODO

        # 3. Start Exo, create the agent record under conventions.AGENT_SLUG,
        #    and create + enable the single Harbor adapter that serves every
        #    trial conversation, listening on conventions.socket_path().
        #
        #    The fixed slug is what removes the need for any plugin -> agent
        #    handoff: agent refs resolve by slug as well as UUID, so the
        #    per-trial agents can address this record without being told its
        #    generated id.
        # TODO

        # 4. Wait for the adapter socket to accept connections. Without this,
        #    a slow Exo boot surfaces as trial 1 failing for no visible
        #    reason. It is also what makes the agents' preflight meaningful —
        #    they probe the same socket to detect a --plugin-less run.
        # TODO: await probe(socket_path, timeout_sec=self._adapter_start_timeout_sec)

        self._job_dir = job.job_dir
        job.on_trial_ended(self._on_trial_ended)
        raise NotImplementedError

    # ----------------------------------------------------------------------
    # Per-trial feedback
    # ----------------------------------------------------------------------

    async def _on_trial_ended(self, event: TrialHookEvent) -> None:
        """Close the loop: release the container, deliver the grade, wait.

        Fires from Trial._finalize(), so the verifier has already run and the
        environment is already stopped. Detaching here rather than in the
        agent is what makes multi-step tasks work — the agent's run() is
        per-step and must not tear down an attachment later steps still need.
        """
        # 1. Recover the agent's handoff from TrialResult.agent_result
        #    .metadata["exo"]. For a multi-step trial, scan step_results too
        #    and take the last one that carries it.
        # TODO

        # 2. Detach the borrowed sandbox. Idempotent, and never issues a
        #    Docker lifecycle command — Harbor owns the container.
        #
        #    Keeping it attached through reflection is not an option, and
        #    would be the wrong call even if it were:
        #      - _finalize() stops (and by default deletes) the environment
        #        before emitting END, so the container is already dead here.
        #        No hook exists with the reward computed AND the container
        #        alive; there is no VERIFICATION_END event.
        #      - Verifier.verify() uploads tests/ into the container as its
        #        first act. A post-verification shell would hand Exo the
        #        grading script, which on a procedurally generated dataset is
        #        learning-to-the-test, not learning.
        #      - Reflection must run in the persistent agent sandbox anyway.
        #        Durable state lives there; anything written in the task
        #        container dies with it.
        #
        #    To give Exo real evidence instead of a live shell, use Harbor
        #    artifacts (collected before the stop) and let it read them from
        #    the trial dir.
        # TODO

        # 3. Send verification_result and block on feedback_processed.
        #    Reward, verifier stdout/stderr, and any infrastructure exception
        #    all go across; Exo has to be able to tell a failed task from a
        #    broken harness.
        # TODO

        # 4. Write exo-feedback.json beside Harbor's result.json. Harbor has
        #    already persisted result.json by now, so this is a sidecar
        #    rather than a mutation.
        #
        #    Record a feedback timeout as its own outcome, distinct from task
        #    correctness — an Exo reflection failure is not a failed task, and
        #    conflating them corrupts the learning curve we are trying to
        #    measure.
        # TODO

        # 5. Snapshot Exo's inventory now that it has finished reflecting, and
        #    append it to the series on_job_end reports over. Measured, no
        #    model call, so it is cheap enough to do every trial — and doing it
        #    per trial is what turns "Exo ended up with 4 tools" into "tool 3
        #    appeared after task 7, and the curve bends at task 9".
        # TODO: self._snapshots.append(TrialSnapshot(...))

        # This hook must be TOTAL. Trial._emit does not guard hook calls, and
        # _finalize() runs inside a finally before `return self.result`, so an
        # exception raised here escapes trial.run() and the trial produces no
        # result at all — not a failed one, none. Catch everything, record it
        # in the sidecar, never propagate.
        raise NotImplementedError

    # ----------------------------------------------------------------------
    # Job end
    # ----------------------------------------------------------------------

    async def on_job_end(self, job_result: JobResult) -> None:
        """Emit the accumulation report, then tear down.

        The durable state under exo_root is the real artifact of a continual
        run — the tools, memory, and source edits Exo built up. Stopping the
        process is fine; that tree must survive regardless. keep_exo_running
        only controls the process.

        Ordering here is load-bearing, because finalize_job_plugins SWALLOWS
        exceptions from this method. A failure past step 2 is invisible.
        """
        # 1. Take the final inventory. Do this before asking Exo anything:
        #    the narrative is a model turn and could itself install a tool or
        #    write a memory, so a report gathered after it describes a state
        #    the act of reporting changed.
        # TODO: final = capture_inventory(...)

        # 2. Assemble the per-trial snapshots into a JobReport and write it.
        #    Measured data lands on disk here, before anything can fail.
        # TODO: write_report(job_dir, JobReport(..., narrative=None))

        # 3. Optionally ask Exo to narrate what it learned, and rewrite the
        #    report with it attached. Self-report: kept in its own field so a
        #    reader cannot mistake it for the measured inventory.
        #
        #    Costs a third adapter exchange (job_report -> job_report_ready)
        #    on top of the two per trial. Skip it and the report is still
        #    complete; record narrative_error rather than raising.
        # TODO

        # 4. If not keep_exo_running, shut Exo down. The socket goes with it,
        #    which is what makes a later stray run fail its preflight instead
        #    of silently reusing a dead job's setup.
        raise NotImplementedError
