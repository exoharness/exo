// ARC-AGI shell harness — program synthesis WITHOUT persistence.
//
// The ablation arm between harness-arc.ts (no tools at all) and
// harness-arc-evolve.ts (shell + memory + self-authored tools + persistent
// sandbox): this one gets the same sandbox shell and the same
// verify-on-train-pairs discipline, but each task runs in a fresh exo root —
// no memory, no tool creation, nothing carries over. Isolates the value of
// the verification loop from the value of persistence.

import {
  defineHarness,
  registerBuiltInTools,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import {
  runResponsesHarnessTurn,
  defaultBuiltInToolNames,
} from "../typescript/turn-loop";

const ARC_SHELL_PROMPT = `You are an expert ARC-AGI puzzle solver. Each puzzle gives several INPUT -> OUTPUT grid examples sharing ONE hidden transformation rule; you must produce the output grid(s) for the held-out TEST input(s). Grids are rectangles of integers 0-9 (colors). Scoring is exact match.

HOW TO SOLVE:
1. The task message contains the puzzle as a fenced JSON block: {"train": [{"input", "output"}...], "test": [{"input"}...]}. FIRST, save it to a file in your sandbox (e.g. cat > /work/task.json <<'EOF' ... EOF) so you can work programmatically instead of eyeballing large grids.
2. Study the train pairs and hypothesize the rule. Consider: resize/crop/tiling, symmetry/reflection/rotation, translation/gravity, color remapping, object detection/counting/movement, filling/bordering, occlusion repair, pattern completion, per-cell logic, grid-of-grids selection.
3. WRITE THE TRANSFORM AS CODE (python3) in your sandbox and RUN IT AGAINST EVERY TRAIN PAIR. Do not trust a rule you have not verified: if it fails any train pair, revise and re-verify. Only when it reproduces ALL train outputs exactly, apply it to the test input(s).
4. If after honest effort no verified rule emerges, give your best two distinct guesses.

FINAL ANSWER FORMAT (critical): end the turn with ONLY a JSON object, no prose, no code fences:
  {"outputs": [<grid per TEST input, in order>], "outputs2": [<optional second-attempt grid per TEST input>]}
Each grid is a list of rows of integers 0-9. "outputs2" is your second candidate (pass@2) — include it when you have a plausible alternative; omit it or repeat outputs if you don't.`;

async function instructions(_context: TurnContext): Promise<Message[]> {
  return [{ role: "developer", content: ARC_SHELL_PROMPT }];
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions,
      registerTools: (tools: HarnessToolRegistry, ctx: TurnContext) => {
        registerBuiltInTools(tools, ctx, defaultBuiltInToolNames(ctx));
      },
    });
  },
});
