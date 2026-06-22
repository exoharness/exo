// Simple Coding Agent — a minimal Exo harness for terminal/coding benchmarks
// (Harbor / Terminal-Bench 2.0). See README.md in this directory.
//
// Deliberately minimal: a single `shell` tool + Exo's Responses turn loop, plus
// one system/developer prompt that frames autonomy, persistence, and verification.
// No memory, adapters, scheduler, MCP, or assistant persona — this isolates Exo's
// core coding-agent ability (turn loop + shell) driving the configured model.
//
// The benchmark task is delivered as the conversation's user turn; the agent acts
// entirely through `shell` (run commands, read/edit files, build, test) until done.

import {
  defineHarness,
  registerBuiltInTools,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import { runResponsesHarnessTurn } from "../typescript/turn-loop";

const AGENT_SYSTEM_PROMPT = `You are an autonomous software engineer operating in a Linux terminal via a shell tool. You are given a task and must complete it end to end on your own.

Operating rules:
- You CANNOT ask the user anything or wait for input. Work entirely on your own until the task is fully done.
- Do NOT stop early. Never give up on a task because it looks hard, large, or "infeasible" — attempt it for real and iterate. A partial, working attempt is far better than refusing.
- Explore first: inspect the working directory, read relevant files, and understand the environment before making changes.
- Make concrete progress with the shell: create and edit files, install what you need, run builds and programs.
- VERIFY before you finish: actually run/build/test your solution and check the output matches what the task requires. If it fails, debug and fix it — repeat until it works.
- Assume any error is yours to fix. Read the error, form a hypothesis, and try again.
- Keep going until you are confident the task's requirements are met; only then end your turn.`;

function instructions(_context: TurnContext): Message[] {
  return [{ role: "developer", content: AGENT_SYSTEM_PROMPT }];
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions,
      registerTools: (tools: HarnessToolRegistry, ctx: TurnContext) => {
        registerBuiltInTools(tools, ctx, ["shell"]);
      },
    });
  },
});
