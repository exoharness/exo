// Self-improving agent for continual-learning benchmarks (clbench).
//
// Extends harness-memory.ts (durable memory) with the rest of exo's
// self-improvement kit — the same reusable pieces exoclaw composes:
//   • MEMORY            — remember/forget + per-turn memory injection
//   • TOOL EVOLUTION    — install_agent_tool / uninstall_agent_tool; tools the
//                         agent writes persist and load every turn
//   • SELF-QUARANTINE   — snapshot_sandbox / rewind_sandbox / list_sandbox_snapshots:
//                         checkpoint the sandbox, try something, rewind if it went bad
//
// This is the "full breadth" self-improving agent (vs harness-memory.ts which is
// memory-only). Tool creation is gated by the agent's enableAgentToolCreation
// flag (`exo agent create --tool-creation enabled`).

import {
  defineHarness,
  registerBuiltInTools,
  registerAgentToolsFromDirectoryIfExists,
  registerLibraryToolModulePath,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import {
  runResponsesHarnessTurn,
  defaultBuiltInToolNames,
  agentToolCreationInstruction,
} from "../typescript/turn-loop";
import { memoryInstruction, registerMemoryTools } from "../exoclaw/memory-tools";
import { registerSandboxTools } from "../exoclaw/sandbox-tools";
import { registerIntrospectionTools } from "../exoclaw/introspection-tools";

const SELF_IMPROVE_PROMPT = `You are an agent evaluated over a sequence of related episodes in a shared setting. Your goal is to perform well across the WHOLE sequence by improving yourself from feedback — not to treat each episode in isolation.

How to operate:
- The information you need is either stated in the task prompt OR discovered through your structured actions — some tasks let you explore by issuing actions (e.g. an exploratory SQL query, a shell/bash command) that the benchmark executes for you and returns the results of. Use those actions to explore. Do NOT rummage your own local filesystem for hidden datasets or "ground truth" — there is none there, and using it would be cheating.
- Think carefully, then respond with EXACTLY the required JSON structure (the schema is given each turn). Always produce that final answer — never end a turn without it.

You IMPROVE YOURSELF across episodes with three mechanisms — use whichever pays off:
1. MEMORY (\`remember\` / \`forget\`). Save durable, reusable knowledge so future episodes act directly instead of re-deriving it. Two kinds pay off: (a) DISCOVERED STRUCTURE — schemas, table/column names, relationships, data quirks, file/layout where things live; save it so you stop re-running the same exploration. (b) STRATEGY LESSONS — what worked or failed, recurring patterns, systematic biases to correct ("I keep under-predicting fast movers — adjust up"). Your saved memory is shown back each turn in a durable-memory block; consult it FIRST and update it as you learn.
2. BUILD YOUR OWN TOOLS (\`install_agent_tool\` / \`uninstall_agent_tool\`). When you find yourself repeating a computation or needing a capability you lack, write yourself a TypeScript tool for it. Installed tools persist and load every turn. A tool only helps if you actually call it — give it a clear description and use it. Remove tools that don't earn their keep.
3. CHECKPOINT / REWIND (\`snapshot_sandbox\` / \`rewind_sandbox\` / \`list_sandbox_snapshots\`). Before a risky or exploratory change to your sandbox, snapshot it; if it goes wrong, rewind to the checkpoint instead of living with a broken state.

Doing less redundant work over time — by remembering structure, building reusable tools, and not getting stuck in broken states — is an explicit goal.`;

async function instructions(context: TurnContext): Promise<Message[]> {
  const messages: Message[] = [{ role: "developer", content: SELF_IMPROVE_PROMPT }];
  const memory = await memoryInstruction(context);
  if (memory !== null) {
    messages.push(memory);
  }
  if (context.agentConfig.enableAgentToolCreation) {
    messages.push(agentToolCreationInstruction());
  }
  return messages;
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions,
      registerTools: async (tools: HarnessToolRegistry, ctx: TurnContext) => {
        registerBuiltInTools(tools, ctx, defaultBuiltInToolNames(ctx));
        registerMemoryTools(tools);
        registerSandboxTools(tools);
        registerIntrospectionTools(tools); // reflect on own history (list_conversation_events)
        // Operator-provided tool libraries, if any are configured (no-op otherwise).
        for (const modulePath of ctx.agentConfig.typescript?.toolModulePaths ?? []) {
          await registerLibraryToolModulePath(tools, ctx, modulePath);
        }
        if (ctx.agentConfig.enableAgentToolCreation) {
          await registerAgentToolsFromDirectoryIfExists(tools, ctx);
        }
      },
    });
  },
});
