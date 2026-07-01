#!/usr/bin/env python3
"""Run exo on ARC-AGI tasks and score exact-match.

ARC-AGI (arcprize.org) is a grid-reasoning benchmark: each task gives a few
input->output grid demonstrations sharing one hidden transformation; the solver
must produce the output grid for held-out test input(s). Scored by exact grid
match (the official metric is pass@2; this runner does pass@1 by default).

We drive host-side exo exactly like the clbench ExoSystem: per task, build a
prompt from the train pairs + test input(s), run exo through a harness, then parse
the predicted grid(s) from the final assistant message and compare to the WITHHELD
true outputs (the public eval JSONs include answers — we strip them from the
prompt and keep them only for scoring). The default harness is the tool-less
`harness-arc.ts`, so the agent cannot read the on-disk answers.

--evolve mode: one PERSISTENT agent across the whole task sequence (continual-
learning protocol, like clbench). The agent keeps its memory, self-authored tools,
and sandbox from task to task (harness-arc-evolve.ts), each task in a fresh
conversation, with an optional verdict-only feedback turn after scoring. Serial by
construction. Reports pass@2 (official ARC metric) alongside pass@1 when the
agent offers a second candidate.
"""
from __future__ import annotations

import argparse
import glob
import json
import os
import re
import shutil
import subprocess
import tempfile
from typing import Any, Optional

_EXO_REPO = os.environ.get("EXO_REPO", "/home/worker/exo")
_EXO_BIN = os.environ.get("EXO_BIN", os.path.join(_EXO_REPO, "target", "release", "exo"))
_HARNESS = os.environ.get(
    "EXO_HARNESS",
    os.path.join(_EXO_REPO, "examples", "simple-coding-agent", "harness-arc.ts"),
)
_MODEL = os.environ.get("MODEL", "gpt-5.5")
# Sandbox is irrelevant with the tool-less ARC harness (no shell ever runs), but
# exo still requires a provider on agent create. docker = belt-and-suspenders.
_SANDBOX = os.environ.get("ARC_SANDBOX", "docker")
# Evolve mode actually runs code in the sandbox; the default ubuntu image has no
# python3, so pin one that does.
_SANDBOX_IMAGE = os.environ.get("ARC_SANDBOX_IMAGE", "python:3.13")
_EVOLVE_HARNESS = os.environ.get(
    "ARC_EVOLVE_HARNESS",
    os.path.join(_EXO_REPO, "examples", "simple-coding-agent", "harness-arc-evolve.ts"),
)


# --- grid / prompt helpers ------------------------------------------------
def grid_to_str(grid: list[list[int]]) -> str:
    return "\n".join(" ".join(str(c) for c in row) for row in grid)


def build_prompt(task: dict) -> str:
    parts = ["Solve this ARC-AGI puzzle.\n"]
    for i, pair in enumerate(task["train"], 1):
        parts.append(f"--- Example {i} INPUT ---\n{grid_to_str(pair['input'])}")
        parts.append(f"--- Example {i} OUTPUT ---\n{grid_to_str(pair['output'])}")
    for i, t in enumerate(task["test"], 1):
        parts.append(f"--- TEST {i} INPUT ---\n{grid_to_str(t['input'])}")
    n = len(task["test"])
    parts.append(
        f'Infer the rule from the {len(task["train"])} examples and apply it to the '
        f'{n} test input(s). Respond with ONLY: {{"outputs": [<grid per test input, '
        f"in order>]}} — each grid a list of rows of integers 0-9, no prose."
    )
    return "\n\n".join(parts)


def strip_answers(task: dict) -> dict:
    """The task JSON with test outputs removed — safe to hand to the agent."""
    return {
        "train": task["train"],
        "test": [{"input": t["input"]} for t in task["test"]],
    }


def build_prompt_evolve(task: dict) -> str:
    """Evolve-mode prompt: machine-readable JSON only (the harness tells the
    agent to save it to a file and work programmatically)."""
    n = len(task["test"])
    return (
        f"New ARC-AGI puzzle: {len(task['train'])} train pair(s), {n} test input(s). "
        "Task JSON (save it to a file, verify your rule on every train pair before "
        "answering):\n\n```json\n"
        + json.dumps(strip_answers(task))
        + '\n```\n\nEnd with ONLY {"outputs": [<grid per test input>], '
        '"outputs2": [<optional second attempt>]}.'
    )


