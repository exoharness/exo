import { describe, expect, it } from "vitest";

import { assistantTextMessage, materializeEventsToMessages } from "../harness";
import { modelResponseEvents, modelUsageRecord } from "./events";

describe("shared model events", () => {
  it("keeps usage when a response has no assistant text", () => {
    const events = modelResponseEvents({
      messages: [],
      usage: modelUsageRecord(
        "model",
        { promptTokens: 12, completionTokens: 3 },
        null,
      ),
      toolCalls: [
        {
          toolCallId: "call",
          request: { functionName: "shell", arguments: { command: "pwd" } },
        },
      ],
    });
    expect(events).toMatchObject([
      {
        type: "messages",
        messages: [],
        usage: { model: "model", prompt_tokens: 12, completion_tokens: 3 },
      },
      { type: "tool_requested", tool_call_id: "call" },
    ]);
  });

  it("prefers the provider's cost, including zero", () => {
    const pricing = new Map([
      ["model", { input_cost_per_token: 1, output_cost_per_token: 1 }],
    ]);
    for (const providerCostUsd of [0, 0.5]) {
      expect(
        modelUsageRecord(
          "model",
          { promptTokens: 100, completionTokens: 10, providerCostUsd },
          pricing,
        ).cost_usd,
      ).toBe(providerCostUsd);
    }
    expect(modelUsageRecord("model", { ttftMs: 12 }, pricing)).toEqual({
      model: "model",
      ttft_ms: 12,
    });
  });

  it("leaves RLM diagnostics out of conversation replay", () => {
    const data = [
      {
        type: "custom",
        event_type: "rlm_model_response",
        payload: { messages: [assistantTextMessage("FINAL(secret)")] },
      },
      ...modelResponseEvents({ messages: [assistantTextMessage("answer")] }),
    ];
    expect(
      materializeEventsToMessages(
        data.map((data, index) => ({
          id: String(index),
          conversationId: "thread",
          turnId: "turn",
          createdAt: "2026-09-29T00:00:00Z",
          data,
        })),
      ),
    ).toEqual([assistantTextMessage("answer")]);
  });
});
