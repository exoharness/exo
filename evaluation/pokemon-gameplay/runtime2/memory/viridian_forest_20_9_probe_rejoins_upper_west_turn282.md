Viridian Forest (turn 282) probe from `(20,9)` facing down quickly rejoins the already-known upper-west lane rather than opening a new connector.

Verified stepwise:
- `(20,9) -> up -> (20,8) -> left -> (19,8) -> left -> (18,8)`
- `(18,8) -> down -> (18,9) -> down -> (18,10)`
- `right` from `(18,10)` is blocked repeatedly
- backing up to `(18,8)`, the next probe triggered a wild battle before further mapping

Interpretation:
- The live state at `(20,9)` is adjacent to the known `(18,9)/(18,8)/(18,10)` upper-west corridor, not a fresh forest exit lead.
- Treat `(20,9)` as another rejoin into solved upper-west geometry, not a breakthrough.