# --- exo driving (mirrors clbench ExoSystem) ------------------------------
def _run(argv: list[str], check: bool = True, timeout: Optional[float] = None) -> str:
    proc = subprocess.run(
        argv, cwd=_EXO_REPO, env=os.environ.copy(),
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=timeout,
    )
    if check and proc.returncode != 0:
        raise RuntimeError(f"exo step failed (rc={proc.returncode}): {argv[4:]}\n{proc.stdout[-1500:]}")
    return proc.stdout or ""


def _last_assistant_text(root: str) -> str:
    events_dirs = glob.glob(os.path.join(root, "**", "conversations", "*", "events"), recursive=True)
    if not events_dirs:
        return ""

    def newest(d: str) -> float:
        return max((os.path.getmtime(f) for f in glob.glob(os.path.join(d, "*.json"))), default=0.0)

    events_dir = max(events_dirs, key=newest)
    msgs = []
    for path in glob.glob(os.path.join(events_dir, "*.json")):
        try:
            ev = json.load(open(path))
        except Exception:
            continue
        if (ev.get("data") or {}).get("type") == "messages":
            msgs.append((ev.get("created_at", ""), ev["data"]))
    msgs.sort(key=lambda e: e[0])
    text = ""
    for _, data in msgs:
        for m in data.get("messages", []):
            if m.get("role") != "assistant":
                continue
            content = m.get("content")
            items = content if isinstance(content, list) else [content]
            parts = [it["text"] for it in items
                     if isinstance(it, dict) and it.get("type") == "text" and it.get("text")]
            if parts:
                text = "\n".join(parts)
    return text


def run_exo(prompt: str) -> str:
    root = tempfile.mkdtemp(prefix="exo-arc-")
    base = [_EXO_BIN, "--root", root, "--secret-backend", "file"]
    try:
        _run(base + ["secret", "set", "openai", "--env", "OPENAI_API_KEY"])
        _run(base + ["model", "register", _MODEL, "--secret", "openai"])
        _run(base + ["agent", "create", "--slug", "t", "--model", _MODEL,
                     "--harness", _HARNESS, "--sandbox-provider", _SANDBOX, "ARC"])
        _run(base + ["conversation", "create", "t", "c"])
        _run(base + ["conversation", "send", "t", "c", "--", prompt], check=False)
        return _last_assistant_text(root)
    finally:
        if os.environ.get("EXO_KEEP_ROOT") != "1":
            shutil.rmtree(root, ignore_errors=True)


def _root_sandbox_ids(root: str) -> list[str]:
    ids = []
    for fp in glob.glob(os.path.join(root, "**", "sandboxes", "*.json"), recursive=True):
        try:
            ids.append(json.load(open(fp))["id"])
        except Exception:
            pass
    return ids


def _docker_cleanup(sandbox_ids: list[str], remove: bool) -> None:
    """Stop (or remove) the docker containers backing these exo sandboxes.
    exo labels each container with exo.sandbox.key=<scope>:<conv>:<sandbox-id>."""
    if not sandbox_ids or _SANDBOX != "docker":
        return
    try:
        out = subprocess.run(
            ["docker", "ps", "-a", "--filter", "label=exo.sandbox.key",
             "--format", '{{.ID}} {{.Label "exo.sandbox.key"}}'],
            stdout=subprocess.PIPE, text=True, timeout=30,
        ).stdout
        for line in out.splitlines():
            cid, _, key = line.partition(" ")
            if any(sid in key for sid in sandbox_ids):
                subprocess.run(["docker", "rm", "-f", cid] if remove
                               else ["docker", "stop", cid],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               timeout=60)
    except Exception as e:
        print(f"    (sandbox cleanup: {e})")


