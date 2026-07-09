Viridian Forest turn 425: from checkpoint `turn423_forest_27_21_upper_opening` at `(27,21)`, probing found a genuinely different east-side continuation rather than immediately collapsing into the old sign pocket. Verified path/local geometry:
- `(27,21) -> up -> (27,20) -> up -> (27,19)`
- east side opens: `(27,19) -> right -> (28,19) -> right -> (29,19)`
- `down` at `(29,19)` is blocked
- west/north refinement: `(26,19) -> up -> (26,18)` works but second `up` there is blocked; `(25,18)` exists; `(25,19)` exists but `left` from `(25,19)` is blocked
- stronger east probe: `(27,19) -> up -> (27,18) -> right -> (28,18)` with `up` blocked there
- then farther east: `(28,18) -> right -> (29,18) -> right -> (30,18) -> up -> (30,17) -> right -> (31,18)` with `left` blocked from `(30,17)`
- from `(31,18)`, continuing `up, up, right, up, left` reaches `(31,15)` after passing through `(31,17)`, `(31,16)`, `(32,16)`, `(32,15)`.
This is a fresh east-lane / upper-grass continuation worth retrying from checkpoints `turn425_forest_27_21_retry` or `turn425_forest_31_18_eastlane`. Be cautious of grass encounters.