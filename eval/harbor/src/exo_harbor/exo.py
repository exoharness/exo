"""Client for driving Exo from the Harbor integration.

Exo is a Rust process and this is Python, so there is no in-process API. Two
out-of-process surfaces exist:

* **The operator CLI** — everything executor-level: agents, conversations,
  adapters, the adapter runner, the tool registry.
* **Exoharness HTTP** (``POST /request``, see docs/exoharness-http.md) — covers
  exoharness state only. Nothing on it makes Exo think.

DECIDED: CLI-only. The CLI is required either way for the executor-level calls,
so using it throughout means one mechanism instead of two. This class is the
seam where per-trial calls could move to HTTP if subprocess overhead or stdout
parsing ever justifies it.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from exo_harbor import conventions
from exo_harbor.protocol import probe

logger = logging.getLogger(__name__)


class ExoCommandError(RuntimeError):
    """A non-zero exit from the exo CLI, carrying stdout/stderr for the log."""


class SandboxDriftError(RuntimeError):
    """The conversation is no longer executing in the container Harbor grades.

    Distinct from a task failure. Raised out of ExoAgent.run(), so Harbor
    records it as trial exception_info rather than a reward of zero — the
    trial is void, not lost.

    Note: this is not an actual error, it is valid for Exo to choose to create
    a new sandbox mid-task. This is a temporary error returned while the Harbor
    integration does not yet handle this case.
    """


@dataclass(frozen=True)
class ExoClient:
    exo_bin: Path
    exo_root: Path
    repo_root: Path
    model: str

    # ---- job-scoped lifecycle (called by the plugin, once) ----------------

    async def ensure_agent(self) -> None:
        """Create the Exo agent if absent. Idempotent.

        Addressed everywhere by conventions.AGENT_SLUG, so nothing has to pass
        a generated id between the plugin and the per-trial agents.
        """
        if await self._exists("agent", "show", conventions.AGENT_SLUG):
            return
        await self.run(
            "agent",
            "create",
            "Harbor eval",
            "--slug",
            conventions.AGENT_SLUG,
            "--model",
            self.model,
            # Without the TypeScript harness the agent has no tools at all,
            # including send_adapter_message — the protocol could not work.
            "--module",
            str(self.repo_root / "exo/harness.ts"),
            "--sandbox-provider",
            "docker",
            # One sandbox shared across every conversation. This is the
            # persistent agent sandbox that reflection writes into, and the
            # thing a task conversation falls back to once its borrowed
            # container is detached.
            "--sandbox-scope",
            "agent",
        )

    async def ensure_harbor_adapter(self, socket_path: Path) -> None:
        """Create the single Harbor adapter for this job. Idempotent.

        There is no `exo adapters create` CLI, and AdapterCreationConfig is
        private to the executor crate, so the adapter is created through Exo
        itself in a dedicated setup conversation.

        Depending on a model call for harness setup is not ideal, but it fails
        loudly rather than silently: if the adapter does not appear, no socket
        appears, and the caller's probe fails the job before any Docker spend.
        """
        setup = conventions.SETUP_CONVERSATION_SLUG
        await self.ensure_conversation(setup)
        await self.run(
            "conversation",
            "send",
            conventions.AGENT_SLUG,
            setup,
            _ADAPTER_SETUP_PROMPT.format(socket_path=socket_path),
        )

    async def ensure_adapter_runner(self, socket_path: Path, *, timeout_sec: float) -> None:
        """Start the adapter runner and wait for the socket. Idempotent.

        The runner is the supervisor that keeps the worker alive, and it is
        what makes a mid-task `rebuild_and_restart_exo` survivable: the drain
        marker lets it finish in flight work and exit, and the reboot notice
        wakes conversations again once Exo is back.
        """
        if await probe(socket_path, timeout_sec=0.5):
            return

        # Detached: the runner must outlive any single CLI call, and spawning
        # via a thread keeps it clear of the event loop's child watcher.
        await asyncio.to_thread(self._spawn_runner)

        if not await probe(socket_path, timeout_sec=timeout_sec):
            raise ExoCommandError(
                f"Harbor adapter socket never appeared at {socket_path}. "
                "The adapter runner started but the adapter may not exist; "
                f"check {self.exo_root / 'exo-adapters.log'}."
            )

    def _spawn_runner(self) -> None:
        log_path = self.exo_root / "exo-adapters.log"
        log_path.parent.mkdir(parents=True, exist_ok=True)
        with log_path.open("ab") as log:
            subprocess.Popen(  # noqa: S603 - fixed argv, no shell
                self._argv(
                    "adapters",
                    "run",
                    "--lock-file",
                    str(self.exo_root / "exo-adapters.lock"),
                    "--drain-marker",
                    str(self.exo_root / "exo-adapters.restart"),
                    "--reboot-notice",
                    str(self.exo_root / "exo-reboot-notice.json"),
                ),
                cwd=self.repo_root,
                stdout=log,
                stderr=log,
                start_new_session=True,
            )

    async def list_tools(self) -> str:
        """Raw `exo tools list` output, for the accumulation report."""
        return await self.run("tools", "list")

    # ---- trial-scoped (called by the agent, per trial) --------------------

    async def ensure_conversation(self, slug: str) -> None:
        """Create the conversation if absent. Idempotent."""
        if await self._exists("conversation", "show", conventions.AGENT_SLUG, slug):
            return
        await self.run(
            "conversation",
            "create",
            conventions.AGENT_SLUG,
            slug,
            "--slug",
            slug,
            # Share the agent-level sandbox rather than getting a private one,
            # so durable work survives the conversation.
            "--sandbox-scope",
            "agent",
        )

    async def attach_sandbox(self, conversation: str, container_id: str) -> str:
        """Borrow a running container as the conversation's sandbox.

        Returns the Exo sandbox id, which is distinct from the Docker id.
        Borrowing grants execute permission only: Exo must never stop or delete
        this container. Harbor owns its lifecycle.

        Snapshot and restore are a different matter and are NOT ownership
        violations — see exoharness/docs/design/harbor-exo-changes.md. Snapshot is a read;
        restore produces a separate warm container and cannot touch Harbor's.
        The risk there is divergence, which verify_sandbox_unchanged catches.
        """
        output = await self.run(
            "conversation",
            "sandbox",
            "attach",
            conventions.AGENT_SLUG,
            conversation,
            "--provider",
            "docker",
            "--external-id",
            container_id,
            "--json",
        )
        return _required_string(_json_object(output, "attach"), "sandbox_id")

    async def verify_sandbox_unchanged(
        self, conversation: str, sandbox_id: str
    ) -> None:
        """Assert the conversation still executes in the container we attached.
        """
        output = await self.run(
            "conversation",
            "sandbox",
            "status",
            conventions.AGENT_SLUG,
            conversation,
            "--json",
        )
        active_sandbox_id = _json_object(output, "sandbox status").get("sandbox_id")
        if active_sandbox_id != sandbox_id:
            raise SandboxDriftError(
                f"conversation {conversation!r} should still use attached sandbox "
                f"{sandbox_id!r}, but its active attached sandbox is "
                f"{active_sandbox_id!r}"
            )

    async def detach_sandbox(self, conversation: str, sandbox_id: str) -> None:
        """Release the borrowed container without stopping it.

        Purely Exo-side bookkeeping; the handle's detach makes no Docker call.
        By the time this runs the container is already stopped and removed, but
        it is still necessary: sandbox resolution picks the most recent
        created-or-attached handle that has not been stopped or detached, so
        without this the conversation stays aimed at the dead container and
        Exo's shell fails partway through reflection.
        """
        await self.run(
            "conversation",
            "sandbox",
            "detach",
            conventions.AGENT_SLUG,
            conversation,
            sandbox_id,
            "--json",
        )

    async def read_conversation_events(
        self,
        conversation: str,
        *,
        types: list[str],
        turn_id: str | None = None,
        limit: int,
    ) -> str:
        """Return canonical Exo conversation events as JSON."""
        args = [
            "conversation",
            "events",
            conventions.AGENT_SLUG,
            conversation,
        ]
        for event_type in types:
            args.extend(("--type", event_type))
        if turn_id is not None:
            args.extend(("--turn-id", turn_id))
        args.extend(("--limit", str(limit)))
        return await self.run(*args)

    # ---- plumbing ---------------------------------------------------------

    def _argv(self, *args: str) -> list[str]:
        """Base argv for every exo call.

        --harness is global and must precede the subcommand. It has to be on
        EVERY invocation, not just `agent create`: the adapter runner is the
        process that executes a woken conversation's turn, and without this it
        builds a harness with only the built-in tools. The symptom is subtle —
        the turn runs fine with `shell` but send_adapter_message is absent, so
        the agent can never answer and the trial dies on the agent timeout.
        """
        return [str(self.exo_bin), "--root", str(self.exo_root), "--harness", "exo", *args]

    async def run(self, *args: str, timeout_sec: float | None = None) -> str:
        """Run one exo CLI command and return stdout."""
        process = await asyncio.create_subprocess_exec(
            *self._argv(*args),
            cwd=self.repo_root,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env={**os.environ, "EXO_PROFILE": os.environ.get("EXO_PROFILE", "practical")},
        )
        try:
            stdout, stderr = await asyncio.wait_for(
                process.communicate(), timeout=timeout_sec
            )
        except asyncio.TimeoutError:
            process.kill()
            raise

        if process.returncode != 0:
            raise ExoCommandError(
                f"exo {' '.join(args)} failed ({process.returncode}): "
                f"{stderr.decode().strip()}"
            )
        return stdout.decode().strip()

    async def _exists(self, *args: str) -> bool:
        """Whether a `show` command resolves.

        Exit code rather than grepping `list` output: a slug can appear as a
        substring of an unrelated line, and the table format is not a contract.
        """
        process = await asyncio.create_subprocess_exec(
            *self._argv(*args),
            cwd=self.repo_root,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await process.communicate()
        if process.returncode == 0:
            return True
        if "not found" in stderr.decode().lower():
            return False
        raise ExoCommandError(
            f"exo {' '.join(args)} failed ({process.returncode}): "
            f"{stderr.decode().strip()}"
        )


def _json_object(output: str, what: str) -> dict[str, Any]:
    """Parse a --json CLI result.

    Structured output rather than scraping prose: the human form of `sandbox
    attach` is "attached Docker container as sandbox X for Y", where the last
    whitespace-separated token is the conversation slug, not the sandbox id.
    """
    try:
        value = json.loads(output)
    except json.JSONDecodeError as error:
        raise ExoCommandError(f"{what} output was not JSON: {output!r}") from error
    if not isinstance(value, dict):
        raise ExoCommandError(f"{what} output was not a JSON object: {output!r}")
    return value


def _required_string(payload: dict[str, Any], key: str) -> str:
    value = payload.get(key)
    if not isinstance(value, str) or not value:
        raise ExoCommandError(f"missing {key} in {payload!r}")
    return value


# Deliberately prescriptive: this is harness setup, not a task, so there is no
# value in leaving Exo room to interpret it. The "report instead of changing"
# clause matters — silently recreating a differently-configured adapter would
# point the job at the wrong socket.
_ADAPTER_SETUP_PROMPT = (
    "Configure the Harbor adapter for this conversation. Call list_adapters "
    "with includeDisabled=true. If there is no enabled adapter named `harbor` "
    "with type `harbor` and socketPath exactly `{socket_path}`, call "
    "create_adapter with name `harbor`, source `library`, and config "
    '{{"type":"harbor","socketPath":"{socket_path}"}}. Do not create a '
    "duplicate. If an adapter named `harbor` exists but is disabled or has "
    "different configuration, report that clearly instead of changing or "
    "deleting it."
)
