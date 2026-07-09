# Strategy playbook

## Early boot / controls
- If boot is blank white, `wait` a few seconds.
- At title/menu, `start` or `a` proceeds.
- On naming screens, `start` accepts defaults.
- A advances dialog and confirms; B cancels; START opens the main menu.
- Default d-pad press moves about one tile.
- If a textbox is open, movement does nothing; clear it first.
- Trust RAM for coords/state and screenshot for UI/world geometry.
- `B` often closes overworld textboxes more cleanly than `A`.
- Blank white/black screens during transitions often just need `wait`.

## General exploration
- Probe systematically with RAM coords instead of retrying the same blocked tile.
- Adjacent sprites may be NPCs, not walls; try `A` before assuming collision.
- Long-hold d-pad presses can reveal more walkable area than short probes suggest.
- Dialog boxes block movement until dismissed with A.
- On signs/NPCs, clear lingering text before resuming movement.
- After trainer battles, overworld post-battle text can still be open even when `battle: none`; clear it before moving.
- If a map search stalls for many turns, consider whether the entire premise is wrong (wrong side of a gate, wrong building exit, optional side pocket).
- Use `probe_path` or `walk_path_until_event` to log exact coords through suspect doors/warps; this is better than inferring from screenshots.
- When one branch is repeatedly proving to be a dead end, prefer rewinding to a clean checkpoint near the branch point rather than continuing from a deep exploratory leaf.
- Big meta-lesson from Route 2: if a map contains the right ecology/trainers/signs for the target area, stop insisting it must be somewhere else. Re-evaluate the label, not just the path.
- New forest lesson: distinguish **genuinely new topology** from **local loops**. Several lower/mid-east probes looked promising but rejoined known lanes; record rejoin points explicitly so they do not get mistaken for progress again.

## Current progress
- Starter: Squirtle, now Lv12.
- Oak's Parcel delivery and Pokédex progression are complete.
- Real progression is north from Viridian onto Route 2 and then through Viridian Forest.
- **Major correction (turn 235): map `0x33` is the actual Viridian Forest interior/south approach, not an optional north-side Route 2 pocket.**
- Current strategic task: continue traversing `0x33` northward/westward toward Pewter / the forest's north exit.
- Current live state at start of turn 250 is awkward: a wild Weedle intro battle on map `0x33` around `(20,8)` with Squirtle at 33/34 HP. Next play turn should clear intro text calmly, then RUN explicitly.

## Pallet / Lab quick notes
- Player's House 1F exit: around `(2,7)` on map `0x25`, press `down`.
- Pallet north exit trigger after game start is on the east side of the north path around `x=10,y=1`.
- Oak's Lab is the southwest/left large building in Pallet.
- Right-side Pallet house can heal Squirtle.
- Verified starter: Squirtle.
- On nickname screen, `START` declines naming.
- Oak parcel return: talk to Oak around `(4,2)` facing right.

## Route 1
- Verified northbound route: from about `(5,18)`, go north to `y=14`, east along the rock barrier to about `x=14`, north through the opening, continue north, then loop west near the top fence and go north at about `(11,0)` to warp into Viridian.
- Viridian south return uses the fence opening around `x≈20,y≈30`.
- Upper grass pocket around `(10..13,6..8)` is dangerous and easy to overprobe.

## Viridian City / Pokecenter
- City sign near `(16,17)`: `VIRIDIAN CITY - The Eternally Green Paradise`.
- East-side building labeled `MART` is the Poké Mart.
- There is a fence/rock barrier around `y=28`; many central/east `up` attempts from below are blocked.
- Usable crossing: from around `(22,28)`, move left to about `x=19`, then north to about `(19,25)`, then right.
- Pokecenter frontage: from outside near `(24,26)`, use `left, up` to enter.
- Nurse Joy: stand at `(3,3)` facing up and press `A`; choose `HEAL`.
- After healing, `B` plus `down` closes the final text and steps away.
- Upper city: broad north fence line near `y≈14` is blocked; to reach upper city from the east half, work left to around `(20,16)` and then up.
- Small house/school entrance is around `(21,15)`/`(21,16)`; non-progressing.
- West exit to Route 22 is far left; from about `(0,14)`, pressing `left` warps out.
- Upper-left `TRAINER TIPS` sign around `(19,2)` is just a sign, not progression.
- Exact north exit to Route 2: from the upper-left corridor, go to about `(17,0)` and press `up` to warp to Route 2 at about `(7,71)`.

## Route 2 south section / gatehouse
- From Viridian landing `(7,71)`, the important lower-left breach is real.
- Lower-left sign area: sign around `(5,66)` says `ROUTE 2 / VIRIDIAN CITY - PEWTER CITY`.
- Key breakthrough: from sign area `(5,64)`, go left to about `(2,64)`, then work north/right to expose the real path upward; this leads back to the upper Route 2 area and then the gatehouse frontage.
- Verified climb from the breach can reach upper area around `(3,48)`, then east to `(8,48)`, north to `(8,46)`, left along the building frontage, then up to the south gatehouse entrance at about `(3,44)`.
- From gatehouse interior `MAP_0x32`, the upper-left aisle leads to `(5,1)`; pressing `up` from there enters map `0x33`.

