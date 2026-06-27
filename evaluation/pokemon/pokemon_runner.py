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
_PREV_SCREEN = "/tmp/exo-pokemon/prev_screen.png"  # the frame BEFORE the last action
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
    """Extract a button list from exo's JSON reply; tolerate chain-of-thought prose
    before the JSON (we now ask the agent to REASON first, then end with the JSON)."""
    obj = None
    t = re.sub(r"```[a-zA-Z]*", "", text).replace("```", "").strip()
    # Prefer the LAST {...} object (the agent reasons first, JSON comes last; its
    # reasoning prose may itself contain braces, so the final object is the answer).
    candidates = [t]
    if "{" in t and "}" in t:
        candidates.append(t[t.rfind("{"): t.rfind("}") + 1])  # last object
        candidates.append(t[t.find("{"): t.rfind("}") + 1])   # widest span (fallback)
    for cand in candidates:
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


# --- game-state / progress (read-only RAM) -------------------------------
# Pokémon Red US WRAM addresses. Used only to MEASURE progress (objective
# metric for comparing prompts) — the emulator state is never written.
_GAME_PATH = os.path.join(os.path.dirname(_SCREEN), "game.json")  # current state, for tools
_RAM = {"map": 0xD35E, "x": 0xD362, "y": 0xD361, "badges": 0xD356,
        "party": 0xD163, "lvl1": 0xD18C, "money0": 0xD347, "money1": 0xD348, "money2": 0xD349,
        "battle": 0xD057}  # wIsInBattle: 0=overworld, 1=wild, 2=trainer


def read_game_state(pyboy) -> dict:
    m = pyboy.memory
    money = m[_RAM["money0"]] * 10000 + m[_RAM["money1"]] * 100 + m[_RAM["money2"]]  # BCD-ish
    return {"map": m[_RAM["map"]], "x": m[_RAM["x"]], "y": m[_RAM["y"]],
            "badges": bin(m[_RAM["badges"]]).count("1"), "party": m[_RAM["party"]],
            "level1": m[_RAM["lvl1"]], "money": money, "in_battle": int(m[_RAM["battle"]] != 0)}


_DELTAS = {"up": (0, -1), "down": (0, 1), "left": (-1, 0), "right": (1, 0)}


def build_minimap(cur_map, cur_x, cur_y, tiles_visited, blocked_dirs, cap=28):
    """Compact ASCII map of the CURRENT map from what we've explored, plus the
    frontier (unexplored exits). Turns blind local hill-climbing into informed
    exploration. '@'=you, '.'=visited, '?'=known-reachable-but-unexplored, ' '=unknown."""
    pts = [(x, y) for (mp, x, y) in tiles_visited if mp == cur_map]
    if not pts:
        return None, []
    visited = set(pts)
    # frontier: visited tile + a direction not confirmed-blocked leading off-visited
    frontier = []  # (dist, x, y, dir)
    fset = set()
    for (x, y) in pts:
        blk = blocked_dirs.get((cur_map, x, y), set())
        for d, (dx, dy) in _DELTAS.items():
            nb = (x + dx, y + dy)
            if d not in blk and nb not in visited:
                frontier.append((abs(x - cur_x) + abs(y - cur_y), x, y, d))
                fset.add(nb)
    frontier.sort()
    minx = min(x for x, y in pts) - 1
    maxx = max(x for x, y in pts) + 1
    miny = min(y for x, y in pts) - 1
    maxy = max(y for x, y in pts) + 1
    # window around current position if the explored area is large
    if maxx - minx > cap:
        minx, maxx = cur_x - cap // 2, cur_x + cap // 2
    if maxy - miny > cap:
        miny, maxy = cur_y - cap // 2, cur_y + cap // 2
    rows = []
    for y in range(miny, maxy + 1):  # y increases downward (matches screen: up=top)
        row = []
        for x in range(minx, maxx + 1):
            if (x, y) == (cur_x, cur_y):
                row.append("@")
            elif (x, y) in visited:
                row.append(".")
            elif (x, y) in fset:
                row.append("?")
            else:
                row.append(" ")
        rows.append("".join(row))
    grid = "\n".join(rows)
    front = [f"({x},{y})→{d}" for (_, x, y, d) in frontier[:6]]
    return grid, front


