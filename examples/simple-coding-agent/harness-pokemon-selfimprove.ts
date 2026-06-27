// Self-evolving Pokémon harness — ONE self-contained file that is the agent's
// entire brain: how it plays (POKEMON_PROMPT), what it perceives (instructions),
// and the tools it can call (INLINE_TOOLS), all here. The agent (or an outer
// evolve loop) rewrites ANY part of THIS file to improve itself; the runner
// re-imports the file every turn (hot-swap) and rolls back if an edit won't load.
//
// DESIGN PRINCIPLE: no Pokémon knowledge baked in. The harness only shows the
// screen (+ the previous frame, so the agent can see what its last action did)
// and gives generic self-improvement machinery: durable memory, a shell, its own
// source mounted read-write, and tools it defines INLINE in this file. How to
// read the screen, navigate, and fight is for the agent to figure out and encode
// here itself.
//
// THE ONE CONTRACT to preserve if you (the agent) edit this file: still show
// yourself the screenshot each turn, and still END every turn with the JSON
// buttons object. The Python driver presses those buttons in the emulator.

import { readFileSync } from "node:fs";

import {
  defineHarness,
  registerBuiltInTools,
  registerAgentTools,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";
import type { Tool } from "@exo/harness/tool";

import { runResponsesHarnessTurn } from "../typescript/turn-loop";
import {
  memoryInstruction,
  registerMemoryTools,
} from "../exoclaw/memory-tools";

// Fixed paths shared with pokemon_runner.py / live_server.py (keep in sync).
const SCREEN_PATH = "/tmp/exo-pokemon/screen.png";
const PREV_SCREEN_PATH = "/tmp/exo-pokemon/prev_screen.png"; // frame before your last action
const GUIDANCE_PATH = "/tmp/exo-pokemon/guidance.json"; // optional live human channel
// This file as seen from inside the sandbox (mounted read-write). You edit THIS
// path to rewrite your policy AND your tools — it's all one file.
const SELF_PATH_IN_SANDBOX = "/workspace/agent/harness-pokemon-self.ts";

// ───────────────────────────────────────────────────────────────────────────
// YOUR TOOLS — defined inline. Add a tool by appending a Tool object here (edit
// this file with your shell); it loads next turn. A Tool looks like:
//
//   {
//     definition: {
//       name: "my_tool",
//       description: "What it does and WHEN to call it.",
//       parameters: { type: "object", additionalProperties: false,
//                     properties: {}, required: [] },
//     },
//     initializationParameters: { type: "object", additionalProperties: false,
//                                 properties: {}, required: [] },
//     initialize() { return { async execute(args) { return { ok: true }; } }; },
//   }
//
// REQUIRED FIELDS: definition, initializationParameters, and initialize (a
// function) — all three, or the tool is rejected at load.
// STRICT SCHEMA RULE (or the whole turn is rejected): BOTH `parameters` and
// `initializationParameters` MUST be JSON Schema objects with
// "additionalProperties": false AND a "required" array listing EVERY key in
// "properties" (optional params: still list them, allow null in their type).
// ───────────────────────────────────────────────────────────────────────────
const INLINE_TOOLS: Tool[] = [
  {
    definition: {
      name: "echo_note",
      description:
        "Sample inline tool (remove me): echoes back the text you pass, to prove inline tools register and run. Call it any time to test.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: { text: { type: "string" } },
        required: ["text"],
      },
    },
    initializationParameters: {
      type: "object",
      additionalProperties: false,
      properties: {},
      required: [],
    },
    initialize() {
      return {
        async execute(args: { text: string }) {
          return { ok: true, echoed: args.text };
        },
      };
    },
  } as Tool,
];

