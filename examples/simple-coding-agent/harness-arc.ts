// ARC-AGI harness — pure reasoning, NO tools. Used by the ARC-AGI eval
// (evaluation/arc-agi). The model infers the hidden grid transformation from the
// train pairs and emits the predicted output grid(s) as JSON in its final
// assistant message; arc_runner.py parses and scores it.
//
// No shell tool by design: ARC needs no environment, and — since the public eval
// answers live as JSON on the host — a tool-less agent simply cannot read them.
// Lives here beside harness.ts so "@exo/harness" + ../typescript/turn-loop
// resolve identically to the other harnesses.

import { defineHarness, type Message, type TurnContext } from "@exo/harness";

import { runResponsesHarnessTurn } from "../typescript/turn-loop";

const ARC_PROMPT = `You are an expert at ARC-AGI abstract-reasoning puzzles. Each puzzle gives several INPUT -> OUTPUT grid examples that all share ONE hidden transformation rule. Grids are rectangles of integers 0-9 (each integer is a color).

How to solve:
- Study ALL training examples together and infer the single rule that maps every input to its output. Consider: grid resize/crop/tiling, symmetry and reflection, rotation, translation/gravity, color remapping, object detection and counting, filling/bordering, pattern completion, and per-cell logic.
- Apply that exact rule to each TEST input to produce its output grid. Get the dimensions right first, then the cell values.
- Sanity-check your rule against every training pair before committing — if it fails any, revise it.

Output format (critical):
- Respond with ONLY a JSON object, no prose and no code fences:
  {"outputs": [<grid for TEST 1>, <grid for TEST 2>, ...]}
- One grid per TEST input, in order. Each grid is a list of rows; each row is a list of integers 0-9. Output exactly the grid(s) — nothing else.`;

async function instructions(_context: TurnContext): Promise<Message[]> {
  return [{ role: "developer", content: ARC_PROMPT }];
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions,
      // No tools registered: pure reasoning. The predicted grid(s) come back as
      // the final assistant message (JSON), which arc_runner.py extracts.
      registerTools: () => {},
    });
  },
});
