Viridian Forest turn 330 consolidation: the top-left edge / upper-connector family is stale solved geometry.

Recent re-probes from `turn325_forest_18_6_fresh` established:
- At `(18,4)` and `(18,3)`, moving `right` is blocked, so there is no hidden east continuation from the top-left descent pocket.
- At `(16,2)` and `(16,1)`, moving `left` is blocked; this is the true accessible left edge of the top row.
- Dropping south from that top-left edge only returns to the known `(16,5)` / `(17,5)` connector family.
- Therefore live states around `(16,5)` are not fresh exploration targets; they belong to the already-classified top-row / upper-connector / sign-pocket loop system.

Practical directive:
- On future action turns, if the live state is around `(16,5)`, rewind first instead of probing from there.