Route 2 north-side outdoor pocket (`map 0x33`) consolidated through turn 170: the true challenge is layout discovery, not a missed obvious north gate.

Key verified structure:
- Enter from the south gatehouse north exit at about `(17,47)`.
- Early west/start area near `(6,30)` is mostly boxed in above; west reaches roughly `(2,30)`.
- Meaningful progress comes from the east/lower lanes, not the apparent lower fence gaps on main Route 2 and not the first top row you see.
- A lower/east route reaches at least `(14,43)` where an NPC says `They're out for POKEMON fights!`
- Mid-right route opens to `(19..23,40..43)` and farther east to around `(29,41)`.
- At about `(28,40)` facing left, an NPC gives the hint `You should carry extras!`
- The nearby sign text is:
  - `TRAINER TIPS`
  - `No stealing of POKEMON from other trainers!`
  - `Catch only wild POKEMON!`
  - `stay away from grassy areas!`
- A trainer at about `(26,33)` is a Bug Catcher who auto-engages.
- From about `(25,34)`, a real upper lane climbs north through the map to around `(25,20)` and then into the upper pocket.
- Another upper sign near `(26,18)` is `TRAINER TIPS` ending with `...your POKEDEX evaluated!`

Most important negative result:
- The entire top outside row around `y=1` is a dead boundary across all tested segments, including roughly `x=6..32`.
- This includes:
  - upper-center/right clearing near `(25,8)`
  - upper-left route reaching `(11,3)`
  - top-row walking across about `x=6..13` at `y=1`
- Pressing `up` there does **not** enter Viridian Forest. The visible fence/tree/sign clusters along that top edge are misleading.

Other boundary notes:
- North is also blocked at least at `(22,40)`, `(23,40)`, `(27,41)` on the mid-right band.
- Far-right boundary caps around `x=32`, though a descending lane exists on that side.
- Around the upper-left/center, `(16,10)` left is blocked; this area is not an immediate hidden entrance either.

Battle/interaction notes tied to this area:
- Wild encounters frequently trigger in grass during probing; fleeing is usually the right choice unless a trainer battle is forced.
- Post-battle white screen is normal; a short `wait` restores the overworld.

Actionable next-step synthesis:
- If resuming from the upper-left/top-row area (turn 170 starts around `(8,1)` facing up), do **not** press `up` again.
- The best unexplored value is to move back down from the top row into center-left lanes and test for a hidden continuation or west/center opening, rather than re-checking the top boundary.