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
        self._workspace: Optional[str] = None
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
        self._workspace = tempfile.mkdtemp(prefix="exo-clbench-ws-")
        self._base = [_EXO_BIN, "--root", self._root, "--secret-backend", "file"]
        self._run(self._base + ["secret", "set", "openai", "--env", "OPENAI_API_KEY"])
        self._run(self._base + ["model", "register", self._model, "--secret", "openai"])
        self._run(
            self._base
            + ["agent", "create", "--slug", "t", "--model", self._model,
               "--harness", _HARNESS, "--sandbox-provider", "local-process", "Exo"]
        )
        self._agent_inited = True

    def _ensure_conversation(self) -> None:
        """Fresh conversation per task instance (clean context for the new scenario);
        the agent and its memory persist, so lessons carry across instances."""
        if self._conv is not None:
            return
        self._conv = f"c{self._conv_n}"
        self._conv_n += 1
        self._run(self._base + ["conversation", "create", "t", self._conv])
        self._run(self._base + ["conversation", "mount", "add", "t", self._conv,
                                self._workspace, self._workspace])

    def _run(self, argv: list[str], check: bool = True) -> str:
        proc = subprocess.run(
            argv, cwd=_EXO_REPO, env=os.environ.copy(),
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
            raise RuntimeError("exo did not produce a schema-valid response after one repair")
        return Response(action=action)

    def _parse(self, schema: type[BaseModel]) -> Optional[BaseModel]:
        text = self._last_assistant_text()
        if not text.strip():
            return None
        try:
            return validate_with_coercion(extract_json(text), schema)
        except Exception:
            return None

    def observe(self, observation: Observation, next_query: Optional[Query] = None) -> None:
        # At an instance boundary, drop the conversation so the next instance starts
        # fresh — the agent (and its durable memory) persists, carrying lessons.
        if observation_marks_instance_complete(observation):
            self._conv = None
        content = (observation.content or "").strip()
        if content:
            self._pending_feedback = content

    def reset(self) -> None:
        for d in (self._root, self._workspace):
            if d:
                shutil.rmtree(d, ignore_errors=True)
        self._root = self._workspace = None
        self._base = []
        self._pending_feedback = None
        self._agent_inited = False
        self._conv = None
        self._conv_n = 0
