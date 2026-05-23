import fs from "node:fs/promises";
import path from "node:path";

import type {
  ConversationConfig,
  JsonObject,
  ToolDefinition,
  ToolResult,
  TurnContext,
} from "./index";
import type { HarnessToolRegistry, ToolInstance } from "./tools";
import {
  DEFAULT_AGENT_TOOL_MANIFEST_PATH,
  loadAgentTool,
  type AgentToolManifest,
  type AgentToolManifestEntry,
} from "./tool-manifest";

export type BuiltInToolName = "shell" | "install_agent_tool";

export function registerBuiltInTools(
  registry: HarnessToolRegistry,
  context: TurnContext,
  names: BuiltInToolName[],
): void {
  for (const name of names) {
    if (name === "shell") {
      const shell = createShellToolInstance(context.conversationConfig);
      if (shell) {
        registry.register(shell);
      }
    } else if (name === "install_agent_tool") {
      registry.register(createInstallAgentToolInstance());
    }
  }
}

export function createShellToolInstance(
  config: ConversationConfig,
): ToolInstance | null {
  if (!config.shellProgram) {
    return null;
  }
  return {
    source: "built_in",
    definition: shellToolDefinition(config.shellProgram),
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: "shell",
          arguments: args,
        });
      },
    },
  };
}

export function buildShellToolDefinitions(
  config: ConversationConfig,
): ToolDefinition[] {
  const tool = createShellToolInstance(config);
  return tool ? [tool.definition] : [];
}

function shellToolDefinition(shellProgram: string): ToolDefinition {
  return {
    name: "shell",
    description: `Run a shell command using ${shellProgram}.`,
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        command: {
          type: "string",
          description: "Shell command to execute.",
        },
      },
      required: ["command"],
    },
  };
}

export function shellToolRequest(args: JsonObject): {
  functionName: "shell";
  arguments: JsonObject;
} {
  return {
    functionName: "shell",
    arguments: args,
  };
}

export type ShellToolResult = ToolResult;

function createInstallAgentToolInstance(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "install_agent_tool",
      description:
        "Install or replace an agent-created TypeScript tool so it can be used in the next model round. The moduleSource must default-export a Tool from @exo/harness. Do not import external npm packages; use Node built-ins, global APIs like fetch, and type-only imports from @exo/harness.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description:
              "Filesystem-safe tool module name, for example curl-tool or grab_webpage.",
          },
          moduleSource: {
            type: "string",
            description:
              "Complete TypeScript source for a module that default-exports a Tool. Use the shape { definition, initializationParameters, initialize(...) }. Do not use zod, inputSchema, outputSchema validators, or call(...) handlers.",
          },
          initialization: {
            type: "object",
            additionalProperties: false,
            properties: {},
            description:
              "Initialization arguments for the tool's initializationParameters schema.",
          },
        },
        required: ["name", "moduleSource", "initialization"],
      },
      outputSchema: {
        type: "object",
        additionalProperties: false,
        properties: {
          ok: { type: "boolean" },
          toolName: { type: "string" },
          modulePath: { type: "string" },
          manifestPath: { type: "string" },
          availableNextRound: { type: "boolean" },
        },
        required: [
          "ok",
          "toolName",
          "modulePath",
          "manifestPath",
          "availableNextRound",
        ],
      },
    },
    handler: {
      execute(args, execution) {
        return installAgentTool(execution.context, args);
      },
    },
  };
}

async function installAgentTool(
  context: TurnContext,
  args: JsonObject,
): Promise<ToolResult> {
  const name = stringArgument(args, "name");
  if (!/^[A-Za-z0-9_-]+$/.test(name)) {
    throw new Error(
      "agent tool name must contain only letters, numbers, underscores, and dashes",
    );
  }
  const moduleSource = stringArgument(args, "moduleSource");
  const initialization = objectArgument(args, "initialization");
  const toolsDirectory = path.dirname(DEFAULT_AGENT_TOOL_MANIFEST_PATH);
  const manifestPath = DEFAULT_AGENT_TOOL_MANIFEST_PATH;
  const modulePath = path.join(toolsDirectory, `${name}.ts`);

  await fs.mkdir(toolsDirectory, { recursive: true });
  await fs.writeFile(modulePath, moduleSource, "utf8");

  const tool = await loadAgentTool(context, {
    modulePath: path.resolve(modulePath),
    initialization,
  });
  if (
    tool.definition.name === "shell" ||
    tool.definition.name === "install_agent_tool"
  ) {
    throw new Error(
      `agent tool cannot replace built-in tool: ${tool.definition.name}`,
    );
  }

  const manifest = await readWritableAgentToolManifest(manifestPath);
  const manifestEntry: AgentToolManifestEntry = {
    modulePath: `./${name}.ts`,
    initialization,
  };
  const existingIndex = manifest.tools.findIndex(
    (entry) => entry.modulePath === manifestEntry.modulePath,
  );
  if (existingIndex >= 0) {
    manifest.tools[existingIndex] = manifestEntry;
  } else {
    manifest.tools.push(manifestEntry);
  }
  await fs.writeFile(
    manifestPath,
    `${JSON.stringify(manifest, null, 2)}\n`,
    "utf8",
  );

  return {
    ok: true,
    toolName: tool.definition.name,
    modulePath,
    manifestPath,
    availableNextRound: true,
  };
}

async function readWritableAgentToolManifest(
  manifestPath: string,
): Promise<AgentToolManifest> {
  try {
    const value = JSON.parse(
      await fs.readFile(manifestPath, "utf8"),
    ) as unknown;
    if (!isRecord(value) || !Array.isArray(value.tools)) {
      throw new Error(
        `agent tool manifest must contain a tools array: ${manifestPath}`,
      );
    }
    return {
      tools: value.tools.map((entry, index) =>
        parseWritableManifestEntry(entry, index),
      ),
    };
  } catch (error) {
    if (isNotFoundError(error)) {
      return { tools: [] };
    }
    throw error;
  }
}

function parseWritableManifestEntry(
  value: unknown,
  index: number,
): AgentToolManifestEntry {
  if (!isRecord(value)) {
    throw new Error(`agent tool manifest entry ${index} must be an object`);
  }
  if (typeof value.modulePath !== "string" || value.modulePath.length === 0) {
    throw new Error(
      `agent tool manifest entry ${index} must have a modulePath`,
    );
  }
  if (!isRecord(value.initialization)) {
    throw new Error(
      `agent tool manifest entry ${index} must have an object initialization value`,
    );
  }
  return {
    modulePath: value.modulePath,
    initialization: value.initialization,
  };
}

function stringArgument(args: JsonObject, name: string): string {
  const value = args[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(
      `install_agent_tool argument ${name} must be a non-empty string`,
    );
  }
  return value;
}

function objectArgument(args: JsonObject, name: string): JsonObject {
  const value = args[name];
  if (!isRecord(value)) {
    throw new Error(`install_agent_tool argument ${name} must be an object`);
  }
  return value;
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
