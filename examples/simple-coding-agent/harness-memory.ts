// Simple Coding Agent + durable memory — for continual-learning benchmarks
// (e.g. Continual Learning Bench / clbench). Same shell tool + verify-before-finish
// prompt as harness.ts, plus exo's agent-writable durable memory (remember/forget)
// and per-turn injection of the saved-memory block. This is what lets the agent
// carry lessons across episodes/instances within a run, which a continual-learning
// benchmark rewards (its Gain = stateful vs. stateless-reset baseline).
//
// harness.ts stays memory-free (it isolates raw agent+model ability for
// Terminal-Bench); this variant adds the learning layer.

import {
  defineHarness,
  registerBuiltInTools,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import { runResponsesHarnessTurn } from "../typescript/turn-loop";
import {
  memoryInstruction,
  registerMemoryTools,
} from "../exoclaw/memory-tools";
import { AGENT_SYSTEM_PROMPT } from "./harness";

const MEMORY_PROMPT = `${AGENT_SYSTEM_PROMPT}

Continual learning across episodes:
- You face a sequence of related episodes in a shared setting. After each, you get feedback (a reward or outcome). Treat the whole sequence as one chance to get better, not isolated puzzles.
- Use the \`remember\` tool to save durable, generalizable lessons — strategies that worked, mistakes to avoid, recurring patterns in the task or opponent. Keep them concise and reusable, not episode-specific trivia. Use \`forget\` to drop lessons that later proved wrong.
- Your saved memory is shown back to you each turn in a durable-memory block. Consult it FIRST and apply prior lessons before acting; update it as you learn.`;

async function instructions(context: TurnContext): Promise<Message[]> {
  const messages: Message[] = [{ role: "developer", content: MEMORY_PROMPT }];
  const memory = await memoryInstruction(context);
  if (memory !== null) {
    messages.push(memory);
  }
  return messages;
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions,
      registerTools: (tools: HarnessToolRegistry, ctx: TurnContext) => {
        registerBuiltInTools(tools, ctx, ["shell"]);
        registerMemoryTools(tools);
      },
    });
  },
});
