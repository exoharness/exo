"""A per-run git repository holding Exo's policy with its lineage.

Exo's policy is spread over three places: its source tree (this repository,
which Exo edits through the /workspace/exo mount), agent-built tools under
`.exo/` in that tree, and memory and skills stored as versioned artifacts in
the run's Exo root. Each run gets `policy/`, a git repository whose first
commit is the policy as the run started and which gains one commit per
graded incident, so `git log -p` reads as what each reflection changed.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

from pydantic import BaseModel

# Agent-built tools live in the repository, not under EXO_ROOT.
TOOL_DIRECTORIES = (".exo/agent-tools", ".exo/tools", ".exo/tool-sources")
# A tiny image for reclaiming files the agent container wrote as root.
OWNERSHIP_IMAGE = "alpine:3.20"
GIT_IDENTITY = ("-c", "user.name=exo", "-c", "user.email=exo@localhost")


class ArtifactVersion(BaseModel):
    """The metadata Exo writes beside each artifact version."""

    artifact_id: str
    path: str
    version: int


def git(repo: Path, *arguments: str) -> str:
    return subprocess.run(
        ["git", *GIT_IDENTITY, "-C", str(repo), *arguments],
        check=True,
        capture_output=True,
        text=True,
    ).stdout


def source_files(repo: Path) -> list[Path]:
    """Tracked and untracked files that git does not ignore, skipping deletions."""
    listing = git(repo, "ls-files", "-z", "--cached", "--others", "--exclude-standard")
    return [
        Path(relative)
        for relative in filter(None, listing.split("\0"))
        if (repo / relative).is_file()
    ]


def latest_artifacts(exo_root: Path) -> dict[str, Path]:
    """Map each agent-level artifact path to the file holding its newest version."""
    latest: dict[str, tuple[int, Path]] = {}
    for metadata in exo_root.glob("exoharness/agents/*/artifacts/*/*.json"):
        version = ArtifactVersion.model_validate_json(metadata.read_text())
        content = metadata.with_suffix(".bin")
        if content.is_file() and version.version > latest.get(version.path, (0, content))[0]:
            latest[version.path] = (version.version, content)
    return {path: content for path, (_, content) in latest.items()}


def reclaim_ownership(repo: Path) -> None:
    """Make files the agent container wrote as root owned by the current user."""
    changed = [
        line[3:]
        for line in git(repo, "status", "--porcelain").splitlines()
        if not line.startswith("D")
    ]
    targets = [
        f"/repo/{relative}"
        for relative in (*TOOL_DIRECTORIES, *changed)
        if (repo / relative).exists()
    ]
    if not targets:
        return
    subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "-v",
            f"{repo}:/repo",
            OWNERSHIP_IMAGE,
            "chown",
            "-R",
            f"{os.getuid()}:{os.getgid()}",
            *targets,
        ],
        check=True,
        stdout=subprocess.DEVNULL,
    )


def clear_tools(repo: Path) -> None:
    """Start a run with no inherited agent-built tools."""
    for relative in TOOL_DIRECTORIES:
        shutil.rmtree(repo / relative, ignore_errors=True)


class PolicyRepo:
    def __init__(self, path: Path, *, repo: Path, exo_root: Path) -> None:
        self.path = path
        self.repo = repo
        self.exo_root = exo_root

    def init(self) -> None:
        self.path.mkdir(parents=True)
        git(self.path, "init", "-q")
        self.commit("initial policy")

    def commit(self, message: str) -> None:
        self.refresh()
        git(self.path, "add", "-A")
        # An incident that changed nothing still gets a commit, so the log has
        # one entry per incident.
        git(self.path, "commit", "-q", "--allow-empty", "-m", message)

    def refresh(self) -> None:
        for name in ("source", "tools", "agent"):
            shutil.rmtree(self.path / name, ignore_errors=True)
        for relative in source_files(self.repo):
            copy_file(self.repo / relative, self.path / "source" / relative)
        for relative in TOOL_DIRECTORIES:
            source = self.repo / relative
            if source.is_dir():
                shutil.copytree(source, self.path / "tools" / Path(relative).name)
        for artifact_path, content in latest_artifacts(self.exo_root).items():
            copy_file(content, self.path / "agent" / artifact_path)


def copy_file(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)
