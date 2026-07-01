"""Exo system for the Continual Learning Bench (continual-learning-bench.com).

This file is symlinked into a clbench checkout at `src/systems/exo/system.py`
(see ../setup.sh), so its relative imports resolve against clbench's `src/`.

Each `respond(query)` runs exo on the host (Simple Coding Agent harness,
local-process sandbox, per-instance temp root/workspace) on the task prompt plus a
schema instruction, then parses exo's final assistant message into the query's
Pydantic `response_schema`. `observe()` stashes the task's feedback into the next
prompt — the only "learning" available on this branch. exo's durable agent memory
(other branch) plugs into observe()/_init() later; this is the eval support for it.

Modeled on the in-tree `codex` system.
"""

from __future__ import annotations

import glob
import json
import os
import shutil
import subprocess
import tempfile
from typing import Any, Optional

from pydantic import BaseModel

from ...interface import (
    ContinualLearningSystem,
    Observation,
    Query,
    Response,
    observation_marks_instance_complete,
)
from ...registry import register_system
from ..utils.structured_output import (
    extract_json,
    schema_to_prompt_instruction,
    validate_with_coercion,
)

_EXO_REPO = os.environ.get("EXO_REPO", "/home/worker/exo")
_EXO_BIN = os.environ.get("EXO_BIN", os.path.join(_EXO_REPO, "target", "release", "exo"))
_HARNESS = os.environ.get(
    "EXO_HARNESS",
    os.path.join(_EXO_REPO, "examples", "simple-coding-agent", "harness.ts"),
)