# --- spend tracking ------------------------------------------------------
_COST_PATH = os.path.join(os.path.dirname(_SCREEN), "cost.json")  # harness reads this


def read_new_cost(root: str, seen: set, acc: dict) -> float:
    """Incremental spend: parse only event files we haven't seen, summing
    data.usage.cost_usd (+ tokens) that responses.ts attaches per model call.
    Mutates `seen` and `acc`; returns the cost added this call. Multiple model
    calls per turn (tool use, retries) are all captured. Glob+diff over the
    whole store is ~0.04s even at 15k files, so it's cheap to call every turn."""
    new_cost = 0.0
    for f in glob.glob(os.path.join(root, "**", "conversations", "*", "events", "*.json"), recursive=True):
        if f in seen:
            continue
        seen.add(f)
        try:
            d = json.load(open(f))
        except Exception:
            continue
        u = (d.get("data") or {}).get("usage")
        if not u or "cost_usd" not in u:
            continue
        new_cost += u.get("cost_usd") or 0.0
        acc["cost"] += u.get("cost_usd") or 0.0
        acc["prompt"] += u.get("prompt_tokens", 0)
        acc["completion"] += u.get("completion_tokens", 0)
        acc["cached"] += u.get("prompt_cached_tokens", 0)
        acc["calls"] += 1
    return new_cost


# --- self-improvement helpers --------------------------------------------
_SELFIMPROVE_SRC = os.path.join(_EXO_REPO, "examples", "simple-coding-agent", "harness-pokemon-selfimprove.ts")
_SELF_HARNESS = os.path.join(_EXO_REPO, "examples", "simple-coding-agent", "harness-pokemon-self.ts")
_HARNESS_MOUNT_HOST = os.path.join(_EXO_REPO, "examples", "simple-coding-agent")
# Exoclaw-style: tools the agent builds with install_agent_tool live as modules
# here (relative to the harness process CWD = the repo), loaded fresh each turn.
_AGENT_TOOLS_DIR = os.path.join(_EXO_REPO, ".exo", "agent-tools")


def quarantine_agent_tool(name: str) -> list:
    """Move an installed agent-tool module out of the load directory so it stops
    loading. Used when a tool's JSON schema is rejected by the API: such a tool
    imports fine but 400s every request, and (unlike the old inline tools) it
    lives in a module file, so rolling back the harness can't remove it. The
    function name in the API error matches the installed module name."""
    moved = []
    qdir = os.path.join(_AGENT_TOOLS_DIR, ".quarantine")
    for fn in (f"{name}.ts", f"{name}.source.ts"):
        src = os.path.join(_AGENT_TOOLS_DIR, fn)
        if os.path.exists(src):
            try:
                os.makedirs(qdir, exist_ok=True)
                shutil.move(src, os.path.join(qdir, fn))
                moved.append(fn)
            except OSError:
                pass
    return moved


def validate_harness(path: str) -> bool:
    """True if the harness module loads (catches the agent breaking its own policy)."""
    proc = subprocess.run(
        ["node", "--import", "tsx", "-e",
         f"import({json.dumps(path)}).then(()=>process.exit(0)).catch(()=>process.exit(1))"],
        cwd=_EXO_REPO, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=120,
    )
    return proc.returncode == 0


