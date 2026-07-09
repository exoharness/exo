Viridian Forest turn 320 consolidation: the live top-row lower-strip state at `(27,2)` is now officially stale.

Turn-319 probing already verified the only substantial continuation from here:
- `right x5` to `(32,2)`
- then down the east wall through `(32,5)`, `(31,6)`, and farther south
- this ultimately rejoins the familiar mid-east trainer/sign pocket around `(30,18)`.

Practical consequence:
- do not spend future action turns exploring directly from live `(27,2)`
- do not use checkpoint `turn319_forest_27_2_toprow_east_start` except for a very specific re-check of that already-classified east-wall descent
- future forest action should begin from an earlier rewind target, not from this solved top-row branch.