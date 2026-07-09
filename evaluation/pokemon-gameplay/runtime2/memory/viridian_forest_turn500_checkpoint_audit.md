Viridian Forest turn 500 checkpoint-family audit: current live state `(17,11)` is stale and only exists because `turn499_forest_17_11_newlane_start` was reloaded after documenting a failed probe. Next action turn should rewind immediately instead of exploring from live.

Key exhausted families as of turn 500:
- Lower-west `turn459_forest_10_42_clearing`: real local progress but only reaches the south-gate backtrack and blocked upper rim; no north exit found.
- South sign / gatehouse family: `(18,46) -> left,left,down,down` is confirmed south gatehouse backtrack; outside doorway alignment also useless.
- West corridor `(6,39)` family: tiny local lane only.
- Post-Kakuna `(15,39)` / `(18,37)` family: rejoins antidote sign lane at `(16,33)`.
- `turn476_forest_22_42_open_area_fresh`: exhausted; east climb rejoins `(26,32)`, right/NPC climb rejoins `(26,30)`, west/south perimeter is only tiny `(17..21,42..43)` pocket.
- Old upper families: `(18,13)`, `(18,6)`, `(20,8)`, `(18,11)`, and `(17,11)` all collapse back into top-center / upper-west microloops.
- `turn491_forest_18_12_postpikachu`: tiny upper-west connector loop only.
- `turn404_forest_17_44_trainer_below`: rejoins lower sign/bug-catcher pocket at `(20,41)`.
- Vertical corridor around `(26,24..26)`: north rejoins sign + Bug Catcher pocket; south reconnects to lower trainer area near `(25,35)` / `(27,35)`.

Practical lesson:
- The next real bottleneck is checkpoint selection, not battle handling.
- On action turns, classify candidate checkpoints by family first; do not continue from the current live tile by default.