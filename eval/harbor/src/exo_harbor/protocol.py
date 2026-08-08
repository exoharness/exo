"""Message types for the Exo Harbor adapter, and the client that sends them.

The adapter worker (exo/adapters/harbor/worker.ts) listens on a local
unix socket. Sending a message is: open the socket, write one JSON line, read
one JSON line back, close. The dataclasses below are those lines; ``_request``
is the four steps. That is the whole file.

Every exchange is one request from the Harbor side and one response from Exo,
correlated by ``trial_id``. Two exchanges exist per trial:

    task_started        -> task_complete          (sent by ExoAgent.run)
    verification_result -> feedback_processed     (sent by ExoSessionPlugin)

Both are blocking: the Harbor side does not advance until Exo answers. The wait
can be long. Exo may rebuild and restart itself mid-task, which ends the turn
without answering; the guardian reboot notice then wakes the conversation again
and work continues. Only the explicit reply means "finished", which is why this
cannot be collapsed into a plain synchronous `exo conversation send`.

Not to be confused with the exoharness HTTP surface used by exo.py. That one
reads and writes exoharness state — agents, conversations, sandboxes — and
nothing on it makes Exo think. This socket is the only way to wake a
conversation and get a reply, which is why the two live in separate modules.

The dataclasses are the contract, mirrored field-for-field in
exo/adapters/harbor/harbor.ts.
"""

from __future__ import annotations

import asyncio
import json
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Literal
from uuid import uuid4


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
        return _with_message_id(self)


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
        return _with_message_id(self)


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
    """The adapter rejected a message, or replied with something unusable."""


# --------------------------------------------------------------------------
# Transport
# --------------------------------------------------------------------------


async def send_task_started(
    socket_path: Path,
    request: TaskStarted,
    *,
    timeout_sec: float,
) -> TaskComplete:
    """Block until Exo reports the task ready to be graded."""
    event = await _request(socket_path, request.payload(), timeout_sec=timeout_sec)
    _expect(event, "task_complete", request.trial_id)
    return TaskComplete(trial_id=request.trial_id, summary=event.get("summary"))


async def send_verification_result(
    socket_path: Path,
    request: VerificationResult,
    *,
    timeout_sec: float,
) -> FeedbackProcessed:
    """Block until Exo finishes reflecting on the grade."""
    event = await _request(socket_path, request.payload(), timeout_sec=timeout_sec)
    _expect(event, "feedback_processed", request.trial_id)
    return FeedbackProcessed(
        trial_id=request.trial_id, summary=event.get("summary")
    )


async def probe(socket_path: Path, *, timeout_sec: float) -> bool:
    """Return True once the adapter socket accepts a connection.

    Polls rather than connecting once: the caller uses this to wait out Exo's
    startup, and to check that a run was launched with --plugin at all.
    """
    deadline = asyncio.get_running_loop().time() + timeout_sec
    while True:
        try:
            _, writer = await asyncio.open_unix_connection(str(socket_path))
            writer.close()
            return True
        except (OSError, asyncio.TimeoutError):
            if asyncio.get_running_loop().time() >= deadline:
                return False
            await asyncio.sleep(0.5)


async def _request(
    socket_path: Path,
    payload: dict[str, Any],
    *,
    timeout_sec: float,
) -> dict[str, Any]:
    """One JSON line out, one JSON line back, over a unix socket.

    The socket is the only coupling to a local runtime. Swapping it for HTTP is
    what a remote Harbor environment would need; nothing above this function
    should have to change.
    """

    async def exchange() -> dict[str, Any]:
        reader, writer = await asyncio.open_unix_connection(str(socket_path))
        try:
            writer.write(f"{json.dumps(payload)}\n".encode())
            await writer.drain()
            line = await reader.readline()
        finally:
            writer.close()
            await writer.wait_closed()

        if not line:
            raise HarborAdapterError("Harbor adapter closed without replying")
        message = json.loads(line)
        if message.get("type") == "error":
            raise HarborAdapterError(str(message.get("message")))
        if message.get("type") != "response":
            raise HarborAdapterError(f"unexpected adapter reply: {message!r}")
        return message["event"]

    return await asyncio.wait_for(exchange(), timeout=timeout_sec)


def _expect(event: dict[str, Any], expected_type: str, trial_id: str) -> None:
    if event.get("type") != expected_type:
        raise HarborAdapterError(
            f"expected {expected_type}, got {event.get('type')!r}"
        )
    if event.get("trial_id") != trial_id:
        # The worker checks this too. Checking on both sides is cheap, and a
        # reply landing on the wrong trial would corrupt a result silently.
        raise HarborAdapterError(
            f"{expected_type} is for trial {event.get('trial_id')!r}, "
            f"not {trial_id!r}"
        )


def _with_message_id(request: Any) -> dict[str, Any]:
    payload = asdict(request)
    # Stamped per send so the adapter can dedupe a redelivered request.
    payload["message_id"] = str(uuid4())
    return payload
