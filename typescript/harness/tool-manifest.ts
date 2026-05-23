import type { JsonObject, TurnContext } from "./index";
import {
  initializeTool,
  type HarnessToolRegistry,
  type HarnessToolSource,
  type Tool,
  type ToolInstance,
} from "./tools";

export interface ToolManifest {
  tools: ToolManifestEntry[];
}

export interface ToolManifestEntry {
  modulePath: string;
  initialization: JsonObject;
}

export type LibraryToolManifest = ToolManifest;
export type LibraryToolManifestEntry = ToolManifestEntry;
export type AgentToolManifest = ToolManifest;
export type AgentToolManifestEntry = ToolManifestEntry;

export async function registerToolsFromManifest(
  registry: HarnessToolRegistry,
  context: TurnContext,
  manifest: ToolManifest,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<void> {
  for (const entry of manifest.tools) {
    registry.register(await loadToolFromManifestEntry(context, entry, source));
  }
}

export function registerLibraryToolsFromManifest(
  registry: HarnessToolRegistry,
  context: TurnContext,
  manifest: LibraryToolManifest,
): Promise<void> {
  return registerToolsFromManifest(registry, context, manifest, "library");
}

export function registerAgentToolsFromManifest(
  registry: HarnessToolRegistry,
  context: TurnContext,
  manifest: AgentToolManifest,
): Promise<void> {
  return registerToolsFromManifest(registry, context, manifest, "agent");
}

export async function loadToolFromManifestEntry(
  context: TurnContext,
  entry: ToolManifestEntry,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<ToolInstance> {
  const tool = await importTool(entry.modulePath, source);
  return initializeTool(tool, source, entry.initialization, context);
}

export function loadLibraryTool(
  context: TurnContext,
  entry: LibraryToolManifestEntry,
): Promise<ToolInstance> {
  return loadToolFromManifestEntry(context, entry, "library");
}

export function loadAgentTool(
  context: TurnContext,
  entry: AgentToolManifestEntry,
): Promise<ToolInstance> {
  return loadToolFromManifestEntry(context, entry, "agent");
}

async function importTool(
  modulePath: string,
  source: Extract<HarnessToolSource, "library" | "agent">,
): Promise<Tool> {
  const module = (await import(modulePath)) as { default?: unknown };
  if (!isTool(module.default)) {
    throw new Error(
      `${source} tool module must default export a Tool: ${modulePath}`,
    );
  }
  return module.default;
}

function isTool(value: unknown): value is Tool {
  if (!value || typeof value !== "object") {
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
