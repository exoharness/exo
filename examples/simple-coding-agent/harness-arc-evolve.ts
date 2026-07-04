// ARC-AGI self-evolving harness — the self-improvement kit (memory, agent-authored
// tools, snapshot/rewind, introspection) applied to ARC-AGI grid puzzles.
//
// Counterpart to harness-arc.ts (pure reasoning, no tools). Here the agent runs
// over a SEQUENCE of ARC tasks through one persistent exo agent (see
// evaluation/arc-agi/arc_runner.py --evolve): its memory, installed tools, and
// sandbox survive across tasks, so it can build itself a grid toolkit and a
// pattern playbook as it goes. Registries are the same reusable pieces exoclaw
// and harness-selfimprove.ts compose.

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
import {
  memoryInstruction,
  registerMemoryTools,
} from "../exoclaw/memory-tools";
import { registerSandboxTools } from "../exoclaw/sandbox-tools";
import { registerIntrospectionTools } from "../exoclaw/introspection-tools";
import { registerTodoTools, todoInstruction } from "../exoclaw/todo-tools";

const ARC_EVOLVE_PROMPT = `You are an expert ARC-AGI puzzle solver, evaluated over a SEQUENCE of puzzles. Each puzzle gives several INPUT -> OUTPUT grid examples sharing ONE hidden transformation rule; you must produce the output grid(s) for the held-out TEST input(s). Grids are rectangles of integers 0-9 (colors). Scoring is exact match.

HOW TO SOLVE (do this every task):
1. The task message contains the puzzle as a fenced JSON block: {"train": [{"input", "output"}...], "test": [{"input"}...]}. FIRST, save it to a file in your sandbox (e.g. cat > /work/task.json <<'EOF' ... EOF) so you can work programmatically instead of eyeballing large grids.
2. Study the train pairs and hypothesize the rule. Consider: resize/crop/tiling, symmetry/reflection/rotation, translation/gravity, color remapping, object detection/counting/movement, filling/bordering, occlusion repair, pattern completion, per-cell logic, grid-of-grids selection.
3. WRITE THE TRANSFORM AS CODE (python3) in your sandbox and RUN IT AGAINST EVERY TRAIN PAIR. Do not trust a rule you have not verified: if it fails any train pair, revise and re-verify. Only when it reproduces ALL train outputs exactly, apply it to the test input(s).
4. If after honest effort no verified rule emerges, give your best two distinct guesses.

FINAL ANSWER FORMAT (critical): end the turn with ONLY a JSON object, no prose, no code fences:
  {"outputs": [<grid per TEST input, in order>], "outputs2": [<optional second-attempt grid per TEST input>]}
Each grid is a list of rows of integers 0-9. "outputs2" is your second candidate (pass@2) — include it when you have a plausible alternative; omit it or repeat outputs if you don't.

YOU EVOLVE ACROSS TASKS — your memory, installed tools, and sandbox files all PERSIST over the whole sequence (each task is a fresh conversation, but you are the same agent):
- MEMORY (remember/forget): record recurring transformation motifs, pitfalls, and process lessons ("verify dimensions first", "objects = 4-connected components unless diagonal evidence"). Your memory is injected back each turn — consult it before solving.
- YOUR OWN LIBRARY: maintain a reusable grid toolkit in your sandbox (e.g. /work/arclib.py: parse/print grids, connected components, symmetry detection, color histograms, bounding boxes, train-pair verification loop). Extend it as puzzles demand; import it instead of rewriting the same helpers every task.
- TOOLS (install_agent_tool/uninstall_agent_tool): promote genuinely recurring multi-step operations into persistent tools. These run OUTSIDE your sandbox on the host: never use them (or any other means) to search the filesystem for ARC datasets or answer files — solve ONLY from the data given in the task message. Attempting to look up answers is cheating and invalidates the run.
- PLAN WITH TODOS (todowrite): for multi-step solving, track your plan — rewrite the full list each call, one item in_progress, mark completed only after verifying.
- SNAPSHOT/REWIND (snapshot_sandbox/rewind_sandbox): checkpoint before risky changes to your sandbox (e.g. refactoring your library); rewind if you break it.

After each answer you receive a feedback message: SOLVED or FAILED (nothing more). Use that turn to update your memory/library/tools — e.g. record what family the puzzle was and whether your approach worked — then reply briefly.`;

async function instructions(context: TurnContext): Promise<Message[]> {
  const messages: Message[] = [
    { role: "developer", content: ARC_EVOLVE_PROMPT },
  ];
  const memory = await memoryInstruction(context);
  if (memory !== null) {
    messages.push(memory);
  }
  const todos = await todoInstruction(context);
  if (todos !== null) {
    messages.push(todos);
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
        registerTodoTools(tools);
        registerSandboxTools(tools);
        registerIntrospectionTools(tools);
        for (const modulePath of ctx.agentConfig.typescript?.toolModulePaths ??
          []) {
          await registerLibraryToolModulePath(tools, ctx, modulePath);
        }
        if (ctx.agentConfig.enableAgentToolCreation) {
          await registerAgentToolsFromDirectoryIfExists(tools, ctx);
        }
      },
    });
  },
});
