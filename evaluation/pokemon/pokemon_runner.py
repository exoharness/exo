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


def read_memory(root: str) -> list:
    """Richest durable-memory store (most entries) from the agent's artifacts."""
    best: list = []
    for p in glob.glob(os.path.join(root, "**", "artifacts", "**", "*.bin"), recursive=True):
        try:
            d = json.load(open(p))
        except Exception:
            continue
        if isinstance(d, dict) and isinstance(d.get("entries"), list) and len(d["entries"]) > len(best):
            best = d["entries"]
    return best


def main() -> None:
    ap = argparse.ArgumentParser(description="exo plays Pokémon via PyBoy.")
    ap.add_argument("--rom", default=os.environ.get("POKEMON_ROM"), help="path to a .gb/.gbc ROM you own")
    ap.add_argument("--state", default=os.environ.get("POKEMON_STATE"), help="optional PyBoy save state to start from")
    ap.add_argument("--save-state", default=os.environ.get("POKEMON_SAVE_STATE"), help="write a PyBoy save state at the end (e.g. to skip the intro next time)")
    ap.add_argument("--exo-root", default=os.environ.get("POKEMON_EXO_ROOT"), help="reuse an existing exo --root (continuation: keeps the agent + durable memory)")
    ap.add_argument("--steps", type=int, default=int(os.environ.get("POKEMON_STEPS", "40")))
    ap.add_argument("--press-frames", type=int, default=8, help="frames a button is held")
    ap.add_argument("--settle-frames", type=int, default=24, help="frames to advance after a press")
    ap.add_argument("--boot-frames", type=int, default=900, help="frames to advance before turn 1 (skip boot logos)")
    ap.add_argument("--conv-reset-every", type=int, default=int(os.environ.get("POKEMON_CONV_RESET", "0")),
                    help="start a fresh conversation every N turns (durable memory carries continuity; bounds context/latency over long runs). 0=never")
    ap.add_argument("--memory-snapshot-every", type=int, default=int(os.environ.get("POKEMON_MEM_SNAPSHOT", "0")),
                    help="dump the durable-memory store to <out>/memory/turn_NNNN.json every N turns. 0=off")
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

        # Continuation mode: reuse an existing exo root so the agent + its durable
        # memory carry across runs (e.g. extending a finished run). Otherwise make
        # a fresh agent.
        if args.exo_root and os.path.isdir(args.exo_root):
            root = args.exo_root
            base = [_EXO_BIN, "--root", root, "--secret-backend", "file"]
            print(f"continuing with existing exo agent/memory at {root}")
        else:
            root = tempfile.mkdtemp(prefix="exo-pokemon-")
            base = [_EXO_BIN, "--root", root, "--secret-backend", "file"]
            _run(base + ["secret", "set", "openai", "--env", "OPENAI_API_KEY"])
            _run(base + ["model", "register", _MODEL, "--secret", "openai"])
            _run(base + ["agent", "create", "--slug", "t", "--model", _MODEL,
                         "--harness", _HARNESS, "--sandbox-provider", "docker", "Pokemon"])
        conv_n = 0
        conv = f"c{conv_n}"
        _run(base + ["conversation", "create", "t", conv])
        mem_dir = os.path.join(args.out, "memory")
        if args.memory_snapshot_every:
            os.makedirs(mem_dir, exist_ok=True)

        log = []
        for step in range(args.steps):
            # Roll the conversation periodically: chat history is wiped (bounding
            # context + latency), but the agent + its durable memory persist, so
            # memory becomes the thing that carries the game forward.
            if args.conv_reset_every and step > 0 and step % args.conv_reset_every == 0:
                conv_n += 1
                conv = f"c{conv_n}"
                _run(base + ["conversation", "create", "t", conv])
            frame = pyboy.screen.image.convert("RGB")
            frame.save(_SCREEN)                                   # what the harness reads
            frame.resize((480, 432)).save(os.path.join(frames_dir, f"{step:04d}.png"))
            _run(base + ["conversation", "send", "t", conv, "--",
                         f"Turn {step}: here is the current screen. Choose your next button(s)."], check=False)
            reply = _last_assistant(root)
            buttons = parse_buttons(reply)
            print(f"[{step:03d}] conv={conv} buttons={buttons}  reply={reply[:110]!r}", flush=True)
            log.append({"step": step, "conv": conv, "buttons": buttons, "reply": reply[:500]})
            # live state for the web view (live_server.py serves it)
            try:
                reasoning = (re.search(r'"reasoning"\s*:\s*"([^"]*)"', reply) or [None, ""])[1]
                json.dump({"turn": step, "total": args.steps, "conv": conv, "buttons": buttons,
                           "reasoning": reasoning, "memory": read_memory(root)},
                          open(os.path.join(os.path.dirname(_SCREEN), "state.json"), "w"))
            except Exception:
                pass
            if args.memory_snapshot_every and step % args.memory_snapshot_every == 0:
                json.dump(read_memory(root), open(os.path.join(mem_dir, f"turn_{step:04d}.json"), "w"), indent=2)
            for b in buttons:
                pyboy.button(b, args.press_frames)
                for _ in range(args.press_frames + args.settle_frames):
                    pyboy.tick()
            if not buttons:
                for _ in range(args.settle_frames):
                    pyboy.tick()

        # final frame + log
        pyboy.screen.image.convert("RGB").resize((480, 432)).save(os.path.join(frames_dir, f"{args.steps:04d}.png"))
        if args.save_state:
            with open(args.save_state, "wb") as f:
                pyboy.save_state(f)
            print(f"saved PyBoy state -> {args.save_state}")
        final_memory = read_memory(root)
        if args.memory_snapshot_every:
            json.dump(final_memory, open(os.path.join(mem_dir, f"turn_{args.steps:04d}.json"), "w"), indent=2)
        json.dump({"rom": os.path.basename(args.rom), "model": _MODEL, "steps": args.steps,
                   "conv_reset_every": args.conv_reset_every, "final_memory": final_memory, "log": log},
                  open(os.path.join(args.out, "session.json"), "w"), indent=2)
        print(f"\nDone. {args.steps} turns. {len(final_memory)} memory entries. Output in {args.out}")
        if os.environ.get("EXO_KEEP_ROOT") != "1":
            shutil.rmtree(root, ignore_errors=True)
    finally:
        pyboy.stop()


if __name__ == "__main__":
    main()
