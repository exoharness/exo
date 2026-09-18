"""ExoAgent — per-task driver, killed after completion of task and reinstantiated.

See https://www.harborframework.com/docs/agents#external-agents for more details
on the lifecycle."""

from __future__ import annotations

import logging
import re
import subprocess
from pathlib import Path
from typing import Any, override

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from exo_harbor import conventions
from exo_harbor.exo import PI_HARNESS, ExoClient
from exo_harbor.trajectory import export_trial_trajectory

logger = logging.getLogger(__name__)


class ExoAgent(BaseAgent):
    SUPPORTS_RESUME = False
    SUPPORTS_ATIF = True
    SUPPORTS_WINDOWS = False

    def __init__(
        self,
        *args: Any,
        exo_root: str | Path,
        exo_bin: str | Path,
        exo_repo_root: str | Path,
        exo_model: str,
        harness: str = "exo",
        task_timeout_sec: float | str | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self._model = exo_model
        self._harness = harness
        self._task_timeout_sec = (
            float(task_timeout_sec) if task_timeout_sec is not None else None
        )
        self._client = ExoClient(
            exo_bin=Path(exo_bin),
            exo_root=Path(exo_root),
            repo_root=Path(exo_repo_root),
            harness=harness,
        )
        self._container_id: str | None = None
        self._sandbox_id: str | None = None

    @property
    def _conversation(self) -> str:
        """Convo slug computed on demand rather than in __init__ because
        Harbor assigns context_id *after* constructing the agent."""
        assert self.context_id is not None, "Harbor has not assigned context_id yet"
        return conventions.trial_conversation_slug(str(self.context_id))

    @staticmethod
    @override
    def name() -> str:
        return "exo"

    @override
    def version(self) -> str | None:
        # TODO: eventually, should have a version included from the exo binary.
        return None

    @override
    async def setup(self, environment: BaseEnvironment) -> None:
        self._container_id = get_harbor_docker_container_id(environment.session_id)
        if self._harness == PI_HARNESS:
            # Exo's pi harness runs the `pi` binary inside the sandbox, and
            # Harbor's task images do not ship it.
            await install_pi(environment)

        # setup dedicated conversation for the trial
        await self._client.ensure_conversation(self._conversation)
        # The executor runs every turn of this conversation in the attached
        # container from here on. Harbor owns the container and removes it
        # after grading; Exo only borrows it.
        self._sandbox_id = await self._client.attach_container(
            self._conversation, self._container_id
        )

        logger.info(
            "trial %s attached Harbor container %s as sandbox %s in conversation %s",
            self.context_id,
            self._container_id[:12],
            self._sandbox_id,
            self._conversation,
        )

    @override
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        """Hand Exo the task and export its trajectory for Harbor."""
        assert self._sandbox_id is not None, "ExoAgent.run called before setup"

        try:
            await self._client.send(
                self._conversation, instruction, timeout_sec=self._task_timeout_sec
            )
        finally:
            context.metadata = {
                **(context.metadata or {}),
                conventions.CONVERSATION_METADATA_KEY: self._conversation,
            }

            # A timed-out trial still has a partial trajectory worth keeping.
            try:
                await export_trial_trajectory(
                    self._client,
                    self._conversation,
                    str(self.context_id),
                    instruction,
                    self._model,
                    self.logs_dir / "trajectory.json",
                )
            except Exception:
                logger.exception(
                    "trial %s failed to export its trajectory", self.context_id
                )


# TODO: it's a kludge that we have to replicate the setup behavior across here
# and the docker container initialization for regular Exoharness Pi executor
# (referring to exoharness/containers/pi-sandbox/Dockerfile).
# We ought to fix this – would require refacotriing the *setup* logic out
# into its own script that can be invoked from both places.

# Matches the node major exoharness/containers/pi-sandbox builds on; pi's bundle
# needs node 22.
NODE_VERSION = "v22.15.0"
PI_PACKAGE = "@earendil-works/pi-coding-agent"
PI_INSTALL_TIMEOUT_SEC = 600

# Installed under /usr/local rather than through nvm because exo reaches the
# container with a plain `docker exec`, which has no login shell to load nvm.
PI_INSTALL_SCRIPT = f"""set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl xz-utils
case "$(uname -m)" in
  x86_64) arch=x64 ;;
  aarch64) arch=arm64 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
curl -fsSL "https://nodejs.org/dist/{NODE_VERSION}/node-{NODE_VERSION}-linux-$arch.tar.xz" \\
  | tar -xJ -C /usr/local --strip-components=1
npm install -g --ignore-scripts "{PI_PACKAGE}"
pi --version
"""

async def install_pi(environment: BaseEnvironment) -> None:
    """Install node and the Pi coding agent into Harbor's task container."""
    result = await environment.exec(
        command=PI_INSTALL_SCRIPT,
        user="root",
        timeout_sec=PI_INSTALL_TIMEOUT_SEC,
    )
    if result.return_code != 0:
        raise RuntimeError(
            f"installing pi into the task container failed ({result.return_code}): "
            f"{(result.stderr or result.stdout or '').strip()[-2000:]}"
        )
    logger.info("installed %s", (result.stdout or "").strip().splitlines()[-1:])


def get_harbor_docker_container_id(session_id: str) -> str:
    """Resolve Harbor's main container from its normalized Compose project name."""
    MAIN_SERVICE = "main"
    PROJECT_LABEL = "com.docker.compose.project"
    SERVICE_LABEL = "com.docker.compose.service"

    # Harbor's Compose project name normalization
    name = session_id.lower()
    if not name or not name[0].isalnum():
        name = f"0{name}"
    project = re.sub(r"[^a-z0-9_-]", "-", name)

    ids = _docker(
        "ps",
        "--filter",
        f"label={PROJECT_LABEL}={project}",
        "--filter",
        f"label={SERVICE_LABEL}={MAIN_SERVICE}",
        "--format",
        "{{.ID}}",
    ).split()

    if not ids:
        raise RuntimeError(
            f"no running {MAIN_SERVICE} container for Compose project {project!r}"
        )
    if len(ids) > 1:
        raise RuntimeError(
            f"{len(ids)} running {MAIN_SERVICE} containers for Compose project "
            f"{project!r}; refusing to guess"
        )
    return ids[0]


def _docker(*args: str) -> str:
    result = subprocess.run(
        ["docker", *args], capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise RuntimeError(f"docker {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout.strip()
