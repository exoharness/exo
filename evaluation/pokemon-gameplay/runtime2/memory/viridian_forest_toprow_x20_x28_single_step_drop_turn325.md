Viridian Forest turn 325: after rewinding from stale live `(20,2)` to `turn287_forest_18_6_postbattle`, a systematic top-row probe refined the already-solved upper strip.

From `(18,6)`, the route `left,left,(left blocked), up,up,(left blocked), down,(left blocked), up,up,(left blocked), up` reaches the top row at `(16,2)` and then `(16,1)`. Moving right along the top row to `(28,1)`, every tested column from `x=21` through `x=28` behaves the same way:
- `down` from `(x,1)` reaches `(x,2)`
- a second `down` is blocked
- `up` returns to `(x,1)`

So the stretch `(21..28,1)` is just the known top row with a one-tile lower strip at `y=2`, not a fresh descent. This further supports that the current/top-row family around `(20,2)` is stale solved geometry and should be rewound from, not explored as new progress.