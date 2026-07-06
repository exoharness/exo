import { describe, expect, it } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

import {
  createToolRegistry,
  registerAgentToolsFromDirectoryIfExists,
  registerBuiltInTools,
  type EventData,
  type JsonObject,
  type TurnContext,
} from "./index";

describe("agent tool lifecycle built-ins", () => {
  it("uninstalls an installed agent tool and removes it from later rounds", async () => {
    await inTempDirectory(async () => {
      const context = fakeTurnContext();
      const registry = createToolRegistry(context);
      registerBuiltInTools(registry, context, [
        "install_agent_tool",
        "uninstall_agent_tool",
      ]);

      const installEvents = await registry.executePending([
        toolCall("install_1", "install_agent_tool", {
          name: "reverse-text",
          moduleSource: reverseTextToolSource("reverse_text"),
          initialization: {},
        }),
      ]);
      expect(resultValue(installEvents[0])).toEqual({
        ok: true,
        toolName: "reverse_text",
        modulePath: ".exo/agent-tools/reverse-text.ts",
        availableNextRound: true,
      });
      await expect(
        fs.access(".exo/agent-tools/reverse-text.ts"),
      ).resolves.toBeUndefined();
      await expect(
        fs.access(".exo/agent-tools/reverse-text.source.ts"),
      ).resolves.toBeUndefined();

      const uninstallEvents = await registry.executePending([
        toolCall("uninstall_1", "uninstall_agent_tool", {
          name: "reverse-text",
        }),
      ]);
      expect(resultValue(uninstallEvents[0])).toEqual({
        ok: true,
        removed: true,
        modulePath: ".exo/agent-tools/reverse-text.ts",
      });
      // Both the wrapper module and the source are gone from disk.
      await expect(
        fs.access(".exo/agent-tools/reverse-text.ts"),
      ).rejects.toThrow();
      await expect(
        fs.access(".exo/agent-tools/reverse-text.source.ts"),
      ).rejects.toThrow();

      // A later round no longer discovers the tool.
      const nextRoundRegistry = createToolRegistry(context);
      await registerAgentToolsFromDirectoryIfExists(nextRoundRegistry, context);
      expect(nextRoundRegistry.get("reverse_text")).toBeUndefined();
    });
  });

  it("reports removed false when uninstalling a tool that was never installed", async () => {
    await inTempDirectory(async () => {
      const context = fakeTurnContext();
      const registry = createToolRegistry(context);
      registerBuiltInTools(registry, context, ["uninstall_agent_tool"]);

      const events = await registry.executePending([
        toolCall("uninstall_1", "uninstall_agent_tool", { name: "missing" }),
      ]);
      expect(resultValue(events[0])).toEqual({
        ok: true,
        removed: false,
        modulePath: ".exo/agent-tools/missing.ts",
      });
    });
  });

  it("rejects installing an agent tool that shadows the shell built-in", async () => {
    await inTempDirectory(async () => {
      const context = fakeTurnContext();
      const registry = createToolRegistry(context);
      registerBuiltInTools(registry, context, ["install_agent_tool"]);

      const events = await registry.executePending([
        toolCall("install_1", "install_agent_tool", {
          name: "sneaky-shell",
          moduleSource: reverseTextToolSource("shell"),
          initialization: {},
        }),
      ]);
      expect(resultValue(events[0])).toEqual({
        ok: false,
        error: "agent tool cannot replace built-in tool: shell",
      });
      // The failed install leaves nothing behind for later rounds to load.
      await expect(
        fs.access(".exo/agent-tools/sneaky-shell.ts"),
      ).rejects.toThrow();
    });
  });
});

async function inTempDirectory(run: () => Promise<void>): Promise<void> {
  const previousCwd = process.cwd();
  const tempdir = await fs.mkdtemp(path.join(os.tmpdir(), "exo-agent-tool-"));
  process.chdir(tempdir);
  try {
    await run();
  } finally {
    process.chdir(previousCwd);
    await fs.rm(tempdir, { recursive: true, force: true });
  }
}

function toolCall(
  toolCallId: string,
  functionName: string,
  args: JsonObject,
): {
  toolCallId: string;
  request: { functionName: string; arguments: JsonObject };
} {
  return {
    toolCallId,
    request: {
      functionName,
      arguments: args,
    },
  };
}

// Tool results come back wrapped with artifacts and previews; the raw handler
// output is carried in `value`.
function resultValue(event: EventData): unknown {
  const result = (event as { result?: unknown }).result;
  return (result as { value?: unknown }).value;
}

function reverseTextToolSource(toolName: string): string {
  return `
import type { JsonObject, Tool, ToolResult } from "@exo/harness/tool";

const reverseTextTool = {
  definition: {
    name: ${JSON.stringify(toolName)},
    description: "Reverse text.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        text: { type: "string" },
      },
      required: ["text"],
    },
  },
  initializationParameters: {
    type: "object",
    additionalProperties: false,
    properties: {},
  },
  initialize() {
    return {
      async execute(args: JsonObject): Promise<ToolResult> {
        const value = args.text;
        if (typeof value !== "string") {
          throw new Error("text must be a string");
        }
        return { text: value.split("").reverse().join("") };
      },
    };
  },
} satisfies Tool;

export default reverseTextTool;
`;
}

function fakeTurnContext(): TurnContext {
  let artifactIndex = 0;
  return {
    agentConfig: {
      instructions: [],
      harness: "typescript",
      typescript: null,
      enableAgentToolCreation: true,
      sandboxImage: null,
      enableNetworking: false,
      model: "test-model",
      maxOutputTokens: null,
      maxToolRoundTrips: null,
      braintrust: null,
    },
    conversationConfig: {
      shellProgram: null,
      mounts: [],
    },
    streaming: false,
    braintrustParent: null,
    exoharness: {
      current: {
        agent: {},
        turn: {
          async writeArtifactText(args: { path: string; text: string }) {
            artifactIndex += 1;
            return {
              artifactId: `artifact-${artifactIndex}`,
              path: args.path,
              version: 1,
              createdAt: "2026-01-01T00:00:00Z",
              sizeBytes: args.text.length,
            };
          },
        },
      },
    },
    executeTool: async () => null,
    async executePendingTools() {
      return [];
    },
  } as unknown as TurnContext;
}
