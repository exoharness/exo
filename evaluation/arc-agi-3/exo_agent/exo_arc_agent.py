"""Exo agent for ARC-AGI-3 (arcprize.org), the interactive reasoning benchmark.

This file is symlinked into a clone of arcprize/ARC-AGI-3-Agents at
`agents/templates/exo_arc_agent.py` (see ../setup.sh), so its relative import
(`..agent`) and the `arcengine` SDK resolve inside that framework. setup.sh also
adds it to the agent registry, so `uv run main.py --agent=exoarc --game=<id>`
finds it.

The arcengine framework owns the game loop and API (scorecards, frames, actions);
this class is the policy. Each game gets ONE persistent host-side exo conversation
(so exo accumulates context and can learn the game across steps, like a person).
Per step we render the current frame + legal actions into a prompt, run exo, and
parse its JSON reply into a GameAction. Modeled on the in-tree `LLM` template, but
the decision is made by exo (CLI, host-side) rather than a direct API call.

The default exo harness is tool-less (harness-arc3.ts): ARC-AGI-3 needs no shell,
and a tool-less agent cannot reach anything outside the prompt.
"""
from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import tempfile
from typing import Any, Optional

from arcengine import FrameData, GameAction, GameState

from ..agent import Agent

_EXO_REPO = os.environ.get("EXO_REPO", "/home/worker/exo")
_EXO_BIN = os.environ.get("EXO_BIN", os.path.join(_EXO_REPO, "target", "release", "exo"))
_HARNESS = os.environ.get(
    "EXO_HARNESS",
    os.path.join(_EXO_REPO, "examples", "simple-coding-agent", "harness-arc3.ts"),
)
_MODEL = os.environ.get("MODEL", "gpt-5.5")
# Sandbox is irrelevant (the harness registers no tools), but agent create needs one.
_SANDBOX = os.environ.get("ARC_SANDBOX", "docker")


class ExoArc(Agent):
    """Drives host-side exo as an ARC-AGI-3 policy (registered as `exoarc`)."""

    MAX_ACTIONS: int = int(os.environ.get("ARC3_MAX_ACTIONS", "80"))

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._root: Optional[str] = None
        self._base: list[str] = []
        self._ready = False

    # --- exo lifecycle (one agent + conversation per game) ----------------
    def _ensure_exo(self) -> None:
        if self._ready:
            return
        if not os.path.exists(_EXO_BIN):
            raise RuntimeError(f"exo binary not found at {_EXO_BIN}; build it or set EXO_BIN")
        self._root = tempfile.mkdtemp(prefix="exo-arc3-")
        self._base = [_EXO_BIN, "--root", self._root, "--secret-backend", "file"]
        self._run(self._base + ["secret", "set", "openai", "--env", "OPENAI_API_KEY"])
        self._run(self._base + ["model", "register", _MODEL, "--secret", "openai"])
        self._run(self._base + ["agent", "create", "--slug", "t", "--model", _MODEL,
                                "--harness", _HARNESS, "--sandbox-provider", _SANDBOX, "ExoArc"])
        self._run(self._base + ["conversation", "create", "t", "c"])
        self._ready = True

    def _run(self, argv: list[str], check: bool = True) -> str:
        proc = subprocess.run(
            argv, cwd=_EXO_REPO, env=os.environ.copy(),
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        if check and proc.returncode != 0:
            raise RuntimeError(f"exo step failed (rc={proc.returncode}): {argv[4:]}\n{proc.stdout[-1200:]}")
        return proc.stdout or ""

    def _ask(self, prompt: str) -> str:
        """Send one turn to the persistent conversation; return the final assistant
        text. exo prints turn output as lines like `[hh:mm:ss] assistant: <text>`
        (reasoning shows as `assistant: [reasoning]`); we take the text after the
        last `assistant:` marker — the same stdout-parsing approach the Horizon
        host-agent uses."""
        out = self._run(self._base + ["conversation", "send", "t", "c", "--", prompt], check=False)
        parts = re.split(r"\bassistant:\s*", out)
        tail = parts[-1].strip() if len(parts) > 1 else out
        # strip the timestamp prefix exo prepends to subsequent lines, if any
        return re.sub(r"^\[\d\d:\d\d:\d\d\]\s*", "", tail).strip()

    # --- frame rendering + reply parsing ----------------------------------
    @staticmethod
    def _render_frame(frame: list[list[list[int]]]) -> str:
        if not frame:
            return "(empty)"
        # frame is a stack of layers; the top layer is the current view.
        grid = frame[-1]
        return "\n".join(" ".join(f"{c:2d}" for c in row) for row in grid)

    def _build_prompt(self, latest: FrameData) -> str:
        avail = ", ".join(f"ACTION{n}" for n in latest.available_actions) or "(none listed)"
        return (
            f"Game {latest.game_id} — state {latest.state.name}, "
            f"levels completed {latest.levels_completed}/{latest.win_levels}.\n"
            f"Current grid (top-left is x=0,y=0; rows are y, columns are x):\n"
            f"{self._render_frame(latest.frame)}\n\n"
            f"Available actions this turn: {avail}.\n"
            f"Choose the single next action and reply with ONLY the JSON object."
        )

    @staticmethod
    def _parse_action(text: str) -> tuple[str, Optional[int], Optional[int]]:
        obj = None
        t = re.sub(r"```[a-zA-Z]*", "", text).replace("```", "").strip()
        for cand in (t, t[t.find("{"): t.rfind("}") + 1] if "{" in t and "}" in t else ""):
            if not cand:
                continue
            try:
                obj = json.loads(cand)
                break
            except Exception:
                continue
        if not isinstance(obj, dict):
            # last resort: grab an ACTION token from the text
            m = re.search(r"\b(RESET|ACTION[1-7])\b", t)
            return (m.group(1) if m else "ACTION5"), None, None
        name = str(obj.get("action", "ACTION5")).upper().strip()
        x = obj.get("x")
        y = obj.get("y")
        return name, (int(x) if isinstance(x, (int, float)) else None), (int(y) if isinstance(y, (int, float)) else None)

    # --- arcengine Agent interface ----------------------------------------
    def is_done(self, frames: list[FrameData], latest_frame: FrameData) -> bool:
        return latest_frame.state is GameState.WIN

    def choose_action(self, frames: list[FrameData], latest_frame: FrameData) -> GameAction:
        # NOT_PLAYED (game start) and GAME_OVER require a RESET to (re)start;
        # the API rejects other actions in those states. Mechanical, not a choice.
        if latest_frame.state in (GameState.NOT_PLAYED, GameState.GAME_OVER):
            action = GameAction.RESET
            action.reasoning = "auto-reset to (re)start the game"
            return action

        self._ensure_exo()
        reply = self._ask(self._build_prompt(latest_frame))
        name, x, y = self._parse_action(reply)
        try:
            action = GameAction.from_name(name)
        except Exception:
            action = GameAction.ACTION5  # safe default if the name is unrecognized
        if action.is_complex():
            cx = 0 if x is None else max(0, min(63, x))
            cy = 0 if y is None else max(0, min(63, y))
            action.set_data({"x": cx, "y": cy})
        action.reasoning = {"source": "exo", "model": _MODEL, "raw": reply[-400:]}
        return action

    def cleanup(self, *args: Any, **kwargs: Any) -> Any:
        if self._root and os.environ.get("EXO_KEEP_ROOT") != "1":
            shutil.rmtree(self._root, ignore_errors=True)
        self._root = None
        self._ready = False
        try:
            return super().cleanup(*args, **kwargs)
        except Exception:
            return None
