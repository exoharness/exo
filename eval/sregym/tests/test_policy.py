from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import policy  # noqa: E402


def make_repo(root: Path) -> Path:
    repo = root / "repo"
    repo.mkdir()
    git = ["git", "-c", "user.name=t", "-c", "user.email=t@t", "-C", str(repo)]
    (repo / "harness.ts").write_text("export const policy = 1;\n")
    (repo / ".gitignore").write_text(".exo/\n")
    subprocess.run([*git, "init", "-q"], check=True)
    subprocess.run([*git, "add", "."], check=True)
    subprocess.run([*git, "commit", "-q", "-m", "base"], check=True)
    return repo


def write_artifact(exo_root: Path, artifact_id: str, path: str, version: int, content: str) -> None:
    directory = exo_root / "exoharness/agents/agent-1/artifacts" / artifact_id
    directory.mkdir(parents=True, exist_ok=True)
    (directory / f"{version}.json").write_text(
        json.dumps({"artifact_id": artifact_id, "path": path, "version": version})
    )
    (directory / f"{version}.bin").write_text(content)


class PolicyRepoTests(unittest.TestCase):
    def test_lineage_records_source_tools_and_agent_state(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            repo = make_repo(root)
            exo_root = root / "exo"
            write_artifact(exo_root, "mem", "memory/exo-memory.json", 1, "[]")
            lineage = policy.PolicyRepo(root / "policy", repo=repo, exo_root=exo_root)

            lineage.init()

            self.assertEqual((root / "policy/source/harness.ts").read_text(), "export const policy = 1;\n")
            self.assertEqual((root / "policy/agent/memory/exo-memory.json").read_text(), "[]")
            self.assertFalse((root / "policy/tools").exists())

            (repo / "harness.ts").write_text("export const policy = 2;\n")
            (repo / "new-tool.ts").write_text("export const tool = true;\n")
            (repo / ".exo/agent-tools").mkdir(parents=True)
            (repo / ".exo/agent-tools/k8s-scan.ts").write_text("scan\n")
            write_artifact(exo_root, "mem", "memory/exo-memory.json", 2, '["lesson"]')
            write_artifact(exo_root, "skill", "skills/triage.json", 1, "{}")

            lineage.commit("trial 1: shop (anon): diagnosis PASS, mitigation fail")
            lineage.commit("trial 2: nothing changed")
            self.assertEqual(lineage.trial_count(), 2)

            log = policy.git(root / "policy", "log", "--format=%s")
            self.assertEqual(
                log.splitlines(),
                ["trial 2: nothing changed", "trial 1: shop (anon): diagnosis PASS, mitigation fail", "initial policy"],
            )
            changed = policy.git(root / "policy", "diff", "--name-only", "HEAD~2", "HEAD~1").splitlines()
            self.assertEqual(
                sorted(changed),
                [
                    "agent/memory/exo-memory.json",
                    "agent/skills/triage.json",
                    "source/harness.ts",
                    "source/new-tool.ts",
                    "tools/agent-tools/k8s-scan.ts",
                ],
            )
            self.assertEqual((root / "policy/agent/memory/exo-memory.json").read_text(), '["lesson"]')

    def test_clear_tools_removes_inherited_tool_directories(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            repo = Path(temporary_directory)
            for relative in policy.TOOL_DIRECTORIES:
                (repo / relative).mkdir(parents=True)
                (repo / relative / "x").write_text("x")

            policy.clear_tools(repo)

            self.assertTrue((repo / ".exo").exists())
            self.assertFalse(any((repo / relative).exists() for relative in policy.TOOL_DIRECTORIES))


if __name__ == "__main__":
    unittest.main()
