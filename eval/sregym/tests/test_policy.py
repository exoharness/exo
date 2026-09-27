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
    (repo / ".gitignore").write_text(".exo/\n.local/\ntarget/\nnode_modules/\n")
    subprocess.run([*git, "init", "-q", "-b", "main"], check=True)
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
    def test_clone_is_self_contained_on_its_own_branch(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            repo = make_repo(root)
            source = root / "run" / "exo-source"
            source.parent.mkdir()

            policy.create_policy_repo(repo, source, policy.head(repo))

            self.assertEqual((source / "harness.ts").read_text(), "export const policy = 1;\n")
            self.assertEqual(policy.git(source, "branch", "--show-current").strip(), "policy")
            self.assertEqual(policy.git(source, "remote").strip(), "")
            self.assertTrue((source / ".local").is_dir())
            self.assertTrue((source / ".git").is_dir())

    def test_lineage_records_source_tools_and_agent_state(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            repo = make_repo(root)
            source = root / "exo-source"
            policy.create_policy_repo(repo, source, policy.head(repo))
            exo_root = root / "exo"
            write_artifact(exo_root, "mem", "memory/exo-memory.json", 1, "[]")

            policy.record_policy(source, exo_root=exo_root, message="initial policy")
            initial = policy.head(source)
            self.assertEqual((source / ".exo/agent/memory/exo-memory.json").read_text(), "[]")
            self.assertFalse(policy.source_changed(source, since=initial))

            (source / "harness.ts").write_text("export const policy = 2;\n")
            (source / "new-tool.ts").write_text("export const tool = true;\n")
            (source / ".exo/agent-tools").mkdir(parents=True)
            (source / ".exo/agent-tools/k8s-scan.ts").write_text("scan\n")
            (source / "target").mkdir()
            (source / "target/exo").write_text("binary")
            write_artifact(exo_root, "mem", "memory/exo-memory.json", 2, '["lesson"]')
            write_artifact(exo_root, "skill", "skills/triage.json", 1, "{}")
            self.assertTrue(policy.source_changed(source, since=initial))

            policy.record_policy(
                source, exo_root=exo_root, message="trial 1: shop (anon): diagnosis PASS, mitigation fail"
            )
            policy.record_policy(source, exo_root=exo_root, message="trial 2: nothing changed")
            self.assertEqual(policy.trial_count(source), 2)

            log = policy.git(source, "log", "--format=%s")
            self.assertEqual(
                log.splitlines(),
                ["trial 2: nothing changed", "trial 1: shop (anon): diagnosis PASS, mitigation fail", "initial policy", "base"],
            )
            changed = policy.git(source, "diff", "--name-only", "HEAD~2", "HEAD~1").splitlines()
            self.assertEqual(
                sorted(changed),
                [
                    ".exo/agent-tools/k8s-scan.ts",
                    ".exo/agent/memory/exo-memory.json",
                    ".exo/agent/skills/triage.json",
                    "harness.ts",
                    "new-tool.ts",
                ],
            )
            self.assertNotIn("target/exo", policy.git(source, "ls-files"))

    def test_a_memory_update_alone_is_not_a_source_change(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            repo = make_repo(root)
            source = root / "exo-source"
            policy.create_policy_repo(repo, source, policy.head(repo))
            exo_root = root / "exo"
            write_artifact(exo_root, "mem", "memory/exo-memory.json", 1, "[]")
            policy.record_policy(source, exo_root=exo_root, message="initial policy")
            accepted = policy.head(source)

            write_artifact(exo_root, "mem", "memory/exo-memory.json", 2, '["fact"]')
            # The runner copies artifacts in before it asks; simulate that copy.
            (source / ".exo/agent/memory/exo-memory.json").write_text('["fact"]')

            self.assertFalse(policy.source_changed(source, since=accepted))

    def test_restore_keeps_the_rejected_state_in_history(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            repo = make_repo(root)
            source = root / "exo-source"
            policy.create_policy_repo(repo, source, policy.head(repo))
            exo_root = root / "exo"
            policy.record_policy(source, exo_root=exo_root, message="initial policy")
            good = policy.head(source)

            (source / "harness.ts").write_text("broken\n")
            (source / "extra.ts").write_text("x")
            (source / ".exo/tool-sources/bad").mkdir(parents=True)
            (source / ".exo/tool-sources/bad/index.ts").write_text("bad")
            (source / "target").mkdir()
            (source / "target/exo").write_text("binary")
            policy.record_policy(source, exo_root=exo_root, message="trial 1: probe FAILED")
            rejected = policy.head(source)

            policy.restore_source(source, good)
            policy.record_policy(source, exo_root=exo_root, message="trial 1: reverted")

            self.assertEqual((source / "harness.ts").read_text(), "export const policy = 1;\n")
            self.assertFalse((source / "extra.ts").exists())
            self.assertFalse((source / ".exo/tool-sources/bad").exists())
            self.assertTrue((source / "target/exo").exists())
            self.assertEqual(policy.git(source, "rev-parse", "HEAD~1").strip(), rejected)
            self.assertEqual(policy.git(source, "diff", "--stat", good, "HEAD").strip(), "")
            self.assertFalse(policy.source_changed(source, since=policy.head(source)))

    def test_prune_build_outputs_leaves_the_source(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            source = Path(temporary_directory)
            for relative in policy.BUILD_OUTPUTS:
                (source / relative).mkdir()
                (source / relative / "x").write_text("x")
            (source / "harness.ts").write_text("keep")

            policy.prune_build_outputs(source)

            self.assertFalse(any((source / relative).exists() for relative in policy.BUILD_OUTPUTS))
            self.assertTrue((source / "harness.ts").exists())


if __name__ == "__main__":
    unittest.main()
