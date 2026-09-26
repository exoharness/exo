import { expect, it, vi } from "vitest";
import {
  type EventData,
  type SandboxProcess,
  type TurnContext,
} from "@exo/harness";
import { resolveSandboxLlmBinding } from "@exo/model-runtime/shared";
import pi from "./pi-harness";

vi.mock("@exo/model-runtime/shared", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@exo/model-runtime/shared")>()),
  resolveSandboxLlmBinding: vi.fn(),
}));

function fixture(events: unknown[]) {
  const recorded: EventData[] = [];
  const process: SandboxProcess = {
    reused: false,
    stdout: new ReadableStream({
      start(controller) {
        controller.enqueue(
          events.map((event) => JSON.stringify(event)).join("\n") + "\n",
        );
        controller.close();
      },
    }),
    stderr: new ReadableStream({ start: (controller) => controller.close() }),
    writeStdin: vi.fn(async () => {}),
    closeStdin: vi.fn(async () => {}),
    close: vi.fn(async () => {}),
    wait: vi.fn(async () => 0),
  };
  const context = {
    agentConfig: { instructions: [] },
    conversationConfig: {
      permissionPolicy: { type: "always_ask" },
      toolPolicies: {},
    },
    tools: [],
    exoharness: {
      current: {
        conversation: {
          record: { id: "conversation-id" },
          getEvents: async () => ({ events: [] }),
        },
        turn: {
          addEvents: async (events: EventData[]) => {
            recorded.push(...events);
          },
        },
      },
    },
    authorizeTool: vi.fn(async () => {}),
    executeTool: vi.fn(async () => ({ ok: true })),
    startSandboxProcess: vi.fn(async () => process),
  } as unknown as TurnContext;
  vi.mocked(resolveSandboxLlmBinding).mockResolvedValue({
    name: "model",
    model: "gpt-5-mini",
    baseUrl: null,
  });
  return { context, process, recorded };
}

it.each([
  ["openai", 150],
  ["anthropic", 100],
])(
  "normalizes %s usage including tool-only steps",
  async (provider, promptTokens) => {
    const { context, recorded } = fixture([
      {
        type: "message_end",
        message: {
          role: "assistant",
          model: "model",
          content: [],
          stopReason: "toolUse",
          usage: {
            input: 100,
            output: 10,
            cacheRead: 30,
            cacheWrite: 20,
            cost: { total: 0.5 },
          },
        },
      },
      { type: "agent_settled" },
    ]);
    vi.mocked(resolveSandboxLlmBinding).mockResolvedValue({
      name: "model",
      model: `${provider}/model`,
      baseUrl: null,
    });
    await pi.runTurn(context);
    expect(recorded).toEqual([
      expect.objectContaining({
        type: "messages",
        messages: [],
        usage: {
          model: "model",
          prompt_tokens: promptTokens,
          completion_tokens: 10,
          prompt_cached_tokens: 30,
          prompt_cache_creation_tokens: 20,
          cost_usd: 0.5,
        },
      }),
    ]);
  },
);

it("accepts always_ask, forwards native and MCP approvals, and records a denial once", async () => {
  const result = { content: [{ type: "text", text: "Tool denied" }] };
  const { context, process, recorded } = fixture([
    {
      type: "extension_ui_request",
      id: "native",
      method: "input",
      title: "exo.authorize_tool",
      placeholder: JSON.stringify({
        functionName: "pi.bash",
        arguments: { command: "pwd" },
      }),
    },
    {
      type: "tool_execution_start",
      toolCallId: "call",
      toolName: "bash",
      args: { command: "pwd" },
    },
    { type: "tool_execution_end", toolCallId: "call", result, isError: true },
    {
      type: "extension_ui_request",
      id: "mcp",
      method: "input",
      title: "exo.execute_tool",
      placeholder: JSON.stringify({
        functionName: "exo_mcp__test__read",
        arguments: {},
      }),
    },
    { type: "agent_settled" },
  ]);
  vi.mocked(context.authorizeTool).mockRejectedValue(new Error("Tool denied"));
  context.tools.push({
    name: "exo_mcp__test__read",
    description: "x".repeat(140_000),
    parameters: { type: "object" },
  });
  expect(pi.nativeToolApprovals).toBe(true);
  await pi.runTurn(context);
  expect(context.authorizeTool).toHaveBeenCalledWith({
    functionName: "pi.bash",
    arguments: { command: "pwd" },
  });
  expect(context.executeTool).toHaveBeenCalledWith({
    functionName: "exo_mcp__test__read",
    arguments: {},
  });
  expect(recorded).toContainEqual({
    type: "tool_result",
    tool_call_id: "call",
    result: { result, is_error: true },
  });
  expect(process.writeStdin).toHaveBeenNthCalledWith(
    1,
    JSON.stringify(context.tools) + "\n",
  );
  const start = vi.mocked(context.startSandboxProcess).mock.calls[0][0];
  expect(start.env).not.toHaveProperty("EXO_PI_TOOLS");
  expect(Math.max(...start.command.map((arg) => arg.length))).toBeLessThan(
    128 * 1024,
  );
  expect(process.writeStdin).toHaveBeenCalledWith(
    JSON.stringify({
      type: "extension_ui_response",
      id: "native",
      value: JSON.stringify({ ok: false, error: "Tool denied" }),
    }) + "\n",
  );
  expect(process.close).toHaveBeenCalledOnce();
});

it("rejects unknown tool policies before starting Pi", async () => {
  const { context } = fixture([]);
  context.conversationConfig.toolPolicies = {
    "pi.bahs": { type: "always_ask" },
  };
  await expect(pi.runTurn(context)).rejects.toThrow(
    "unknown tool in tool_policies: pi.bahs",
  );
  expect(context.startSandboxProcess).not.toHaveBeenCalled();
});
