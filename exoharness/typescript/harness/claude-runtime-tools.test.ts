import { expect, it, vi } from "vitest";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import {
  claudeRuntimeTools,
  CLAUDE_RUNTIME_SERVER,
} from "./claude-runtime-tools";
import type { EventData, TurnContext } from "./index";

it.each([
  { modelInput: null, output: { answer: 42 }, isError: false },
  { modelInput: "Client context", output: { answer: 42 }, isError: false },
  {
    modelInput: null,
    output: { ok: false, error: { type: "tool_error", message: "No data" } },
    isError: true,
  },
])(
  "exposes the tool schema and delivers client results ($isError, $modelInput)",
  async ({ modelInput, output, isError }) => {
    const recorded: EventData[] = [];
    const context = {
      agentConfig: { frontendTools: [{ name: "lookup" }] },
      tools: [
        {
          name: "lookup",
          description: "Read client data",
          parameters: {
            type: "object",
            properties: { key: { type: "string" } },
            required: ["key"],
          },
        },
      ],
      mcpServers: [],
      exoharness: {
        current: {
          conversation: {
            getEvents: async () => ({
              events: [
                {
                  data: {
                    type: "custom",
                    event_type: "agent_runtime.frontend_tool_response",
                    payload: {
                      tool_call_id: recorded[0].tool_call_id,
                      result: {
                        type: "frontend_tool_success",
                        model_input: modelInput,
                      },
                    },
                  },
                },
              ],
            }),
          },
          turn: {
            record: { id: "turn", sessionId: "session" },
            addEvents: vi.fn(async (events: EventData[]) => {
              recorded.push(...events);
            }),
          },
        },
      },
      executeTool: vi.fn(async (_request, id) => {
        expect(recorded).toContainEqual(
          expect.objectContaining({ type: "tool_requested", tool_call_id: id }),
        );
        return output;
      }),
    } as unknown as TurnContext;
    const server = claudeRuntimeTools(context)[CLAUDE_RUNTIME_SERVER];
    const client = new Client({ name: "test", version: "1" });
    const [clientTransport, serverTransport] =
      InMemoryTransport.createLinkedPair();
    await server.instance.connect(serverTransport);
    await client.connect(clientTransport);
    try {
      const listed = await client.listTools();
      expect(listed.tools[0].inputSchema).toEqual(context.tools[0].parameters);
      const result = await client.callTool({
        name: "lookup",
        arguments: { key: "a" },
      });
      expect(result.content).toEqual([
        { type: "text", text: modelInput ?? JSON.stringify(output) },
      ]);
      expect(result.isError).toBe(isError);
      const id = recorded[0].tool_call_id;
      expect(context.executeTool).toHaveBeenCalledWith(
        { functionName: "lookup", arguments: { key: "a" } },
        id,
      );
      expect(recorded[1]).toEqual({
        type: "tool_result",
        tool_call_id: id,
        result: output,
      });
    } finally {
      await client.close();
      await server.instance.close();
    }
  },
);
