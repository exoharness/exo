import { describe, expect, it } from "vitest";
import completion from "./fixtures/raw-response-completed.json";
import {
  CodexUsageAccumulator,
  codexUsageEvent,
  type CodexTokenUsageUpdate,
} from "./usage";

describe("Codex usage events", () => {
  it("converts captured token counts into a usage event", () => {
    const usage = completion.params.usage;
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

  it("ignores incomplete or invalid token counts", () => {
    const invalidParams: (CodexTokenUsageUpdate | null)[] = [
      {},
      null,
      update({ inputTokens: 10, totalTokens: 13 }),
      update({ inputTokens: -1, outputTokens: 3, totalTokens: 13 }),
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

  it("accepts independent token totals and ignores older notifications", () => {
    const accumulator = new CodexUsageAccumulator();
    accumulator.record(
      update({ inputTokens: 10, outputTokens: 3, totalTokens: 14 }, 12),
    );
    accumulator.record(
      update({ inputTokens: 20, outputTokens: 2, totalTokens: 22 }, 11),
    );
    expect(accumulator.value).toMatchObject({
      inputTokens: 10,
      outputTokens: 3,
      totalTokens: 14,
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
): CodexTokenUsageUpdate {
  return {
    threadId: "thread",
    turnId: "turn",
    tokenUsage: { last, total: { totalTokens } },
  };
}
