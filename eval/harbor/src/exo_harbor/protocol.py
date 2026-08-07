"""Message types for the Exo Harbor adapter, and the client that sends them.

The adapter worker (examples/exo/adapters/harbor/worker.ts) listens on a local
unix socket. Sending a message is: open the socket, write one JSON line, read
one JSON line back, close. The dataclasses below are those lines; ``_request``
is the four steps. That is the whole file.

Every exchange is one request from the Harbor side and one response from Exo,
correlated by ``trial_id``. Two exchanges exist per trial:

    task_started        -> task_complete          (sent by ExoAgent.run)
    verification_result -> feedback_processed     (sent by ExoSessionPlugin)

Both are blocking: the Harbor side does not advance until Exo answers.

Not to be confused with the exoharness HTTP surface used by exo.py. That one
reads and writes exoharness state — agents, conversations, sandboxes — and
nothing on it makes Exo think. This socket is the only way to wake a
conversation and get a reply, which is why the two live in separate modules.

The dataclasses are the contract, mirrored field-for-field in
examples/exo/adapters/harbor/harbor.ts. Keeping them typed means a rename
fails here rather than becoming a payload the adapter nacks mid-trial.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal


# --------------------------------------------------------------------------
# Requests (Harbor -> Exo)
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class TaskStarted:
    """Wake Exo to work a task in an already-attached sandbox."""

    trial_id: str
    task_name: str
    instruction: str
    conversation_id: str
    sandbox_id: str
    # Which step of a multi-step task this is; None for single-step tasks.
    step_name: str | None = None
    deadline_at: str | None = None
    type: Literal["task_started"] = "task_started"

    def payload(self) -> dict[str, Any]:
        # TODO: asdict + stamp a fresh message_id for adapter-side dedup.
        raise NotImplementedError


@dataclass(frozen=True)
class VerificationException:
    type: str
    message: str
    traceback: str | None = None


@dataclass(frozen=True)
class VerificationResult:
    """Hand Exo the grade so it can reflect before the next trial."""

    trial_id: str
    task_name: str
    conversation_id: str
    rewards: dict[str, float] | None = None
    verifier_stdout: str | None = None
    verifier_stderr: str | None = None
    # Set when the trial died infrastructurally rather than scoring zero. Exo
    # must be able to tell "I failed the task" from "the harness broke", or it
    # will learn from noise.
    exception: VerificationException | None = None
    type: Literal["verification_result"] = "verification_result"

    def payload(self) -> dict[str, Any]:
        # TODO: as above.
        raise NotImplementedError


# --------------------------------------------------------------------------
# Responses (Exo -> Harbor)
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class TaskComplete:
    trial_id: str
    summary: str | None = None


@dataclass(frozen=True)
class FeedbackProcessed:
    trial_id: str
    # What Exo durably changed in response: memory written, tool installed,
    # prompt edited. This is the self-improvement audit trail.
    summary: str | None = None


class HarborAdapterError(RuntimeError):
    pass


# --------------------------------------------------------------------------
# Transport
# --------------------------------------------------------------------------


async def send_task_started(
    socket_path: Path,
    request: TaskStarted,
    *,
    timeout_sec: float,
) -> TaskComplete:
    """Block until Exo reports the task done.

    Raises HarborAdapterError on adapter-level failure; asyncio.TimeoutError
    if Exo never answers. Both must surface as Harbor agent errors rather than
    being swallowed into a zero score.
    """
    # TODO: _request(...), assert the response type is task_complete, and that
    # its trial_id matches. A response for the wrong trial is a bug, not a
    # retry.
    raise NotImplementedError


async def send_verification_result(
    socket_path: Path,
    request: VerificationResult,
    *,
    timeout_sec: float,
) -> FeedbackProcessed:
    """Block until Exo finishes reflecting on the grade."""
    # TODO: same shape as above, expecting feedback_processed.
    raise NotImplementedError


async def probe(socket_path: Path, *, timeout_sec: float) -> bool:
    """Return True once the adapter socket accepts a connection.

    Used by the plugin to wait out adapter startup before the first trial,
    so a slow Exo boot does not look like a task failure.
    """
    raise NotImplementedError


async def _request(
    socket_path: Path,
    payload: dict[str, Any],
    *,
    timeout_sec: float,
) -> dict[str, Any]:
    """One JSON line out, one JSON line back, over a unix socket.

    The socket is the only coupling to a local runtime. Swapping it for HTTP
    is what a remote Harbor environment would need; nothing above this
    function should have to change.
    """
    # TODO: asyncio.open_unix_connection, write f"{json.dumps(payload)}\n",
    # readline, parse, close. Wrap the whole thing in wait_for(timeout_sec).
    raise NotImplementedError
