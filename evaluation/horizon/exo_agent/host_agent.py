"""Host-side exo agent for Harbor benchmarks whose sandbox has no inbound
network and runs no agent code (e.g. Horizon).

Unlike `ExoAgent` (installed: exo runs *inside* the sandbox), `ExoHostAgent` runs
exo on the **host** — so its model calls work over the host's network — and routes
exo's shell `exec` *into* the sandbox via exo's `proxy` sandbox provider:

    exo (host)  --HTTP /exec-->  this agent's bridge  --environment.exec()-->  sandbox

The bridge is a localhost aiohttp server backed by Harbor's `environment.exec`.
exo is pointed at it with `--sandbox-provider proxy` + `EXO_PROXY_EXEC_URL`.
"""

from __future__ import annotations

import asyncio
import json
import os
import shlex
import tempfile
from typing import Any

from aiohttp import web

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


class ExoHostAgent(BaseAgent):
    SUPPORTS_ATIF: bool = False

    def __init__(self, *args: Any, exo_bin: str | None = None, exo_repo: str | None = None,
                 extra_env: dict[str, str] | None = None, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        # Harbor passes --ae values here (not os.environ for host agents).
        self._extra_env: dict[str, str] = extra_env or {}
        # The exo repo (for tsx harness resolution) is one level up from here.
        here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        self._repo = exo_repo or os.environ.get("EXO_REPO") or os.path.dirname(os.path.dirname(here))
        self._bin = (
            exo_bin
            or os.environ.get("EXO_BIN")
            or os.path.join(self._repo, "target", "release", "exo")
        )
        self._harness = os.path.join(
            self._repo, "examples", "simple-coding-agent", "harness.ts"
        )

    @staticmethod
    def name() -> str:
        return "exo-host"

    def version(self) -> str | None:
        return "dev"

    def _get_env(self, key: str) -> str | None:
        return self._extra_env.get(key) or os.environ.get(key)

    @property
    def _model(self) -> str:
        return self._parsed_model_name or (self.model_name or "gpt-5.5")

    async def setup(self, environment: BaseEnvironment) -> None:
        # Nothing to install in the sandbox: exo runs on the host and proxies its
        # shell into the sandbox. The host exo binary is built ahead of the run.
        return None

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        key = self._get_env("OPENAI_API_KEY") or ""

        # Discover the sandbox's working directory; exo's synthetic default
        # workspace path doesn't exist in arbitrary task images, so we remap to this.
        pwd = await environment.exec("pwd")
        taskdir = (pwd.stdout or "/").strip() or "/"

        # Bridge: exo's proxy exec -> environment.exec() in the sandbox.
        async def handle_exec(request: web.Request) -> web.Response:
            body = await request.json()
            req_cwd = body.get("cwd") or ""
            cwd = req_cwd if (req_cwd and not req_cwd.startswith("/home/exo")) else taskdir
            env = body.get("env") or None
            timeout = body.get("timeout_secs")
            try:
                res = await environment.exec(
                    body["command"], cwd=cwd, env=env, timeout_sec=timeout
                )
                return web.json_response(
                    {
                        "exit_code": res.return_code,
                        "stdout": res.stdout or "",
                        "stderr": res.stderr or "",
                    }
                )
            except Exception as e:  # noqa: BLE001 — surface as a failed command, don't crash exo
                return web.json_response(
                    {"exit_code": 1, "stdout": "", "stderr": f"bridge error: {e}"}
                )

        app = web.Application()
        app.router.add_post("/exec", handle_exec)
        runner = web.AppRunner(app)
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]  # ephemeral port
        exec_url = f"http://127.0.0.1:{port}/exec"

        root = tempfile.mkdtemp(prefix="exohost-")
        env = {
            **os.environ,
            "OPENAI_API_KEY": key,
            "EXO_PROXY_EXEC_URL": exec_url,
        }
        exo = [self._bin, "--root", root, "--secret-backend", "file"]
        model = self._model

        try:
            await self._exo(exo + ["secret", "set", "openai", "--env", "OPENAI_API_KEY"], env)
            await self._exo(exo + ["model", "register", model, "--secret", "openai"], env)
            await self._exo(
                exo
                + [
                    "agent", "create", "--slug", "t", "--model", model,
                    "--harness", self._harness,
                    "--sandbox-provider", "proxy", "ExoHost",
                ],
                env,
            )
            await self._exo(exo + ["conversation", "create", "t", "c"], env)
            out = await self._exo(
                exo + ["conversation", "send", "t", "c", instruction], env, check=False
            )
            context.metadata = {"exo_transcript_tail": out[-4000:]}
        finally:
            await runner.cleanup()

    async def _exo(self, argv: list[str], env: dict[str, str], check: bool = True) -> str:
        """Run an exo CLI step on the host from the repo root (so tsx resolves)."""
        proc = await asyncio.create_subprocess_exec(
            *argv,
            cwd=self._repo,
            env=env,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        stdout, _ = await proc.communicate()
        text = (stdout or b"").decode("utf-8", "replace")
        self.logger.info("exo %s -> rc=%s\n%s", argv[4] if len(argv) > 4 else "", proc.returncode, text[-2000:])
        if check and proc.returncode != 0:
            raise RuntimeError(f"exo step failed (rc={proc.returncode}): {' '.join(shlex.quote(a) for a in argv)}\n{text[-2000:]}")
        return text