@register_system("exo")
class ExoSystem(ContinualLearningSystem):
    """Drives exo as a clbench system (host-side, local-process sandbox)."""

    supports_baseline = True
    parallel_safe = True  # per-instance temp root + workspace isolate state

    def __init__(self, model: str = "gpt-5.5", **kwargs: Any) -> None:
        self._model = model
        self._pending_feedback: Optional[str] = None
        self._root: Optional[str] = None
        self._cwd: Optional[str] = None  # per-run cwd so .exo/agent-tools is isolated
        self._base: list[str] = []
        self._agent_inited = False
        self._conv: Optional[str] = None  # current per-instance conversation slug
        self._conv_n = 0

    @property
    def name(self) -> str:
        return "exo"

    # --- exo lifecycle ------------------------------------------------------
    def _init_agent(self) -> None:
        """Once per run: create the agent. Its durable memory persists across the
        run's instances (that's the continual-learning carrier); reset() (per run,
        and per baseline instance) tears it down for a clean slate."""
        if self._agent_inited:
            return
        if not os.path.exists(_EXO_BIN):
            raise RuntimeError(f"exo binary not found at {_EXO_BIN}; build it or set EXO_BIN")
        self._root = tempfile.mkdtemp(prefix="exo-clbench-root-")
        # Per-run cwd: exo resolves its harness runner + @exo/harness relative to
        # cwd, and agent-built tools (install_agent_tool) land in cwd/.exo/agent-tools.
        # A shared cwd (_EXO_REPO) would leak learned tools across parallel runs and
        # into the stateless baseline (contaminating Gain). So give each run its own
        # cwd that symlinks just the resolution bits back to the repo.
        self._cwd = tempfile.mkdtemp(prefix="exo-clbench-cwd-")
        for x in ("tsconfig.json", "node_modules", "typescript", "examples", "package.json"):
            src = os.path.join(_EXO_REPO, x)
            if os.path.exists(src):
                try:
                    os.symlink(src, os.path.join(self._cwd, x))
                except FileExistsError:
                    pass
        self._base = [_EXO_BIN, "--root", self._root, "--secret-backend", "file"]
        self._run(self._base + ["secret", "set", "openai", "--env", "OPENAI_API_KEY"])
        self._run(self._base + ["model", "register", self._model, "--secret", "openai"])
        # Docker sandbox = isolated container (NO host mount), so the agent's shell
        # cannot reach the host's clbench repo / ground-truth data. (local-process
        # would expose the host filesystem and let the agent read the answer keys.)
        self._run(
            self._base
            + ["agent", "create", "--slug", "t", "--model", self._model,
               "--harness", _HARNESS, "--tool-creation", "enabled",
               "--sandbox-provider", "docker", "Exo"]
        )
        self._agent_inited = True

    def _ensure_conversation(self) -> None:
        """Fresh conversation per task instance (clean context for the new scenario);
        the agent and its memory persist, so lessons carry across instances. No host
        mount — task data arrives via the prompt; the container provides scratch FS."""
        if self._conv is not None:
            return
        self._conv = f"c{self._conv_n}"
        self._conv_n += 1
        self._run(self._base + ["conversation", "create", "t", self._conv])

    def _run(self, argv: list[str], check: bool = True) -> str:
        proc = subprocess.run(
            argv, cwd=(self._cwd or _EXO_REPO), env=os.environ.copy(),
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        if check and proc.returncode != 0:
            raise RuntimeError(f"exo step failed (rc={proc.returncode}): {argv[4:]}\n{proc.stdout[-1500:]}")
        return proc.stdout or ""

    def _last_assistant_text(self) -> str:
        """Final assistant text from the CURRENT conversation (newest event store)."""
        conv_dirs = glob.glob(
            os.path.join(self._root or "", "**", "conversations", "*", "events"),
            recursive=True,
        )
        if not conv_dirs:
            return ""

        def newest_mtime(d: str) -> float:
            files = glob.glob(os.path.join(d, "*.json"))
            return max((os.path.getmtime(f) for f in files), default=0.0)

        events_dir = max(conv_dirs, key=newest_mtime)  # the conversation we just sent to
        events = []
        for path in glob.glob(os.path.join(events_dir, "*.json")):
            try:
                ev = json.load(open(path))
            except Exception:
                continue
            if (ev.get("data") or {}).get("type") == "messages":
                events.append((ev.get("created_at", ""), ev["data"]))
        events.sort(key=lambda e: e[0])
        text = ""
        for _, data in events:
            for msg in data.get("messages", []):
                if msg.get("role") != "assistant":
                    continue
                content = msg.get("content")
                items = content if isinstance(content, list) else [content]
                parts = [
                    it["text"] for it in items
                    if isinstance(it, dict) and it.get("type") == "text" and it.get("text")
                ]
                if parts:
                    text = "\n".join(parts)  # keep the latest assistant text turn
        return text

    # --- clbench interface -------------------------------------------------
    def respond(self, query: Query) -> Response:
        self._init_agent()
        self._ensure_conversation()
        prompt = query.prompt
        if self._pending_feedback:
            prompt = (
                f"Feedback on your previous answer: {self._pending_feedback}\n"
                "Use it to do better this time.\n\n" + prompt
            )
            self._pending_feedback = None
        prompt += schema_to_prompt_instruction(query.response_schema)

        send = self._base + ["conversation", "send", "t", self._conv, "--"]
        self._run(send + [prompt], check=False)
        action = self._parse(query.response_schema)
        if action is None:
            # one repair attempt: re-ask for JSON only, same conversation
            self._run(send + ["Your last message must be ONLY a JSON object matching the schema "
                              "given earlier — no prose, no code fences. Output it now."], check=False)
            action = self._parse(query.response_schema)
        if action is None:
            # exo never emitted a schema-valid answer (e.g. it rabbit-holed on
            # exploration without concluding). Treat that as a failed-to-answer
            # instance — zero credit — rather than crashing the whole run, which
            # would also abort every other (good) instance in the baseline.
            fallback = self._fallback_action(query.response_schema)
            if fallback is None:
                raise RuntimeError("exo did not produce a schema-valid response after one repair")
            return Response(action=fallback, metadata={"no_valid_answer": True})
        return Response(action=action)

    @staticmethod
    def _fallback_action(schema: type[BaseModel]) -> Optional[BaseModel]:
        """Build a minimal schema-valid 'null' answer (type-based zero defaults).

        Used only when exo fails to produce any valid response; the resulting
        answer is intentionally wrong so the instance scores zero. Returns None
        if the schema is too complex to default, in which case the caller raises.
        """
        defaults: dict[str, Any] = {}
        for fname, field in schema.model_fields.items():
            ann = field.annotation
            if ann in (float, int):
                defaults[fname] = 0
            elif ann is str:
                defaults[fname] = ""
            elif ann is bool:
                defaults[fname] = False
            elif not field.is_required():
                continue
            else:
                return None
        try:
            return schema(**defaults)
        except Exception:
            return None

    def _parse(self, schema: type[BaseModel]) -> Optional[BaseModel]:
        text = self._last_assistant_text()
        if not text.strip():
            return None
        try:
            return validate_with_coercion(extract_json(text), schema)
        except Exception:
            return None

    def _read_memory(self) -> tuple[list, int, int]:
        """Richest durable-memory store + remember/forget tool-call counts from this
        run's exo root. The store is versioned (`artifacts/<id>/<n>.bin`, shape
        {entries:[...]}); the copy with the most entries is the latest."""
        if not self._root:
            return [], 0, 0
        best: list = []
        for p in glob.glob(os.path.join(self._root, "**", "artifacts", "**", "*.bin"), recursive=True):
            try:
                d = json.load(open(p))
            except Exception:
                continue
            if isinstance(d, dict) and isinstance(d.get("entries"), list) and len(d["entries"]) >= len(best):
                best = d["entries"]
        remember_calls = forget_calls = 0
        for p in glob.glob(os.path.join(self._root, "**", "events", "*.json"), recursive=True):
            try:
                s = open(p).read()
            except Exception:
                continue
            remember_calls += s.count('"tool_name":"remember"') + s.count('"tool_name": "remember"')
            forget_calls += s.count('"tool_name":"forget"') + s.count('"tool_name": "forget"')
        return best, remember_calls, forget_calls

    def _snapshot_memory(self) -> None:
        """Persist the current memory store to EXO_MEM_SNAPSHOT_DIR for the report.
        clbench's parallel (`permute`) runner never calls get_run_artifacts, so this
        is the reliable capture hook (called from observe() at every instance
        boundary, while the root is still alive). One file per exo root; gen_report
        picks the richest across the dir — so per-instance baseline resets (tiny
        stores) never clobber the stateful run's accumulated memory."""
        snap_dir = os.environ.get("EXO_MEM_SNAPSHOT_DIR")
        if not snap_dir or not self._root:
            return
        best, remember_calls, forget_calls = self._read_memory()
        try:
            os.makedirs(snap_dir, exist_ok=True)
            out = os.path.join(snap_dir, os.path.basename(self._root) + ".json")
            with open(out, "w") as f:
                json.dump({"memory_entries": best, "remember_calls": remember_calls,
                           "forget_calls": forget_calls}, f)
        except Exception:
            pass

    def get_run_artifacts(self) -> Optional[dict[str, Any]]:
        """Export durable memory if clbench's serial runner asks (kept for the
        non-parallel path; the parallel runner relies on _snapshot_memory instead)."""
        best, remember_calls, forget_calls = self._read_memory()
        if not self._root:
            return None
        return {
            "artifact_type": "exo",
            "memory_count": len(best),
            "remember_calls": remember_calls,
            "forget_calls": forget_calls,
            "memory_entries": best,
            "memory_files": {"memory.json": json.dumps({"entries": best}, indent=2)},
        }

    def observe(self, observation: Observation, next_query: Optional[Query] = None) -> None:
        # At an instance boundary, drop the conversation so the next instance starts
        # fresh — the agent (and its durable memory) persists, carrying lessons.
        if observation_marks_instance_complete(observation):
            self._conv = None
            self._snapshot_memory()  # capture for the report (parallel runner won't)
        content = (observation.content or "").strip()
        if content:
            self._pending_feedback = content

    def reset(self) -> None:
        # EXO_KEEP_ROOT=1 leaves temp roots on disk for memory inspection/debugging.
        if os.environ.get("EXO_KEEP_ROOT") != "1" and self._root:
            shutil.rmtree(self._root, ignore_errors=True)
        if os.environ.get("EXO_KEEP_ROOT") != "1" and self._cwd:
            shutil.rmtree(self._cwd, ignore_errors=True)
        self._root = None
        self._cwd = None
        self._base = []
        self._pending_feedback = None
        self._agent_inited = False
        self._conv = None
        self._conv_n = 0
