Viridian Forest turn 411: after clearing the wild Weedle aftermath, the post-battle state at `(20,40)` is not a wholly new branch; tiny dry-road probes show it reconnects quickly into the known lower open-hub family.

Verified local geometry from checkpoint `turn411_forest_20_40_postweedle`:
- `left` from `(20,40)` works to `(19,40)`.
- `up` from `(19,40)` is blocked.
- `right,right` from there reaches `(21,40)`.
- `down` from `(21,40)` works to `(21,41)`.
- Additional tiny probe mapped:
  - `(21,41) -> left -> (20,41)`
  - `down -> (20,42)`
  - `left -> (19,42)`
  - `up -> (19,41)`
  - `right -> (20,41) -> (21,41)`
  - `down -> (21,42)`
  - `right -> (22,42)`

Interpretation:
- This area is a small dry-road connector that rises to the previously known `(21,40)` branch and also drops/rejoins the known lower open area around `(22,42)`.
- Since it reconnects quickly into known lower-lane topology, do not spend many more turns live-probing from `(20,40)`; prefer rewinding to `turn409_forest_21_40_newlane` or `turn387_forest_23_43_openlane_start` for cleaner branch tests.