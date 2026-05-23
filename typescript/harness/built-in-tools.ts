import type {
  ConversationConfig,
  JsonObject,
  ToolDefinition,
  ToolResult,
  TurnContext,
} from "./index";
import type { HarnessToolRegistry, ToolInstance } from "./tools";

export type BuiltInToolName = "shell";

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
