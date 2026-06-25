// Pokémon-playing harness — vision in, button presses out, with durable memory.
// Used by the Pokémon eval (evaluation/pokemon). Each turn it reads the current
// Game Boy screenshot (written by pokemon_runner.py at a fixed path), injects it
// as an image user message plus the agent's saved memory, and asks the model for
// the next button(s) as JSON. The driver presses them in PyBoy and advances.
//
// No shell tool (the emulator lives in the Python driver), but the agent DOES get
// remember/forget: across a long session it can persist goals, the map, party
// state, etc., so progress survives even as the conversation rolls forward. The
// screenshot is injected via `instructions` (not stored), so only the CURRENT
// frame is ever in context — image cost stays flat.

import { readFileSync } from "node:fs";

import {
  defineHarness,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import { runResponsesHarnessTurn } from "../typescript/turn-loop";
import {
  memoryInstruction,
  registerMemoryTools,
} from "../exoclaw/memory-tools";

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
- Make real progress: explore, talk to NPCs, navigate menus deliberately, win battles. Advance dialogue with "a".
- Don't get stuck: if the screen didn't change after your last press, try something different (a different direction, or b). Avoid mashing the same button pointlessly.

Memory (use it — this is a long game):
- You have \`remember\` and \`forget\` tools. SAVE durable, useful facts: your current objective, where you are and where you're headed, town/route layouts and exits you've found, your party and their levels, items, and what you just accomplished. Update memory as the situation changes.
- Your saved memory is shown back to you each turn. Consult it FIRST so you keep making forward progress instead of wandering or repeating yourself.

Every turn, after optionally updating memory, you MUST end by replying with ONLY a JSON object — no prose, no code fences:
{"buttons": ["<1-3 of: up,down,left,right,a,b,start,select, pressed in order>"], "reasoning": "<one short sentence>"}`;

async function instructions(context: TurnContext): Promise<Message[]> {
  const messages: Message[] = [{ role: "developer", content: POKEMON_PROMPT }];
  const memory = await memoryInstruction(context);
  if (memory !== null) {
    messages.push(memory);
  }
  try {
    const b64 = readFileSync(SCREEN_PATH).toString("base64");
    messages.push({
      role: "user",
      content: [{ type: "image", image: `data:image/png;base64,${b64}` }],
    });
  } catch {
    // No screenshot yet — the driver writes it before each turn; fall back to text.
  }
  return messages;
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions,
      registerTools: (tools: HarnessToolRegistry) => {
        registerMemoryTools(tools);
      },
    });
  },
});