const POKEMON_PROMPT = `You are an AI agent playing Pokémon Red on a Game Boy. Your job is to LEARN TO PLAY THE GAME WELL and make as much progress as you can. Each turn you see the current screen and respond with button presses. The buttons are: up, down, left, right, a, b, start, select. Figure out how the game works by playing it.

Each turn you are shown the PREVIOUS frame (the screen just before your last action) next to the CURRENT one. Compare them to see what your last action actually did — if they look the same, your action had no effect, so don't just repeat it: try something else or improve yourself.

YOU IMPROVE YOURSELF BY EDITING YOUR OWN SOURCE. Everything about you — how you play AND the tools you can call — lives in ONE file at ${SELF_PATH_IN_SANDBOX}, mounted read-write. You have a \`shell\`. \`cat\` the file, then rewrite any part of it to get better. It reloads next turn; if an edit won't load, the system restores the last working version and tells you, so small experiments are safe. Three things you can change:
1. YOUR POLICY (the prompt/strategy text in that file) — bake in strategies you learn.
2. YOUR TOOLS — they're defined in the INLINE_TOOLS array in that file. Add a tool by appending a Tool object; change or remove one by editing the array. A Tool object MUST have all three of: \`definition\` (with name/description/parameters), \`initializationParameters\`, and an \`initialize()\` function — copy the shape of the existing entry. STRICT SCHEMA RULE (or the tool is rejected and breaks the turn): BOTH \`parameters\` and \`initializationParameters\` must be JSON Schema objects with "additionalProperties": false AND a "required" array listing EVERY key in "properties" (optional params: still list them, allow null in their type). A tool only helps if you actually CALL it — give it a description that says plainly WHEN to use it, and if you want it used every turn, wire that into your policy text.
3. HOW YOU PERCEIVE — e.g. change how the screen is presented to you.
You also have durable MEMORY (\`remember\`/\`forget\`) for facts that should persist across turns (memory survives even when your conversation resets; keep it lean, not a diary).

WORK TOWARD A GOAL, don't just react. Keep an explicit objective in memory and each turn check whether you're actually progressing toward it. If you've been wandering, repeating, or revisiting the same places, that's a signal you've LOST THE THREAD — stop and reconsider, or improve yourself. Reaching new places / advancing the game is progress; circling the same area is not.

You do NOT have to press a button every turn — when it's worth it, spend a turn improving yourself (edit your file / save memory) and reply with EMPTY buttons.

THINK FIRST, THEN ACT. Before choosing buttons, reason in plain text for a few sentences:
- What is on the screen RIGHT NOW, and what KIND of screen is it? (Look carefully — different screens need different inputs; the same button does different things on different screens.)
- Compare to the PREVIOUS frame: did my last action actually change anything, or did nothing happen? If nothing changed, what I tried did not work — don't just repeat it.
- Given what's actually on screen, what are my real options, and what is the best next action toward my objective?
Work it out — do not skip straight to buttons.

THEN, on the LAST line of your reply, output ONLY the action as JSON (nothing after it):
{"buttons": ["<0-3 of: up,down,left,right,a,b,start,select, in order; empty list = a self-improvement turn>"], "reasoning": "<one line>"}`;

async function instructions(context: TurnContext): Promise<Message[]> {
  const messages: Message[] = [{ role: "developer", content: POKEMON_PROMPT }];
  const memory = await memoryInstruction(context);
  if (memory !== null) messages.push(memory);
  // Optional live human channel (a person watching can send a hint). Not game
  // knowledge baked into the harness — just a passthrough if present.
  try {
    const g = JSON.parse(readFileSync(GUIDANCE_PATH, "utf8"));
    if (g && typeof g.text === "string" && g.text.trim()) {
      messages.push({
        role: "developer",
        content: `📣 A human watching just sent this hint (optional): ${g.text.trim()}`,
      });
    }
  } catch {
    /* no guidance */
  }
  // Generic observation (NOT game knowledge): the PREVIOUS frame (before your last
  // action) next to the current one, so you can see what your last action did.
  try {
    const pb64 = readFileSync(PREV_SCREEN_PATH).toString("base64");
    messages.push({
      role: "user",
      content: [
        {
          type: "text",
          text: "PREVIOUS frame (the screen BEFORE your last action). Compare it to the current frame to see what your last action did:",
        },
        { type: "image", image: `data:image/png;base64,${pb64}` },
      ],
    });
  } catch {
    /* no previous frame yet (first turn) */
  }
  // The current screen — what you act on now.
  try {
    const b64 = readFileSync(SCREEN_PATH).toString("base64");
    messages.push({
      role: "user",
      content: [
        {
          type: "text",
          text: "CURRENT frame (now) — choose your next action:",
        },
        { type: "image", image: `data:image/png;base64,${b64}` },
      ],
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
        registerBuiltInTools(tools, ctx, ["shell"]);
        registerMemoryTools(tools);
        // The agent's own tools, defined inline in this file (above).
        await registerAgentTools(tools, ctx, INLINE_TOOLS);
      },
    });
  },
});
