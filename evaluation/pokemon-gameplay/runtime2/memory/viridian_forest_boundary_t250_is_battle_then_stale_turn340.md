Viridian Forest turn 340 checkpoint note: `boundary_t250` is not a good exploratory action-turn starting point.

Observed repeatedly on turns 331 and 339:
- Loading `boundary_t250` drops immediately into a wild Weedle battle around the upper-connector area near `(20,8)`.
- The wild battle can be escaped safely with the standard forest wild-run recipe.
- After the escape, overworld resumes in the already-solved upper-west / upper-connector family (e.g. around `(18,8)` / `(20,8)`), which is stale geometry.

Practical guidance:
- Do not choose `boundary_t250` as the preferred fresh rewind target for forest progress.
- Only use it if a battle-state recovery test is specifically needed.
- Prefer `turn325_forest_18_6_fresh`, `turn309_forest_17_5_newdrop`, `turn299_forest_16_12_start`, `turn297_forest_18_13_loaded`, or other listed branch checkpoints instead.