# Strategy playbook

## Early boot / controls
- If boot is blank white, `wait` a few seconds.
- At title/menu, `start` or `a` proceeds.
- On naming screens, `start` accepts defaults.
- A advances dialog/confirms; B cancels; START opens the main menu.
- Default d-pad press moves about one tile.
- If a textbox is open, movement does nothing; clear it first.
- Trust RAM for coords/state and screenshot for UI/world geometry.
- `B` often closes overworld textboxes more cleanly than `A`.
- Blank white/black screens during transitions often just need `wait`.

## General exploration
- Probe systematically with RAM coords instead of retrying the same blocked tile.
- Adjacent sprites may be NPCs, not walls; try `A` before assuming collision.
- After trainer battles, overworld text may still be open even when `battle: none`; clear it before moving.
- Use `probe_path` or `walk_path_until_event` to log exact coords through suspect doors/warps.
- When a branch proves to be a dead end or rejoins known ground, record the rejoin point and rewind to a clean checkpoint near the branch point.
- In forest mapping, distinguish genuinely new topology from local loops; do not keep wiggling inside already-classified pockets.
- Favor dry-road probes before grass-heavy tiles.
- During scheduled self-improvement turns, do not touch the emulator; consolidate what was learned first.
- If a stagnation warning is active, prefer rewinding out of the wedge and classifying one more non-solution rather than continuing live from a stale state.
- Turn-500 rule: if the current live state is already a proven stale checkpoint family, ignore the tempting screen geometry and rewind next action turn.
- Turn-500 rule: choose future forest probes by checkpoint family class, not by whichever live tile looks interesting.

## Current progress
- Starter: Squirtle Lv12.
- Oak's Parcel delivery and Pokédex progression complete.
- Current meaningful task: find the true north/Pewter-side exit of Viridian Forest from a genuinely different checkpoint family.
- Current live screen on turn 500 is Viridian Forest `(17,11)` facing up because `turn499_forest_17_11_newlane_start` was reloaded after documentation. This state is stale and should be rewound away from next action turn.

## Key route notes
- Viridian north exit: from upper-left corridor, go to about `(17,0)` and press `up` to Route 2 `(7,71)`.
- Viridian Pokecenter: from outside near `(24,26)`, use `left, up` to enter. Nurse Joy is `(3,3)` facing up; choose `HEAL`.
- Route 2 south breakthrough: from sign area `(5,64)`, go left to about `(2,64)`, then north/right to expose the real path upward to the gatehouse.
- Gatehouse: from interior `MAP_0x32`, upper-left aisle to `(5,1)`, then `up` enters forest map `0x33`.
- Important correction: the forest exit discovered at `(15,46) -> down -> down` leads to the **south** gatehouse. From inside that gatehouse, walking `down x8` exits to Route 2 south at about `(3,43)`, and the outside doorway alignment is also useless. This is not forward progress to Pewter.

## Viridian Forest / map 0x33
- Entry from south gatehouse settles near `(17,47)`.
- Reproducible early route: north to `(17,43)`, right to `(20,43)`, up to `(20,41)`, then branch.
- West clearing around `(2,30..31)` is a dead local branch.
- Sign-row / antidote area around `(14..18,32)` is a local choke; north blocked there.
- Trainer at about `(26,33)` challenges immediately.

### Forest status / demotions
- Lower-west family from `turn459_forest_10_42_clearing` is heavily classified. It reaches the lower sign area and the south-gate backtrack, but not the real exit.
- The sign-clearing route `(18,46) -> left,left,down,down` is the confirmed south gatehouse backtrack.
- The upper rim from south-sign area `(15,45)` only reaches blocked tiles around `(10..14,40..41)` and reconnects locally.
- West-corridor family around `(6,39)` is only a tiny local lane.
- Post-Kakuna family around `(15,39)` / `(18,37)` rejoins the antidote sign lane at `(16,33)`.
- `turn476_forest_22_42_open_area_fresh` is exhausted: east climb rejoins signlane at `(26,32)`, right/NPC climb rejoins signlane at `(26,30)`, west/south perimeter is only a tiny `(17..21,42..43)` pocket.
- Old upper families are stale too:
  - `turn277_forest_18_13_visible` climbs back to top-center `(16,5)`.
  - `turn287_forest_18_6_postbattle` right-side idea is only a tiny upper-west weave.
  - `turn286_forest_20_8_visible` east probe is only a small upper-east loop with north blocked at `(24,8)` / `(23,8)`.
  - `turn257_post_kakuna_18_11` is a local loop with `left` blocked at `(16,11)` and `right` blocked at `(18,13)`.
  - `turn485_forest_17_11_newlane` also rejoins stale top-center `(16,5)`.
