// Pokémon-playing harness — vision in, button presses out. Used by the Pokémon
// eval (evaluation/pokemon). Each turn it reads the current Game Boy screenshot
// (written by pokemon_runner.py at a fixed path), injects it as an image user
// message, and asks the model for the next button(s) as JSON. The driver presses
// them in the PyBoy emulator and advances the game.
//
// No tools: the emulator lives in the Python driver, not exo's sandbox. The image
// is injected via `instructions` (not stored in the conversation), so only the
// CURRENT frame is ever in context — past frames don't accumulate, keeping image
// cost flat while the text history (past button choices) carries memory forward.

import { readFileSync } from "node:fs";

import { defineHarness, type Message, type TurnContext } from "@exo/harness";

import { runResponsesHarnessTurn } from "../typescript/turn-loop";

// Fixed path shared with pokemon_runner.py (keep in sync). One game per host.
const SCREEN_PATH = "/tmp/exo-pokemon/screen.png";

const POKEMON_PROMPT = `You are playing Pokémon on a Game Boy by looking at the screen and choosing button presses. The image is the current 160x144 screen.

Controls:
- d-pad: "up" "down" "left" "right" move the cursor / your character.
- "a": confirm, talk, interact, advance dialogue, select a menu item.
- "b": cancel / back / close a menu / speed through text.
- "start": open the main menu. "select": rarely used.

How to play well:
- Read the screen carefully: are you in the overworld, a dialogue box, a menu, or a battle? Act accordingly.
- Make real progress: explore, talk to NPCs, navigate menus deliberately, and win battles. Advance dialogue with "a".
- Your previous turns (what you saw and pressed) are in this conversation — use them to keep track of where you are and what you were doing. Do NOT mash the same button pointlessly; if nothing changed, try something different.

Respond with ONLY a JSON object — no prose, no code fences:
{"buttons": ["<1-3 of: up,down,left,right,a,b,start,select, pressed in order>"], "reasoning": "<one short sentence>"}`;

async function instructions(_context: TurnContext): Promise<Message[]> {
  const messages: Message[] = [{ role: "developer", content: POKEMON_PROMPT }];
  try {
    const b64 = readFileSync(SCREEN_PATH).toString("base64");
    messages.push({
      role: "user",
      content: [{ type: "image", image: `data:image/png;base64,${b64}` }],
    });
  } catch {
    // No screenshot yet (shouldn't happen — the driver writes it before each
    // turn). Fall back to text-only; the driver will retry.
  }
  return messages;
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions,
      registerTools: () => {},
    });
  },
});
