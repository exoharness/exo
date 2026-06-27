#!/usr/bin/env python3
"""Summarize a Pokémon run for the experiment table. Usage: analyze_run.py runs/exp1_v3"""
import json, sys, os, glob

out = sys.argv[1]
s = json.load(open(os.path.join(out, "session.json")))
log = s.get("log", [])
prog = s.get("progress", {})

# movement: fraction of move-turns where position actually changed
moves = changed = 0
prev = None
for e in log:
    g = e.get("game") or {}
    pos = (g.get("map"), g.get("x"), g.get("y"))
    btns = e.get("buttons") or []
    is_move = any(b in ("up", "down", "left", "right") for b in btns)
    if is_move and prev is not None:
        moves += 1
        if pos != prev:
            changed += 1
    prev = pos
move_rate = (changed / moves) if moves else 0.0

# self-improvement: tools currently in agent-tools dir
tools = glob.glob(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".exo", "agent-tools", "*.source.ts"))
tool_names = [os.path.basename(t).replace(".source.ts", "") for t in tools]

# cost trend: first vs last 20-turn avg per-turn
cs = s.get("cost_series", [])
per = [c[2] for c in cs]
first20 = sum(per[:20]) / max(1, len(per[:20]))
last20 = sum(per[-20:]) / max(1, len(per[-20:]))

print(f"=== {out} ===")
print(f"turns:        {s.get('steps')}  cost: ${s.get('cost_total'):.2f}")
print(f"PROGRESS:     maps={prog.get('n_maps')} {prog.get('maps_visited')}  badges={prog.get('max_badges')}  "
      f"tiles={prog.get('tiles_visited')}  final={prog.get('final_game')}")
print(f"movement:     {changed}/{moves} move-turns actually moved ({move_rate:.0%})  <- low = stuck/walls")
print(f"self-improve: tools={len(tool_names)} {tool_names}  memory_entries={len(s.get('final_memory', []))}")
print(f"cost/turn:    first20=${first20:.4f}  last20=${last20:.4f}  ({'DOWN' if last20<first20 else 'up'})")
print(f"memory (final):")
for m in s.get("final_memory", []):
    print(f"   - {m.get('text','')[:150]}")
