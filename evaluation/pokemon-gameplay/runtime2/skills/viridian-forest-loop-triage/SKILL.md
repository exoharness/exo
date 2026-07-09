---
name: viridian-forest-loop-triage
description: Decide quickly whether a Viridian Forest state is fresh progress or a known loop, and choose the best rewind target instead of re-probing solved pockets.
---
# Viridian Forest loop triage

Use this when you are in Viridian Forest and suspect the current state may already belong to solved geometry.

## Goal
Avoid wasting turns on repeatedly rediscovering the same pockets, top-row drops, and sign/trainer rejoins.

## Immediate classification checklist

### Treat as **known loop / solved geometry** if any of these are true
- You are around **(30,18)** in the mid-east sign/trainer hub.
- You are around **(26,30)** or **(27,30)** with the lower-right Bug Catcher/sign pocket visible nearby.
- You descended the **far-right top row** from around `(32,1)` and are now moving south on the east wall.
- You are in the **mid-west pocket** around `(11..14,15..19)`.
- You are back near the **upper-west lane** around `(18,7)`, `(18,9)`, `(18,11)`, or `(20,8)` without having found a map change or clearly unseen corridor.
- A route from your current state matches one of these known reconnections:
  - `(25,23) -> up x5, right x2 -> (27,18)`
  - `(32,1) -> down to east wall -> south -> (30,18)`
  - east-edge south corridor eventually returns to `(27,30)` / `(30,18)`
  - `(20,8)` eastward connector returns to the known upper-east corridor near `(25,11)`

### Treat as **potentially fresh** only if at least one is true
- You found a **new map change**.
- You reached coordinates not already documented in the forest notes.
- You found a corridor that does **not** quickly reconnect to `(30,18)`, `(26,30)`, the upper-west lane, or the mid-west pocket.
- You have a clean checkpoint right before probing and can test a short branch safely.

## Rewind policy
If the current state is classified as a known loop, rewind instead of exploring outward.

Preferred rewind order:
1. `turn287_forest_18_6_postbattle`
2. `turn286_forest_20_8_visible`
3. `turn279_forest_18_7_visible`
4. `turn278_forest_18_9_visible`
5. `turn277_forest_18_13_visible`
6. `turn263_forest_20_8_start`
7. `turn257_post_kakuna_18_11`
8. `turn256_forest_18_9_start`

Choose the earliest checkpoint in that chain that gives a genuinely different branch angle than the current solved pocket.

## Probe discipline when you do continue
- Save a checkpoint before the probe.
- Use `probe_path` or `walk_path_until_event`, not long blind movement.
- Stop quickly if the path settles into any of the solved markers above.
- If a wild battle starts, use the installed wild-run recipe; do not let battle cleanup derail the classification.

## What to record afterward
If a branch proves to be a loop, write down:
- the entry checkpoint/state
- the short confirming path
- the exact rejoin marker

If a branch is truly fresh, record:
- start state
- exact tile sequence
- new coordinates reached
- whether there is a trainer/sign/item/warp/map change
