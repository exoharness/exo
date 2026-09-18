"""Exo CLI typed wrapper."""

from __future__ import annotations

import asyncio
import os
import re
from dataclasses import dataclass
from pathlib import Path

from exo_harbor import conventions


EXO_HARNESS = "exo"
BASIC_HARNESS = "basic"
PI_HARNESS = "pi"

# `exo conversation sandbox attach` reports the new sandbox in prose:
# "attached Docker container as sandbox <id> for <conversation>".
ATTACHED_SANDBOX_ID = re.compile(r"as sandbox (\S+) for ")


class ExoCommandError(RuntimeError):
    """An Exo CLI command failed."""


@dataclass(frozen=True)
class ExoClient:
    exo_bin: Path
    exo_root: Path
    repo_root: Path
    harness: str = EXO_HARNESS

    async def ensure_agent(self, model: str) -> None:
        if await self._exists("agent", "show", conventions.AGENT_SLUG):
            return
        arguments = [
            "agent",
            "create",
            "Harbor eval",
            "--slug",
            conventions.AGENT_SLUG,
            "--model",
            model,
            "--provider",
            "docker",
            "--sandbox-scope",
            "agent",
        ]
        if self.harness == EXO_HARNESS:
            # The module is where memory, skills and self-editing live; the
            # basic harness has none of it and only gets a shell.
            arguments.extend(
                (
                    "--module",
                    str(self.repo_root / conventions.HARNESS_MODULE),
                    "--tool-creation",
                    "enabled",
                )
            )
        await self._run(*arguments)

    async def ensure_conversation(self, slug: str) -> None:
        if await self._exists(
            "conversation", "show", conventions.AGENT_SLUG, slug
        ):
            return
        await self._run(
            "conversation",
            "create",
            conventions.AGENT_SLUG,
            slug,
            "--slug",
            slug,
            "--sandbox-scope",
            "conversation",
        )

    async def attach_container(
        self,
        conversation: str,
        container_id: str,
        *,
        default_workdir: str | None = None,
    ) -> str:
        """Attach Harbor's task container and return the Exo sandbox id.

        An attached sandbox is where the conversation's turns run from then
        on; the executor prefers it over anything it would otherwise build
        from the conversation's configuration.
        """
        arguments = [
            "conversation",
            "sandbox",
            "attach",
            conventions.AGENT_SLUG,
            conversation,
            "--provider",
            "docker",
            "--external-id",
            container_id,
        ]
        if default_workdir is not None:
            arguments.extend(("--default-workdir", default_workdir))
        output = await self._run(*arguments)
        match = ATTACHED_SANDBOX_ID.search(output)
        if match is None:
            raise ExoCommandError(
                f"could not find the sandbox id in attach output: {output!r}"
            )
        return match.group(1)

    async def send(
        self, conversation: str, prompt: str, *, timeout_sec: float | None
    ) -> str:
        """Run one Exo turn to completion and return its printed messages.

        Blocks for as long as the turn takes. On timeout the subprocess is
        killed, which aborts the turn, but convo state remains.
        """
        return await self._run(
            "conversation",
            "send",
            conventions.AGENT_SLUG,
            conversation,
            prompt,
            timeout_sec=timeout_sec,
        )

    async def read_conversation_events(
        self,
        conversation: str,
        *,
        types: list[str],
        turn_id: str | None = None,
        limit: int,
    ) -> str:
        """Return canonical conversation events as JSON."""
        arguments = [
            "conversation",
            "events",
            conventions.AGENT_SLUG,
            conversation,
        ]
        for event_type in types:
            arguments.extend(("--type", event_type))
        if turn_id is not None:
            arguments.extend(("--turn-id", turn_id))
        arguments.extend(("--limit", str(limit)))
        return await self._run(*arguments)

    async def _run(self, *args: str, timeout_sec: float | None = None) -> str:
        process = await asyncio.create_subprocess_exec(
            *self._argv(*args),
            cwd=self.repo_root,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self._environment(),
        )
        try:
            stdout, stderr = await asyncio.wait_for(
                process.communicate(), timeout=timeout_sec
            )
        except (asyncio.TimeoutError, asyncio.CancelledError):
            # Kill rather than terminate: the turn holds a sandbox and we want
            # the process gone before the caller moves on.
            process.kill()
            await process.wait()
            raise
        if process.returncode != 0:
            raise ExoCommandError(
                f"exo {' '.join(args)} failed ({process.returncode}): "
                f"{stderr.decode().strip()}"
            )
        return stdout.decode().strip()

    async def _exists(self, *args: str) -> bool:
        process = await asyncio.create_subprocess_exec(
            *self._argv(*args),
            cwd=self.repo_root,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=self._environment(),
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

    def _environment(self) -> dict[str, str]:
        return {
            **os.environ,
            "EXO_PROFILE": os.environ.get("EXO_PROFILE", "practical"),
            "EXO_ROOT": str(self.exo_root),
        }

    def _argv(self, *args: str) -> list[str]:
        return [
            str(self.exo_bin),
            "--root",
            str(self.exo_root),
            "--harness",
            self.harness,
            *args,
        ]
