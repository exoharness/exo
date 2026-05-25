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
import { DEFAULT_AGENT_TOOL_DIRECTORY, loadAgentTool } from "./tool-modules";

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
        "Install or replace an agent-created TypeScript tool so it can be used in the next model round. The moduleSource must use type-only imports from @exo/harness/tool and default-export a Tool. Do not import external npm packages; use Node built-ins and global APIs like fetch.",
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
              "Complete TypeScript source for a module that default-exports { definition, initializationParameters, initialize(...) } satisfies Tool. Do not use zod, inputSchema, outputSchema validators, call(...) handlers, or runtime imports from @exo/harness/tool.",
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
          availableNextRound: { type: "boolean" },
        },
        required: ["ok", "toolName", "modulePath", "availableNextRound"],
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
  const toolsDirectory = DEFAULT_AGENT_TOOL_DIRECTORY;
  const modulePath = path.join(toolsDirectory, `${name}.ts`);
  const sourcePath = path.join(toolsDirectory, `${name}.source.ts`);
  const tempDirectory = path.join(toolsDirectory, ".tmp");
  const tempId = `${process.pid}.${Date.now()}`;
  const tempSourcePath = path.join(
    tempDirectory,
    `${name}.${tempId}.source.ts`,
  );
  const tempModulePath = path.join(tempDirectory, `${name}.${tempId}.ts`);

  await fs.mkdir(toolsDirectory, { recursive: true });
  await fs.mkdir(tempDirectory, { recursive: true });
  let tool: ToolInstance | null = null;
  try {
    await fs.writeFile(tempSourcePath, moduleSource, "utf8");
    await fs.writeFile(
      tempModulePath,
      agentToolWrapperSource(
        `./${path.basename(tempSourcePath)}`,
        initialization,
      ),
      "utf8",
    );
    tool = await loadAgentTool(context, path.resolve(tempModulePath));
    if (
      tool.definition.name === "shell" ||
      tool.definition.name === "install_agent_tool"
    ) {
      throw new Error(
        `agent tool cannot replace built-in tool: ${tool.definition.name}`,
      );
    }
  } finally {
    await fs.rm(tempSourcePath, { force: true });
    await fs.rm(tempModulePath, { force: true });
  }
  if (!tool) {
    throw new Error("agent tool validation did not return a tool");
  }

  await fs.writeFile(sourcePath, moduleSource, "utf8");
  await fs.writeFile(
    modulePath,
    agentToolWrapperSource(`./${path.basename(sourcePath)}`, initialization),
    "utf8",
  );

  return {
    ok: true,
    toolName: tool.definition.name,
    modulePath,
    availableNextRound: true,
  };
}

function agentToolWrapperSource(
  sourceModulePath: string,
  initialization: JsonObject,
): string {
  return `import tool from ${JSON.stringify(sourceModulePath)};

export default {
  tool,
  initialization: ${JSON.stringify(initialization, null, 2)},
};
`;
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
