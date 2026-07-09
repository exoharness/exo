Viridian Forest turn 399-400: lower signfork pocket around (24..26,41) / (25..26,40) is only a small safe dry-road loop, not yet a new branch.

Context
- After escaping the wild Kakuna from the `turn398_forest_25_40_signfork_start` probe, overworld resumed near `(24,41)` and later settled live at `(25,41)` facing down on turn 400.
- This area is just west/south of the lower sign `(25,40)` and near the non-trainer Bug Catcher advice lane.

Verified safe local loop
- `(24,41) -> (25,41) -> (26,41) -> (26,40) -> (25,40) -> (25,41)`
- Pressing `A` in this immediate pocket produced no useful hidden interaction/progression.

Interpretation
- The tiles `(24..26,41)` plus `(25..26,40)` form at least one small dry-road pocket with no immediate event.
- This pocket should be treated as local geometry classification only, unless a future probe exits it without rejoining the already-known stale north route from `(25,40)` or the lower NPC/sign lane family.

Practical guidance
- Good nearby reset: `turn397_forest_25_40_signfork`.
- Avoid re-testing the known north route from `(25,40)`; it already rejoins the `(25,34)` lower-trainer family.
- If continuing from live `(25,41)`, use only tiny stepwise dry-road probes and rewind quickly after learning a single new tile fact.