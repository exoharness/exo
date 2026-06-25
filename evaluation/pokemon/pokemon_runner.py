#!/usr/bin/env python3
"""Drive exo (gpt-5.5 vision) playing Pokémon on a Game Boy via PyBoy.

The loop, each turn:
  1. capture the emulator screen -> write it to the path the harness reads
  2. run one exo turn (the pokemon harness injects the screenshot as an image and
     asks for the next buttons)
  3. parse exo's JSON reply {"buttons": [...]} and press them in PyBoy
  4. advance frames so the game responds, then repeat

One persistent exo conversation runs for the whole session, so exo accumulates
context (what it has seen/done) across turns. Frames are saved so you can make a
GIF/video of exo playing (see README).

You must supply your own legally-obtained ROM (POKEMON_ROM); ROMs are copyrighted
and are not included.
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
from typing import Optional

from pyboy import PyBoy

_EXO_REPO = os.environ.get("EXO_REPO", "/home/worker/exo")
_EXO_BIN = os.environ.get("EXO_BIN", os.path.join(_EXO_REPO, "target", "release", "exo"))
_HARNESS = os.environ.get(
    "EXO_HARNESS",
    os.path.join(_EXO_REPO, "examples", "simple-coding-agent", "harness-pokemon.ts"),
)
_MODEL = os.environ.get("MODEL", "gpt-5.5")
# Must match SCREEN_PATH in harness-pokemon.ts.
_SCREEN = "/tmp/exo-pokemon/screen.png"
_BUTTONS = {"up", "down", "left", "right", "a", "b", "start", "select"}


def _run(argv: list[str], check: bool = True) -> str:
    proc = subprocess.run(
        argv, cwd=_EXO_REPO, env=os.environ.copy(),
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )
    if check and proc.returncode != 0:
        raise RuntimeError(f"exo step failed (rc={proc.returncode}): {argv[4:]}\n{proc.stdout[-1200:]}")
    return proc.stdout or ""


def _last_assistant(root: str) -> str:
    dirs = glob.glob(os.path.join(root, "**", "conversations", "*", "events"), recursive=True)
    if not dirs:
        return ""
    ev_dir = max(dirs, key=lambda d: max((os.path.getmtime(f) for f in glob.glob(os.path.join(d, "*.json"))), default=0.0))
    msgs = []
    for path in glob.glob(os.path.join(ev_dir, "*.json")):
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
            items = m.get("content") if isinstance(m.get("content"), list) else [m.get("content")]
            parts = [it["text"] for it in items if isinstance(it, dict) and it.get("type") == "text" and it.get("text")]
            if parts:
                text = "\n".join(parts)
    return text


def parse_buttons(text: str) -> list[str]:
    """Extract a button list from exo's JSON reply; tolerate prose/fences."""
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
    raw = obj.get("buttons") if isinstance(obj, dict) else None
    if not isinstance(raw, list):
        # last resort: scan for button words in order
        raw = re.findall(r"\b(up|down|left|right|a|b|start|select)\b", t.lower())
    out = [str(b).lower().strip() for b in raw]
    return [b for b in out if b in _BUTTONS][:3]


def main() -> None:
    ap = argparse.ArgumentParser(description="exo plays Pokémon via PyBoy.")
    ap.add_argument("--rom", default=os.environ.get("POKEMON_ROM"), help="path to a .gb/.gbc ROM you own")
    ap.add_argument("--state", default=os.environ.get("POKEMON_STATE"), help="optional PyBoy save state to start from")
    ap.add_argument("--steps", type=int, default=int(os.environ.get("POKEMON_STEPS", "40")))
    ap.add_argument("--press-frames", type=int, default=8, help="frames a button is held")
    ap.add_argument("--settle-frames", type=int, default=24, help="frames to advance after a press")
    ap.add_argument("--boot-frames", type=int, default=900, help="frames to advance before turn 1 (skip boot logos)")
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "runs", "latest"))
    args = ap.parse_args()

    if not args.rom or not os.path.exists(args.rom):
        raise SystemExit(f"ROM not found: {args.rom!r}. Set POKEMON_ROM to a ROM you own.")

    os.makedirs(os.path.dirname(_SCREEN), exist_ok=True)
    frames_dir = os.path.join(args.out, "frames")
    os.makedirs(frames_dir, exist_ok=True)

    pyboy = PyBoy(args.rom, window="null")
    pyboy.set_emulation_speed(0)  # unbounded; we control pacing
    try:
        if args.state and os.path.exists(args.state):
            with open(args.state, "rb") as f:
                pyboy.load_state(f)
        else:
            for _ in range(args.boot_frames):
                pyboy.tick()

        root = tempfile.mkdtemp(prefix="exo-pokemon-")
        base = [_EXO_BIN, "--root", root, "--secret-backend", "file"]
        _run(base + ["secret", "set", "openai", "--env", "OPENAI_API_KEY"])
        _run(base + ["model", "register", _MODEL, "--secret", "openai"])
        _run(base + ["agent", "create", "--slug", "t", "--model", _MODEL,
                     "--harness", _HARNESS, "--sandbox-provider", "docker", "Pokemon"])
        _run(base + ["conversation", "create", "t", "c"])

        log = []
        for step in range(args.steps):
            frame = pyboy.screen.image.convert("RGB")
            frame.save(_SCREEN)                                   # what the harness reads
            frame.resize((480, 432)).save(os.path.join(frames_dir, f"{step:04d}.png"))
            _run(base + ["conversation", "send", "t", "c", "--",
                         f"Turn {step}: here is the current screen. Choose your next button(s)."], check=False)
            reply = _last_assistant(root)
            buttons = parse_buttons(reply)
            print(f"[{step:03d}] buttons={buttons}  reply={reply[:120]!r}")
            log.append({"step": step, "buttons": buttons, "reply": reply[:500]})
            for b in buttons:
                pyboy.button(b, args.press_frames)
                for _ in range(args.press_frames + args.settle_frames):
                    pyboy.tick()
            if not buttons:
                for _ in range(args.settle_frames):
                    pyboy.tick()

        # final frame + log
        pyboy.screen.image.convert("RGB").resize((480, 432)).save(os.path.join(frames_dir, f"{args.steps:04d}.png"))
        json.dump({"rom": os.path.basename(args.rom), "model": _MODEL, "steps": args.steps, "log": log},
                  open(os.path.join(args.out, "session.json"), "w"), indent=2)
        print(f"\nDone. {args.steps} turns. Frames + session.json in {args.out}")
        if os.environ.get("EXO_KEEP_ROOT") != "1":
            shutil.rmtree(root, ignore_errors=True)
    finally:
        pyboy.stop()


if __name__ == "__main__":
    main()
