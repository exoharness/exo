import { existsSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

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
  runWorkerclawHarnessTurn,
} from "./turn-loop.js";

const WORKERCLAW_IDENTITY_PROMPT = readFileSync(
  new URL("./prompts/me.md", import.meta.url),
  "utf8",
).trim();
const DEFAULT_LOCAL_PROMPT_PATH = ".exo/workerclaw-profile.md";
const DEFAULT_WORKERCLAW_REPO = "/workspace/exo";
const DEFAULT_WORKERCLAW_SELF_MAP = `${DEFAULT_WORKERCLAW_REPO}/examples/workerclaw/SELF.md`;

export default defineHarness({
  async runTurn(context) {
    await runWorkerclawHarnessTurn(context, {
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
  const toolModulePaths = context.agentConfig.typescript?.toolModulePaths ?? [];
  for (const modulePath of toolModulePaths) {
    if (modulePath.endsWith("olivia-native-tools.ts")) {
      continue;
    }
    await registerWorkerclawToolModule(tools, context, modulePath);
  }
  const nativeResult = await registerOliviaNativeToolsIfPresent(
    tools,
    context,
    toolModulePaths,
  );
  await registerOliviaInvokeFallbackIfNeeded(
    tools,
    context,
    toolModulePaths,
    nativeResult,
  );
  if (context.agentConfig.enableAgentToolCreation) {
    await registerAgentToolsFromDirectoryIfExists(tools, context);
  }
}

async function registerWorkerclawToolModule(
  registry: HarnessToolRegistry,
  context: TurnContext,
  modulePath: string,
): Promise<void> {
  const mod = await import(pathToFileURL(modulePath).href);
  if (typeof mod.registerOliviaWorkerTools === "function") {
    await mod.registerOliviaWorkerTools(registry, context);
    return;
  }
  await registerLibraryToolModulePath(registry, context, modulePath);
}

async function registerOliviaNativeToolsIfPresent(
  registry: HarnessToolRegistry,
  context: TurnContext,
  modulePaths: string[],
): Promise<{ registered: string[]; skipped: string[] }> {
  const nativePath = modulePaths.find((path) =>
    path.endsWith("olivia-native-tools.ts"),
  );
  if (!nativePath) {
    return { registered: [], skipped: [] };
  }
  if (process.env.OLIVIA_NATIVE_TOOLS === "false") {
    return { registered: [], skipped: [] };
  }
  const mod = await import(pathToFileURL(nativePath).href);
  if (typeof mod.registerOliviaWorkerTools !== "function") {
    return { registered: [], skipped: [] };
  }
  return mod.registerOliviaWorkerTools(registry, context);
}

function resolveOliviaToolsFallbackPath(modulePaths: string[]): string | null {
  const nativePath = modulePaths.find((path) =>
    path.endsWith("olivia-native-tools.ts"),
  );
  if (!nativePath) {
    return null;
  }
  return nativePath.replace(/olivia-native-tools\.ts$/, "olivia-tools.ts");
}

async function registerOliviaInvokeFallbackIfNeeded(
  registry: HarnessToolRegistry,
  context: TurnContext,
  modulePaths: string[],
  nativeResult: { registered: string[]; skipped: string[] },
): Promise<void> {
  const shouldFallback =
    process.env.OLIVIA_INVOKE_FALLBACK === "true" ||
    nativeResult.registered.length === 0;
  if (!shouldFallback) {
    return;
  }
  const fallbackPath = resolveOliviaToolsFallbackPath(modulePaths);
  if (!fallbackPath || !existsSync(fallbackPath)) {
    return;
  }
  await registerLibraryToolModulePath(registry, context, fallbackPath);
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
        "You have full autonomy to plan and execute work. Maintain a task tree throughout the job using task_tree_init, task_tree_upsert_node, and task_tree_update_status. Depth 1 = objectives, depth 2 = sub-objectives, depth 3 = TODO leaves (isLeaf true). Update node status as you work: pending → in_progress → completed/failed. Report outputs with report_deliverable. When all work is done, call complete_task once. You may create external adapters (Slack, WhatsApp, Signal, Discord, IRC) with create_adapter and reply with send_adapter_message; do not auto-send model text externally.",
    },
    oliviaToolLayerInstruction(context),
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

function oliviaToolLayerInstruction(context: TurnContext): Message {
  const layers = [
    "Tool layers (use the best match; all may be registered in the same turn):",
    "1. Olivia platform tools — registered natively by name (e.g. createPresentation, webSearch, githubCreateRepo) from the worker's enabled catalog, plus E2B library modules (e2b_*). The task briefing (# Available tools) lists the same catalog for reference. Prefer these for GitHub, presentations, deploy, Google, etc.",
    "2. WorkerClaw substrate — task_tree_*, report_deliverable, complete_task, adapters, sandbox/introspection tools.",
  ];
  if (context.agentConfig.enableAgentToolCreation) {
    layers.push(
      "3. Agent-installed tools — previously saved under .exo/agent-tools/ (reloaded each round). Install new ones with install_agent_tool when no platform tool fits; use uninstall_agent_tool to remove obsolete or duplicate helpers. Do not reinstall tools that duplicate Olivia catalog tools (Unless the tool is not working).",
    );
  }
  return { role: "developer", content: layers.join("\n") };
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
