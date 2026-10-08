import { randomUUID } from "node:crypto";
import {
  createSdkMcpServer,
  type McpSdkServerConfigWithInstance,
} from "@anthropic-ai/claude-agent-sdk";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import {
  toJsonObject,
  toolRequestedEvent,
  toolResultEvent,
  type TurnContext,
} from "./index";
import {
  clientModelInputContent,
  getClientToolModelInput,
  toolResultFailed,
} from "./client-tools";

export const CLAUDE_RUNTIME_SERVER = "exo_runtime";

export function claudeRuntimeToolName(context: TurnContext, name: string) {
  return context.tools.find(
    (tool) => name === `mcp__${CLAUDE_RUNTIME_SERVER}__${tool.name}`,
  )?.name;
}

export function claudeRuntimeTools(
  context: TurnContext,
): Record<string, McpSdkServerConfigWithInstance> {
  const native = new Set(
    context.mcpServers.flatMap((server) =>
      server.tools.map((tool) => tool.exposedName),
    ),
  );
  const tools = context.tools.filter((tool) => !native.has(tool.name));
  if (tools.length === 0) return {};
  if (
    context.mcpServers.some((server) => server.name === CLAUDE_RUNTIME_SERVER)
  ) {
    throw new Error(`MCP server name is reserved: ${CLAUDE_RUNTIME_SERVER}`);
  }
  const server = createSdkMcpServer({ name: CLAUDE_RUNTIME_SERVER });
  server.instance.server.registerCapabilities({ tools: {} });
  server.instance.server.setRequestHandler(ListToolsRequestSchema, () => ({
    tools: tools.map((tool) => {
      const schema = toJsonObject(tool.parameters);
      if (schema.type !== "object") {
        throw new Error(`Tool ${tool.name} requires an object input schema`);
      }
      return {
        name: tool.name,
        description: tool.description,
        inputSchema: { ...schema, type: "object" as const },
      };
    }),
  }));
  server.instance.server.setRequestHandler(
    CallToolRequestSchema,
    async (call) => {
      if (!tools.some((tool) => tool.name === call.params.name)) {
        throw new Error(`Unknown runtime tool: ${call.params.name}`);
      }
      // MCP calls have their own transport IDs; assign a canonical Exo call ID.
      const id = `claude-runtime-${randomUUID()}`;
      const request = {
        functionName: call.params.name,
        arguments: toJsonObject(call.params.arguments ?? {}),
      };
      const { turn } = context.exoharness.current;
      await turn.addEvents([toolRequestedEvent({ toolCallId: id, request })]);
      try {
        const result = await context.executeTool(request, id);
        const input = await getClientToolModelInput(
          context,
          request.functionName,
          id,
        );
        const content =
          input == null
            ? [{ type: "text" as const, text: JSON.stringify(result) }]
            : clientModelInputContent(input);
        await turn.addEvents([toolResultEvent(id, result)]);
        return {
          content,
          isError: toolResultFailed(result),
        };
      } catch (error) {
        const result = { ok: false, error: String(error) };
        await turn.addEvents([toolResultEvent(id, result)]);
        return {
          isError: true,
          content: [{ type: "text" as const, text: JSON.stringify(result) }],
        };
      }
    },
  );
  return { [CLAUDE_RUNTIME_SERVER]: server };
}
