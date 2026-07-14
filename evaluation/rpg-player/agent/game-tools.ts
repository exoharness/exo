import { describeState, type EmulatorClient } from "./emulator-client";
import type { AgentTool, ToolResult } from "./tool-types";

// Sega Master System pad. button1 doubles as cancel, button2 as confirm in
// most SMS games (Phantasy Star included); pause is the console button that
// opens the status menu in Phantasy Star.
const BUTTONS = ["up", "down", "left", "right", "button1", "button2", "pause"];

export function gameTools(emulator: EmulatorClient): AgentTool[] {
  return [
    {
      name: "press_buttons",
      description:
        "Press a sequence of Sega Master System buttons, one after another. Each button is held for hold_frames then released, followed by wait_frames of settle time (60 frames = 1 second). In Phantasy Star: button2 confirms / advances dialog, button1 cancels, pause opens the status/magic/item menu, d-pad moves and navigates menus. Returns the resulting screen.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          buttons: {
            type: "array",
            items: { type: "string", enum: BUTTONS },
            description: "1-20 buttons pressed in order.",
          },
          hold_frames: {
            type: ["number", "null"],
            description:
              "Frames to hold each button (default 8, max 120). Longer holds walk further per press.",
          },
          wait_frames: {
            type: ["number", "null"],
            description:
              "Frames to wait after each release (default 40, max 600). Increase when animations or dialog need time.",
          },
        },
        required: ["buttons"],
      },
      execute: async (args) => {
        const buttons = args.buttons;
        if (
          !Array.isArray(buttons) ||
          buttons.some((button) => !BUTTONS.includes(String(button)))
        ) {
          return { text: `buttons must be an array of ${BUTTONS.join("/")}` };
        }
        const frame = await emulator.press(
          buttons.map(String),
          numberOrNull(args.hold_frames),
          numberOrNull(args.wait_frames),
        );
        return {
          text: `pressed [${buttons.join(", ")}]\n${describeState(frame.state)}`,
          frame,
        };
      },
    },
    {
      name: "wait",
      description:
        "Let the game run for N frames without pressing anything (60 frames = 1 second). Use when a cutscene, animation, or dialog is still playing.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          frames: {
            type: "number",
            description: "Frames to advance (max 3600).",
          },
        },
        required: ["frames"],
      },
      execute: async (args) => {
        const frames = Number(args.frames);
        if (!Number.isFinite(frames) || frames < 1) {
          return { text: "frames must be a positive number" };
        }
        const frame = await emulator.tick(Math.min(Math.round(frames), 3600));
        return {
          text: `waited ${frames} frames\n${describeState(frame.state)}`,
          frame,
        };
      },
    },
    {
      name: "screenshot",
      description:
        "Fetch the current screen and game state without advancing the game.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {},
        required: [],
      },
      execute: async () => {
        const frame = await emulator.frame();
        return { text: describeState(frame.state), frame };
      },
    },
    {
      name: "claim_milestone",
      description:
        "Record a gameplay milestone you believe you just reached (e.g. 'left Camineet for the first time', 'recruited Myau', 'beat Medusa'). Claims go to the event log as YOUR claims — the harness does not verify them — but they anchor your history and show up in run narration. Claim sparingly: real story/party/boss progress only.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          milestone: {
            type: "string",
            description: "One line describing the milestone.",
          },
        },
        required: ["milestone"],
      },
      execute: async (args): Promise<ToolResult> => {
        const milestone = String(args.milestone ?? "").trim();
        if (milestone.length === 0 || milestone.length > 200) {
          return { text: "milestone must be 1-200 chars" };
        }
        return {
          text: `milestone claimed: ${milestone}`,
          improvement: `CLAIMED: ${milestone}`,
        };
      },
    },
    {
      name: "save_checkpoint",
      description:
        "Save an emulator save-state under a name so you can rewind to this exact moment later with load_checkpoint. Save before risky sections (dungeons, boss fights, long walks).",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description: "Checkpoint name, [A-Za-z0-9_.-]{1,64}.",
          },
        },
        required: ["name"],
      },
      execute: async (args) => {
        const frame = await emulator.saveCheckpoint(String(args.name ?? ""));
        return { text: `checkpoint '${args.name}' saved`, frame };
      },
    },
    {
      name: "load_checkpoint",
      description:
        "Rewind the game to a previously saved checkpoint. Use when you are wedged (party wiped, softlocked in a menu, hopelessly lost).",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: { type: "string", description: "Checkpoint name to load." },
        },
        required: ["name"],
      },
      execute: async (args): Promise<ToolResult> => {
        const frame = await emulator.loadCheckpoint(String(args.name ?? ""));
        return {
          text: `rewound to checkpoint '${args.name}'\n${describeState(frame.state)}`,
          frame,
          improvement: `REWIND: ${args.name}`,
        };
      },
    },
    {
      name: "list_checkpoints",
      description: "List all saved checkpoint names.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {},
        required: [],
      },
      execute: async () => {
        const names = await emulator.listCheckpoints();
        return {
          text: names.length === 0 ? "no checkpoints yet" : names.join("\n"),
        };
      },
    },
  ];
}

function numberOrNull(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}
