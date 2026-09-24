import { describe, expect, it } from "vitest";
import { type CodexProtocolLogEntry } from "./app-server";
import { type JsonValue } from "../harness";
import completion from "./fixtures/raw-response-completed.json";
import { accumulateCodexUsage, codexUsageEvent } from "./usage";

describe("Codex usage events", () => {
  it("converts a captured completion notification into a usage event", () => {
    const entry: CodexProtocolLogEntry = {
      sequence: 1,
      direction: "server_to_client",
      message: completion,
    };
    const usage = accumulateCodexUsage(null, entry);
    if (!usage) throw new Error("Expected Codex usage");
    expect(codexUsageEvent("gpt-5.6-sol", usage, null)).toEqual({
      type: "messages",
      messages: [],
      response_id: undefined,
      usage: {
        model: "gpt-5.6-sol",
        prompt_tokens: 6800,
        completion_tokens: 62,
        prompt_cached_tokens: 0,
        prompt_cache_creation_tokens: 6797,
        completion_reasoning_tokens: 12,
      },
    });
    expect(
      accumulateCodexUsage(usage, { ...entry, direction: "client_to_server" }),
    ).toBe(usage);
    expect(
      accumulateCodexUsage(usage, {
        ...entry,
        message: { ...completion, method: "thread/tokenUsage/updated" },
      }),
    ).toBe(usage);
  });

  it("sums per-call usage across tool rounds without adding thread totals", () => {
    const first = accumulateCodexUsage(
      null,
      response({
        usage: {
          inputTokens: 10,
          outputTokens: 3,
          totalTokens: 13,
          cachedInputTokens: 4,
          cacheWriteInputTokens: 6,
          reasoningOutputTokens: 2,
        },
      }),
    );
    const total = accumulateCodexUsage(
      first,
      response({
        usage: {
          inputTokens: 5,
          outputTokens: 2,
          totalTokens: 7,
          cachedInputTokens: 1,
          cacheWriteInputTokens: 2,
          reasoningOutputTokens: 1,
        },
      }),
    );
    expect(total).toEqual({
      inputTokens: 15,
      outputTokens: 5,
      totalTokens: 20,
      cachedInputTokens: 5,
      cacheWriteInputTokens: 8,
      reasoningOutputTokens: 3,
    });
    if (!total) throw new Error("Expected Codex usage");
    expect(
      codexUsageEvent(
        "model",
        total,
        new Map([
          [
            "model",
            {
              input_cost_per_token: 0.001,
              output_cost_per_token: 0.002,
              cache_read_input_token_cost: 0.0001,
              cache_creation_input_token_cost: 0.003,
            },
          ],
        ]),
      ),
    ).toMatchObject({
      usage: {
        prompt_tokens: 15,
        completion_tokens: 5,
        prompt_cached_tokens: 5,
        prompt_cache_creation_tokens: 8,
        completion_reasoning_tokens: 3,
        cost_usd: expect.closeTo(0.0445, 10),
      },
    });
  });

  it("ignores context estimates and incomplete usage", () => {
    const invalidParams: JsonValue[] = [
      {},
      {
        tokenUsage: {
          last: { inputTokens: 0, outputTokens: 0, totalTokens: 13000 },
        },
      },
      { usage: { inputTokens: 10, totalTokens: 13 } },
      { usage: { inputTokens: -1, outputTokens: 3, totalTokens: 13 } },
    ];
    for (const params of invalidParams) {
      expect(accumulateCodexUsage(null, response(params))).toBeNull();
    }
  });

  it("leaves missing pricing unset", () => {
    expect(
      codexUsageEvent("unpriced", { inputTokens: 10, outputTokens: 2 }, null),
    ).toMatchObject({
      usage: { model: "unpriced", prompt_tokens: 10, completion_tokens: 2 },
    });
    expect(
      codexUsageEvent("unpriced", { inputTokens: 10, outputTokens: 2 }, null),
    ).not.toHaveProperty("usage.cost_usd");
  });
});

function response(params: JsonValue): CodexProtocolLogEntry {
  return {
    sequence: 1,
    direction: "server_to_client",
    message: { method: "rawResponse/completed", params },
  };
}
