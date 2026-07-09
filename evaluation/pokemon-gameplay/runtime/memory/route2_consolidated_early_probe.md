Route 2 early probe consolidation: real progression is north from Viridian, but the first explored west/top grass area is partly a trap.

Verified layout so far:
- Entry from Viridian north puts the player around `(7,71)` on map `0x0d`.
- The central path is open north at least to about `(7,57)`.
- From `(7,57)`, going left reaches a west-side lane.
- Taking that west lane north works to about `(3,48)`.
- At the top-west edge, `north` is blocked. Moving right from `(3,48)` reaches about `(7,48)`, but farther right is blocked.
- Around `(8,48)`, moving `down` is also blocked, so this top patch is not a clean through-route.

Encounter / transition notes:
- Grass encounters here can trigger with misleading partial-overworld or black-screen transition artifacts before the battle fully renders.
- Do not interpret those artifacts as a softlock; check battle flag and advance with `A` if battle text is present.
- On turn 100, Squirtle leveled to 9 here; `grew to level 9!` was still battle text even though it looked like the fight was basically over.

Practical takeaway:
- Stop looping this same top grass patch.
- On return to play, clear current battle text, then retreat or re-approach from a lower/surer Route 2 position and inspect for a different lane, gatehouse, or Viridian Forest entrance.