def read_created_tools() -> list:
    """Tools the agent has built, for the live view. Exoclaw-style: tools are
    installed as modules in .exo/agent-tools/<name>.ts (not inline in the
    harness), so list each module and pull a description from its source."""
    try:
        names = sorted(
            f[:-3] for f in os.listdir(_AGENT_TOOLS_DIR)
            if f.endswith(".ts") and not f.endswith(".source.ts")
        )
    except OSError:
        return []
    out = []
    for name in names:
        desc = ""
        for cand in (f"{name}.source.ts", f"{name}.ts"):
            try:
                src = open(os.path.join(_AGENT_TOOLS_DIR, cand)).read()
            except OSError:
                continue
            m = re.search(r"""description:\s*["'`]([^"'`]+)""", src)
            if m:
                desc = m.group(1)
                break
        out.append({"name": name, "source": desc})
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description="exo plays Pokémon via PyBoy.")
    ap.add_argument("--rom", default=os.environ.get("POKEMON_ROM"), help="path to a .gb/.gbc ROM you own")
    ap.add_argument("--state", default=os.environ.get("POKEMON_STATE"), help="optional PyBoy save state to start from")
    ap.add_argument("--save-state", default=os.environ.get("POKEMON_SAVE_STATE"), help="write a PyBoy save state at the end (e.g. to skip the intro next time)")
    ap.add_argument("--exo-root", default=os.environ.get("POKEMON_EXO_ROOT"), help="reuse an existing exo --root (continuation: keeps the agent + durable memory)")
    ap.add_argument("--self-improve", action="store_true", default=bool(os.environ.get("POKEMON_SELF_IMPROVE")),
                    help="let the agent create tools + rewrite its OWN harness (mounted rw); validate + roll back broken self-edits")
    ap.add_argument("--steps", type=int, default=int(os.environ.get("POKEMON_STEPS", "40")))
    ap.add_argument("--press-frames", type=int, default=8, help="frames a button is held")
    ap.add_argument("--settle-frames", type=int, default=24, help="frames to advance after a press")
    ap.add_argument("--boot-frames", type=int, default=900, help="frames to advance before turn 1 (skip boot logos)")
    ap.add_argument("--conv-reset-every", type=int, default=int(os.environ.get("POKEMON_CONV_RESET", "0")),
                    help="start a fresh conversation every N turns (durable memory carries continuity; bounds context/latency over long runs). 0=never")
    ap.add_argument("--memory-snapshot-every", type=int, default=int(os.environ.get("POKEMON_MEM_SNAPSHOT", "0")),
                    help="dump the durable-memory store to <out>/memory/turn_NNNN.json every N turns. 0=off")
    ap.add_argument("--reflect-every", type=int, default=int(os.environ.get("POKEMON_REFLECT", "0")),
                    help="every N turns, run a REFLECTION turn (no button press): the agent must "
                         "step back and make one concrete self-improvement. 0=off")
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "runs", "latest"))
    args = ap.parse_args()

    if not args.rom or not os.path.exists(args.rom):
        raise SystemExit(f"ROM not found: {args.rom!r}. Set POKEMON_ROM to a ROM you own.")

    os.makedirs(os.path.dirname(_SCREEN), exist_ok=True)
    try:  # drop a stale cost.json so turn 0 doesn't read a prior run's spend
        os.remove(_COST_PATH)
    except OSError:
        pass
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

        # Self-improve: the agent edits its OWN harness. Use a gitignored COPY so
        # the repo's committed harness stays clean, and keep a known-good backup.
        harness = _HARNESS
        if args.self_improve:
            # A prior run's docker sandbox (root) may have left the agent-edited copy
            # root-owned; copyfile would then hit PermissionError. The dir is
            # worker-owned, so removing the stale file first always works.
            for stale in (_SELF_HARNESS, _SELF_HARNESS + ".good"):
                try:
                    os.remove(stale)
                except OSError:
                    pass
            shutil.copyfile(_SELFIMPROVE_SRC, _SELF_HARNESS)
            shutil.copyfile(_SELF_HARNESS, _SELF_HARNESS + ".good")
            harness = _SELF_HARNESS
            print(f"self-improve: agent will edit {_SELF_HARNESS}")

        # Continuation mode: reuse an existing exo root so the agent + its durable
        # memory carry across runs. Otherwise make a fresh agent.
        if args.exo_root and os.path.isdir(args.exo_root):
            root = args.exo_root
            base = [_EXO_BIN, "--root", root, "--secret-backend", "file"]
            print(f"continuing with existing exo agent/memory at {root}")
        else:
            root = tempfile.mkdtemp(prefix="exo-pokemon-")
            base = [_EXO_BIN, "--root", root, "--secret-backend", "file"]
            # Fresh agent → fresh tools. The agent-tools dir lives in the repo (not
            # the per-run root), so a fresh run would otherwise inherit tools built
            # by previous runs — including any that were quarantined for bad schemas.
            if args.self_improve:
                shutil.rmtree(_AGENT_TOOLS_DIR, ignore_errors=True)
            _run(base + ["secret", "set", "openai", "--env", "OPENAI_API_KEY"])
            _run(base + ["model", "register", _MODEL, "--secret", "openai"])
            _run(base + ["agent", "create", "--slug", "t", "--model", _MODEL,
                         "--harness", harness, "--sandbox-provider", "docker", "Pokemon"])
        conv_n = 0
        conv = f"c{conv_n}"
        _run(base + ["conversation", "create", "t", conv])
        if args.self_improve:
            # Mount the harness dir read-write so the agent's shell can edit its
            # own policy; edits propagate to the host and reload next turn.
            _run(base + ["conversation", "mount", "add", "t", conv,
                         _HARNESS_MOUNT_HOST, "/workspace/agent", "--rw"], check=False)
        mem_dir = os.path.join(args.out, "memory")
        if args.memory_snapshot_every:
            os.makedirs(mem_dir, exist_ok=True)
        self_edits = 0
        last_harness_hash = None
        mem_dir = os.path.join(args.out, "memory")
        if args.memory_snapshot_every:
            os.makedirs(mem_dir, exist_ok=True)

        # spend tracking: cumulative cost + per-turn cost series for the live graph
        cost_seen: set = set()
        cost_acc = {"cost": 0.0, "prompt": 0, "completion": 0, "cached": 0, "calls": 0}
        cost_series: list = []  # [turn, cumulative_usd, this_turn_usd]

        # progress tracking (objective game metric)
        maps_visited: set = set()
        tiles_visited: set = set()
        max_badges = 0
        progress_series: list = []  # [turn, n_maps, badges, level1]
        # stuck detection: ground-truth "did the last move actually move me"
        prev_pos = None
        stuck_count = 0
        last_buttons: list = []
        blocked_dirs: dict = {}  # (map,x,y) -> set of directions confirmed blocked
        prev_frame = None  # the screenshot shown last turn, to diff against
        # self-improvement events (for the live spend chart): when the agent builds a
        # tool, edits its policy, or banks a new memory, mark the turn it happened.
        improvement_events: list = []  # [{turn, kind}]
        prev_tool_n = None
        prev_edit_n = 0
        prev_mem_n = None

        log = []
        for step in range(args.steps):
            # Roll the conversation periodically: chat history is wiped (bounding
            # context + latency), but the agent + its durable memory persist, so
            # memory becomes the thing that carries the game forward.
            if args.conv_reset_every and step > 0 and step % args.conv_reset_every == 0:
                old_conv = conv
                conv_n += 1
                conv = f"c{conv_n}"
                _run(base + ["conversation", "create", "t", conv])
                if args.self_improve:  # mount is per-conversation; re-attach it
                    _run(base + ["conversation", "mount", "add", "t", conv,
                                 _HARNESS_MOUNT_HOST, "/workspace/agent", "--rw"], check=False)
                # Free old conversation sandboxes so they don't pile up over a long
                # run (uncleaned sandboxes OOM'd the box once). `conversation delete`
                # leaves the docker container RUNNING, so force-remove ALL exo
                # containers here — the new conversation recreates its sandbox on its
                # next send. This keeps the count at ~1 instead of growing ~1/reset.
                _run(base + ["conversation", "delete", "t", old_conv], check=False)
                subprocess.run("docker ps -aq -f name=exo- | xargs -r docker rm -f",
                               shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            # Self-edit guard: if the agent changed its single harness file last turn,
            # import-check it. Roll back to the last good version if it won't even load.
            # NOTE: we do NOT promote to .good here — a tool with valid TS but a bad
            # JSON schema imports fine yet 400s the API, so .good is only advanced
            # AFTER a turn actually runs cleanly (see the post-send promotion below).
            if args.self_improve:
                try:
                    hh = hash(open(_SELF_HARNESS).read())
                    if hh != last_harness_hash:
                        if validate_harness(_SELF_HARNESS):
                            if last_harness_hash is not None:
                                self_edits += 1
                                print(f"  [self-edit] harness changed + imports OK at turn {step}", flush=True)
                        else:
                            shutil.copyfile(_SELF_HARNESS + ".good", _SELF_HARNESS)
                            print(f"  [self-edit] broken import at turn {step} -> ROLLED BACK", flush=True)
                        last_harness_hash = hash(open(_SELF_HARNESS).read())
                except Exception:
                    pass
            frame = pyboy.screen.image.convert("RGB")
            # Save the frame shown last turn so the harness can present it next to the
            # current one — the agent compares them itself (no computed diff). Generic
            # observation, not game knowledge.
            if prev_frame is not None:
                prev_frame.save(_PREV_SCREEN)
            else:
                try:
                    os.remove(_PREV_SCREEN)  # clear any stale prev frame at run start
                except OSError:
                    pass
            frame.save(_SCREEN)                                   # what the harness reads
            frame.resize((480, 432)).save(os.path.join(frames_dir, f"{step:04d}.png"))
            # read objective game state for THIS frame (matches the screenshot shown).
            # This reflects the RESULT of last turn's buttons, so we can tell the
            # agent — from ground truth — whether its last move actually moved it.
            gs = read_game_state(pyboy)
            cur_pos = (gs["map"], gs["x"], gs["y"])
            dirs_pressed = [b for b in last_buttons if b in ("up", "down", "left", "right")]
            moved_last = prev_pos is not None and cur_pos != prev_pos
            # Only trust position-based wall/stuck detection in the OVERWORLD. During a
            # battle/dialogue the position freezes for non-wall reasons, which would
            # falsely mark every direction as a wall (poisoning the map).
            if prev_pos is not None and dirs_pressed and not gs.get("in_battle"):
                stuck_count = 0 if moved_last else stuck_count + 1
                # On a no-move turn, EVERY direction pressed was blocked from prev_pos
                # (== cur_pos). This builds a reliable local map of walls per tile.
                if not moved_last:
                    blocked_dirs.setdefault(prev_pos, set()).update(dirs_pressed)
            elif gs.get("in_battle"):
                stuck_count = 0  # not stuck — just in a battle
            maps_visited.add(gs["map"])
            tiles_visited.add(cur_pos)
            max_badges = max(max_badges, gs["badges"])
            here_blocked = sorted(blocked_dirs.get(cur_pos, set()))
            gs["maps_visited"] = len(maps_visited)
            gs["tiles_visited"] = len(tiles_visited)
            gs["moved_last"] = moved_last
            gs["stuck"] = stuck_count
            gs["last_buttons"] = last_buttons
            gs["blocked_here"] = here_blocked
            gs["untried_here"] = [d for d in ("up", "down", "left", "right") if d not in here_blocked]
            grid, frontier = build_minimap(gs["map"], gs["x"], gs["y"], tiles_visited, blocked_dirs)
            gs["minimap"] = grid
            gs["frontier"] = frontier
            try:  # expose to agent tools that want structured position data
                json.dump(gs, open(_GAME_PATH, "w"))
            except Exception:
                pass
            # Reflection turn: periodically force the agent to step back and make ONE
            # concrete self-improvement instead of pressing a button. Separates
            # "improve" from "play" so self-improvement actually happens.
            is_reflect = bool(args.reflect_every) and step > 0 and step % args.reflect_every == 0
            if is_reflect:
                msg = (f"🔧 REFLECTION TURN {step} — DO NOT press a game button this turn. "
                       f"Step back and review how it's going (progress: {len(maps_visited)} maps, "
                       f"badges {max_badges}, {len(tiles_visited)} tiles; are you stuck or looping?). "
                       f"Take exactly ONE concrete self-improvement action NOW using your tools: "
                       f"(a) consolidate/clean your durable memory into a few sharp facts incl. routes that worked, "
                       f"OR (b) install/refine a tool that would help you play better (e.g. one that reads "
                       f"/tmp/exo-pokemon/game.json to track routes/visited tiles), "
                       f"OR (c) self-edit your policy harness to bake in a strategy you've learned. "
                       f"Actually call the tool/shell — don't just describe it. Then reply {{\"buttons\":[],\"reasoning\":\"what I improved\"}}.")
            else:
                msg = (f"Turn {step}: here is the current screen. Decide your next action — "
                       f"move (buttons) OR, if it's worth it, take a self-improvement turn (empty buttons).")
            # The harness version that will load for THIS turn's send (the agent may
            # edit it again mid-turn; we promote THIS one to .good only if it runs clean).
            harness_used = open(_SELF_HARNESS).read() if args.self_improve else None
            send = base + ["conversation", "send", "t", conv, "--", msg]
            out = _run(send, check=False)
            # Self-heal: a tool with valid TS but a bad JSON schema imports fine yet
            # 400s the WHOLE request. Exoclaw-style tools live as modules in
            # .exo/agent-tools/, so the offending one would reload and re-400 every
            # turn — rolling back the harness can't remove it. QUARANTINE the named
            # module so it stops loading, then retry. (Also roll back the harness in
            # case a self-edit was involved.) Without this, one bad tool bricks the run.
            rolled_back = False
            for _ in range(2):
                m = re.search(r"Invalid schema for function '([^']+)'", out)
                if not (args.self_improve and m):
                    break
                bad = m.group(1)
                quarantined = quarantine_agent_tool(bad)
                shutil.copyfile(_SELF_HARNESS + ".good", _SELF_HARNESS)
                last_harness_hash = hash(open(_SELF_HARNESS).read())
                rolled_back = True
                print(f"  [self-heal] bad tool schema ('{bad}') -> QUARANTINED {quarantined or '(no module file)'} "
                      f"+ rolled back harness + retried", flush=True)
                out = _run(send, check=False)
            # Promote to .good only after a turn that ran cleanly on harness_used.
            if args.self_improve and not rolled_back and harness_used is not None \
                    and not re.search(r"Invalid schema for function", out):
                try:
                    with open(_SELF_HARNESS + ".good", "w") as gf:
                        gf.write(harness_used)
                except Exception:
                    pass
            reply = _last_assistant(root)
            buttons = [] if is_reflect else parse_buttons(reply)  # reflection = no game input
            # spend this turn (all model calls since the last turn) + cumulative
            turn_cost = read_new_cost(root, cost_seen, cost_acc)
            cost_series.append([step, round(cost_acc["cost"], 4), round(turn_cost, 4)])
            recent = [c[2] for c in cost_series[-20:]]
            prev = [c[2] for c in cost_series[-40:-20]]
            recent_avg = sum(recent) / len(recent) if recent else 0.0
            prev_avg = sum(prev) / len(prev) if prev else 0.0
            try:  # harness reads this to see (and aim to reduce) its own spend
                json.dump({"total_usd": round(cost_acc["cost"], 4), "turn_usd": round(turn_cost, 4),
                           "recent_avg_usd": round(recent_avg, 4), "prev_avg_usd": round(prev_avg, 4),
                           "calls": cost_acc["calls"], "turn": step}, open(_COST_PATH, "w"))
            except Exception:
                pass
            progress_series.append([step, len(maps_visited), max_badges, gs["level1"]])
            tag = "REFLECT " if is_reflect else ""
            print(f"[{step:03d}] {tag}conv={conv} buttons={buttons} +${turn_cost:.3f} (tot ${cost_acc['cost']:.2f}) "
                  f"map={gs['map']} pos=({gs['x']},{gs['y']}) maps={len(maps_visited)} badges={max_badges}  reply={reply[:70]!r}", flush=True)
            log.append({"step": step, "conv": conv, "buttons": buttons, "turn_cost": round(turn_cost, 4),
                        "game": gs, "reply": reply[:500]})
            # live state for the web view (live_server.py serves it)
            try:
                reasoning = (re.search(r'"reasoning"\s*:\s*"([^"]*)"', reply) or [None, ""])[1]
                state = {"turn": step, "total": args.steps, "conv": conv, "buttons": buttons,
                         "reasoning": reasoning, "memory": read_memory(root),
                         "cost_total": round(cost_acc["cost"], 4), "cost_recent_avg": round(recent_avg, 4),
                         "cost_prev_avg": round(prev_avg, 4), "cost_series": cost_series,
                         "game": gs, "maps_visited": len(maps_visited), "badges": max_badges,
                         "progress_series": progress_series}
                if args.self_improve:
                    state["self_edits"] = self_edits
                    state["tools"] = read_created_tools()
                    try:
                        state["harness"] = open(_SELF_HARNESS).read()
                    except Exception:
                        pass
                # Detect self-improvement moments (increase in tools / policy edits /
                # memory) and record the turn, so the live chart can mark them.
                tool_n = len(state.get("tools", []))
                mem_n = len(state.get("memory", []))
                if prev_tool_n is None:  # baseline at turn 0 (don't mark carried-in state)
                    prev_tool_n, prev_mem_n = tool_n, mem_n
                else:
                    if tool_n > prev_tool_n:
                        improvement_events.append({"turn": step, "kind": "tool"})
                    if self_edits > prev_edit_n:
                        improvement_events.append({"turn": step, "kind": "policy"})
                    if mem_n > prev_mem_n:
                        improvement_events.append({"turn": step, "kind": "memory"})
                    prev_tool_n, prev_edit_n, prev_mem_n = tool_n, self_edits, mem_n
                state["improvement_events"] = improvement_events
                json.dump(state, open(os.path.join(os.path.dirname(_SCREEN), "state.json"), "w"))
            except Exception:
                pass
            if args.memory_snapshot_every and step % args.memory_snapshot_every == 0:
                json.dump(read_memory(root), open(os.path.join(mem_dir, f"turn_{step:04d}.json"), "w"), indent=2)
            prev_pos = cur_pos       # for next turn's moved/stuck comparison
            last_buttons = buttons
            prev_frame = frame       # for next turn's screen diff
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
                   "conv_reset_every": args.conv_reset_every, "final_memory": final_memory,
                   "cost_total": round(cost_acc["cost"], 4), "cost_tokens": cost_acc,
                   "cost_series": cost_series, "improvement_events": improvement_events, "log": log,
                   "progress": {"maps_visited": sorted(maps_visited), "n_maps": len(maps_visited),
                                "max_badges": max_badges, "tiles_visited": len(tiles_visited),
                                "final_game": gs if 'gs' in dir() else None},
                   "progress_series": progress_series},
                  open(os.path.join(args.out, "session.json"), "w"), indent=2)
        print(f"\nDone. {args.steps} turns. {len(final_memory)} memory entries. "
              f"Spent ${cost_acc['cost']:.2f}. Maps={len(maps_visited)} badges={max_badges}. Output in {args.out}")
        if os.environ.get("EXO_KEEP_ROOT") != "1":
            shutil.rmtree(root, ignore_errors=True)
    finally:
        pyboy.stop()
        # Reap this run's sandboxes so they can't pile up across runs (OOM guard).
        subprocess.run("docker ps -aq -f name=exo- -f status=exited | xargs -r docker rm",
                       shell=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    main()
