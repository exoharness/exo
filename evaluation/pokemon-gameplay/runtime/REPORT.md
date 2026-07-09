# Overnight run report

- turns played: 250
- milestones: 23
- self-improvement actions: 306
- agent-built tools: advance_dialog, clear_battle_text, clear_battle_text_safe, early_battle_mash, early_wild_run_opening, flee_wild_battle, paced_battle_progress, probe_path, route2_start_to_gatehouse_south, route2_to_forest_approach, service_desk_probe, walk_path_until_event, wild_run_recover
- memory files: 123

## Milestones
- turn 1: Entered Pallet Town for the first time
- turn 1: Entered Player's House 2F for the first time
- turn 16: Entered Player's House 1F for the first time
- turn 19: Entered Oak's Lab for the first time
- turn 21: Pokedex grew to 4 owned
- turn 22: Party grew to 1: Squirtle L0
- turn 25: Entered Route 1 for the first time
- turn 28: Highest party level is now 6
- turn 34: Entered Viridian City for the first time
- turn 38: Entered Viridian Pokecenter for the first time
- turn 54: Entered Viridian Mart for the first time
- turn 58: Highest party level is now 7
- turn 74: Highest party level is now 8
- turn 79: Entered Viridian School for the first time
- turn 82: Entered Route 22 for the first time
- turn 95: Entered Route 2 for the first time
- turn 99: Highest party level is now 9
- turn 111: Entered MAP_0x32 for the first time
- turn 111: Entered Route 2 Gate (Viridian Forest south) for the first time
- turn 127: Highest party level is now 10
- turn 141: Highest party level is now 11
- turn 153: Entered Viridian House for the first time
- turn 179: Highest party level is now 12

## Last 10 turn summaries
### Turn 241
What I did
- Saved a fresh checkpoint: `turn241_forest_32_11`.
- Followed the directive to try genuinely different probing from the stuck upper-east forest state instead of repeating the same pushes into the top edge.
- Mapped the upper-right branch stepwise and learned:
  - from `(32,11)`, I can climb and swing left to a higher lane around `(28,8)`,
  - then route farther through `(25,8)`, `(31,7)`, `(31,5)`, `(32,2)`, `(27,1)`, and across the top row to `(31,1)`,
  - but **north is blocked** at all tested top-edge spots, including `(28,8)`, `(25,8)`, `(24,8)`, `(27,1)`, and `(31,1)`.
- Wrote
### Turn 242
Turn 242 summary

What I did
- Saved a fresh checkpoint at the live forest state:
  - `turn242_forest_30_14`
- Tried a genuinely different route from the current upper-east forest lane instead of repeating the same top-edge pushes.
- Probed downward from `(30,14)` and found a new mid-east clearing:
  - `down x4` to `(30,18)`
  - `left x3` to `(27,18)`
  - `up` from `(27,18)` is blocked
  - `left` to `(26,18)`, then `down` to `(26,20)`, `left` to `(25,20)`, then back `up` to `(25,18)`
- Continued from there and confirmed this clearing links back to the already-known upper blocked lane:
  - from
### Turn 243
Turn 243 summary

What I did
- Saved a fresh checkpoint at the stuck upper forest state:
  - `turn243_forest_25_8`
- Followed the “do something genuinely different” directive by probing the upper-east forest lanes instead of repeating blocked top-edge pushes.
- Verified a new local loop/opening structure:
  - From `(25,8)`, moving left to `(20,8)` works.
  - From `(20,8)`, only one step down to `(20,9)` is open; further down there is blocked.
  - From that row, rightward movement returns across to `(25,9)`.
- Probed the east side more aggressively:
  - From `(28,8)`, moving right reaches `(32,
### Turn 244
Turn 244 summary

What I did
- Cleared a lingering sign textbox at `(26,18)` that said `...via PC to get your POKéDEX evaluated!`
- Moved east to `(29,18)` and saved a fresh checkpoint:
  - `turn244_forest_29_18`
- Probed the nearby NPC/sign area systematically and found an interactable NPC at `(30,18)` facing down.
- Read that NPC’s advice:
  - starts with `Darn!`
  - ends with `...to catch some stronger ones!`
  - so this is just flavor/advice, not progression.
- After clearing the dialog, probed movement around the NPC and found a new opening:
  - from `(30,18)`, `up` works to `(30,17)` the
### Turn 245
Turn 245 summary

What I did
- Used the live wild battle as a low-risk way to advance the stalled state at `(30,13)` in Viridian Forest (`map 0x33`).
- Chose **TACKLE** twice against a wild **Kakuna Lv4**.
- Carefully used the proven slow-battle cadence:
  - `A` to act
  - long waits (~90 frames) for slow text/animation
  - single `A` presses to advance text
- Confirmed Kakuna used **HARDEN**, then saw:
  - `Enemy KAKUNA's DEFENSE rose!`
