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


# --- exo driving (mirrors clbench ExoSystem) ------------------------------
def _run(argv: list[str], check: bool = True) -> str:
    proc = subprocess.run(
        argv, cwd=_EXO_REPO, env=os.environ.copy(),
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
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


# --- parsing + scoring ----------------------------------------------------
def _json_candidates(text: str):
    t = re.sub(r"```[a-zA-Z]*", "", text).replace("```", "").strip()
    yield t
    if "{" in t and "}" in t:
        yield t[t.index("{"): t.rindex("}") + 1]
    if "[" in t and "]" in t:
        yield t[t.index("["): t.rindex("]") + 1]


def parse_outputs(text: str, n_test: int) -> Optional[list]:
    obj = None
    for cand in _json_candidates(text):
        try:
            obj = json.loads(cand)
            break
        except Exception:
            continue
    if obj is None:
        return None
    if isinstance(obj, dict):
        grids = obj.get("outputs") or ([obj["output"]] if "output" in obj else None)
    elif isinstance(obj, list):
        # bare single grid (rows of ints) vs. a list of grids
        if obj and isinstance(obj[0], list) and obj[0] and isinstance(obj[0][0], int):
            grids = [obj]
        else:
            grids = obj
    else:
        grids = None
    return grids


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


# --- main -----------------------------------------------------------------
def main() -> None:
    ap = argparse.ArgumentParser(description="Run exo on ARC-AGI tasks (exact-match, pass@1).")
    ap.add_argument("--data-dir", required=True, help="dir of ARC task JSONs (e.g. ARC-AGI/data/evaluation)")
    ap.add_argument("--n", type=int, default=int(os.environ.get("ARC_N", "10")), help="how many tasks")
    ap.add_argument("--offset", type=int, default=0)
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                                   "results", "latest.json"))
    args = ap.parse_args()

    files = sorted(glob.glob(os.path.join(args.data_dir, "*.json")))
    files = files[args.offset: args.offset + args.n]
    if not files:
        raise SystemExit(f"no task JSONs in {args.data_dir}")

    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    results = []
    n_solved = 0
    for i, fp in enumerate(files, 1):
        task = json.load(open(fp))
        name = os.path.splitext(os.path.basename(fp))[0]
        try:
            text = run_exo(build_prompt(task))
            preds = parse_outputs(text, len(task["test"]))
            ok = solved(task, preds)
        except Exception as e:
            preds, ok = None, False
            print(f"  [{i}/{len(files)}] {name}: ERROR {e}")
        n_solved += int(ok)
        results.append({"task": name, "solved": ok, "n_test": len(task["test"]),
                        "parsed": preds is not None})
        print(f"  [{i}/{len(files)}] {name}: {'SOLVED' if ok else 'miss'} "
              f"(running {n_solved}/{i} = {n_solved/i:.1%})")

    summary = {"data_dir": args.data_dir, "model": _MODEL, "harness": _HARNESS,
               "n": len(files), "solved": n_solved, "pass_at_1": n_solved / len(files),
               "results": results}
    json.dump(summary, open(args.out, "w"), indent=2)
    print(f"\nARC-AGI pass@1: {n_solved}/{len(files)} = {n_solved/len(files):.1%}  ->  {args.out}")


if __name__ == "__main__":
    main()
