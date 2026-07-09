Viridian Forest turn 382: the new western lower-area NPC pocket around `(15,40)` is not immediately dead, but its first visible screen is a small local corridor rather than a breakthrough by itself.

Verified movement from the live state:
- Start around `(15,40)` facing left with Bug Catcher visible south of the big tree.
- `down` reaches `(15,41)`; `A` facing the Bug Catcher from there does nothing (too far / no interaction).
- `down,left,left,up,up,right,right,right` maps the local west side and reaches `(16,40)`.
- From `(16,40)`, `down` is blocked repeatedly.
- East is open: `(16,40) -> (18,40)`.
- From there, `up` reaches `(18,39)` and `left,left` reaches `(16,39)`.
- Continuing north from `(16,39)` reaches `(16,36)`, then right to `(18,36)`, down to `(18,38)`, and left to `(15,38)`.

Practical takeaway:
- This western lower-area family definitely has dry-road walkable space north/east of the original `(15,40)` view.
- The initial south/down tile under `(16,40)` is blocked, so the obvious route is to climb north through the corridor instead.
- Next useful probe should continue from a safe checkpoint like `turn382_forest_16_40_westcorridor` or rewind to `turn381_forest_22_42_open_area` / `turn382_forest_15_40_west_npc_live` and keep mapping north/east lanes before entering grass.