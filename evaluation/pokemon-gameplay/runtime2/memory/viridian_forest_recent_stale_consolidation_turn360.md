Viridian Forest turn 360 consolidation: the recent suspect checkpoints/families tested from turns 351-359 are all stale and should be rewind-first.

Confirmed stale in this block:
- `turn258_forest_15_17_start` / `(15,17)` rejoins the mid-west pocket.
- Top-row west edge `(16,1)` is the real left limit; no hidden north opening there.
- Far-right top-row stair descent from `(29,1)->(32,7)` is stale east-wall/top-row geometry.
- `turn352_forest_17_5_rightside_probe_start` / live `(19,3)` rejoins the upper-connector family.
- `turn271_forest_32_28_start` / `(32,28)` rejoins the lower trainer-pocket family.
- `turn355_forest_18_14_fresh` / `(18,14)->(16,13)` rejoins the stale upper-west/mid-west system.
- `turn252_post_kakuna_16_8` and live `(16,8)` only loop through `(18,6)`, `(19,8)`, and nearby upper-connector tiles.
- Live `(17,5)` is also stale and should be rewound immediately.

Operational takeaway:
- Treat the entire upper-connector / top-center / top-row / east-wall / trainer-sign-pocket / mid-west collection as classified.
- Live `(18,6)` itself is not a productive action-turn start unless doing narrowly targeted classification; scheduled action turns should rewind first.
- Best remaining strategy is to use rewind-first triage and search for any checkpoint family outside the classified forest systems rather than probing adjacent live stale states.