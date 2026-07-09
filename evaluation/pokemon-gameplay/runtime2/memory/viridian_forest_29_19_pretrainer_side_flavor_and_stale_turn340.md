Viridian Forest turn 340 consolidation: the Bug Catcher side tile at `(29,19)` facing down in map `0x33` is part of the already-solved trainer/sign pocket, not fresh progress.

Verified across turns 334, 338, and 339:
- From `(28,18)` / `(29,18)`, stepping to `(29,19)` puts the player beside the Bug Catcher.
- Talking from that side position gives only overworld flavor text; it does not trigger a trainer battle from that tile.
- Nearby movement around `(25,18) -> (28,18) -> (29,19) -> (29,18) -> (30,18)` stays inside the known trainer/sign pocket.
- This family reconnects to the stale rejoin hub around `(30,18)` and should not be treated as a fresh branch.

Action rule: if live state is `(29,19)` or adjacent side-facing Bug Catcher tiles in this pocket, rewind before spending an action turn.