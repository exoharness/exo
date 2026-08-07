"""Client for driving Exo from the Harbor integration.

Exo is a Rust process and this is Python, so there is no in-process API. Two
out-of-process surfaces exist, and they cover different things:

* **Exoharness HTTP** (``POST /request``, see docs/exoharness-http.md) — a
  unary JSON transport over ``protocol::Request``. Covers agents,
  conversations, sandboxes (including AttachSandbox/DetachSandbox), artifacts,
  and secrets. Structured errors, no process spawn. ``aiohttp`` is already a
  Harbor dependency.
* **The operator CLI** — everything executor-level. Adapters, process
  lifecycle, and the tool registry are not on the HTTP transport
  (``adapter`` does not appear in the exoharness protocol at all), so these
  have no alternative.

That splits cleanly along how often each is called:

    job-scoped, once     start Exo, create the adapter, read the tool
                         registry for the report      -> CLI, unavoidable
    trial-scoped, per N  ensure_conversation, attach, detach
                         -> CLI today; HTTP is the better fit

DECIDED: CLI-only. The CLI is required either way for the job-scoped calls, so
using it for everything means one mechanism instead of two.

The methods below are still grouped by scope, because this class is the seam
where the per-trial calls would move to HTTP. Reasons that would justify the
move: structured error responses instead of parsing stdout, no subprocess spawn
on a path that runs ~3x per trial, and access to SnapshotSandbox, which has no
CLI equivalent and is what the sandbox question in docs/eval-design.md would
need.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path


class ExoCommandError(RuntimeError):
    """A non-zero exit from the exo CLI, carrying stdout/stderr for the log."""


class SandboxDriftError(RuntimeError):
    """The conversation is no longer executing in the container Harbor grades.

    Distinct from a task failure. Raised out of ExoAgent.run(), so Harbor
    records it as trial exception_info rather than a reward of zero — the
    trial is void, not lost.
    
    Note: this is not an actually error, it is valid for Exo to choose to
    create a new sandbox mid-task. This is a temporary error returned while
    the Harbor integration does not yet handle this case.
    """


@dataclass(frozen=True)
class ExoClient:
    exo_bin: Path
    exo_root: Path

    # ---- job-scoped lifecycle (called by the plugin, once) ----------------

    def ensure_agent(self, *, model: str) -> str:
        """Create the Exo agent if absent; return its id. Idempotent."""
        raise NotImplementedError

    def create_harbor_adapter(self, *, agent_id: str, socket_path: Path) -> str:
        """Create and enable the Harbor adapter; return its id.

        One adapter serves every trial conversation for the job. Exo adapters
        are conversation-scoped by default, so this is the seam where Harbor's
        needs and Exo's adapter model meet — see docs/design/harbor-integration.md.
        """
        raise NotImplementedError

    def shutdown(self) -> None:
        """Stop the Exo process. Idempotent."""
        raise NotImplementedError

    # ---- trial-scoped (called by the agent, per trial) --------------------

    def ensure_conversation(self, *, agent_id: str, name: str) -> str:
        """Select or create the conversation for a trial; return its id."""
        raise NotImplementedError

    def attach_sandbox(
        self,
        *,
        agent_id: str,
        conversation_id: str,
        container_id: str,
    ) -> str:
        """Borrow a running container as the conversation's sandbox.

            exo conversation sandbox attach <agent> <conversation> \\
                --provider docker --external-id <container-id>

        Returns the Exo sandbox id, which is distinct from the Docker id.
        Borrowing grants execute permission only: Exo must never stop or
        delete this container. Harbor owns its lifecycle.

        Snapshot and restore are a different matter and are NOT ownership
        violations — see docs/design/harbor-exo-changes.md. Snapshot is a read;
        restore produces a separate warm container and cannot touch Harbor's.
        The risk there is divergence (Exo working in a copy while Harbor grades
        the original), which verify_sandbox_unchanged below is what catches.
        """
        raise NotImplementedError

    def verify_sandbox_unchanged(
        self,
        *,
        agent_id: str,
        conversation_id: str,
        sandbox_id: str,
        container_id: str,
    ) -> None:
        """Assert the conversation still executes in the container we attached.

        Raises SandboxDriftError if not. Called at task completion, BEFORE
        run() returns, because Harbor starts the verifier the moment it does.

        The failure this exists for is live today. CreateSandbox is not
        blocked for a conversation, and event-log resolution falls back to
        "the most recent created or attached sandbox that has not been stopped
        or detached" — so Exo can create a fresh sandbox mid-task, do all its
        work there, and leave the borrowed container untouched. Harbor then
        grades an empty container and records a legitimate-looking zero. No
        error surfaces anywhere. That is the worst class of eval bug: silently
        wrong, not loud.

        TODO: BLOCKED ON EXO. This asks Exo which sandbox the conversation is
        currently on, and no such read command exists —
        ConversationSandboxCommands has Attach, Detach, and Run, but no
        status. Needs something like:

            exo conversation sandbox status <agent> <conversation>

        returning the active sandbox id and its attachment descriptor, so we
        can compare both against sandbox_id and container_id. Roughly 30 lines
        in crates/cli/src/main.rs. Until it lands this method cannot be
        implemented; the call site in agent.py is wired up and waiting.

        (A probe through `sandbox run` reading a host-written nonce would work
        without touching Exo, but it is a heuristic and would miss a
        snapshot-restore copy, which carries the nonce in its filesystem.
        Not worth building given the real command is small.)
        """
        raise NotImplementedError

    def detach_sandbox(
        self,
        *,
        agent_id: str,
        conversation_id: str,
        sandbox_id: str,
    ) -> None:
        """Release the borrowed container without stopping it.

        Purely Exo-side bookkeeping. BorrowedDockerSandboxHandle::detach()
        returns the attachment descriptor and makes no Docker call; the
        harness then marks the sandbox not-running, drops it from the live
        map, and appends SandboxDetached.

        By the time this runs the container is already stopped and (with
        Harbor's default environment.delete) removed. It is still necessary:
        sandbox resolution picks the most recent created-or-attached handle
        that has not been stopped or detached, so without this the
        conversation stays aimed at the dead container and Exo's shell fails
        on a raw Docker inspect error partway through reflection. Detaching
        pops resolution back to the persistent agent sandbox, which is where
        reflection has to run anyway to write anything durable.

        Idempotent — cleanup runs after success, timeout, and cancellation
        alike, and may run twice. The harness handles the repeat itself:
        detach_sandbox returns the stored attachment when already stopped.

        KNOWN EDGE: active_sandbox_handle serves from a live-handle cache, and
        on a miss rebuilds via the borrowed backend's attach(), which calls
        inspect_running_docker_container and fails on a stopped container. A
        miss means the Exo process restarted since the attach — so detach
        succeeds normally but errors after a mid-job Exo restart, precisely
        when the container is certain to be dead. Catch it, record it, and
        carry on; the cost is a stale running=true record, not a lost trial.
        """
        raise NotImplementedError
