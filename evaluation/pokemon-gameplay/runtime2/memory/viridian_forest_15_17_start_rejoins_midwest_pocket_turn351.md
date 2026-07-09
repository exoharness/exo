Viridian Forest turn 351: older checkpoint `turn258_forest_15_17_start` at `(15,17)` is not fresh progress. Step-probing showed the local lane feeds directly into the already-solved mid-west pocket family.

Verified path facts:
- From `(15,17)`, left to `(13,17)`, up to `(13,15)`, but right from `(13,15)` is blocked.
- Continuing up to `(13,13)` then left reaches `(11,13)`.
- From there, down reaches `(11,18)`, but left at `(11,18)` is blocked.
- Returning upward and right just reconnects to `(13,15)` / `(13,17)`.

Conclusion: the `(15,17)` checkpoint family belongs to the same solved mid-west / center-left loop around roughly `(11..15,13..18)`. Do not spend future action turns exploring outward from `turn258_forest_15_17_start`; rewind instead.