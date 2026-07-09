Route 2 north-side Bug Catcher + battle timing notes: the overworld trainer around `(26,33)` in the Route 2 / Viridian Forest south-side outdoor pocket is hostile and starts a Bug Catcher battle when talked to. Battle sequence observed: first enemy Weedle Lv6, then Caterpie Lv6. Squirtle leveled to 10 during the fight. Important battle handling lessons from this trainer:

- Visible move order in Squirtle's move list is **TACKLE / TAIL WHIP / BUBBLE**.
- If the move cursor is on Tail Whip, `down, A` selects Bubble.
- Blind multi-inputs are risky; one mistake used Tail Whip on Caterpie.
- After `used BUBBLE!`, the game can sit on the move text/animation for a long time. The reliable pattern was **wait first, then use single A presses with generous settle**, not repeated mash.
- Enemy move/result text can chain across multiple boxes and look odd (for example Weedle text resolving as `attack missed! But, it failed!`). Treat these as ordinary text boxes and advance carefully.
- Trainer battles on this approach should be handled manually rather than with wild flee/mash routines.

Live state at start of turn 130: still in the same trainer battle versus Caterpie Lv6, Squirtle Lv10 at 9/30 HP, command menu visible.
