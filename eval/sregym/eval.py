#!/usr/bin/env python3
"""Run self-evolving Exo against SREGym's native benchmark runner."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
from policy import PolicyRepo, clear_tools, reclaim_ownership  # noqa: E402


SREGYM_REPOSITORY = "https://github.com/SREGym/SREGym.git"
SREGYM_REF = "809f4167437f5477727984ef62d7f65d93c3ebda"
AGENT_SLUG = "sregym-eval"
HARNESS_MODULE = "exo/harness.ts"
MUTATION_TOOLS = {
    "forget",
    "install_agent_tool",
    "install_skill",
    "manage_tool",
    "remember",
    "rebuild_and_restart_exo",
    "uninstall_agent_tool",
    "uninstall_skill",
}
SREGYM_PATCH = Path(__file__).with_name("sregym.patch")
REVIEW_TIMEOUT_ENV = "SREGYM_REVIEW_TIMEOUT_SECONDS"
EXTRA_MOUNTS_ENV = "SREGYM_AGENT_EXTRA_MOUNTS"
# Where Exo expects its own source tree; see exo/harness.ts and exo/SELF.md.
EXO_REPO_MOUNT = "/workspace/exo"
GUARDIAN_SCRIPT = "exo/scripts/exo-service-guardian"
EXO_PROFILES = ("practical", "memory-only")
REFLECTION_OPENING = """This incident has been graded; the results are below. The cluster is still deployed as you left it, so you can inspect it if you want to understand what happened. The benchmark no longer accepts submissions for this incident. What the incident cost you is in your own event log: each messages event from list_conversation_events carries token counts and cost_usd for one model call.

If anything from this incident would help you get future incidents right, or figure them out faster or more cheaply, """
# Only self-modifying profiles have the tools these sentences refer to.
REFLECTION_SELF_MODIFICATION = """act on it now: remember it, build a tool, add a skill, or change your own policy or implementation (code changes take effect after rebuild_and_restart_exo). Only durable changes carry forward; what you say in this reply does not."""
REFLECTION_MEMORY_ONLY = """remember it now. Only what you store carries forward; what you say in this reply does not."""
REFLECTION_CLOSING = """ If nothing is worth keeping, say so and stop.

