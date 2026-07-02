// Per-turn prompt assembly: fixed system rules + the agent's own playbook,
// todos, memory index, objective progress, recent turn summaries, and the
// current screen.

import fs from "node:fs/promises";

import { describeState, type FramePayload } from "./emulator-client";
import type { ProgressTracker } from "./events";
import { imageMessage, textMessage } from "./model";
import type { SelfStore } from "./self-tools";

export interface TurnRecord {
  turn: number;
  summary: string;
  milestones: string[];
  improvements: string[];
}

const RECENT_TURNS_FULL = 15;
const OLDER_TURNS_KEPT = 60;

export async function buildTurnInput(options: {
  systemPromptPath: string;
  store: SelfStore;
  progress: ProgressTracker;
  history: TurnRecord[];
  turn: number;
  frame: FramePayload;
  directive: string | null;
}): Promise<unknown[]> {
  const systemPrompt = await fs.readFile(options.systemPromptPath, "utf8");
  const playbook = await options.store.playbook();
  const todos = await options.store.todos();
  const memoryIndex = await options.store.memoryIndex();

  const developerSections = [
    `# Your playbook (you own this; edit with update_playbook)\n${playbook}`,
    `# Your todos (edit with update_todos)\n${renderTodos(todos)}`,
    `# Your memory files (read with read_memory)\n${renderMemoryIndex(memoryIndex)}`,
    `# Objective progress (derived from game RAM, not your claims)\n${options.progress.summary()}\nRecent milestones:\n${
      options.progress.recentMilestones(8).join("\n") || "(none yet)"
    }`,
    `# Recent turns\n${renderHistory(options.history)}`,
  ];

  const input: unknown[] = [
    textMessage("system", systemPrompt.trim()),
    textMessage("developer", developerSections.join("\n\n")),
  ];
  if (options.directive !== null) {
    input.push(
      textMessage(
        "developer",
        `# Directive for this turn\n${options.directive}`,
      ),
    );
  }
  input.push(
    imageMessage(
      `Turn ${options.turn}. Current screen and state:\n${describeState(options.frame.state)}`,
      options.frame.screenshot_b64,
    ),
  );
  return input;
}

function renderTodos(todos: { text: string; status: string }[]): string {
  if (todos.length === 0) {
    return "(empty — set some goals)";
  }
  const marks: Record<string, string> = {
    done: "x",
    in_progress: ">",
    pending: " ",
  };
  return todos
    .map((todo) => `- [${marks[todo.status] ?? " "}] ${todo.text}`)
    .join("\n");
}

function renderMemoryIndex(
  index: { name: string; firstLine: string }[],
): string {
  if (index.length === 0) {
    return "(none yet)";
  }
  return index.map((entry) => `- ${entry.name}: ${entry.firstLine}`).join("\n");
}

function renderHistory(history: TurnRecord[]): string {
  if (history.length === 0) {
    return "(this is your first turn)";
  }
  const recent = history.slice(-RECENT_TURNS_FULL);
  const older = history.slice(0, -RECENT_TURNS_FULL).slice(-OLDER_TURNS_KEPT);
  const lines: string[] = [];
  if (history.length > older.length + recent.length) {
    lines.push(
      `(${history.length - older.length - recent.length} earlier turns omitted)`,
    );
  }
  for (const record of older) {
    // Older turns survive only as milestones/improvements to bound tokens.
    const highlights = [...record.milestones, ...record.improvements];
    if (highlights.length > 0) {
      lines.push(`turn ${record.turn}: ${highlights.join("; ")}`);
    }
  }
  for (const record of recent) {
    const extras = [...record.milestones, ...record.improvements];
    lines.push(
      `turn ${record.turn}: ${record.summary}${
        extras.length > 0 ? ` [${extras.join("; ")}]` : ""
      }`,
    );
  }
  return lines.join("\n");
}

export function reflectionDirective(): string {
  return [
    "This is a scheduled self-improvement turn. Do NOT press any buttons.",
    "Review your recent turns: what worked, what failed, what did you have to rediscover?",
    "Then improve yourself before playing on:",
    "- update_playbook with anything you keep re-learning (button timing, menu paths, world layout, battle heuristics)",
    "- save_memory for larger knowledge (maps, quest steps, NPC info)",
    "- update_todos so your goal stack matches reality",
    "- install_tool if you keep repeating a mechanical sequence a tool could do in one call",
    "Finish with a short summary of what you changed.",
  ].join("\n");
}

export function stuckDirective(stuckTurns: number): string {
  return [
    `WARNING: the game state has not changed for ${stuckTurns} consecutive turns. Whatever you are doing is not working.`,
    "Options, in order: try genuinely different buttons or timing; re-read the screen carefully; write a memory about what does NOT work here; load_checkpoint to rewind out of the wedge.",
  ].join("\n");
}
