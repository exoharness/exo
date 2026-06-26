// Self-improving Pokémon harness — vision in, button presses out, PLUS the agent
// can extend and rewrite itself. Used by the Pokémon eval (evaluation/pokemon)
// in --self-improve mode.
//
// On top of the base loop (read the current screenshot, emit {"buttons":[...]}),
// this gives the agent exoclaw-style self-control, scoped to the game:
//   • durable memory (remember/forget),
//   • capability extension: it can write NEW tools with install_agent_tool; they
//     persist in .exo/agent-tools and reload every turn,
//   • policy self-edit: it has a shell, and its OWN harness source is mounted
//     read-write in the sandbox — it can rewrite this file to change how it plays;
//     edits take effect on the next turn. The runner validates the harness each
//     turn and rolls back to the last working version if an edit won't load.
//
// THE ONE CONTRACT TO PRESERVE if you (the agent) edit this file: still read the
// screenshot at SCREEN_PATH, still end every turn with the JSON buttons object.
// The Python driver presses those buttons in the emulator.

import { readFileSync } from "node:fs";

import {
  defineHarness,
  registerBuiltInTools,
  registerAgentToolsFromDirectoryIfExists,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import {
  runResponsesHarnessTurn,
  agentToolCreationInstruction,
} from "../typescript/turn-loop";
import {
  memoryInstruction,
  registerMemoryTools,
} from "../exoclaw/memory-tools";

// Fixed paths shared with pokemon_runner.py / live_server.py (keep in sync).
const SCREEN_PATH = "/tmp/exo-pokemon/screen.png";
const GUIDANCE_PATH = "/tmp/exo-pokemon/guidance.json";
// This harness file as seen from inside the sandbox (mounted read-write). The
// agent edits THIS path to rewrite its own policy.
const SELF_PATH_IN_SANDBOX = "/workspace/agent/harness-pokemon-self.ts";

const POKEMON_PROMPT = `You are an AI agent PLAYING Pokémon on a Game Boy, and you can improve yourself as you play. Each turn you see the current 160x144 screen and choose button presses.

Controls: d-pad "up"/"down"/"left"/"right" move; "a" confirms/talks/advances dialogue; "b" cancels/closes/speeds text; "start" opens the menu.

Play well: read the screen (overworld? dialogue? menu? battle?) and act. Make real progress — explore, talk, navigate menus, win battles. If the screen didn't change after your last press, try something different; don't mash pointlessly.

YOU CAN IMPROVE YOURSELF (use this when it would help you play better):
1. MEMORY — \`remember\`/\`forget\`. Save durable facts: current objective, where you are/headed, party + levels, items, map layout, what just happened. Consult it first each turn.
2. NEW TOOLS — use \`install_agent_tool\` to write yourself a new TypeScript tool when a helper would make you play better (e.g. a tool that reads the screenshot file and extracts a tile grid, tracks visited coordinates, or plans a path). Installed tools persist and are available next turn. Remove ones that don't help with \`uninstall_agent_tool\`.
   STRICT SCHEMA RULE (or the tool is rejected and breaks every later turn): the tool's parameters MUST be a JSON Schema object with "additionalProperties": false AND a "required" array that lists EVERY key in "properties". If a parameter is optional, still list it in required and allow null in its type. Keep parameters minimal.
3. SELF-EDIT YOUR POLICY — you have a \`shell\`, and your own harness source is mounted read-write at ${SELF_PATH_IN_SANDBOX}. You can literally rewrite how you play.
   DO THIS when your approach is failing — looping, repeating the same buttons, or stuck in one spot for many turns: don't just keep pressing buttons. Read your harness (\`cat ${SELF_PATH_IN_SANDBOX}\`), then rewrite the POKEMON_PROMPT text in it to bake in a sharper, concrete strategy (an explicit procedure for what you're stuck on, or the heuristics you've learned), and write the file back with the shell. Edits take effect NEXT turn — it's your most powerful improvement lever, so use it deliberately at least once when you're stuck rather than looping.
   CRITICAL when editing: keep the plumbing — it must still readFileSync(SCREEN_PATH) and inject the image, and still make you output the JSON buttons object. Change the PROMPT/strategy text, not the mechanics. If your edit fails to load, the system restores the last good version and tells you, so small deliberate experiments are safe.

BE PROACTIVE about self-improvement — it's expected, not optional. Early on, save your current objective to memory. When you notice you're repeating actions, stuck, or wishing you had a capability (e.g. "I keep losing track of where I've explored"), don't just keep pressing buttons — CREATE a tool or IMPROVE your policy to fix it. A good agent visibly gets better at the game over time.

Every turn, after any self-improvement, you MUST end by replying with ONLY this JSON (no prose, no fences):
{"buttons": ["<1-3 of: up,down,left,right,a,b,start,select, in order>"], "reasoning": "<one short sentence>"}`;

async function instructions(context: TurnContext): Promise<Message[]> {
  const messages: Message[] = [
    { role: "developer", content: POKEMON_PROMPT },
    { role: "developer", content: agentToolCreationInstruction().content },
  ];
  const memory = await memoryInstruction(context);
  if (memory !== null) messages.push(memory);
  try {
    const g = JSON.parse(readFileSync(GUIDANCE_PATH, "utf8"));
    if (g && typeof g.text === "string" && g.text.trim()) {
      messages.push({
        role: "developer",
        content: `📣 LIVE DIRECTION FROM THE PLAYER (a human watching just sent this — prioritize it): ${g.text.trim()}`,
      });
    }
  } catch {
    /* no guidance */
  }
  try {
    const b64 = readFileSync(SCREEN_PATH).toString("base64");
    messages.push({
      role: "user",
      content: [{ type: "image", image: `data:image/png;base64,${b64}` }],
    });
  } catch {
    /* no screenshot yet */
  }
  return messages;
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions,
      registerTools: async (tools: HarnessToolRegistry, ctx: TurnContext) => {
        registerBuiltInTools(tools, ctx, [
          "shell",
          "install_agent_tool",
          "uninstall_agent_tool",
        ]);
        registerMemoryTools(tools);
        await registerAgentToolsFromDirectoryIfExists(tools, ctx);
      },
    });
  },
});
