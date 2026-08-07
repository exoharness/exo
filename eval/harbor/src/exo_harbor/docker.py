"""Host-side resolution of the Docker container Harbor started for a trial.

Harbor's DockerEnvironment brings the task up with Docker Compose under a
project name derived from the public ``environment.session_id``, with the task
container as the ``main`` service. We find it by label from the host.

Deliberately not done via ``BaseEnvironment.exec()``: that runs *inside* the
task container, where recovering the host-side container identity is
unreliable. This is host work and belongs in the external agent.
"""

from __future__ import annotations

from dataclasses import dataclass


class DockerResolutionError(RuntimeError):
    pass


@dataclass(frozen=True)
class DockerContainer:
    container_id: str
    project: str


def compose_project_name(session_id: str) -> str:
    """Reproduce Harbor's Compose project-name normalization.

    1. lowercase
    2. prefix "0" if the first character is not alphanumeric
    3. replace anything outside [a-z0-9_-] with "-"

    Mirrors Harbor's own logic, so it must be re-checked on a Harbor bump.
    """
    raise NotImplementedError


def resolve_main_container(session_id: str) -> DockerContainer:
    """Find the single running ``main`` container for this trial.

        docker ps --filter label=com.docker.compose.project=<project>
                  --filter label=com.docker.compose.service=main
                  --format '{{.ID}}'

    Zero matches means the environment is gone. More than one is ambiguous.
    Both must raise: attaching an arbitrary container would silently grade the
    wrong machine.
    """
    # TODO: run the query, then re-inspect the chosen id to confirm its labels
    # still match before handing it to Exo.
    raise NotImplementedError
