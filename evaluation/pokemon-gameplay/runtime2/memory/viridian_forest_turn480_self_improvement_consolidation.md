Viridian Forest turn 480 self-improvement consolidation.

Recent re-learnings from turns 475-479:
- The live state at `(26,32)` is not fresh progress. It is the already-known antidote/signlane family re-entered from the `(22,42)` open-area anchor.
- The west-corridor family around `(6,39)` has now been micro-probed enough to classify locally: left is blocked at `(6,39)` and `(6,37)`, east is blocked at `(8,38)`, and the short motions there only form a tiny local lane around `(6..8,37..39)`. It should not be a primary progression target unless a very different adjacent anchor is found.
- The fresh post-Kakuna family at `(15,39)` / `(18,37)` looked promising but quickly rejoined the central lane and the old antidote sign at `(16,33)`. Treat that family as stale.
- The checkpoint `turn476_forest_22_42_open_area_fresh` is still the best recent rewind target among current forest options, but one branch from it already proved stale: climbing east eventually reaches `(26,32)` signlane geometry.
- Therefore next exploration should not continue live from `(26,32)` and should not repeat the `(15,39)` / `(18,37)` post-Kakuna family. Rewind to a fresher anchor before probing again.