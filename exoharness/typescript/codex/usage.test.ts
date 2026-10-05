import { describe, expect, it } from "vitest";
import { type JsonValue } from "../harness";
import completion from "./fixtures/raw-response-completed.json";
import { CodexUsageAccumulator, codexUsageEvent } from "./usage";

describe("Codex usage events", () => {
  it("converts a captured completion notification into a usage event", () => {
    const accumulator = new CodexUsageAccumulator();
    accumulator.record(update(completion.params.usage));
    const usage = accumulator.value;
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
    accumulator.record(update(completion.params.usage));
    expect(accumulator.value).toBe(usage);
    accumulator.record(completion.params);
    expect(accumulator.value).toBe(usage);
  });

  it("sums per-call usage across tool rounds without adding thread totals", () => {
    const accumulator = new CodexUsageAccumulator();
    accumulator.record(
      update({
        inputTokens: 10,
        outputTokens: 3,
        totalTokens: 13,
        cachedInputTokens: 4,
        cacheWriteInputTokens: 6,
        reasoningOutputTokens: 2,
      }),
    );
    accumulator.record(
      update(
        {
          inputTokens: 5,
          outputTokens: 2,
          totalTokens: 7,
          cachedInputTokens: 1,
          cacheWriteInputTokens: 2,
          reasoningOutputTokens: 1,
        },
        20,
      ),
    );
    const total = accumulator.value;
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
      null,
      {
        tokenUsage: {
          total: { totalTokens: 13000 },
          last: { inputTokens: 0, outputTokens: 0, totalTokens: 13000 },
        },
      },
      update({ inputTokens: 10, totalTokens: 13 }),
      update({ inputTokens: -1, outputTokens: 3, totalTokens: 13 }),
      update({ inputTokens: 10, outputTokens: 3, totalTokens: 14 }),
      update({ inputTokens: 10, outputTokens: 3, totalTokens: 13 }, 12),
    ];
    for (const params of invalidParams) {
      const accumulator = new CodexUsageAccumulator();
      accumulator.record(params);
      expect(accumulator.value).toBeNull();
    }
  });

  it("counts only this resumed turn, including identical successive model calls", () => {
    const accumulator = new CodexUsageAccumulator();
    const last = { inputTokens: 10, outputTokens: 3, totalTokens: 13 };
    accumulator.record(update(last, 1013));
    accumulator.record(update(last, 1013));
    accumulator.record(update(last, 1026));
    expect(accumulator.value).toMatchObject({
      inputTokens: 20,
      outputTokens: 6,
      totalTokens: 26,
    });
  });

  it("sums two captured calls on a resumed turn without adding earlier turns", () => {
    const accumulator = new CodexUsageAccumulator();
    accumulator.record(
      update(
        {
          inputTokens: 12047,
          outputTokens: 225,
          totalTokens: 12272,
          cachedInputTokens: 10624,
          cacheWriteInputTokens: 0,
          reasoningOutputTokens: 61,
        },
        46941,
      ),
    );
    accumulator.record(
      update(
        {
          inputTokens: 12597,
          outputTokens: 166,
          totalTokens: 12763,
          cachedInputTokens: 11648,
          cacheWriteInputTokens: 0,
          reasoningOutputTokens: 46,
        },
        59704,
      ),
    );
    expect(accumulator.value).toEqual({
      inputTokens: 24644,
      outputTokens: 391,
      totalTokens: 25035,
      cachedInputTokens: 22272,
      cacheWriteInputTokens: 0,
      reasoningOutputTokens: 107,
    });
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

function update(
  last: Record<string, number>,
  totalTokens = last.totalTokens,
): JsonValue {
  return {
    threadId: "thread",
    turnId: "turn",
    tokenUsage: { last, total: { totalTokens } },
  };
}