## Viridian Forest / map 0x33
- Treat `map 0x33` as the actual Viridian Forest area entered from the south gatehouse.
- Entry from south gatehouse top door settles near `(17,47)`.
- Known reproducible early route from entrance area: north to `(17,43)`, right to `(20,43)`, up to `(20,41)`, then various lanes branch through the forest maze.
- Forest evidence already seen: Bug Catcher trainers, wild Caterpie/Weedle/Kakuna/Metapod/Pikachu, and the antidote/forest-leaving signs.
- West clearing around `(2,30..31)` is a dead local branch, not the exit.
- Sign-row / antidote area around `(14..18,32)` is a local choke; north is blocked there.
- Trainer at about `(26,33)` is a Bug Catcher who challenges immediately.
- Strong recent progress came from the east side instead of the overprobed west/sign area.
- Verified east-side climb so far:
  - from roughly `(22,41)`, go right to `(26,41)`
  - then north to `(26,36)` and continue up to about `(26,32)` / `(25,30)`
  - from `(25,30)`, a long straight north corridor reaches `(25,22)` and then `(25,10)`
  - near the top, east is mostly blocked except one step to `(26,10)` and north there only reaches `(26,8)`
  - shifting west from the upper corridor allowed progress to `(18,7)`
  - later probes re-established the upper-east area around `(32,11)`
- Right-edge/mid-east refinement from turns 246-249:
  - from `(32,11)`, going down reaches `(32,16)` cleanly
  - from `(32,16)`, continuing down reaches a broader lower clearing around `(32,20)` / `(31,22)` with visible NPCs
  - but that lower clearing **rejoins** the known mid-east NPC/sign area around `(30,18)` rather than opening a new exit route
  - from `(25,18)`, going right to `(27,18)` then down opens `(27,20..22)`, but **right is blocked** across the tested lower row there
  - from `(25,23)`, going `up x5, right x2` returns directly to `(27,18)`; this lower-grass state is a loop, not fresh progress
- Upper-west refinement from turn 249 onward:
  - west from the upper-east area around `(27,8)` reaches at least the lane around `(24,8)` and `(23,9)`
  - current live battle at turn 250 occurring around `(20,8)` strongly suggests the western continuation from the upper lane is the next promising unexplored branch
- Practical next-step heuristic: prioritize the upper-west continuation from roughly `(24..20,8..9)` after safely escaping the live Weedle, because the southern/right-edge branches have mostly been proven to loop back.

## Battle heuristics
- Early wild fights can sometimes be handled with repeated `A`, but blind mash is unsafe once move choice matters.
- Squirtle knows **Tackle**, **Tail Whip**, **Bubble**. Visible order: **TACKLE / TAIL WHIP / BUBBLE**.
- If the move cursor is on **TAIL WHIP**, then `down + A` selects **BUBBLE**; plain `A` wastes a turn.
- With low HP, avoid unnecessary battles; fleeing is preferred.
- If stuck in move or Pokémon submenu, `B` backs out cleanly to the main battle menu.
- Reliable flee recovery from messy substates: use `B` until at the 4-option menu, then try `down, A` or `right, A`; if needed, `down, right, A` from FIGHT also works.
- After `Got away safely!`, one more `A` is usually needed, then a short `wait` for the white transition.
- After `used TACKLE!` or Bubble text, a short `wait` often reveals the true next state more cleanly than extra inputs.
- Forest trainer fights: Bubble animation/text can lag a long time; prefer long waits (~60-120 frames) plus single `A` presses instead of mashing.
- Kakuna often uses **Harden**; the main risk is wasting turns or mashing through lag, not sudden KO.
- For opening wild battles in this area, the stable pattern is: one or two `A` presses with waits to finish the intro/send-out sequence, confirm the 4-option menu is really up, then select RUN explicitly.
- If the battle screen still says `Go! A!`, do **not** send menu inputs yet; advance intro text first, then wait for the command menu before choosing RUN.
- Full HP is valuable in forest exploration because trainer branches may still be ahead; when healthy, prioritize mapping over grinding.
- When slow early-game battle text appears stuck after `used TACKLE!`, long waits plus isolated `A` presses do eventually resolve it; this worked again on turn 246 against Kakuna.
- The `flee_wild_battle` helper can mis-navigate into ITEM/CANCEL when the battle is not actually at the expected 4-option menu yet; for intro states, prefer `early_wild_run_opening` or manual paced clearing first.

## Tooling / harness notes
- `probe_path` is excellent for discovering stepwise movement effects.
- `walk_path_until_event` is better than raw batched movement when following known routes through grass or near warps because it stops on battle/map change.
- Old `clear_battle_text` is broken (`ctx.frame is not a function`); use the safer state-based replacement instead.
- The first `route2_to_forest_approach` macro was buggy because it over-batched movement; prefer short stepwise movement macros that stop on battle/map change.
- `early_wild_run_opening` is the preferred battle-intro escape tool for forest wilds; `flee_wild_battle` is less reliable if called before the command menu is actually ready.
- Useful checkpoints: `turn201_gate_north_start`, `turn212_kakuna_escaped_28_40`, `turn213_npc_cleared_28_40`, `turn216_start_18_35`, `turn217_post_caterpie_12_32`, `turn221_west_clear_2_31`, `turn228_gatehouse_interior_5_3`, `turn228_north_pocket_start_17_47`, `turn235_route2_8_57_start`, `turn236_gatehouse_4_7`, `turn237_post_metapod_22_41`, `turn238_post_weedle_25_30`, `turn239_forest_25_22`, `turn241_forest_32_11`, `turn242_forest_30_14`, `turn244_forest_29_18`, `turn246_post_kakuna_30_13`, `turn247_forest_32_16`, `turn248_forest_25_18`, `turn249_forest_25_23`.