class EvolveSession:
    """One persistent exo agent for the whole task sequence.

    Memory (agent artifact), self-authored tools (agent-tools dir), and the
    agent-scoped sandbox all live under `root` and survive across the per-task
    conversations. The root is KEPT after the run — it *is* the interesting
    artifact (what did the agent build itself?).
    """

    def __init__(self, root: str, timeout: float):
        self.root = root
        self.timeout = timeout
        self._n = 0
        os.makedirs(root, exist_ok=True)
        base = self._base()
        _run(base + ["secret", "set", "openai", "--env", "OPENAI_API_KEY"])
        _run(base + ["model", "register", _MODEL, "--secret", "openai"])
        _run(base + ["agent", "create", "--slug", "arc", "--model", _MODEL,
                     "--harness", _EVOLVE_HARNESS,
                     "--sandbox-provider", _SANDBOX,
                     "--sandbox-image", _SANDBOX_IMAGE,
                     "--tool-creation", "enabled",
                     "ARC evolve"])

    def _base(self) -> list[str]:
        return [_EXO_BIN, "--root", self.root, "--secret-backend", "file"]

    def solve(self, prompt: str) -> str:
        """Fresh conversation for this task; agent-scoped state persists."""
        self._n += 1
        self._conv = f"task-{self._n:03d}"
        _run(self._base() + ["conversation", "create", "arc", self._conv,
                             "--sandbox-scope", "agent"])
        _run(self._base() + ["conversation", "send", "arc", self._conv, "--", prompt],
             check=False, timeout=self.timeout)
        return _last_assistant_text(self.root)

    def feedback(self, verdict: str) -> None:
        """Verdict-only feedback turn in the SAME conversation, so the agent can
        update its memory/library/tools before the next task."""
        msg = (
            f"RESULT: {verdict}. That task is finished and scored; the next task is "
            "unrelated (a different hidden rule). Use this turn to update your "
            "memory / library / tools based on what worked or failed, then reply "
            "briefly."
        )
        try:
            _run(self._base() + ["conversation", "send", "arc", self._conv, "--", msg],
                 check=False, timeout=min(self.timeout, 300))
        except subprocess.TimeoutExpired:
            print("    (feedback turn timed out; continuing)")


# --- parsing + scoring ----------------------------------------------------
def _json_candidates(text: str):
    t = re.sub(r"```[a-zA-Z]*", "", text).replace("```", "").strip()
    yield t
    if "{" in t and "}" in t:
        yield t[t.index("{"): t.rindex("}") + 1]
    if "[" in t and "]" in t:
        yield t[t.index("["): t.rindex("]") + 1]


def parse_outputs(text: str, n_test: int) -> tuple[Optional[list], Optional[list]]:
    """Returns (first-attempt grids, optional second-attempt grids)."""
    obj = None
    for cand in _json_candidates(text):
        try:
            obj = json.loads(cand)
            break
        except Exception:
            continue
    if obj is None:
        return None, None
    second = None
    if isinstance(obj, dict):
        grids = obj.get("outputs") or ([obj["output"]] if "output" in obj else None)
        second = obj.get("outputs2")
    elif isinstance(obj, list):
        # bare single grid (rows of ints) vs. a list of grids
        if obj and isinstance(obj[0], list) and obj[0] and isinstance(obj[0][0], int):
            grids = [obj]
        else:
            grids = obj
    else:
        grids = None
    return grids, second


def norm_grid(g: Any) -> Optional[list[list[int]]]:
    try:
        return [[int(c) for c in row] for row in g]
    except Exception:
        return None


def solved(task: dict, preds: Optional[list]) -> bool:
    truths = [t["output"] for t in task["test"]]
    if not preds or len(preds) < len(truths):
        return False
    return all(norm_grid(preds[i]) == truths[i] for i in range(len(truths)))


def solved_pass2(task: dict, preds: Optional[list], preds2: Optional[list]) -> bool:
    """Official ARC metric: per test input, a hit if EITHER attempt matches."""
    truths = [t["output"] for t in task["test"]]

    def hit(i: int) -> bool:
        for p in (preds, preds2):
            if p and len(p) > i and norm_grid(p[i]) == truths[i]:
                return True
        return False

    return all(hit(i) for i in range(len(truths)))


