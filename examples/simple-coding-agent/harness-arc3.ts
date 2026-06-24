// ARC-AGI-3 harness — pure reasoning, NO tools. Used by the ARC-AGI-3 eval
// (evaluation/arc-agi-3). ARC-AGI-3 is INTERACTIVE: the agent is dropped into a
// game with no instructions and must infer the rules/goal by acting and observing
// over many steps. Our Python agent (exo_arc_agent.py, an arcengine Agent) keeps
// ONE exo conversation per game and, each step, sends the current grid frame +
// the legal actions; exo replies with the single next action as JSON. This file
// is the system prompt + the no-tool turn loop; the per-step frame arrives as the
// conversation message. Lives beside the other harnesses so imports resolve.

import { defineHarness, type Message, type TurnContext } from "@exo/harness";

import { runResponsesHarnessTurn } from "../typescript/turn-loop";

const ARC3_PROMPT = `You are playing an ARC-AGI-3 interactive reasoning game. You start with NO instructions, NO stated goal, and NO rules — you must figure them out by acting and watching how the grid changes, like a person handed an unfamiliar game.

Each turn you are shown the current game state: a grid of integers (colors), the game state, your level progress, and the set of actions that are legal right now. Choose the SINGLE best next action.

Strategy:
- Build a theory of the rules from how the grid responds to your actions, and update it every turn. Your past turns are in this conversation — use them. Don't repeat actions that did nothing; probe deliberately to test hypotheses, then exploit what works to complete levels.
- Actions: ACTION1..ACTION7 are the game's controls (their meaning is for you to discover). ACTION6 is a pointer/click and needs grid coordinates x and y, each 0-63 (x = column, y = row). RESET restarts. Only pick from the actions listed as available this turn.

Output (critical): respond with ONLY a JSON object, no prose, no code fences:
{"action": "ACTION1|ACTION2|ACTION3|ACTION4|ACTION5|ACTION6|ACTION7|RESET", "x": <int 0-63, only for ACTION6>, "y": <int 0-63, only for ACTION6>, "reasoning": "<one short sentence>"}`;

async function instructions(_context: TurnContext): Promise<Message[]> {
  return [{ role: "developer", content: ARC3_PROMPT }];
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions,
      // No tools: the agent reasons about the frame and returns one action as
      // JSON in its final message; exo_arc_agent.py parses it into a GameAction.
      registerTools: () => {},
    });
  },
});
