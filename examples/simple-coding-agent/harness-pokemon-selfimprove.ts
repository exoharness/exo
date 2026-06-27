// Self-improving Pokémon harness — exoclaw-style. This ONE file is the agent's
// whole brain: how it plays (the self-control prompt), what it perceives
// (instructions), and which self-improvement levers it has (tools). The agent
// rewrites THIS file to evolve its policy/perception, and builds durable,
// reusable tools the exoclaw way (install_agent_tool), so capabilities persist
// across turns and even across conversation resets. The runner re-imports this
// file every turn (hot-swap) and rolls back any edit that won't load.
//
// DESIGN PRINCIPLE: no Pokémon knowledge baked in. The harness only shows the
// screen (+ the previous frame, so the agent can see what its last action did)
// and gives generic self-improvement machinery — durable memory, a shell, the
// ability to build its own tools, inspect its own history, and edit its own
// source. How to read the screen, navigate, and fight is for the agent to
// figure out and encode here (or in a tool / in memory) itself.
//
// THE ONE CONTRACT to preserve if you (the agent) edit this file: still show
// yourself the screenshot each turn, and still END every turn with the JSON
// buttons object on the last line. The Python driver presses those buttons.

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
  agentToolCreationInstruction,
  defaultBuiltInToolNames,
  runResponsesHarnessTurn,
} from "../typescript/turn-loop";
import {
  memoryInstruction,
  registerMemoryTools,
} from "../exoclaw/memory-tools";
import { registerHostTool } from "../exoclaw/host-tools";

// Fixed paths shared with pokemon_runner.py / live_server.py (keep in sync).
const SCREEN_PATH = "/tmp/exo-pokemon/screen.png";
const PREV_SCREEN_PATH = "/tmp/exo-pokemon/prev_screen.png"; // frame before your last action
const GUIDANCE_PATH = "/tmp/exo-pokemon/guidance.json"; // optional live human channel
// Objective, structured state the runner writes each turn (position, map,
// progress) and your running spend. You can `cat` these in the shell to assess
// yourself — they are NOT shown to you automatically (perception is the screen).
const GAME_STATE_PATH = "/tmp/exo-pokemon/game.json";
const COST_PATH = "/tmp/exo-pokemon/cost.json";
// This file as seen from inside the sandbox (mounted read-write). You edit THIS
// path to rewrite your policy and perception — it's all one file.
const SELF_PATH_IN_SANDBOX = "/workspace/agent/harness-pokemon-self.ts";

// Read-only introspection over your own history (exoclaw-style self-knowledge):
// the canonical conversation event log. Use it to reconstruct what you actually
// did over recent turns — tool calls, errors, session boundaries — so you can
// notice loops and mistakes instead of repeating them.
function registerPokemonIntrospection(registry: HarnessToolRegistry): void {
  registerHostTool(registry, {
    name: "list_conversation_events",
    description:
      "List this conversation's event log, newest first. By default returns lifecycle/host events (session_started/ended, errors, sandbox lifecycle). Pass explicit kinds (e.g. tool_requested, tool_result, messages) to review what you recently did and whether it worked. Use this to catch loops, failed actions, and broken self-edits. Read-only.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        kinds: {
          type: ["array", "null"],
          items: { type: "string" },
          description:
            'Event kinds to return (e.g. ["messages","tool_requested","tool_result"]). Null for the default lifecycle/host set.',
        },
        limit: {
          type: ["number", "null"],
          description:
            "Max events (default 50, capped at 200). Null for default.",
        },
        cursor: {
          type: ["string", "null"],
          description:
            "Event id cursor from a previous call's result, for pagination. Null to start from the newest.",
        },
        direction: {
          type: ["string", "null"],
          enum: ["asc", "desc", null],
          description: "Listing order. Null for desc (newest first).",
        },
      },
      required: ["kinds", "limit", "cursor", "direction"],
    },
  });
}

