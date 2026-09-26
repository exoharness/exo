import { afterEach, expect, it, vi } from "vitest";
import { query, type Query } from "@anthropic-ai/claude-agent-sdk";
import { type EventData, type TurnContext } from "@exo/harness";
import claude from "./claude-code-harness";

vi.mock("@anthropic-ai/claude-agent-sdk", () => ({ query: vi.fn() }));
vi.mock("@exo/model-runtime/responses", () => ({
  errorMessage: (error: unknown) =>
    error instanceof Error ? error.message : String(error),
  traceExecutorTurn: async (_context: unknown, run: () => Promise<unknown>) =>
    run(),
  tracedUnderParent: async (
    _parent: unknown,
    run: (span: { log: () => void }) => Promise<unknown>,
  ) => run({ log() {} }),
}));

afterEach(() => {
  vi.clearAllMocks();
  vi.useRealTimers();
});

const assistant = {
  type: "assistant",
  message: { content: [{ type: "text", text: "Partial reply" }] },
};
const result = {
  type: "result",
  subtype: "success",
  is_error: false,
  total_cost_usd: 0.5,
  usage: {
    input_tokens: 100,
    output_tokens: 10,
    cache_read_input_tokens: 30,
    cache_creation_input_tokens: 20,
  },
};

function fixture(events: AsyncIterable<unknown>) {
  const recorded: EventData[] = [];
  const close = vi.fn();
  vi.mocked(query).mockReturnValue({
    [Symbol.asyncIterator]: () => events[Symbol.asyncIterator](),
    close,
  } as unknown as Query);
  const context = {
    agentConfig: { model: "claude-sonnet-4-6", instructions: [] },
    conversationConfig: { mounts: [], toolPolicies: {} },
    mcpServers: [],
    exoharness: {
      current: {
        agent: { record: { id: "agent" } },
        conversation: {
          record: { id: "thread" },
          getEvents: async () => ({ events: [] }),
        },
        turn: {
          record: { id: "turn" },
          addEvents: async (events: EventData[]) => {
            recorded.push(...events);
          },
        },
      },
    },
    authorizeTool: vi.fn(async () => {}),
  } as unknown as TurnContext;
  return { context, recorded, close };
}

it("waits beyond the old text grace period for the final result and exclusive Anthropic usage", async () => {
  vi.useFakeTimers();
  const { context, recorded, close } = fixture(
    (async function* () {
      yield assistant;
      await new Promise((resolve) => setTimeout(resolve, 6000));
      yield result;
    })(),
  );
  const completion = claude.runTurn(context);
  await vi.advanceTimersByTimeAsync(5000);
  expect(close).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1000);
  await completion;
  expect(recorded).toContainEqual(
    expect.objectContaining({
      type: "messages",
      usage: expect.objectContaining({
        prompt_tokens: 100,
        prompt_cached_tokens: 30,
        prompt_cache_creation_tokens: 20,
      }),
    }),
  );
});

it.each(["ends", "throws"])(
  "fails if the SDK %s after text instead of reporting success",
  async (mode) => {
    const { context } = fixture(
      (async function* () {
        yield assistant;
        if (mode === "throws") throw new Error("transport broke");
      })(),
    );
    await expect(claude.runTurn(context)).rejects.toThrow(
      mode === "throws" ? "transport broke" : "ended without a result",
    );
  },
);

it("allows a 30-second startup but closes an idle query at 60 seconds", async () => {
  vi.useFakeTimers();
  const { context, close } = fixture(
    (async function* () {
      await new Promise((resolve) => setTimeout(resolve, 30_000));
      yield result;
    })(),
  );
  const completion = claude.runTurn(context);
  await vi.advanceTimersByTimeAsync(20_000);
  expect(close).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(10_000);
  await completion;

  let finish: (() => void) | undefined;
  const idle = fixture(
    (async function* () {
      await new Promise<void>((resolve) => {
        finish = resolve;
      });
      yield* [];
    })(),
  );
  idle.close.mockImplementation(() => finish?.());
  const rejected = expect(claude.runTurn(idle.context)).rejects.toThrow(
    "no SDK messages within 60000ms",
  );
  await vi.advanceTimersByTimeAsync(60_000);
  await rejected;
});

it("records an unknown MCP call while its approval hook denies execution", async () => {
  let decision: unknown;
  const { context, recorded } = fixture(
    (async function* () {
      yield { type: "system", subtype: "init", tools: ["mcp__new__tool"] };
      const hook =
        vi.mocked(query).mock.calls[0][0].options?.hooks?.PreToolUse?.[0]
          .hooks[0];
      if (!hook) throw new Error("missing approval hook");
      decision = await hook(
        {
          hook_event_name: "PreToolUse",
          tool_name: "mcp__new__tool",
          tool_input: {},
          tool_use_id: "call",
          session_id: "session",
          transcript_path: "",
          cwd: "/",
        },
        "call",
        { signal: new AbortController().signal },
      );
      yield {
        type: "assistant",
        message: {
          content: [
            { type: "tool_use", id: "call", name: "mcp__new__tool", input: {} },
          ],
        },
      };
      yield {
        type: "user",
        message: {
          content: [
            {
              type: "tool_result",
              tool_use_id: "call",
              content: "MCP tool is not enabled",
              is_error: true,
            },
          ],
        },
      };
      yield result;
    })(),
  );
  await claude.runTurn(context);
  expect(decision).toMatchObject({
    hookSpecificOutput: {
      permissionDecision: "deny",
      permissionDecisionReason: expect.stringContaining(
        "MCP tool is not enabled",
      ),
    },
  });
  expect(context.authorizeTool).not.toHaveBeenCalled();
  expect(recorded).toContainEqual(
    expect.objectContaining({
      type: "tool_requested",
      request: { function_name: "claude.mcp__new__tool", arguments: {} },
    }),
  );
  expect(recorded).toContainEqual(
    expect.objectContaining({
      type: "tool_result",
      result: { content: "MCP tool is not enabled", is_error: true },
    }),
  );
});
