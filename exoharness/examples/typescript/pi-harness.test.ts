import { describe, expect, it } from "vitest";
import type { TurnContext } from "@exo/harness";
import { eventsForPiEvent } from "./pi-harness";

const context = {
  agentConfig: { model: "openai/gpt-5.6-sol" },
} as unknown as TurnContext;

const usage = {
  input: 3,
  output: 249,
  cacheRead: 100,
  cacheWrite: 18703,
  reasoning: 40,
  totalTokens: 19055,
  cost: {
    input: 0.0001,
    output: 0.005,
    cacheRead: 0.0001,
    cacheWrite: 0.09,
    total: 0.0952,
  },
};

describe("pi harness usage", () => {
  it("records the usage of a tool-calling step that has no text", () => {
    const events = eventsForPiEvent(context, {
      type: "message_end",
      message: {
        role: "assistant",
        model: "gpt-5.6-sol",
        stopReason: "toolUse",
        content: [
          { type: "thinking", thinking: "inspect the files" },
          {
            type: "toolCall",
            id: "call_1",
            name: "bash",
            arguments: { command: "ls" },
          },
        ],
        usage,
      },
    });
    expect(events).toEqual([
      {
        type: "messages",
        messages: [],
        response_id: undefined,
        usage: {
          model: "gpt-5.6-sol",
          // pi's input excludes cache reads and writes; exo counts them all.
          prompt_tokens: 3 + 100 + 18703,
          completion_tokens: 249,
          prompt_cached_tokens: 100,
          prompt_cache_creation_tokens: 18703,
          completion_reasoning_tokens: 40,
          cost_usd: 0.0952,
        },
      },
    ]);
  });

  it("keeps the text and usage of the final answer together", () => {
    const events = eventsForPiEvent(context, {
      type: "message_end",
      message: {
        role: "assistant",
        stopReason: "stop",
        content: [{ type: "text", text: "Done." }],
        usage: {
          input: 5,
          output: 2,
          cacheRead: 0,
          cacheWrite: 0,
          cost: { total: 0.001 },
        },
      },
    });
    expect(events).toHaveLength(1);
    expect(events[0].messages).toEqual([
      { role: "assistant", content: "Done." },
    ]);
    expect(events[0].usage).toMatchObject({
      model: "openai/gpt-5.6-sol",
      prompt_tokens: 5,
      completion_tokens: 2,
      cost_usd: 0.001,
    });
  });

  it("ignores user messages and assistant messages with nothing to record", () => {
    expect(
      eventsForPiEvent(context, {
        type: "message_end",
        message: { role: "user", content: [{ type: "text", text: "hi" }] },
      }),
    ).toEqual([]);
    expect(
      eventsForPiEvent(context, {
        type: "message_end",
        message: { role: "assistant", content: [] },
      }),
    ).toEqual([]);
  });
});