- `turn491_forest_18_12_postpikachu` only mapped a tiny upper-west connector loop around `(16..18,10..12)`.
- `turn404_forest_17_44_trainer_below` rejoins the known lower sign/bug-catcher pocket at `(20,41)`.
- The vertical corridor family around `(26,24..26)` is stale: north rejoins the sign + Bug Catcher pocket around `(26..28,18)`, south reconnects to the lower trainer area near `(25,35)` / `(27,35)`.
- Therefore avoid re-proving: upper/top-center rejoins, sign-row choke, south gatehouse, blocked upper rim `(10..14,40)`, fake south perimeter from `(10,42)`, `(6,39)` west corridor, `(15,39)/(18,37)` post-Kakuna, `(22,42)` anchor branches, `(18,12)` connector loop, `(17,44)` trainer-below rejoin, `(26,24..26)` vertical corridor, and `(17,11)->(16,5)`.
- Practical next-turn rule: load `viridian-forest-loop-triage` if uncertain, then pick a checkpoint whose family is not already on the exhausted list above.

## Battle heuristics
- Squirtle knows **TACKLE / TAIL WHIP / BUBBLE**.
- If cursor is on **TAIL WHIP**, `down + A` selects **BUBBLE**.
- With low HP, avoid unnecessary battles; fleeing is preferred.
- If stuck in move or Pokémon submenu, `B` backs out to main battle menu.
- Reliable flee recovery: use `B` until at 4-option menu, then try `down, A` or `right, A`; if needed, `down, right, A` from FIGHT also works.
- After `Got away safely!`, one more `A` is usually needed, then a short `wait` for white transition; sometimes two final `A` presses total are needed before RAM drops `battle` to none.
- Forest trainer fights: Bubble animation/text can lag a long time; prefer long waits (~60-120 frames) plus single `A` presses.
- Consolidated wild-run rule in forest:
  1. intro text (`Wild X appeared!`) → `A`
  2. send-out text (`Go! A!`) / animation lag → wait, then single `A`
  3. visible 4-option menu → choose RUN explicitly
- If the screen still says `Go! A!`, do **not** send menu inputs yet.
- If a cautious wild-run attempt lands in the move menu instead of the 4-option menu, use `B` once to recover before selecting RUN.
- If the battle is wedged in ITEM submenu, manual recovery is `B, B`, then wait for proper flow, then select RUN explicitly.
- If text looks partial after choosing RUN, clearing with `A, A` often finishes the escape.
- If text looks partial or shows enemy move text, assume it is still resolving; use patience and single `A` presses rather than menu inputs.

## Tooling / harness notes
- `probe_path` is excellent for stepwise movement discovery.
- `walk_path_until_event` is better than raw batched movement near grass/warps because it stops on battle/map change.
- Old `clear_battle_text` is broken (`ctx.frame is not a function`); use the safer state-based replacement instead.
- `early_wild_run_opening` is preferred for battle-intro escape; `flee_wild_battle` is less reliable before the command menu is ready.
- `paced_battle_progress` works well to finish slow battle text safely.
- `wild_run_recover` is not fully trustworthy for ITEM-submenu wedges; prefer `wild_run_recover_safe` or manual `B, B` + paced progress + explicit RUN.
- The limiting factor now is checkpoint-family selection, not battle execution; spending a call on checkpoint triage is worthwhile.