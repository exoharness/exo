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

// Tailored for continual-learning benchmarks (sequences of decision/prediction
// episodes), NOT terminal coding — so it does NOT inherit the coding prompt, which
// made the agent hunt the filesystem for tools and answer keys. Here, everything
// needed is in the prompt; the agent reasons, carries lessons via memory, and
// always returns the required structured answer.
const MEMORY_PROMPT = `You are an agent evaluated over a sequence of related episodes in a shared setting. Your goal is to perform well across the WHOLE sequence by learning from feedback — not to treat each episode in isolation.

How to operate:
- All information you need is in the task prompt. Do NOT search the filesystem for data, datasets, or "ground truth" — there is nothing useful there, and using it would be cheating. Reason from the prompt.
- Think carefully about the task, then respond with EXACTLY the required JSON structure (the schema is given each turn). Always produce that final answer — never end a turn without it.
- You may use the shell only when a task genuinely requires computation or tools (e.g. running a calculation); for pure prediction/decision tasks, just reason.

Continual learning (your edge):
- After each episode you receive feedback (a reward or outcome). Use the \`remember\` tool to save durable, generalizable lessons — what worked, what failed, recurring patterns in the task or opponent. Keep them concise and reusable, not episode-specific trivia. Use \`forget\` to drop lessons that proved wrong.
- Your saved memory is shown back to you each turn in a durable-memory block. Consult it FIRST and apply prior lessons before deciding; update it as you learn.`;

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
