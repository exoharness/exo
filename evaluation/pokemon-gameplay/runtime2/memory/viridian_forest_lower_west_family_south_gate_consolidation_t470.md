Viridian Forest lower-west family consolidation through turn 470.

## Best current fresh family
- Main anchor: `turn459_forest_10_42_clearing`
- Current live state after rewind: Viridian Forest `(10,42)` facing down.
- Discovery route from `turn404_forest_14_40_npc_overlook`:
  - `left,left,left,down,down,left,up,up,left`
  - reaches the fresh lower-west clearing around `(10,42)`.

## Verified local geometry in this family
- East/perimeter probe from the clearing reaches the sign area around `(18,46)`.
- The Bug Catcher near `(15,43)` is flavor text only (`I came here with some friends!...`).
- West/north probe from the clearing opened a west-side corridor up to about `(7,36)` and then to the upper lane around `(8,30)`, but those routes rejoin old sign-row / upper-lane geometry rather than creating new progress.
- From west-corridor anchor `(6,39)`, probe `up,up,right,right,right,down,down,down,right,up` loops back to the clearing area around `(10,43)`.

## Important correction: apparent exit is the south backtrack
- From the lower sign area around `(15,46)`, pressing `down` twice changes map to `Viridian Forest South Gate` around `(5,0)`.
- Inside that gatehouse, walking `down x8` exits to Route 2 south around `(3,43)`.
- The gatehouse top doorway is blocked from inside around `(4,1)`.
- Therefore this newly found exit is only the forest's south entrance/backtrack, not the Pewter-side north exit.

## Practical implication
- Continue treating `(10,42)` / the lower-west family as the best remaining anchor inside the forest, but do not spend more time proving the south gate again.
- Next useful work should be genuinely different probes from the lower-west family that avoid simply climbing back into the known upper/sign-row loops or dropping into the confirmed south exit.