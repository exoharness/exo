import fs from "node:fs/promises";
import type { Dirent } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

import type { JsonObject, TurnContext } from "./index";
import {
  initializeTool,
  type HarnessToolRegistry,
  type HarnessToolSource,
  type Tool,
  type ToolInstance,
} from "./tools";

export const DEFAULT_AGENT_TOOL_DIRECTORY = ".exo/agent-tools";
let agentToolImportVersion = 0;

export interface ToolModule {
  tools: ToolModuleEntry[];
}

export interface ToolModuleEntry {
  tool: Tool;
  initialization?: JsonObject;
}

export type ToolModuleExport =
  | Tool
  | ToolModuleEntry
  | ToolModule
  | Array<Tool | ToolModuleEntry>;

export function defineToolModule(module: ToolModule): ToolModule {
  return module;
}

export function defineToolModuleEntry(entry: ToolModuleEntry): ToolModuleEntry {
  return entry;
}

export async function registerTools(
  registry: HarnessToolRegistry,
  context: TurnContext,
  exported: ToolModuleExport,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<void> {
  for (const entry of normalizeToolModuleExport(exported, source)) {
    registry.register(await initializeToolModuleEntry(context, entry, source));
  }
}

export function registerLibraryTools(
  registry: HarnessToolRegistry,
  context: TurnContext,
  exported: ToolModuleExport,
): Promise<void> {
  return registerTools(registry, context, exported, "library");
}

export function registerAgentTools(
  registry: HarnessToolRegistry,
  context: TurnContext,
  exported: ToolModuleExport,
): Promise<void> {
  return registerTools(registry, context, exported, "agent");
}

export async function registerToolModulePath(
  registry: HarnessToolRegistry,
  context: TurnContext,
  modulePath: string,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<void> {
  await registerTools(
    registry,
    context,
    await loadToolModule(modulePath, source),
    source,
  );
}

export function registerLibraryToolModulePath(
  registry: HarnessToolRegistry,
  context: TurnContext,
  modulePath: string,
): Promise<void> {
  return registerToolModulePath(registry, context, modulePath, "library");
}

export function registerAgentToolModulePath(
  registry: HarnessToolRegistry,
  context: TurnContext,
  modulePath: string,
): Promise<void> {
  return registerToolModulePath(registry, context, modulePath, "agent");
}

export async function registerAgentToolsFromDirectoryIfExists(
  registry: HarnessToolRegistry,
  context: TurnContext,
  toolsDirectory = DEFAULT_AGENT_TOOL_DIRECTORY,
): Promise<void> {
  let entries: Dirent[];
  try {
    entries = await fs.readdir(toolsDirectory, { withFileTypes: true });
  } catch (error) {
    if (isNotFoundError(error)) {
      return;
    }
    throw error;
  }

  const modulePaths = entries
    .filter((entry) => entry.isFile())
    .map((entry) => entry.name)
    .filter((name) => name.endsWith(".ts") && !name.endsWith(".source.ts"))
    .sort()
    .map((name) => path.resolve(toolsDirectory, name));

  for (const modulePath of modulePaths) {
    await registerAgentToolModulePath(registry, context, modulePath);
  }
}

export async function loadToolModule(
  modulePath: string,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<ToolModuleExport> {
  const module = (await import(importSpecifier(modulePath, source))) as Record<
    string,
    unknown
  >;
  const exported =
    module.default ?? module.toolModule ?? module.tool ?? module.tools;
  if (!exported) {
    throw new Error(
      `${source} tool module must export a Tool, ToolModuleEntry, or ToolModule: ${modulePath}`,
    );
  }
  if (module.tools && exported === module.tools) {
    return { tools: normalizeToolArray(module.tools, source) };
  }
  return normalizeToolModuleExport(exported, source);
}

export async function loadAgentTool(
  context: TurnContext,
  modulePath: string,
): Promise<ToolInstance> {
  const entries = normalizeToolModuleExport(
    await loadToolModule(modulePath, "agent"),
    "agent",
  );
  if (entries.length !== 1) {
    throw new Error(
      `agent tool module must export exactly one tool: ${modulePath}`,
    );
  }
  return initializeToolModuleEntry(context, entries[0], "agent");
}

function initializeToolModuleEntry(
  context: TurnContext,
  entry: ToolModuleEntry,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<ToolInstance> {
  return initializeTool(
    entry.tool,
    source,
    entry.initialization ?? entry.tool.initialization ?? {},
    context,
  );
}

function normalizeToolModuleExport(
  exported: unknown,
  source: Extract<HarnessToolSource, "library" | "agent">,
): ToolModuleEntry[] {
  if (Array.isArray(exported)) {
    return normalizeToolArray(exported, source);
  }
  if (isTool(exported)) {
    return [{ tool: exported }];
  }
  if (isToolModuleEntry(exported)) {
    return [exported];
  }
  if (isToolModule(exported)) {
    return normalizeToolArray(exported.tools, source);
  }
  throw new Error(
    `${source} tool module export must be a Tool, ToolModuleEntry, or ToolModule`,
  );
}

function normalizeToolArray(
  values: unknown,
  source: Extract<HarnessToolSource, "library" | "agent">,
): ToolModuleEntry[] {
  if (!Array.isArray(values)) {
    throw new Error(`${source} tool module tools export must be an array`);
  }
  return values.flatMap((value) => normalizeToolModuleExport(value, source));
}

function importSpecifier(
  modulePath: string,
  source: Extract<HarnessToolSource, "library" | "agent">,
): string {
  if (source !== "agent") {
    return modulePath;
  }
  if (modulePath.startsWith("data:")) {
    return modulePath;
  }
  const url = modulePath.startsWith("file:")
    ? new URL(modulePath)
    : path.isAbsolute(modulePath)
      ? pathToFileURL(modulePath)
      : null;
  if (!url) {
    return modulePath;
  }
  agentToolImportVersion += 1;
  url.searchParams.set("agentToolVersion", String(agentToolImportVersion));
  return url.href;
}

function isTool(value: unknown): value is Tool {
  if (!isRecord(value)) {
    return false;
  }
  const candidate = value as {
    definition?: unknown;
    initializationParameters?: unknown;
    initialize?: unknown;
  };
  return (
    Boolean(candidate.definition) &&
    Boolean(candidate.initializationParameters) &&
    typeof candidate.initialize === "function"
  );
}

function isToolModuleEntry(value: unknown): value is ToolModuleEntry {
  if (!isRecord(value) || !isTool(value.tool)) {
    return false;
  }
  if (value.initialization === undefined) {
    return true;
  }
  return isRecord(value.initialization);
}

function isToolModule(value: unknown): value is ToolModule {
  return isRecord(value) && Array.isArray(value.tools);
}

function isRecord(value: unknown): value is JsonObject {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function isNotFoundError(error: unknown): boolean {
  return (
    error !== null &&
    typeof error === "object" &&
    "code" in error &&
    (error as { code?: unknown }).code === "ENOENT"
  );
}