const POKEMON_PROMPT = `You are an AI agent playing Pokémon Red on a Game Boy. Your job is to LEARN TO PLAY THE GAME WELL and make as much progress as you can, and to GET BETTER AT IT OVER TIME by improving yourself. Each turn you see the current screen and respond with button presses. The buttons are: up, down, left, right, a, b, start, select. Figure out how the game works by playing it — no strategy is baked in for you.

Each turn you are shown the PREVIOUS frame (the screen just before your last action) next to the CURRENT one. Compare them to see what your last action actually did — if they look the same, your action had no effect, so don't just repeat it: try something else, or improve yourself.

═══ YOUR SELF-IMPROVEMENT LEVERS ═══
You are a self-improving agent. Everything about you is yours to change. You have FIVE levers — use them deliberately, not just reflexively pressing buttons:

1. DURABLE MEMORY (\`remember\` / \`forget\`). Save facts that must outlive the current turn — your objective, routes/exits that worked, party state, what just happened. Memory survives even when your conversation is reset, so it is the thread that carries the game forward. Keep it LEAN and high-signal (sharp facts, not a diary); your saved memory is shown back to you each turn — consult it FIRST.

2. BUILD YOUR OWN TOOLS (\`install_agent_tool\` / \`uninstall_agent_tool\`). When you find yourself doing the same fiddly thing repeatedly, build a reusable tool for it instead. Installed tools PERSIST into later turns (and later conversations) — this is how you accumulate real capability. A great first tool: one that reads ${GAME_STATE_PATH} and returns your structured position/map/visited-tiles/frontier so you can navigate deliberately instead of guessing from pixels. (The exact moduleSource contract for install_agent_tool is given to you separately — follow it precisely or the install is rejected.) A tool only helps if you actually CALL it: give it a description that says plainly WHEN to use it, and if it should run every turn, say so in your policy.

3. SELF-EDIT YOUR POLICY (\`shell\`, editing ${SELF_PATH_IN_SANDBOX}). This file IS your brain — your prompt, your strategy, and how you perceive the screen. \`cat\` it, then rewrite any part to bake in what you've learned. It reloads next turn; if an edit won't load, the system restores the last working version and tells you, so small experiments are safe. Change your strategy text, change how the screen is presented to you, anything.

4. INSPECT YOURSELF (\`list_conversation_events\`). Review what you actually did over recent turns — your tool calls, their results, errors, failed self-edits. Use this when you suspect you're looping or something quietly broke.

5. SHELL for self-assessment. You can \`cat ${GAME_STATE_PATH}\` for objective ground truth (your map, x/y, whether your last move actually moved you, a minimap, and the unexplored frontier) and \`cat ${COST_PATH}\` to see your spend — aim to make real progress efficiently, not to burn tokens spinning.

You do NOT have to press a button every turn. When it's worth it, spend a turn improving yourself (build a tool, edit your policy, bank a memory) and reply with EMPTY buttons.

═══ HOW TO PLAY ═══
WORK TOWARD A GOAL, don't just react. Keep an explicit objective in memory and each turn check whether you're actually progressing toward it. Reaching new places / advancing the game is progress; circling the same area, repeating, or revisiting the same tiles is LOSING THE THREAD — when that happens, stop and reconsider, or use a self-improvement lever above.

THINK FIRST, THEN ACT. Before choosing buttons, reason in plain text for a few sentences:
- What is on the screen RIGHT NOW, and what KIND of screen is it? (Different screens — overworld, dialogue, menu, battle — need different inputs; the same button does different things on different screens.)
- Compare to the PREVIOUS frame: did my last action change anything? If nothing changed, what I tried did not work — don't just repeat it.
- Given what's actually on screen and my objective, what is the best next action? (And is this a turn better spent improving myself?)
Work it out — do not skip straight to buttons.

THEN, on the LAST line of your reply, output ONLY the action as JSON (nothing after it):
{"buttons": ["<0-3 of: up,down,left,right,a,b,start,select, in order; empty list = a self-improvement turn>"], "reasoning": "<one line>"}`;

async function instructions(context: TurnContext): Promise<Message[]> {
  const messages: Message[] = [{ role: "developer", content: POKEMON_PROMPT }];
  // Precise contract for building tools with install_agent_tool (only when tool
  // creation is enabled for this agent).
  if (context.agentConfig.enableAgentToolCreation) {
    messages.push(agentToolCreationInstruction());
  }
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
  // Generic observation (NOT game knowledge): the PREVIOUS frame (before your
  // last action) next to the current one, so you can see what your last action did.
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
        // shell + (when enabled) install_agent_tool / uninstall_agent_tool.
        registerBuiltInTools(tools, ctx, defaultBuiltInToolNames(ctx));
        registerMemoryTools(tools);
        registerPokemonIntrospection(tools);
        // Load every tool the agent has built so far (persisted to disk),
        // exoclaw-style, so capabilities accumulate across turns.
        if (ctx.agentConfig.enableAgentToolCreation) {
          await registerAgentToolsFromDirectoryIfExists(tools, ctx);
        }
      },
    });
  },
});
