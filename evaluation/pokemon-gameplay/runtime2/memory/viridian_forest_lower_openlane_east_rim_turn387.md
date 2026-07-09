Viridian Forest turn 387: from the lower open-lane state `(23,43)`, probing east found a real farther-east extension rather than only the previously mapped west loop. Verified dry-road / edge geometry:

- `right x5` reaches `(28,43)`
- at `(28,43)`, `down` is blocked
- `right` then `up,up` reaches `(29,41)` and `right` reaches `(30,41)`
- continuing `right` reaches `(31,41)` and then `(31,40)`
- from `(31,40)`, `up` is blocked
- `right` reaches far east edge `(32,40)`; further `right` is blocked
- from there, `down x3` reaches `(32,43)`
- bottom edge then goes back west through `(31,43) -> (30,43) -> (29,43)`
- `up,up` from `(29,43)` returns to `(29,41)`

So the lower-area east side includes a rectangular eastern rim / grass patch bounded roughly by `x=29..32`, `y=40..43`. This is genuinely different local geometry from the already-classified west sign/NPC loop, but no exit/warp/trainer was found in this probe.