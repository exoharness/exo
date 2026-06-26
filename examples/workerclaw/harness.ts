import { existsSync, readFileSync } from "node:fs";

import {
  defineHarness,
  registerBuiltInTools,
  registerAgentToolsFromDirectoryIfExists,
  registerLibraryToolModulePath,
  registerAdapterTools,
  type BuiltInToolName,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import { registerIntrospectionTools } from "./introspection-tools";
import { registerSandboxTools } from "./sandbox-tools";
import { registerTaskTreeTools } from "./task-tree-tools";
import { registerSchedulerTools } from "./scheduler-tools";
import {
  basicHarnessInstructions,
  defaultBuiltInToolNames,
  runResponsesHarnessTurn,
} from "../typescript/turn-loop";

const WORKERCLAW_IDENTITY_PROMPT = readFileSync(
  new URL("./prompts/me.md", import.meta.url),
  "utf8",
).trim();
const DEFAULT_LOCAL_PROMPT_PATH = ".exo/workerclaw-profile.md";
const DEFAULT_WORKERCLAW_REPO = "/workspace/exo";
const DEFAULT_WORKERCLAW_SELF_MAP = `${DEFAULT_WORKERCLAW_REPO}/examples/workerclaw/SELF.md`;

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions: workerclawInstructions,
      registerTools: registerWorkerclawTools,
    });
  },
});

async function registerWorkerclawTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  registerBuiltInTools(tools, context, builtInToolNames(context));
  registerTaskTreeTools(tools);
  registerAdapterTools(tools);
  registerIntrospectionTools(tools);
  registerSandboxTools(tools);
  if (process.env.WORKERCLAW_ENABLE_SCHEDULER === "true") {
    registerSchedulerTools(tools);
  }
  for (const modulePath of context.agentConfig.typescript?.toolModulePaths ??
    []) {
    await registerLibraryToolModulePath(tools, context, modulePath);
  }
  if (context.agentConfig.enableAgentToolCreation) {
    await registerAgentToolsFromDirectoryIfExists(tools, context);
  }
}

function builtInToolNames(context: TurnContext): BuiltInToolName[] {
  return defaultBuiltInToolNames(context);
}

function workerclawInstructions(context: TurnContext): Message[] {
  const repoPath = process.env.WORKERCLAW_REPO ?? DEFAULT_WORKERCLAW_REPO;
  const selfMapPath =
    process.env.WORKERCLAW_SELF_MAP ?? DEFAULT_WORKERCLAW_SELF_MAP;
  const instructions: Message[] = [
    ...basicHarnessInstructions(context),
    {
      role: "developer",
      content: WORKERCLAW_IDENTITY_PROMPT,
    },
    {
      role: "developer",
      content:
        "You have full autonomy to plan and execute work. Maintain a task tree throughout the job using task_tree_init, task_tree_upsert_node, and task_tree_update_status. Depth 1 = objectives, depth 2 = sub-objectives, depth 3 = TODO leaves (isLeaf true). Update node status as you work: pending → in_progress → completed/failed. Report outputs with report_deliverable. When all work is done, call complete_task once. You may create external adapters (Slack, WhatsApp, Signal, Discord, IRC) with create_adapter and reply with send_adapter_message; do not auto-send model text externally. Extra tools may be loaded from toolModulePaths — use them for execution.",
    },
    {
      role: "developer",
      content: `WorkerClaw source is at ${repoPath}. See ${selfMapPath} for layout. Local overrides may live in ${process.env.WORKERCLAW_LOCAL_PROMPT_FILE ?? DEFAULT_LOCAL_PROMPT_PATH}.`,
    },
  ];
  const localPrompt = readLocalPrompt();
  if (localPrompt !== null) {
    instructions.push({
      role: "developer",
      content: localPrompt,
    });
  }
  return instructions;
}

function readLocalPrompt(): string | null {
  const path =
    process.env.WORKERCLAW_LOCAL_PROMPT_FILE ?? DEFAULT_LOCAL_PROMPT_PATH;
  if (!existsSync(path)) {
    return null;
  }
  const contents = readFileSync(path, "utf8").trim();
  return contents.length === 0 ? null : contents;
}
