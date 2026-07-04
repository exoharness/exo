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
- The information you need is either stated in the task prompt OR discovered through your structured actions — some tasks let you explore by issuing actions (e.g. an exploratory SQL query, a shell/bash command) that the benchmark executes for you and returns the results of. Use those actions to explore. Do NOT rummage your own local filesystem for hidden datasets or "ground truth" — there is none there, and using it would be cheating.
- Think carefully, then respond with EXACTLY the required JSON structure (the schema is given each turn). Always produce that final answer — never end a turn without it.
- For pure prediction/decision tasks, just reason from the prompt. For exploratory tasks, issue the exploration actions the schema offers, then answer.

Continual learning (your edge):
- After each episode you receive feedback (a reward or outcome). Use the \`remember\` tool to save durable, reusable knowledge; use \`forget\` to drop or supersede entries that proved wrong or stale.
- Two kinds of memory pay off, depending on the task:
  1. DISCOVERED STRUCTURE — when a task lets you explore (a database, a repo, an environment), the structure you uncover is gold: table/column names, schemas, relationships, data quirks, file layout, where things live. SAVE it, so future episodes act directly instead of re-running the same exploratory queries/commands. Doing less redundant exploration over time is an explicit goal.
  2. STRATEGY LESSONS — what worked or failed, recurring patterns, systematic biases to correct (e.g. "I keep under-predicting fast movers — adjust up").
- Your saved memory is shown back to you each turn in a durable-memory block. Consult it FIRST: if you already recorded the schema/structure, skip re-discovering it and use it. Update memory as you learn.`;

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
