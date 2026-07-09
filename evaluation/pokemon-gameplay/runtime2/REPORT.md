# Run report

- turns played: 500
- milestones: 23
- self-improvement actions: 926
- agent-built tools: advance_dialog, clear_battle_text, clear_battle_text_safe, early_battle_mash, early_wild_run_opening, flee_wild_battle, paced_battle_progress, probe_path, route2_start_to_gatehouse_south, route2_to_forest_approach, service_desk_probe, walk_path_until_event, wild_run_recover, wild_run_recover_safe
- skills: viridian-forest-loop-triage, viridian-forest-wild-run-recipe
- memory files: 347

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
### Turn 491
Turn 491 summary

What I did
- Loaded and followed the installed wild-battle escape recipe.
- Safely escaped the live wild Pikachu from the `Go! A!` phase:
  - waited through send-out lag
  - single `A` to advance
  - waited again until the real 4-option menu appeared
  - used `down, right, A` to select **RUN**
  - cleared the escape text with `A, A`
- Returned to overworld at:
  - **Viridian Forest `(18,12)`**
  - **battle: none**
  - **Squirtle 33/34 HP**
- Saved a fresh checkpoint:
  - `turn491_forest_18_12_postpikachu`
- Used `probe_path` to classify the immediate nearby geometry.

What I 
### Turn 492
Turn 492 summary

What I did
- Followed the stagnation warning and rewound out of the live `(17,10)` upper-west loop.
- Loaded a different lower-area checkpoint:
  - `turn404_forest_17_44_trainer_below`
- Saved safety checkpoint:
  - `turn492_forest_17_44_loaded`
- Ran a dry-road probe from `(17,44)`:
  - `up,up,left,left,up,right,right,right,up,left,up,right`

What I learned
- `turn404_forest_17_44_trainer_below` is also stale.
- The probe gets blocked trying to push north from `(17,43)`, then simply rejoins the known lower sign/bug-catcher pocket:
  - `(17,44) -> (17,43)` then blocked
  - ea
### Turn 493
Turn 493 summary

What I did
- Followed the anti-stagnation directive and rewound out of the stale live state at `(17,44)`.
- Loaded a genuinely different old checkpoint:
  - `turn365_forest_17_38_lane`
- Saved:
  - `turn493_forest_17_38_lane_start`
- Probed from `(17,38)` with:
  - `up,up,left,left,up,right,right,right,up,left,up,left`
- Result: this route climbs directly back into the known antidote sign row, ending at `(16,33)`. I recorded:
  - `viridian_forest_17_38_lane_rejoins_antidote_sign_turn493`

Then I tried another genuinely different checkpoint:
- Loaded:
  - `turn367_forest_26_24
### Turn 494
Turn 494 summary

What I did
- Loaded the installed wild-battle escape procedure and used it correctly on the live wild Kakuna at `(25,28)`.
- Advanced the intro text with `A`.
- Waited through the long `Go! A!` send-out phase instead of sending menu inputs too early.
- Once the 4-option battle menu appeared, selected **RUN** with `down, right, A`.
- Cleared `Got away safely!` with `A, A`, waited through the white transition, and returned to overworld safely.
- Saved a fresh checkpoint:
  - `turn494_forest_25_28_postkakuna`

What I learned
- This battle state was handled cleanly by the phase-b
### Turn 495
Turn 495 summary

What I did
- Saved the live state as `turn495_forest_25_28_live`.
- Probed the current area with a systematic alternating path:
  - `left, right, up, left, right, up, left, right, up, left, right, up`
- This triggered a wild Kakuna during the probe.
- Escaped safely by following the cautious battle cadence:
  - advanced `Wild KAKUNA appeared!`
  - waited through the long `Go! A!` send-out phase
  - waited until the real 4-option battle menu appeared
  - chose **RUN** with `down, right, A`
  - cleared the escape text and waited through the white fade
- Saved a durable note as 
### Turn 496
Turn 496 summary

What I did
- Saved the live state as `turn496_forest_26_26_start`.
- Probed the current corridor from `(26,26)`:
  - `up x8` reached `(26,18)`.
  - Short checks around there showed the familiar sign + Bug Catcher pocket again, including `(28,18)`.
- Continued the corridor the other direction:
  - from `(27,27)`, `down x10` reached `(27,37)`.
  - `right` was blocked there.
  - `up, up, left, left` then landed at `(25,35)`, visually in the known lower trainer-visible/lower-lane family.

What I learned
- The `(26,24..26)` vertical corridor family is stale and now classified on b
### Turn 497
Turn 497 summary

What I did
- Checked checkpoints and loaded the **viridian-forest-loop-triage** skill to avoid continuing a stale loop blindly.
- From the live state at **Viridian Forest (25,35)**, I used a short stepwise probe:
  - `up, up, up, left, left, up, right, right, up, up`
- That immediately moved once to **(25,34)** and then triggered a **wild Pikachu** battle.
- I then switched to the **viridian-forest-wild-run-recipe** and tried to escape carefully:
  - confirmed intro text `Wild PIKACHU appeared!`
  - pressed `A` to advance
  - waited through the long send-out phase until `Go! 
### Turn 498
Turn 498 summary

What I did
- Cleared the live wild Pikachu battle safely from the ThunderShock text:
  - used `paced_battle_progress` twice
  - battle ended with Squirtle at **28/34 HP**
  - returned to overworld at **Viridian Forest (25,34)**
- Followed the stagnation directive and **rewound out of the wedge** instead of continuing the stale `(25,34)` / lower-lane family.
- Loaded a genuinely different checkpoint:
  - `turn415_forest_13_33_upperleft`
- Saved a fresh safety checkpoint:
  - `turn498_forest_13_33_upperleft_start`
- Probed that upper-left/sign-adjacent family.

What I learned
-
### Turn 499
Loaded `turn485_forest_17_11_newlane`, saved `turn499_forest_17_11_newlane_start`, and tested a new route from `(17,11)`.

Result:
- Probe `up, up, right, up, left, up, right, right, up, left, up, left`
- It climbed through `(18,7) -> (18,6) -> (17,6) -> (17,5)` and ended at **`(16,5)`**
- So this checkpoint is **not fresh**; it rejoins the already-solved **top-center / upper-connector** family.

I recorded this as memory:
- `viridian_forest_17_11_newlane_rejoins_16_5_turn499`

Then I rewound back to:
- `turn499_forest_17_11_newlane_start`

Also updated:
- playbook
- todos

What I learned:
- `
### Turn 500
Updated self-improvement only; no buttons pressed.

Changed:
- **Playbook**: added turn-500 rules to treat the current live `(17,11)` forest state as stale, prioritize checkpoint-family triage over tempting live geometry, and summarized the key exhausted forest families.
- **Memory**: saved `viridian_forest_turn500_checkpoint_audit`, a compact audit of which Viridian Forest checkpoint families are already exhausted and why.
- **Todos**: now explicitly say to rewind away from the live stale `(17,11)` state next action turn and use checkpoint-family triage before more probing.

What worked latel
