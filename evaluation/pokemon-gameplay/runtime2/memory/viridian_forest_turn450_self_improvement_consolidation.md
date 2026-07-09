Viridian Forest turn 450 self-improvement consolidation: recent repeats show the clean lower hub at `(23,43)` is still a useful anchor, but its most tempting routes are now classified.

Key recent lessons
- From `turn387_forest_23_43_openlane_start`, the route `up, up, right, right, right, right` reaches the lower Bug Catcher at about `(27,41)`.
- That Bug Catcher is only a local blocker / flavor NPC; `up` there is blocked by the sprite, so this line should only be used for tiny local checks, then rewound.
- Broad northeast pushes from `(23,43)` repeatedly rejoin the stale east-rim wedge around `(29..32,40..41)`.
- The once-suspicious `turn446_forest_26_28_newcorridor` family also collapsed: after the Weedle interruption was escaped, the post-battle overworld at about `(25,24)` proved to be the old upper-east vertical-corridor rejoin, not fresh progress.

Practical directive
- Next exploration turn should avoid spending many actions on the `(23,43) -> Bug Catcher` line, the broad `(23,43) -> east rim` line, and the `turn446_forest_26_28_newcorridor` family.
- Prefer either one very specific micro-test from a remaining lower anchor, or a genuinely different checkpoint/family entirely.