Grader feedback:
"""
# Task-start text: the harness prompt lists these abilities, but nothing else
# tells the model they are in scope for the task or how to weigh them.
SELF_MODIFICATION_BRIEF = f"""As you work, you may modify yourself to help you accomplish this task and the ones after it more efficiently (quickly, cheaply). You may inspect your own code at `{EXO_REPO_MOUNT}` (start with `exo/SELF.md`) and change it, then activate the change with rebuild_and_restart_exo; write tools with install_agent_tool; create skills with install_skill; and store durable facts with remember. Do so where what you have observed in the course of your work suggests it would help. Anything you build or remember persists across incidents; new tools are callable on your next model round, while code changes take effect from your next turn. Your top goal is to get the right answer. Doing it quicker and more cheaply is a secondary goal, never at the cost of correctness."""
MEMORY_ONLY_BRIEF = """As you work, you may store durable facts with remember to help you with this task and the ones after it; anything you remember persists across incidents. Your top goal is to get the right answer. Doing it quicker and more cheaply is a secondary goal, never at the cost of correctness."""


def parse_args(arguments: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run Exo through SREGym's native staged evaluator."
    )
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("--suite", default="sregym-lite")
    selection.add_argument("--problem")
    parser.add_argument("--model", default="gpt-5.5")
    parser.add_argument("--provider-model")
    parser.add_argument("--judge-model")
    parser.add_argument("--api-key-env", default="OPENAI_API_KEY")
    parser.add_argument("--base-url")
    parser.add_argument("--profile", choices=("full", "svelte"), default="full")
    parser.add_argument(
        "--stages",
        nargs="+",
        choices=("diagnosis", "mitigation"),
    )
    parser.add_argument("--n-attempts", type=int, default=1)
    parser.add_argument("--agent-timeout", type=int, default=1800)
    parser.add_argument("--turn-timeout", type=int)
    parser.add_argument(
        "--internet-access", choices=("filtered", "open"), default="filtered"
    )
    parser.add_argument("--allow-agent-endpoint", action="append", default=[])
    parser.add_argument(
        "--container-hardening", choices=("on", "off"), default="on"
    )
    parser.add_argument("--noise", action="store_true")
    parser.add_argument("--exo-profile", choices=EXO_PROFILES, default="practical")
    parser.add_argument("--reflection", action="store_true")
    parser.add_argument("--reflection-timeout", type=int, default=900)
    parser.add_argument("--baseline", type=int)
    parser.add_argument("--resume", type=Path)
    parser.add_argument("--force-build-sregym", action="store_true")
    parser.add_argument("--skip-exo-build", action="store_true")
    parser.add_argument("--sregym-root", type=Path)
    parser.add_argument("--run-dir", type=Path)
    parser.add_argument("--api-port", type=int, default=8000)
    parser.add_argument("--mcp-port", type=int, default=9954)
    parser.add_argument("--k8s-proxy-port", type=int, default=16443)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(arguments)
    if args.reflection and args.n_attempts > 1:
        # The graded cluster shows Exo the answer, which would contaminate a
        # later attempt at the same problem.
        parser.error("--reflection requires --n-attempts 1")
    if args.resume and not args.run_dir:
        # Resuming continues the same Exo agent, so it needs that run's directory.
        parser.error("--resume requires --run-dir of the interrupted run")
    return args


def run(
    command: list[str],
    *,
    cwd: Path,
    env: dict[str, str] | None = None,
    capture_output: bool = False,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        text=True,
        capture_output=capture_output,
        check=True,
    )


def require_command(name: str) -> str:
    command = shutil.which(name)
    if command is None:
        raise ValueError(f"required command is not on PATH: {name}")
    return command


def slug(value: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-") or "run"


def default_run_dir(repo: Path, args: argparse.Namespace) -> Path:
    timestamp = dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")
    selection = args.problem or args.suite
    return repo / ".local/sregym-evals" / f"{timestamp}-{slug(selection)}"


def ensure_sregym_checkout(path: Path, *, repo: Path) -> None:
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        run(
            [
                "git",
                "clone",
                "--recurse-submodules",
                SREGYM_REPOSITORY,
                str(path),
            ],
            cwd=repo,
        )
        run(["git", "checkout", "--detach", SREGYM_REF], cwd=path)
        run(
            ["git", "submodule", "update", "--init", "--recursive"],
            cwd=path,
        )
    actual_ref = run(
        ["git", "rev-parse", "HEAD"], cwd=path, capture_output=True
    ).stdout.strip()
    if actual_ref != SREGYM_REF:
        raise ValueError(
            f"SREGym checkout is at {actual_ref}, expected pinned ref {SREGYM_REF}: {path}"
        )


def ensure_sregym_patch(sregym_root: Path, patch: Path = SREGYM_PATCH) -> None:
    """Apply the Exo agent registration, egress exemption, and review hold."""
    applied = subprocess.run(
        ["git", "apply", "--check", "--reverse", str(patch)],
        cwd=sregym_root,
        capture_output=True,
    )
    if applied.returncode == 0:
        return
    run(["git", "apply", str(patch)], cwd=sregym_root)


class ExoClient:
    def __init__(self, *, binary: Path, root: Path, repo: Path, profile: str) -> None:
        self.binary = binary
        self.root = root
        self.repo = repo
        self.profile = profile

    def command(self, *arguments: str) -> list[str]:
        return [
            str(self.binary),
            "--root",
            str(self.root),
            "--harness",
            "exo",
            *arguments,
        ]

    def execute(self, *arguments: str, timeout: int | None = None) -> str:
        result = subprocess.run(
            self.command(*arguments),
            cwd=self.repo,
            env={**os.environ, "EXO_ROOT": str(self.root), "EXO_PROFILE": self.profile},
            text=True,
            capture_output=True,
            timeout=timeout,
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"exo {shlex.join(arguments)} failed ({result.returncode}): "
                f"{result.stderr.strip()}"
            )
        return result.stdout.strip()

    def exists(self, *arguments: str) -> bool:
        result = subprocess.run(
            self.command(*arguments),
            cwd=self.repo,
            env={**os.environ, "EXO_ROOT": str(self.root)},
            text=True,
            capture_output=True,
        )
        if result.returncode == 0:
            return True
        if "not found" in result.stderr.lower():
            return False
        raise RuntimeError(result.stderr.strip())

    def ensure_agent(self, model: str) -> None:
        if not self.exists("agent", "show", AGENT_SLUG):
            self.create_agent(model)
        # Mirror `exo.sh`: Exo's own sandbox sees its source tree too. The
        # mount is deduplicated, so this is safe to repeat.
        self.execute(
            "agent", "mount", "add", AGENT_SLUG, str(self.repo), EXO_REPO_MOUNT, "--rw"
        )

    def create_agent(self, model: str) -> None:
        self.execute(
            "agent",
            "create",
            "SREGym eval",
            "--slug",
            AGENT_SLUG,
            "--model",
            model,
            "--provider",
            "docker",
            "--sandbox-scope",
            "agent",
            "--module",
            str(self.repo / HARNESS_MODULE),
            "--tool-creation",
            "enabled",
        )

    def ensure_conversation(self, conversation: str) -> None:
        if self.exists("conversation", "show", AGENT_SLUG, conversation):
            return
        self.execute(
            "conversation",
            "create",
            AGENT_SLUG,
            conversation,
            "--slug",
            conversation,
            "--sandbox-scope",
            "conversation",
        )

    def attach(self, conversation: str, container_id: str) -> None:
        self.execute(
            "conversation",
            "sandbox",
            "attach",
            AGENT_SLUG,
            conversation,
            "--provider",
            "docker",
            "--external-id",
            container_id,
            "--default-workdir",
            "/logs",
        )

    def send(self, conversation: str, instruction: str, timeout: int) -> None:
        self.execute(
            "conversation",
            "send",
            AGENT_SLUG,
            conversation,
            instruction,
            timeout=timeout,
        )

    def events(self, conversation: str) -> dict[str, Any]:
        output = self.execute(
            "conversation",
            "events",
            AGENT_SLUG,
            conversation,
            "--type",
            "messages",
            "--type",
            "tool_requested",
            "--type",
            "tool_result",
            "--limit",
            "10000",
        )
        return json.loads(output)


def setup_model(
    client: ExoClient,
    *,
    model: str,
    provider_model: str,
    api_key_env: str,
    base_url: str | None,
) -> None:
    secret = slug(api_key_env)
    client.execute("secret", "set", secret, "--env", api_key_env)
    arguments = [
        "model",
        "register",
        model,
        "--secret",
        secret,
        "--model",
        provider_model,
    ]
    if base_url:
        arguments.extend(("--base-url", base_url))
    client.execute(*arguments)


def sregym_command(args: argparse.Namespace) -> list[str]:
    command = [
        "uv",
        "run",
        "main.py",
        "--agent",
        "exo",
        "--model",
        args.provider_model or args.model,
        "--judge-model",
        args.judge_model or args.provider_model or args.model,
        "--profile",
        args.profile,
        "--n-attempts",
        str(args.n_attempts),
        "--agent-timeout",
        str(args.agent_timeout),
        "--internet-access",
        args.internet_access,
        "--container-hardening",
        args.container_hardening,
    ]
    command.extend(("--problem", args.problem) if args.problem else ("--suite", args.suite))
    if args.stages:
        command.extend(("--stages", *args.stages))
    if args.noise:
        command.append("--noise")
    if args.baseline is not None:
        command.extend(("--baseline", str(args.baseline)))
    if args.resume:
        command.extend(("--resume", str(args.resume.resolve())))
    if args.force_build_sregym:
        command.append("--force-build")
    for endpoint in args.allow_agent_endpoint:
        command.extend(("--allow-agent-endpoint", endpoint))
    return command


def api_json(port: int, path: str) -> dict[str, Any]:
    with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=2) as response:
        return json.load(response)


def wait_for_review(port: int, process: subprocess.Popen[bytes]) -> dict[str, Any] | None:
    """Return the grades once SREGym holds the attempt for review.

    None means the attempt ended without a review, for example because the
    agent timed out before submitting every stage.
    """
    while process.poll() is None:
        try:
            stage = api_json(port, "/status").get("stage")
            if stage == "review":
                return api_json(port, "/results")
            if stage in {"tearing_down", "done", "aborted"}:
                return None
        except (OSError, urllib.error.URLError, json.JSONDecodeError):
            pass
        time.sleep(1)
    return None


def release_review(port: int) -> None:
    request = urllib.request.Request(f"http://127.0.0.1:{port}/release", method="POST")
    try:
        with urllib.request.urlopen(request, timeout=5):
            pass
    except (OSError, urllib.error.URLError):
        # Nothing is held: the attempt ended some other way, or SREGym exited.
        pass


def build_reflection(results: dict[str, Any], *, self_modification: bool) -> str:
    return (
        REFLECTION_OPENING
        + (REFLECTION_SELF_MODIFICATION if self_modification else REFLECTION_MEMORY_ONLY)
        + REFLECTION_CLOSING
        + json.dumps(results, indent=2, default=str)
        + "\n"
    )


def wait_for_app(port: int, process: subprocess.Popen[bytes]) -> dict[str, Any]:
    while process.poll() is None:
        try:
            status = api_json(port, "/status")
            if status.get("stage") in {"diagnosis", "mitigation"}:
                return api_json(port, "/get_app")
        except (OSError, urllib.error.URLError, json.JSONDecodeError):
            pass
        time.sleep(1)
    raise RuntimeError("SREGym exited before an agent stage became ready")


def active_agent_containers() -> list[str]:
    result = subprocess.run(
        [
            "docker",
            "ps",
            "--filter",
            "name=^/sregym-exo-run-",
            "--format",
            "{{.ID}}",
        ],
        text=True,
        capture_output=True,
        check=True,
    )
    return result.stdout.split()


def wait_for_container(
    process: subprocess.Popen[bytes], handled: set[str]
) -> str | None:
    while process.poll() is None:
        containers = [item for item in active_agent_containers() if item not in handled]
        if len(containers) > 1:
            raise RuntimeError(f"multiple unhandled SREGym Exo containers: {containers}")
        if containers:
            return containers[0]
        time.sleep(1)
    return None


def inspect_container(container_id: str) -> tuple[str, Path]:
    result = subprocess.run(
        ["docker", "inspect", container_id],
        text=True,
        capture_output=True,
        check=True,
    )
    details = json.loads(result.stdout)[0]
    environment = dict(
        item.split("=", 1) for item in details["Config"]["Env"] if "=" in item
    )
    artifact_id = environment["SREGYM_ARTIFACT_ID"]
    logs = next(
        Path(mount["Source"])
        for mount in details["Mounts"]
        if mount["Destination"] == "/logs"
    )
    return artifact_id, logs


def build_instruction(
    app: dict[str, Any], *, api_port: int, stages: list[str], self_modification: bool
) -> str:
    namespaces = ", ".join(app.get("namespaces") or [app["namespace"]])
    endpoint = f"http://host.docker.internal:{api_port}/submit"
    stage_instructions: list[str] = []
    if "diagnosis" in stages:
        stage_instructions.append(
            "1. Diagnose the incident. Submit a precise root-cause description with "
            f"`curl -sS -X POST {endpoint} -H 'Content-Type: application/json' "
            "-d '{\"stage\":\"diagnosis\",\"solution\":\"YOUR DIAGNOSIS\"}'`."
        )
    if "mitigation" in stages:
        stage_instructions.append(
            "2. Apply a root-cause fix and verify recovery. Then submit mitigation with "
            f"`curl -sS -X POST {endpoint} -H 'Content-Type: application/json' "
            "-d '{\"stage\":\"mitigation\",\"solution\":\"\"}'`."
        )
    return f"""You are the SRE agent in an automated SREGym evaluation.

