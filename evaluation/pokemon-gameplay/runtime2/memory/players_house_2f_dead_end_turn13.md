Player's House 2F appears to be a dead-end/corrupted wedge in this run state.

Verified again on turn 13:

- From clean free-movement state at (3,7) facing down, probe_path confirmed only a 2-row room: x=1..3..5 and y=6..7 are reachable.
- `down` from (3,7) returns no movement and no warp.
- `up` to (3,6) works; TV/SNES at (3,6) is interactable.
- Pressing A at the TV shows the usual two-line inspect text (`AAAA is playing the SNES!` and `...Okay! It's time to go!`).
- B cleanly dismisses the text back to the same unwedgeable room.
- Trying unusual non-movement buttons from the clean TV-facing state (`a`, `b`, `start`, `select` with long waits) only reopened or advanced the SNES text; no hidden event or transition occurred.

Conclusion: treat this state as non-progressing/corrupted rather than as a normal room puzzle. Best next step is to abandon this state entirely (checkpoint/load if any exist, otherwise wait for harness reset or restart from boot) rather than spend more turns reproving the same room.
