import { existsSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

import {
  defineHarness,
  registerBuiltInTools,
  registerAgentToolsFromDirectoryIfExists,
  registerLibraryToolModulePath,
  registerAdapterTools,
  registerSkillTools,
  skillsInstruction,
  type BuiltInToolName,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import { registerIntrospectionTools } from "./introspection-tools";
import { registerSandboxTools } from "./sandbox-tools";
import { registerTaskTreeTools } from "./task-tree-tools";
import { registerSchedulerTools } from "./scheduler-tools";
import { memoryInstruction, registerMemoryTools } from "./memory-tools.js";
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
  registerMemoryTools(tools);
  registerSkillTools(tools);
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

async function workerclawInstructions(
  context: TurnContext,
): Promise<Message[]> {
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
        "You have full autonomy to plan and execute work. Maintain a task tree throughout the job using task_tree_init, task_tree_upsert_node, and task_tree_update_status. Depth 1 = objectives, depth 2 = sub-objectives, depth 3 = TODO leaves (isLeaf true). Update node status as you work: pending → in_progress → completed/failed. Report client outputs with report_deliverable (presentations, files, deployed URLs) — never send E2B desktop/VNC stream URLs to the client; those are internal Live view only. Fix recoverable sandbox/tool errors with executeCommand or e2b_run_command — do not call complete_task with status failed for fixable issues. When all TODO leaves are completed and deliverables are reported, call complete_task once. You may create external adapters (Slack, WhatsApp, Signal, Discord, IRC) with create_adapter and reply with send_adapter_message; do not auto-send model text externally.",
    },
    {
      role: "developer",
      content: [
        "## Self-evolution (memory, skills, tools)",
        "Persist what you learn across jobs:",
        "- remember / forget — durable facts (client prefs, project conventions, lessons). Injected every turn.",
        "- install_skill / use_skill / list_skills / uninstall_skill — reusable procedures in agent-skills format. Call use_skill before matching work. These are learned skills you author; they complement (do not replace) any methodology skills injected in the task briefing.",
        "- install_agent_tool — when the same helper is needed across rounds/jobs and no catalog tool covers it.",
        "Prefer remember for short facts, install_skill for multi-step playbooks, install_agent_tool for callable code helpers.",
      ].join("\n"),
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
  const memory = await memoryInstruction(context);
  if (memory !== null) {
    instructions.push(memory);
  }
  const skills = await skillsInstruction(context);
  if (skills !== null) {
    instructions.push(skills);
  }
  return instructions;
}

function oliviaToolLayerInstruction(context: TurnContext): Message {
  const layers = [
    "Tool layers (use the best match; all may be registered in the same turn):",
    "1. Olivia platform tools — registered natively by name (e.g. createPresentation, webSearch, githubCreateRepo) from the worker's enabled catalog, plus E2B library modules (e2b_*). The task briefing (# Available tools) lists the catalog plus meta-tools for reference. Prefer these for GitHub, presentations, deploy, Google, etc.",
    "2. WorkerClaw substrate — task_tree_*, report_deliverable, complete_task, adapters, sandbox/introspection, remember/forget, install_skill/use_skill.",
  ];
  if (context.agentConfig.enableAgentToolCreation) {
    layers.push(
      "3. install_agent_tool / uninstall_agent_tool — first-class. Install a reusable TypeScript tool under .exo/agent-tools/ when you need the same helper more than once and no Olivia catalog tool covers it. Previously installed agent tools reload each round. Do not reinstall duplicates of working Olivia catalog tools.",
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