# --- main -----------------------------------------------------------------
def main() -> None:
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description="Run exo on ARC-AGI tasks (exact-match).")
    ap.add_argument("--data-dir", required=True, help="dir of ARC task JSONs (e.g. ARC-AGI/data/evaluation)")
    ap.add_argument("--n", type=int, default=int(os.environ.get("ARC_N", "10")), help="how many tasks")
    ap.add_argument("--offset", type=int, default=0)
    ap.add_argument("--out", default=os.path.join(here, "results", "latest.json"))
    ap.add_argument("--evolve", action="store_true",
                    help="one persistent self-evolving agent across the sequence (serial)")
    ap.add_argument("--feedback", choices=["verdict", "none"], default="verdict",
                    help="evolve mode: post-scoring feedback turn (default verdict-only)")
    ap.add_argument("--task-timeout", type=float,
                    default=float(os.environ.get("ARC_TASK_TIMEOUT", "1200")),
                    help="evolve mode: seconds per solve turn before scoring a miss")
    args = ap.parse_args()

    files = sorted(glob.glob(os.path.join(args.data_dir, "*.json")))
    files = files[args.offset: args.offset + args.n]
    if not files:
        raise SystemExit(f"no task JSONs in {args.data_dir}")

    session = None
    harness = _HARNESS
    if args.evolve:
        harness = _EVOLVE_HARNESS
        root = os.path.join(here, "results", "evolve-roots",
                            os.path.basename(args.out).removesuffix(".json") or "run")
        _docker_cleanup(_root_sandbox_ids(root), remove=True)  # orphans of a prior run
        shutil.rmtree(root, ignore_errors=True)
        session = EvolveSession(root, timeout=args.task_timeout)
        print(f"==> evolve mode: persistent root {root} (kept after the run)")

    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    results = []
    n_solved = n_solved2 = 0
    for i, fp in enumerate(files, 1):
        task = json.load(open(fp))
        name = os.path.splitext(os.path.basename(fp))[0]
        preds = preds2 = None
        ok = ok2 = False
        try:
            if session is not None:
                try:
                    text = session.solve(build_prompt_evolve(task))
                except subprocess.TimeoutExpired:
                    print(f"  [{i}/{len(files)}] {name}: TIMEOUT after {args.task_timeout:.0f}s")
                    text = _last_assistant_text(session.root)
            else:
                text = run_exo(build_prompt(task))
            preds, preds2 = parse_outputs(text, len(task["test"]))
            ok = solved(task, preds)
            ok2 = solved_pass2(task, preds, preds2)
        except Exception as e:
            print(f"  [{i}/{len(files)}] {name}: ERROR {e}")
        n_solved += int(ok)
        n_solved2 += int(ok2)
        results.append({"task": name, "solved": ok, "solved_pass2": ok2,
                        "n_test": len(task["test"]), "parsed": preds is not None})
        print(f"  [{i}/{len(files)}] {name}: {'SOLVED' if ok else ('pass@2' if ok2 else 'miss')} "
              f"(running {n_solved}/{i} = {n_solved/i:.1%}, pass@2 {n_solved2}/{i})")
        if session is not None and args.feedback == "verdict" and i < len(files):
            session.feedback("SOLVED" if ok2 else "FAILED")

    summary = {"data_dir": args.data_dir, "model": _MODEL, "harness": harness,
               "evolve": bool(session), "n": len(files),
               "solved": n_solved, "pass_at_1": n_solved / len(files),
               "solved_pass2": n_solved2, "pass_at_2": n_solved2 / len(files),
               "results": results}
    if session is not None:
        summary["evolve_root"] = session.root
        # Stop (don't remove) the run's containers: the sandbox contents are part
        # of the kept artifact (docker start / docker cp to audit), but a stopped
        # container costs no RAM.
        _docker_cleanup(_root_sandbox_ids(session.root), remove=False)
    json.dump(summary, open(args.out, "w"), indent=2)
    print(f"\nARC-AGI pass@1: {n_solved}/{len(files)} = {n_solved/len(files):.1%} | "
          f"pass@2: {n_solved2}/{len(files)} = {n_solved2/len(files):.1%}  ->  {args.out}")


if __name__ == "__main__":
    main()
