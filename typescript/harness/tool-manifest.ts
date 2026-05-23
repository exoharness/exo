import fs from "node:fs/promises";
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

export const DEFAULT_AGENT_TOOL_MANIFEST_PATH =
  ".exo/agent-tools/manifest.json";
let agentToolImportVersion = 0;

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

export async function registerAgentToolsFromManifestPathIfExists(
  registry: HarnessToolRegistry,
  context: TurnContext,
  manifestPath = DEFAULT_AGENT_TOOL_MANIFEST_PATH,
): Promise<void> {
  const manifest = await readToolManifestIfExists(manifestPath);
  if (manifest) {
    await registerAgentToolsFromManifest(registry, context, manifest);
  }
}

export async function readToolManifestIfExists(
  manifestPath: string,
): Promise<ToolManifest | null> {
  try {
    return await readToolManifest(manifestPath);
  } catch (error) {
    if (isNotFoundError(error)) {
      return null;
    }
    throw error;
  }
}

export async function readToolManifest(
  manifestPath: string,
): Promise<ToolManifest> {
  const raw = JSON.parse(await fs.readFile(manifestPath, "utf8")) as unknown;
  if (!isRecord(raw) || !Array.isArray(raw.tools)) {
    throw new Error(
      `tool manifest must contain a tools array: ${manifestPath}`,
    );
  }

  return {
    tools: raw.tools.map((entry, index) =>
      parseManifestEntry(entry, manifestPath, index),
    ),
  };
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
  const module = (await import(importSpecifier(modulePath, source))) as {
    default?: unknown;
  };
  if (!isTool(module.default)) {
    throw new Error(
      `${source} tool module must default export a Tool: ${modulePath}`,
    );
  }
  return module.default;
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

function parseManifestEntry(
  value: unknown,
  manifestPath: string,
  index: number,
): ToolManifestEntry {
  if (!isRecord(value)) {
    throw new Error(`tool manifest entry ${index} must be an object`);
  }
  if (typeof value.modulePath !== "string" || value.modulePath.length === 0) {
    throw new Error(`tool manifest entry ${index} must have a modulePath`);
  }
  if (!isRecord(value.initialization)) {
    throw new Error(
      `tool manifest entry ${index} must have an object initialization value`,
    );
  }

  return {
    modulePath: resolveManifestModulePath(manifestPath, value.modulePath),
    initialization: value.initialization,
  };
}

function resolveManifestModulePath(
  manifestPath: string,
  modulePath: string,
): string {
  if (
    modulePath.startsWith("data:") ||
    modulePath.startsWith("file:") ||
    path.isAbsolute(modulePath)
  ) {
    return modulePath;
  }
  return path.resolve(path.dirname(manifestPath), modulePath);
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
