Route 2 north-side outdoor pocket (`map 0x33`) center-left corridor + trainer state through turn 180: from the upper-left/top-row area, the useful new progress is a descent into center-left lanes, not more `up` presses on the dead top row.

Verified movement / layout

- A descent path exists roughly on column `x=8`, from the upper-left area down to at least `y=21`.
- At `(6,13)` and `(6,16)`, `left` is blocked.
- At `(8,15)` and `(8,23)`, `right` is blocked.
- Around `y=21`, left works from `(8,21)` to `(6,21)`, but further left is blocked there.
- Lower-left continuation exists: `(6,21)` downward to about `(6,23)`, then east/west connections lead through `(3,23)`.
- Near the center-left trainer area:
  - `(2,20) -> up` reaches `(2,19)`, but another `up` is blocked.
  - `(2,19) -> left` reaches `(1,19)`.
  - `(2,19) -> right` is blocked.
  - `(3,23) -> up` reaches `(3,22)`, but another `up` is blocked.

Trainer / dialog

- The apparent object when facing up at about `(2,19)` is a Bug Catcher trainer, not scenery.
- Defeating him required careful `wait` + single-`A` cadence because early battle text lagged badly.
- Squirtle beat his Weedle and reached Lv12.
- At the end of turn 180's start state, the player is still standing at `(2,19)` facing up with the trainer's post-battle textbox open. Visible text begins:
  - `I'm looking for the ...`
- Important: even with `battle: none`, this textbox must be cleared before movement testing; otherwise movement inputs will be wasted.

Practical next-step note

- Next live turn should first clear the trainer's overworld text, then probe from this center-left corridor for any west/center passage rather than returning to the known-dead top edge.