- Returned cleanly to the move menu once, with PP dropping from **30/35 to 29/35**, confirming the first Tackle fully resolved.
- Began the second Tackle; by 
### Turn 246
Turn 246 summary

What I did
- Finished the slow wild Kakuna battle safely from the stuck `used TACKLE!` state:
  - waited with the proven slow cadence
  - advanced text with single `A` presses
  - returned to overworld at `(30,13)` with Squirtle still **33/34 HP**
- Saved a fresh checkpoint:
  - `turn246_post_kakuna_30_13`
- Did genuinely different forest probing instead of repeating the stalled top-edge pushes.
- Mapped the nearby right-edge branch:
  - from `(30,13)`, `up, up` reaches `(30,11)` but left is blocked there
  - moving right returns to the known upper-east state `(32,11)`
  - fr
### Turn 247
What I did
- Saved a fresh checkpoint at the live right-edge forest state:
  - `turn247_forest_32_16`
- Followed the directive to do something genuinely different: I approached the visible southwest branch instead of repeating top-edge pushes.
- Moved from the right-edge lane down into a new lower clearing:
  - from `(32,16)` down to `(32,20)`
  - left is blocked at `(31,20)` beyond one step
  - then down to `(31,22)` where the screen revealed two NPCs and a broader opening
- Probed back up/left around that opening and reached the known mid-east NPC area:
  - stepwise path ended at `(30,18)`
-
### Turn 248
What I did
- Saved a fresh checkpoint: `turn248_forest_25_18`.
- Tried a genuinely different local probe from the sign/NPC area instead of repeating top-edge pushes.
- Verified this local structure:
  - from `(25,18)`, `right,right` reaches `(27,18)`
  - from there `down` opens to `(27,22)`
  - **right is blocked across the whole tested row at `(27,20..22)`**
- Probed the lower grass corridor and hit a wild Kakuna at `(25,21)`.
- Escaped safely despite a menu misstate:
  - the automated run helper got stuck and opened the **ITEM** pocket (`ANTIDOTE x1 / CANCEL`) instead of fully escaping
  - b
### Turn 249
What I did
- Saved a fresh checkpoint: `turn249_forest_25_23`.
- Tested the “new” lower-grass state at `(25,23)` instead of assuming it was progress.
- Verified it loops back into already-known territory:
  - `up x5, right x2` returns to the sign/NPC area at `(27,18)`, where `up` is blocked.
  - Continuing through the upper lanes led back to the previously mapped upper-east clearing around `(27,8)`.
- Saved this as memory:
  - `viridian_forest_rejoin_from_25_23_turn249`
- Probed farther west from the upper-east area and found genuinely different walkable tiles:
  - reached `(24,8)`, `(23,9)`, 
### Turn 250
Updated self-improvement materials only, as directed.

Changed:
- **Playbook updated** with the main lessons from turns 247–250:
  - right-edge/lower-grass forest branches mostly **loop back** into known mid-east territory
  - the best remaining lead is the **upper-west lane** from around `(24..20,8..9)`
  - `flee_wild_battle` can mis-navigate if used before the battle menu is really ready
  - current live state is a **Weedle intro battle** at `Go! A!`, so next turn should use the cautious intro-clear-then-RUN pattern
- **Memory saved**: `viridian_forest_mid_east_loops_and_upper_west_hint_turn

## Inspect
- runtime/agent.log (full narration)
- runtime/events.jsonl (every tool call)
- runtime/run.gif (the whole run animated)
- runtime/playbook.md + runtime/memory/ + runtime/tools/ (self-authored)
- runtime/backups/ (state snapshot at every run boundary)
