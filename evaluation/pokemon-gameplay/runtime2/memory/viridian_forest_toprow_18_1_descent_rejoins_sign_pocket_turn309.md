Viridian Forest turn 309: the apparent fresh top-row descent from `(18,1)` is real but non-progressing.

- Starting from checkpoint `turn283_forest_18_1_toprow`, the confirmed descending path is:
  - `(18,1) -> (18,2) -> (19,2) -> (19,3) -> (18,3) -> (18,4) -> (17,4) -> (17,5)`
- This justified creating checkpoint `turn309_forest_17_5_newdrop` because the branch looked genuinely different from prior far-right top-row drops.
- However, continuing the probe from that newdrop state eventually reconnects to the already-known Bug Catcher / sign pocket rather than producing an exit or fresh corridor.
- Practical lesson:
  - treat the `(18,1)` center-left descent as classified
  - it is different geometry from the far-right drop, but strategically it is still a loop
  - future top-row work should not spend more turns descending there unless testing a clearly untried side split immediately near `(17,5)`.
- Also note: current live states around `(16,10)` / `(16,12)` belong to the same stale upper-west connector family and are worse exploration anchors than `turn309_forest_17_5_newdrop` or other clean checkpoints.