Application: {app['app_name']}
Namespaces: {namespaces}

{app.get('descriptions', '')}

Work autonomously. Do not ask for confirmation. Use kubectl and the available Linux tools to inspect the live cluster. Do not stop after explaining the answer: every configured stage must be submitted through the benchmark API.

{os.linesep.join(stage_instructions)}

Diagnosis is graded for the faulty component and root cause. Mitigation is graded from live system health and whether the root cause was fixed rather than masked. The benchmark API may block a mitigation submission briefly while diagnosis grading finishes; wait for its response.

{SELF_MODIFICATION_BRIEF if self_modification else MEMORY_ONLY_BRIEF}
"""


def improvement_actions(events: dict[str, Any]) -> list[dict[str, Any]]:
    actions: list[dict[str, Any]] = []
    actions_by_call_id: dict[str, dict[str, Any]] = {}
    for event in events.get("events", []):
        data = event.get("data", {})
        if data.get("type") == "tool_result":
            action = actions_by_call_id.get(data.get("tool_call_id"))
            if action is not None:
                result = data.get("result", {})
                action["result"] = result
                if "ok" in result:
                    action["succeeded"] = bool(result["ok"])
                elif "is_error" in result:
                    action["succeeded"] = not bool(result["is_error"])
            continue
        if data.get("type") != "tool_requested":
            continue
        request = data.get("request", {})
        name = request.get("function_name")
        if name in MUTATION_TOOLS:
            call_id = data.get("tool_call_id")
            action = {
                "timestamp": event.get("created_at"),
                "tool_call_id": call_id,
                "tool": name,
                "arguments": request.get("arguments", {}),
                "succeeded": None,
            }
            actions.append(action)
            if isinstance(call_id, str):
                actions_by_call_id[call_id] = action
    return actions


def write_artifacts(
    logs: Path,
    *,
    events: dict[str, Any],
    conversation: str,
    container_id: str,
    reflected: bool,
) -> None:
    logs.mkdir(parents=True, exist_ok=True)
    (logs / "exo-trajectory.json").write_text(json.dumps(events, indent=2) + "\n")
    report = {
        "conversation": conversation,
        "container": container_id,
        "reflected": reflected,
        "actions": improvement_actions(events),
    }
    (logs / "exo-self-improvements.json").write_text(
        json.dumps(report, indent=2) + "\n"
    )


def write_run_manifest(run_dir: Path, *, args: argparse.Namespace, repo: Path, command: list[str]) -> None:
    """Record how the run was started, so results can be traced to their setup."""
    manifest = {
        "started_at": dt.datetime.now(dt.UTC).isoformat(),
        "arguments": {
            key: (str(value) if isinstance(value, Path) else value)
            for key, value in vars(args).items()
        },
        "sregym_ref": SREGYM_REF,
        "sregym_command": command,
        "exo_commit": run(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True).stdout.strip(),
        "task_brief": {
            "self_modification": SELF_MODIFICATION_BRIEF,
            "memory_only": MEMORY_ONLY_BRIEF,
        },
        "reflection_instructions": {
            "opening": REFLECTION_OPENING,
            "self_modification": REFLECTION_SELF_MODIFICATION,
            "memory_only": REFLECTION_MEMORY_ONLY,
            "closing": REFLECTION_CLOSING,
        },
    }
    manifests = run_dir / "run.json"
    # A resumed run appends its own entry, keeping the original.
    entries = json.loads(manifests.read_text()) if manifests.exists() else []
    entries.append(manifest)
    manifests.write_text(json.dumps(entries, indent=2) + "\n")


def stop_guardian_services(repo: Path, *, exo_root: Path) -> None:
    """Stop the scheduler and adapters a rebuild_and_restart_exo call started."""
    if not (exo_root / "exo-service-guardian-actions.log").exists():
        return
    subprocess.run(
        [str(repo / GUARDIAN_SCRIPT), "stop-services"],
        cwd=repo,
        env={**os.environ, "EXO_ROOT": str(exo_root)},
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def stop_container(container_id: str) -> None:
    subprocess.run(
        ["docker", "stop", "--time", "3", container_id],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def grade_summary(results: dict[str, Any] | None) -> str:
    if results is None:
        return "ungraded"
    grades = []
    for stage in ("Diagnosis", "Mitigation"):
        if stage in results:
            outcome = results[stage]
            passed = isinstance(outcome, dict) and outcome.get("success") is True
            grades.append(f"{stage.lower()} {'PASS' if passed else 'fail'}")
    return ", ".join(grades) or "ungraded"


def run_trials(
    process: subprocess.Popen[bytes],
    *,
    client: ExoClient,
    args: argparse.Namespace,
    policy: PolicyRepo,
) -> None:
    handled: set[str] = set()
    stages = args.stages or ["diagnosis", "mitigation"]
    # SREGym kills the agent at its own timeout, which normally ends the turn;
    # this backstop only catches a turn that never notices.
    timeout = args.turn_timeout or args.agent_timeout + 120

    while (container_id := wait_for_container(process, handled)) is not None:
        handled.add(container_id)
        artifact_id, logs = inspect_container(container_id)
        conversation = f"trial-{slug(artifact_id)}-{container_id[:8]}"
        app = wait_for_app(args.api_port, process)
        instruction = build_instruction(
            app,
            api_port=args.api_port,
            stages=stages,
            self_modification=args.exo_profile == "practical",
        )
        print(f"\n=== Exo trial {artifact_id} ({container_id[:12]}) ===", flush=True)
        client.ensure_conversation(conversation)
        client.attach(conversation, container_id)
        reflected = False
        results = None
        try:
            try:
                client.send(conversation, instruction, timeout)
            except (subprocess.TimeoutExpired, RuntimeError) as error:
                # A trial that SREGym timed out or whose turn failed is still
                # graded (or recorded incomplete) by SREGym; keep the suite going.
                print(f"trial turn ended abnormally: {error}", file=sys.stderr, flush=True)
            if args.reflection:
                results = wait_for_review(args.api_port, process)
                if results is None:
                    print("SREGym ended the attempt without a review", flush=True)
                else:
                    print("Reflecting on the graded incident", flush=True)
                    try:
                        client.send(
                            conversation,
                            build_reflection(results, self_modification=args.exo_profile == "practical"),
                            args.reflection_timeout,
                        )
                        reflected = True
                    except (subprocess.TimeoutExpired, RuntimeError) as error:
                        print(f"reflection ended abnormally: {error}", file=sys.stderr, flush=True)
        finally:
            try:
                if args.reflection:
                    # Let SREGym tear down before its agent container disappears.
                    release_review(args.api_port)
                events = client.events(conversation)
                write_artifacts(
                    logs,
                    events=events,
                    conversation=conversation,
                    container_id=container_id,
                    reflected=reflected,
                )
            finally:
                stop_container(container_id)
                reclaim_ownership(client.repo)
                policy.commit(
                    f"trial {policy.trial_count() + 1}: {app['app_name']} ({artifact_id}): "
                    f"{grade_summary(results)}"
                )


def main() -> int:
    args = parse_args()
    try:
        if args.n_attempts < 1 or args.agent_timeout < 1:
            raise ValueError("n-attempts and agent-timeout must be positive")
        repo = Path(__file__).resolve().parents[2]
        run_dir = (args.run_dir or default_run_dir(repo, args)).resolve()
        sregym_root = (
            args.sregym_root or repo / ".local/sregym-evals/upstream/SREGym"
        ).resolve()
        command = sregym_command(args)

        if args.dry_run:
            print(f"SREGym ref: {SREGYM_REF}")
            print(f"SREGym root: {sregym_root}")
            print(f"Run directory: {run_dir}")
            print(f"Command: {shlex.join(command)}")
            return 0

        if not os.environ.get(args.api_key_env):
            raise ValueError(f"{args.api_key_env} is not set")
        for required in ("cargo", "docker", "git", "node", "pnpm", "uv"):
            require_command(required)
        if subprocess.run(
            ["docker", "info"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode:
            raise ValueError("Docker is unavailable; run this through ./eval.sh")

        resuming = args.resume is not None
        if resuming:
            if not (run_dir / "exo").is_dir():
                raise ValueError(f"--run-dir is not an earlier run to resume: {run_dir}")
        else:
            run_dir.mkdir(parents=True, exist_ok=False)
        write_run_manifest(run_dir, args=args, repo=repo, command=command)
        ensure_sregym_checkout(sregym_root, repo=repo)
        ensure_sregym_patch(sregym_root)
        run(["uv", "sync"], cwd=sregym_root)

        exo_binary = repo / "target/debug/exo"
        if args.skip_exo_build:
            if not exo_binary.is_file():
                raise ValueError(f"missing Exo binary: {exo_binary}")
        else:
            run(["cargo", "build", "-p", "exo"], cwd=repo)
            run(["pnpm", "install", "--frozen-lockfile"], cwd=repo)

        client = ExoClient(
            binary=exo_binary, root=run_dir / "exo", repo=repo, profile=args.exo_profile
        )
        policy = PolicyRepo(run_dir / "policy", repo=repo, exo_root=client.root)
        if resuming:
            # The same agent continues with its memory, skills, and tools.
            client.ensure_agent(args.model)
            policy.commit(f"resumed from {args.resume.resolve()}")
        else:
            setup_model(
                client,
                model=args.model,
                provider_model=args.provider_model or args.model,
                api_key_env=args.api_key_env,
                base_url=args.base_url,
            )
            client.ensure_agent(args.model)
            # Every run starts with no inherited agent-built tools; the
            # previous run's tools are in its own policy repository.
            reclaim_ownership(repo)
            clear_tools(repo)
            policy.init()

        environment = {
            **os.environ,
            # Exo runs in SREGym's agent container, so mount its source tree
            # there the way `exo.sh` mounts it into Exo's own sandbox. An
            # empty anonymous volume covers .local: it holds the SREGym
            # checkout with every fault's code and earlier runs' grades.
            # Without self-modification the tree is read-only, since Exo's
            # TypeScript loads fresh every turn and a shell edit would
            # otherwise change its policy without any rebuild.
            EXTRA_MOUNTS_ENV: json.dumps(
                [
                    f"{repo}:{EXO_REPO_MOUNT}:{'rw' if args.exo_profile == 'practical' else 'ro'}",
                    f"{EXO_REPO_MOUNT}/.local",
                ]
            ),
            "API_PORT": str(args.api_port),
            "MCP_SERVER_PORT": str(args.mcp_port),
            "K8S_PROXY_PORT": str(args.k8s_proxy_port),
        }
        if args.reflection:
            # SREGym's own deadline for the review hold; the runner releases
            # first unless its reflection turn hangs past its timeout.
            environment[REVIEW_TIMEOUT_ENV] = str(args.reflection_timeout + 120)
        if args.base_url:
            environment["JUDGE_API_BASE"] = args.base_url
            environment["JUDGE_API_KEY"] = os.environ[args.api_key_env]
        existing_containers = active_agent_containers()
        if existing_containers:
            raise ValueError(
                "another SREGym Exo agent container is already running: "
                + ", ".join(existing_containers)
            )
        print(f"Run directory: {run_dir}")
        print(f"SREGym checkout: {sregym_root} ({SREGYM_REF})")
        print(f"Command: {shlex.join(command)}", flush=True)
        process = subprocess.Popen(command, cwd=sregym_root, env=environment)
        try:
            run_trials(process, client=client, args=args, policy=policy)
            return_code = process.wait()
        except BaseException:
            process.terminate()
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            raise
        finally:
            stop_guardian_services(repo, exo_root=client.root)
            reclaim_ownership(repo)
            policy.commit("final policy")
        if return_code != 0:
            raise subprocess.CalledProcessError(return_code, command)

        result_files = sorted((sregym_root / "results").glob("**/exo_ALL_results.csv"))
        if not result_files:
            raise ValueError(f"SREGym wrote no Exo results under {sregym_root / 'results'}")
        jobs_dir = repo / ".local/sregym-evals/harbor-jobs"
        run(
            [
                sys.executable,
                str(Path(__file__).with_name("postprocess.py")),
                str(result_files[-1].parent),
                "--jobs-dir",
                str(jobs_dir),
                "--job-name",
                run_dir.name,
            ],
            cwd=repo,
        )
        print("\n=== Results ===")
        print(result_files[-1])
        print(f"Exo state, policy repository, and per-trial audit data: {run_dir}")
        return 0
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("\nEvaluation stopped.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
