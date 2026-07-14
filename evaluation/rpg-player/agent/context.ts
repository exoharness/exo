// Per-turn prompt assembly: fixed system rules + the agent's own playbook,
// todos, memory index, objective progress, recent turn summaries, and the
// current screen.

import fs from "node:fs/promises";

import { describeState, type FramePayload } from "./emulator-client";
import type { ProgressTracker } from "./events";
import { imageMessage, textMessage } from "./model";
import type { SelfStore } from "./self-tools";
import type { SkillIndexEntry } from "./skills";

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
  skillsIndex: SkillIndexEntry[];
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
    `# Your skills (load full instructions with use_skill; install/update with install_skill)\n${renderSkills(options.skillsIndex)}`,
    `# Objective progress (screen novelty measured by the harness, not your claims)\n${options.progress.summary()}\nRecent milestones:\n${
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

function renderSkills(index: SkillIndexEntry[]): string {
  if (index.length === 0) {
    return "(none yet — install_skill turns a hard-won procedure into a durable, reloadable capability)";
  }
  return index
    .map((entry) => `- ${entry.name}: ${entry.description}`)
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

// Turn 1 only: force the model to surface its latent knowledge of the game
// before touching the controls. Models retrieve what they know when cued —
// left alone they play reactively and their walkthrough knowledge stays
// buried. One dedicated dump turn converts it into durable notes.
export function bootstrapDirective(): string {
  return [
    "This is your first turn: a knowledge-bootstrap turn. Do NOT press any buttons yet.",
    "You have read about Phantasy Star (Sega Master System, 1987) in your training data: walkthroughs, FAQs, maps, tips. Retrieve everything useful and write it down NOW, while it is cheap — once you start playing you will only have your notes.",
    "Write memory files for at least: the opening quest chain and how to progress the story; where and how to recruit Myau, Odin, and Noah; the world layout (planets, towns, and what connects them); combat and grinding advice for the early game; menu/shop/church mechanics and what the PAUSE menu does.",
    "IMPORTANT: your memory of this game is fallible. Prefix every fact you have not verified on screen with '(unverified)'. When gameplay later confirms or refutes one, update the file — a confidently wrong note is worse than no note.",
    "Then update_playbook with the controls you know and a short opening strategy, and update_todos with your goal stack (first goals: get through the intro, check your starting equipment and money, leave Camineet).",
    "Finish with a summary of what you wrote down.",
  ].join("\n");
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
    "- install_skill if you have proven out a multi-step procedure (a battle recipe, a navigation method, a shop/heal loop) worth reloading on demand instead of rediscovering",
    "Also do these two audits:",
    "- Audit one memory file: re-read it as if you had never played — is it specific, is it verified, would it actually guide you? Fix or delete what fails that test, and upgrade any '(unverified)' facts the game has since confirmed or refuted.",
    "- Diagnose your biggest bottleneck: name the capability that, if you had it, would most improve your play. If you can build it with install_tool, build it now. If you cannot build it with your current tools, record it in a memory file named 'harness_wishlist' with a precise spec of what you need and why.",
    "Finish with a short summary of what you changed.",
  ].join("\n");
}

export function stuckDirective(stuckTurns: number): string {
  return [
    `WARNING: the screen has been pixel-identical at the start of ${stuckTurns} consecutive turns. Whatever you are doing is not being accepted by the game.`,
    "Options, in order: try genuinely different buttons or timing (button2 confirms, button1 cancels, pause toggles the menu); re-read the screen carefully; write a memory about what does NOT work here; load_checkpoint to rewind out of the wedge.",
  ].join("\n");
}
