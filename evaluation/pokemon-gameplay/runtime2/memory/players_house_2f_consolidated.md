Player's House 2F consolidated wedge notes: current clean state is at (3,7) facing down with no textbox; bed left, TV/desk above, plant right, and no obvious visible stair graphic on the bottom edge.

## Verified observations

- Movement within the room works normally when no textbox is open.
- Walkable area discovered so far is roughly x=1..5, y=6..7.
- Bed on left and plant on right are solid obstacles.
- TV/desk area above is solid/interactable; pressing A there can produce the SNES/"Okay! It's time to go!" messages.
- START menu opens/closes normally, so controls are not frozen.
- From bottom row positions including center (3,7), repeated Down presses do not move or warp.
- Waiting in place and long-hold Down also do not trigger anything.

## Implications

- This is not merely a lingering-dialog problem.
- This is not a simple timing issue.
- The expected downstairs warp either is not on the apparent bottom-center tile, requires a different approach/interaction, or this map/state differs from standard expectations.

## Recommended next experiments

1. Use A to inspect floor/room features from several facings, especially around center-bottom and near TV/desk.
2. Re-approach the center-bottom tile from left/right columns and from the upper row.
3. Treat the screenshot layout literally instead of assuming standard house geometry.
4. If still impossible, gather a more explicit coordinate-to-screen map for every reachable tile.
