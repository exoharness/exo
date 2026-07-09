Viridian Forest turn 440 consolidation: many late lower-area branches were real local geometry but not progression, so future probing should triage them aggressively.

Key late confirmations:
- `turn431_forest_29_33_npcside` climbs back into the stale `(27,26..34)` trainer/sign family; it is only a branch-test anchor, not a main lead.
- `turn376_forest_27_43_trainers_area` dry-road north/east probing rejoins the stale east-rim family around `(29..31,40)`.
- `turn403_forest_28_40_npcside` southeast extension to `(31,43)` also collapses back into east-rim geometry.
- `turn403_forest_29_41_grassedge` and `turn387_forest_23_43_openlane_start` have repeatedly rejoined signfork or east-rim local loops.
- `turn417_forest_18_35_retry2` / live `(18,32)` sit in the stale upper-left sign-row choke.

Practical planning lesson:
1. Prefer tiny branch tests from the remaining lower anchors first.
2. If a probe reaches east-rim states like `(30..32,40..43)`, signfork `(24..26,40..41)`, or trainer/sign rejoin areas like `(27,26..34)` / `(30,18)`, record it and rewind immediately.
3. Only after exhausting those lower anchors should work resume from tertiary older families like `turn365_forest_17_40_fresh` near current live `(16,40)`.