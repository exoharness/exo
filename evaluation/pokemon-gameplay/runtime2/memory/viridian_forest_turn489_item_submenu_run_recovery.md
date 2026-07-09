Viridian Forest turn 489 battle-recovery lesson: `wild_run_recover` can falsely appear to succeed while the screen is still stuck in the ITEM submenu during a wild battle.

Verified manual recovery sequence:
- While wedged in ITEM/CANCEL, press `B, B` to back out.
- Do **not** assume the 4-option command menu is immediately ready if the screen is still on `Go! A!` or other transition text.
- Use patient battle-text progression (`wait`, then single `A`) until normal battle flow / command menu returns.
- Then choose RUN explicitly.

Outcome on turn 489:
- After `B, B, down, right, A` and paced text progression, the battle resolved back to overworld at Viridian Forest `(20,9)`.
- Main lesson is procedural: submenu recovery must prioritize backing out with `B` before any RUN-selection macro, and send-out text must be cleared before menu inputs.