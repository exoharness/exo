Route 22 lower-road consolidation: the broad road at y=14-15 is traversable essentially end-to-end, but it is a dead probe lane for upward progression.

Verified from RAM-guided probing:

- Starting around `(21,14)`, moving right works continuously to at least `(37,14)`.
- At the far east, `right` beyond about `x=37` is blocked by the cliff/wall.
- `up` is blocked from the lower road at many positions including roughly `x=6,8,10,11,12,13,14,15,20,21,26,35,37` on `y=14`, plus `x=34` and nearby on `y=15`.
- At the far west, moving left works to about `(2,14)`, but farther left is blocked by the map edge/wall.
- Near the far west, moving `down` from `(2,14)` reaches `(2,15)`, but further `down` is blocked; this does not open a new lane.
- Near the far east, moving `down` from `(37,14)` reaches `(37,15)`, and moving left along `y=15` is possible, but `up` remains blocked there too.

Conclusion: the visible lower road on Route 22 is not the route to rival/progression. Continuing to brute-force `up` from this road is wasted effort.
