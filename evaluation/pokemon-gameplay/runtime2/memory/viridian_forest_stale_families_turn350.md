Viridian Forest stale-family consolidation through turn 350: the remaining repeatedly-tested checkpoint families have now been classified enough to justify aggressive rewind discipline.

## Newly solidified stale families
- **Upper connector / upper-west microloops**:
  - `(18,6)` family is fully stale for ordinary action turns.
  - `A` checks around suspicious tiles there found no hidden sign/object/NPC.
  - Live `(19,8)` from `turn344_forest_19_8_fresh` is only a tiny local right-side square, not a new branch.
- **Top row family**:
  - Generic `(x,1) -> (x,2)` drops across much of the top row are exhausted.
  - The center-left `(18,1)` descent and the different diagonal variation both rejoin solved upper-connector/sign-pocket geometry.
  - Far-right top-row descent and far-east lower-strip descent both rejoin `(30,18)` sign/trainer pocket.
- **Trainer/sign pocket family**:
  - `(25,18)`, `(26,20)`, `(27,18)`, `(29,19)`, `(30,18)`, and east-wall states around `(31,8)..(32,11)` are all entrances or side tiles of the same solved pocket.
- **Mid-west pocket family**:
  - `(11..14,15..19)` remains a closed local loop; several south/southwest descents from upper-west reconnect into it.

## Practical implication
Before the next real action turn, treat these live states as rewind-first:
- `(19,8)` / `turn344_forest_19_8_fresh`
- `(18,6)` unless testing a very specific hypothesis
- `(27,18)`, `(29,19)`, `(30,18)`
- east-wall states around `(31,8)..(32,11)`
- top-row strip microstates around `(x,1)/(x,2)` unless using a targeted missed-branch test

## Preferred rewind order
1. `turn346_forest_18_1_toprow_fresh`
2. `turn341_forest_18_6_fresh`
3. `turn325_forest_18_6_fresh`
4. `turn309_forest_17_5_newdrop`

## Meta-lesson
When a checkpoint family yields multiple different-looking probes that all rejoin the same geometry, stop preserving its child microstates as promising. Mark the family stale in the playbook and move on.