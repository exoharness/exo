"""One git repository per run that is both what Exo edits and its lineage.

Exo's policy has three parts: its source tree, agent-built tools under
`.exo/` in that tree, and memory and skills stored as versioned artifacts in
the run's Exo root. Each run gets a clone of the committed tree under its run
directory. Exo works in that clone through the /workspace/exo mount, and the
runner commits there after every incident: source changes, the tool
directories, and a copy of the latest memory and skill artifacts under
`.exo/agent`. `git log -p` in the run's source reads as what each reflection
changed, and the working tree is the state the next incident starts from.
"""

from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

from pydantic import BaseModel

# Agent-built tools live in the repository, not under EXO_ROOT. They are
# gitignored there, so the runner force-adds them.
TOOL_DIRECTORIES = (".exo/agent-tools", ".exo/tools", ".exo/tool-sources")
# Where the runner copies the newest memory and skill artifacts before a commit.
AGENT_STATE_DIRECTORY = ".exo/agent"
RECORDED_DIRECTORIES = (*TOOL_DIRECTORIES, AGENT_STATE_DIRECTORY)
# Build outputs that make the clone heavy; a resume rebuilds them.
BUILD_OUTPUTS = ("target", "node_modules")
POLICY_BRANCH = "policy"
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


def head(repo: Path) -> str:
    return git(repo, "rev-parse", "HEAD").strip()


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
        for relative in (".exo", *changed)
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


def create_policy_repo(repo: Path, path: Path, commit: str) -> None:
    """Clone `commit` of `repo` into `path` on its own branch, with no remote.

    A clone rather than a worktree, so the run's history lives inside the run
    directory and survives whatever happens to the checkout it came from.
    """
    subprocess.run(
        ["git", "clone", "-q", "--no-checkout", str(repo), str(path)],
        check=True,
    )
    git(path, "checkout", "-q", "-B", POLICY_BRANCH, commit)
    git(path, "remote", "remove", "origin")
    # The agent container covers /workspace/exo/.local with an anonymous
    # volume; Docker cannot create that mountpoint inside a read-only bind
    # mount, so the directory must already exist (it is gitignored).
    (path / ".local").mkdir()


def record_policy(source: Path, *, exo_root: Path, message: str) -> None:
    """Commit the current policy: source, tools, and the latest agent artifacts."""
    state = source / AGENT_STATE_DIRECTORY
    shutil.rmtree(state, ignore_errors=True)
    for artifact_path, content in latest_artifacts(exo_root).items():
        destination = state / artifact_path
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(content, destination)
    git(source, "add", "-A")
    recorded = [relative for relative in RECORDED_DIRECTORIES if (source / relative).is_dir()]
    if recorded:
        git(source, "add", "-f", "--", *recorded)
    # An incident that changed nothing still gets a commit, so the log has
    # one entry per incident.
    git(source, "commit", "-q", "--allow-empty", "-m", message)


def trial_count(source: Path) -> int:
    """Trials committed so far, so a resumed run keeps numbering."""
    log = git(source, "log", "--format=%s")
    return sum(line.startswith("trial ") for line in log.splitlines())


def source_changed(source: Path, *, since: str) -> bool:
    """Whether Exo changed anything since the commit `since`.

    The runner's own artifact copies under `.exo/agent` do not count; a
    memory entry is not a source change.
    """
    status = git(source, "status", "--porcelain", "--", ".", f":(exclude){AGENT_STATE_DIRECTORY}")
    return bool(status.strip()) or head(source) != since


def restore_source(source: Path, commit: str) -> None:
    """Put the working tree back to `commit` without rewriting history.

    The index and tree return to `commit` while HEAD stays where it is, so
    the next commit records the restoration as its own step in the lineage.
    """
    current = head(source)
    git(source, "reset", "-q", "--hard", commit)
    git(source, "reset", "-q", "--soft", current)
    git(source, "clean", "-fdq")
    # New, still-ignored files in the tool directories are part of the change.
    git(source, "clean", "-fdqx", "--", *TOOL_DIRECTORIES)


def prune_build_outputs(source: Path) -> None:
    """Drop the build outputs a completed run no longer needs on disk."""
    for relative in BUILD_OUTPUTS:
        shutil.rmtree(source / relative, ignore_errors=True)
