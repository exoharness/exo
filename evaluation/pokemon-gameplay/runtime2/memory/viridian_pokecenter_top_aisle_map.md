Viridian Pokecenter top-aisle interaction map: right counter at (11,3) is Cable Club, not healing.

State this was mapped from: Viridian Pokecenter map 0x29, low-HP Squirtle, probing the top aisle near the counter.

Verified interaction points:

- `(7,3)` facing left: talks to the left-side top NPC.
- `(9,3)` facing right: talks to the right-side male NPC. Text includes lines like `The receptionist told me...` and advice about using the PC in the corner.
- `(9,3)` facing up: no useful service interaction found.
- `(10,4)` facing up: also talks to the right-side male NPC, not a healer.
- `(11,3)` facing up: talks to the Cable Club receptionist, with text:
  - `Welcome to the Cable Club!`
  - `We're making preparations. Please wait.`
    This does NOT heal.

Critical lesson:

- Near adjacent NPCs/counters, repeated A-mashing can make it look like text is infinite because the final A after closing a textbox immediately re-talks to the same NPC.
- Use single A presses and clear with controlled small A/B batches before moving.

Best next inference:

- The actual Nurse Joy / healing desk should be left of the Cable Club position. Next action when play resumes should be to move left along the top aisle from `(11,3)` and test upward-facing interactions one tile at a time.
