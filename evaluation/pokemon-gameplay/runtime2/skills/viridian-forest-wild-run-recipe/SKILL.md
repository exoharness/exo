---
name: viridian-forest-wild-run-recipe
description: Safely escape common Viridian Forest wild battles by identifying the battle phase first, then using the correct cadence instead of blind mashing.
---
# Viridian Forest wild-run recipe

Use this whenever a routine wild battle interrupts forest mapping and the goal is to preserve HP and return to overworld quickly.

## Core rule
Do **not** treat all battle screens the same. First identify which phase you are in from the screenshot/text.

## Phase-based procedure

### 1) Intro text phase
Examples:
- `Wild WEEDLE appeared!`
- `Wild PIKACHU appeared!`

Action:
- Press `A` once to advance.
- Then reassess the screen.

### 2) Send-out / transition phase
Examples:
- `Go! A!`
- The enemy sprite is visible but your command menu is not.
- The screen looks like battle is still animating.

Action:
- **Do not send RUN/menu inputs yet.**
- Wait briefly, then press a single `A`.
- Reassess.
- Repeat patiently until the 4-option battle menu is genuinely present.

Reason:
- Many failed escapes came from treating `Go! A!` like the command menu.

### 3) Command menu phase
You should see the normal 4-option battle menu.

Action:
- Select **RUN** explicitly.
- Common reliable input from the FIGHT default is `down, right, A`.
- If already at the 4-option menu but cursor position is uncertain, `right, A` or `down, A` may still work depending on current position.

### 4) Move menu mistake phase
If your RUN selection accidentally opened the move list:
- Press `B` once to return to the main 4-option menu.
- Then use explicit RUN selection again.

### 5) ITEM submenu wedge phase
If the battle is stuck in ITEM/CANCEL or another submenu:
- Press `B, B` to back out.
- Reassess; do not assume the 4-option menu is immediately ready.
- If the screen is still `Go! A!` or another transition text, return to the send-out / transition procedure above.
- Only select RUN once the real command menu is back.

This was the key lesson from turn 489.

## After selecting RUN
If escape succeeds and the textbox says `Got away safely!`:
- Press `A` to clear the text.
- Often one more `A` is needed.
- Then wait briefly for the white transition back to overworld.
- Confirm `battle: none` before issuing movement.

## If text/animation feels slow
Use patience:
- wait ~60–90 frames
- then single `A`

This is safer than multi-input mashing during Bubble animations, enemy move text, or send-out lag.

## Practical summary
1. `Wild X appeared!` -> `A`
2. `Go! A!` -> wait, then single `A`
3. 4-option menu -> `down, right, A` for RUN
4. If move menu -> `B`, then RUN
5. If ITEM submenu -> `B, B`, then return to phase detection
6. `Got away safely!` -> `A`, maybe `A`, then short wait

## Tool preference
- Prefer `early_wild_run_opening` when the fight is still in the opening phases.
- Prefer `wild_run_recover_safe` for submenu or uncertain substates.
- Treat older `wild_run_recover` as less trustworthy for ITEM-submenu wedges.