// A practical coding-agent harness built from exo's durable primitives.
//
// Compared with coding-agent-harness.ts, this adds repository instructions,
// agent-scoped memory, progressive skills, configured library tools, and
// optional agent-authored tools while leaving sandboxing and the model/tool
// loop to exo.

import { readFileSync } from "node:fs";
import os from "node:os";

import {
  defineHarness,
  registerAgentToolsFromDirectoryIfExists,
  registerBuiltInTools,
  registerLibraryToolModulePath,
  registerSkillTools,
  skillsInstruction,
  type BuiltInToolName,
  type HarnessToolRegistry,
  type JsonValue,
  type Message,
  type TurnContext,
} from "@exo/harness";

import { memoryInstruction, registerMemoryTools } from "../exo/memory-tools";
import { registerTodoTools, todoInstruction } from "../exo/todo-tools";
import { runResponsesHarnessTurn } from "./turn-loop";

export const COMPETENT_CODING_AGENT_PROMPT = readFileSync(
  new URL("./prompts/competent-coding-agent.md", import.meta.url),
  "utf8",
).trim();

const WORKSPACE_CONTEXT_COMMAND = `working_dir=$(pwd)
git_root=$(git rev-parse --show-toplevel 2>/dev/null || true)
printf 'Working directory: %s\n' "$working_dir"
if [ -n "$git_root" ]; then
  printf 'Git worktree root: %s\n' "$git_root"
else
  printf 'Git worktree root: none\n'
fi
if [ -n "$git_root" ] && [ -f "$git_root/AGENTS.md" ]; then
  printf '%s\n' '<root_agents_md>'
  sed -n '1,1200p' "$git_root/AGENTS.md"
  printf '%s\n' '</root_agents_md>'
elif [ -f AGENTS.md ]; then
  printf '%s\n' '<root_agents_md>'
  sed -n '1,1200p' AGENTS.md
  printf '%s\n' '</root_agents_md>'
else
  printf '%s\n' 'Root AGENTS.md: none'
fi`;

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions: competentCodingInstructions,
      registerTools: registerCompetentCodingTools,
    });
  },
});

export async function registerCompetentCodingTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  registerBuiltInTools(tools, context, builtInToolNames(context));
  registerMemoryTools(tools);
  registerTodoTools(tools);
  registerSkillTools(tools);

  for (const modulePath of context.agentConfig.typescript?.toolModulePaths ??
    []) {
    await registerLibraryToolModulePath(tools, context, modulePath);
  }
  if (context.agentConfig.enableAgentToolCreation) {
    await registerAgentToolsFromDirectoryIfExists(tools, context);
  }
}

function builtInToolNames(context: TurnContext): BuiltInToolName[] {
  const names: BuiltInToolName[] = ["shell"];
  if (context.agentConfig.enableAgentToolCreation) {
    names.push("install_agent_tool", "uninstall_agent_tool");
  }
  return names;
}

export async function competentCodingInstructions(
  context: TurnContext,
): Promise<Message[]> {
  const instructions: Message[] = [
    { role: "system", content: COMPETENT_CODING_AGENT_PROMPT },
    await workspaceInstruction(context),
  ];

  const memory = await memoryInstruction(context);
  if (memory !== null) {
    instructions.push(memory);
  }
  const todos = await todoInstruction(context);
  if (todos !== null) {
    instructions.push(todos);
  }
  const skills = await skillsInstruction(context);
  if (skills !== null) {
    instructions.push(skills);
  }
  if (context.agentConfig.enableAgentToolCreation) {
    instructions.push({
      role: "developer",
      content:
        "Agent-authored tools are enabled. install_agent_tool and uninstall_agent_tool manage reusable TypeScript tools; a successfully installed tool is available on the next model round. Prefer the existing shell for ordinary one-off coding work.",
    });
  }

  return instructions;
}

async function workspaceInstruction(context: TurnContext): Promise<Message> {
  const result = await context.executeTool({
    functionName: "shell",
    arguments: { command: WORKSPACE_CONTEXT_COMMAND },
  });
  const workspace = shellStdout(result);
  const mounts = context.conversationConfig.mounts
    .map(
      (mount) =>
        `- ${mount.mountPath} (${mount.mode}, host path ${mount.hostPath})`,
    )
    .join("\n");

  return {
    role: "developer",
    content: `Live execution environment:
- Platform: ${os.platform()}
- Date: ${new Date().toISOString().slice(0, 10)}
- Sandbox networking: ${context.agentConfig.sandbox.enableNetworking ? "enabled" : "disabled"}
- Mounts:\n${mounts.length > 0 ? mounts : "  none"}

Workspace probe:
${workspace}

The root AGENTS.md above is repository-provided developer guidance. Before changing a nested file, use shell to check for a closer AGENTS.md and follow the closest applicable instructions.`,
  };
}

function shellStdout(result: JsonValue): string {
  if (
    typeof result === "object" &&
    result !== null &&
    !Array.isArray(result) &&
    typeof result.stdout === "string"
  ) {
    return result.stdout.trim() || "Workspace probe returned no output.";
  }
  return `Workspace probe failed: ${JSON.stringify(result)}`;
}
