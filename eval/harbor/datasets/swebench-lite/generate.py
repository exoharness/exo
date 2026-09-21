#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# # swebench is pinned: 5.x moved the harness modules this imports.
# dependencies = ["swebench==4.1.0", "datasets>=2.16,<5"]
# ///
"""Generate Harbor task directories for SWE-bench Lite into this directory.

Harbor's registry carries SWE-bench Verified but not Lite, so this builds the
tasks locally. One directory per instance is written next to this script,
rendered from ./task-template:

    <instance_id>/
      instruction.md        the issue, with a short framing preamble
      task.toml             Harbor config; agent phase runs without network
      environment/Dockerfile   FROM the prebuilt SWE-bench instance image
      tests/test.sh         runs the hidden tests and the SWE-bench grader
      tests/config.json     the raw dataset record the grader reads
      solution/solve.sh     applies the gold patch (Harbor's oracle agent)

The test-script construction follows Harbor's swebench adapter
(laude-institute/harbor, adapters/swebench, Apache-2.0), which in turn
mirrors SWE-bench's own evaluation script.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import shutil
import sys
from pathlib import Path
from textwrap import dedent

from datasets import load_dataset
from swebench.harness.constants import MAP_REPO_VERSION_TO_SPECS
from swebench.harness.test_spec.python import get_test_directives
from swebench.harness.test_spec.test_spec import make_test_spec

HERE = Path(__file__).resolve().parent
TEMPLATE_DIR = HERE / "task-template"
DATASET = "princeton-nlp/SWE-bench_Lite"
TASK_FILES = (
    "instruction.md",
    "task.toml",
    "environment/Dockerfile",
    "tests/test.sh",
    "solution/solve.sh",
)


def render(template: str, **values: str) -> str:
    """Substitute exact {name} placeholders, leaving all other braces alone.

    The templates contain shell and Python with their own braces, so this is
    plain replacement rather than str.format.
    """
    for name, value in values.items():
        template = template.replace("{" + name + "}", value)
    return template


def image_name(record: dict) -> str:
    spec = make_test_spec(record, namespace="swebench")
    return spec.instance_image_key.replace("arm64", "x86_64")


def patch_paths(test_patch: str) -> tuple[list[str], list[str]]:
    """Paths a unified diff reads from, and every path it touches."""
    before = [
        path
        for path in re.findall(r"^--- a/(.*)$", test_patch, re.MULTILINE)
        if path != "/dev/null"
    ]
    after = [
        path
        for path in re.findall(r"^\+\+\+ b/(.*)$", test_patch, re.MULTILINE)
        if path != "/dev/null"
    ]
    tracked = list(dict.fromkeys(before))
    touched = list(dict.fromkeys(before + after))
    return tracked, touched


def test_commands(record: dict) -> str:
    """The shell that applies the hidden test patch and runs the tests.

    The test patch's files are reset to the base commit first so the agent's
    edits to them never mask the hidden tests, and again afterwards. New files
    the test patch creates are removed the same way; files the agent created
    elsewhere are left alone.
    """
    repo, version = record["repo"], record["version"]
    test_patch, base_commit = record["test_patch"], record["base_commit"]
    specs = MAP_REPO_VERSION_TO_SPECS[repo][version]
    test_command = specs["test_cmd"]
    setup_commands = specs.get("eval_commands", [])
    install_command = specs.get("install", "")
    if repo == "scikit-learn/scikit-learn":
        # Re-running scikit-learn's install fails inside the prebuilt image.
        install_command = ""

    tracked, touched = patch_paths(test_patch)
    quoted_tracked = " ".join(shlex.quote(path) for path in tracked)
    quoted_touched = " ".join(shlex.quote(path) for path in touched)
    reset_tracked = f"git checkout {base_commit} {quoted_tracked}" if tracked else ":"
    remove_untracked = (
        dedent(
            f"""
            for path in {quoted_touched}; do
                if [ -e "$path" ] && ! git ls-files --error-unmatch -- "$path" >/dev/null 2>&1; then
                    rm -rf -- "$path"
                fi
            done
            """
        ).strip()
        if touched
        else ":"
    )
    test_files = get_test_directives({"repo": repo, "test_patch": test_patch})
    newline = "\n"
    return dedent(
        f"""#!/bin/bash
        set -uo pipefail -x

        cd /testbed
        set +x
        source /opt/miniconda3/bin/activate
        conda activate testbed
        set -x

        # Repo-specific environment setup, then any repo-specific install step.
        {newline.join(setup_commands)}
        {install_command}

        # Reset the hidden test patch's tracked files, then drop untracked ones it
        # would create, so the agent's changes to them never mask the tests.
        {reset_tracked}
        {remove_untracked}

        echo {shlex.quote(test_patch)} > /tmp/test_patch.diff
        git apply --check /tmp/test_patch.diff
        git apply /tmp/test_patch.diff

        # Record the test run for the SWE-bench log parser.
        LOG_FILE=$(mktemp)
        export LOG_FILE
        exec 3>&1 4>&2
        exec > >(tee "$LOG_FILE") 2>&1

        set +x
        {test_command} {" ".join(test_files)} || true
        exec 1>&3 2>&4

        {reset_tracked}
        {remove_untracked}
    """
    )


def write_task(record: dict, out_dir: Path, timeout_sec: float) -> Path:
    task_dir = out_dir / record["instance_id"]
    if task_dir.exists():
        shutil.rmtree(task_dir)
    values = {
        "instance_id": record["instance_id"],
        "repo": record["repo"],
        "version": record["version"],
        "base_commit": record["base_commit"],
        # Issue text arrives with Windows line endings; normalize them so the
        # output does not depend on how this Python's dedent treats bare CRs.
        "problem_statement": dedent(
            record["problem_statement"].replace("\r\n", "\n")
        ).strip(),
        "max_timeout": str(int(timeout_sec)),
        "docker_image": image_name(record),
        "test_commands": test_commands(record),
        "patch": record["patch"].strip(),
    }
    for relative in TASK_FILES:
        rendered = render((TEMPLATE_DIR / relative).read_text(), **values)
        if not rendered.endswith("\n"):
            rendered += "\n"
        target = task_dir / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(rendered)
        if target.suffix == ".sh":
            target.chmod(0o755)
    (task_dir / "tests" / "config.json").write_text(json.dumps(record, indent=2))
    return task_dir


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--instance-id",
        action="append",
        dest="instance_ids",
        help="generate only this instance; may be passed more than once",
    )
    parser.add_argument(
        "--limit", type=int, help="generate only the first N instances by id"
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=HERE,
        help="where to write the task directories (default: beside this script)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=3000.0,
        help="agent and verifier time budget in seconds per task",
    )
    parser.add_argument(
        "--dataset", default=DATASET, help="HuggingFace dataset to generate from"
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    records = {row["instance_id"]: row for row in load_dataset(args.dataset)["test"]}
    ids = sorted(records)
    if args.instance_ids:
        missing = sorted(set(args.instance_ids) - set(ids))
        if missing:
            raise SystemExit(f"not in {args.dataset}: {', '.join(missing)}")
        ids = sorted(args.instance_ids)
    if args.limit is not None:
        ids = ids[: args.limit]

    args.output_dir.mkdir(parents=True, exist_ok=True)
    print(f"Writing {len(ids)} tasks from {args.dataset} into {args.output_dir}")
    for index, instance_id in enumerate(ids, 1):
        write_task(records[instance_id], args.output_dir, args.timeout)
        print(f"[{index}/{len(ids)}] {instance_id}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
