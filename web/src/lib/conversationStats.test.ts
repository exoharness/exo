import { describe, expect, it } from "vitest";
import {
  computeConversationRollup,
  formatRollupChips,
} from "./conversationStats";
import { makeEvent } from "../test/fixtures";

describe("computeConversationRollup", () => {
  it("returns zeros and nulls for empty events", () => {
    expect(computeConversationRollup([])).toEqual({
      assistantTurns: 0,
      inputTokens: 0,
      outputTokens: 0,
      totalCostUsd: null,
      p50DurationMs: null,
      p95DurationMs: null,
    });
  });

  it("ignores non-messages events", () => {
    const rollup = computeConversationRollup([
      makeEvent({ type: "turn_started" }),
      makeEvent({ type: "session_ended" }),
    ]);
    expect(rollup).toEqual({
      assistantTurns: 0,
      inputTokens: 0,
      outputTokens: 0,
      totalCostUsd: null,
      p50DurationMs: null,
      p95DurationMs: null,
    });
  });

  it("accumulates partial usage without inventing missing fields", () => {
    const rollup = computeConversationRollup([
      makeEvent({
        type: "messages",
        messages: [{ role: "assistant", content: "hi" }],
        response_id: null,
        usage: {
          model: "test-model",
          prompt_tokens: 10,
        },
      }),
      makeEvent({
        type: "messages",
        messages: [{ role: "assistant", content: "bye" }],
        response_id: null,
        usage: {
          model: "test-model",
          completion_tokens: 5,
          cost_usd: 0.001,
          duration_ms: 120,
        },
      }),
    ]);

    expect(rollup.assistantTurns).toBe(2);
    expect(rollup.inputTokens).toBe(10);
    expect(rollup.outputTokens).toBe(5);
    expect(rollup.totalCostUsd).toBeCloseTo(0.001);
    expect(rollup.p50DurationMs).toBe(120);
    expect(rollup.p95DurationMs).toBe(120);
  });

  it("leaves totalCostUsd null when no cost_usd appears", () => {
    const rollup = computeConversationRollup([
      makeEvent({
        type: "messages",
        messages: [{ role: "user", content: "hello" }],
        response_id: null,
        usage: {
          model: "test-model",
          prompt_tokens: 3,
          duration_ms: 50,
        },
      }),
    ]);
    expect(rollup.totalCostUsd).toBeNull();
  });

  it("skips non-finite durations for percentiles", () => {
    const rollup = computeConversationRollup([
      makeEvent({
        type: "messages",
        messages: [],
        response_id: null,
        usage: {
          model: "test-model",
          duration_ms: Number.NaN,
        },
      }),
      makeEvent({
        type: "messages",
        messages: [],
        response_id: null,
        usage: {
          model: "test-model",
          duration_ms: 100,
        },
      }),
      makeEvent({
        type: "messages",
        messages: [],
        response_id: null,
        usage: {
          model: "test-model",
          duration_ms: 300,
        },
      }),
      makeEvent({
        type: "messages",
        messages: [],
        response_id: null,
        usage: {
          model: "test-model",
          duration_ms: 500,
        },
      }),
    ]);
    expect(rollup.p50DurationMs).toBe(300);
    expect(rollup.p95DurationMs).toBe(500);
  });

  it("computes p50 and p95 across sorted durations", () => {
    const durations = [100, 200, 300, 400, 500];
    const rollup = computeConversationRollup(
      durations.map((duration_ms) =>
        makeEvent({
          type: "messages",
          messages: [],
          response_id: null,
          usage: { model: "m", duration_ms },
        }),
      ),
    );
    expect(rollup.p50DurationMs).toBe(300);
    expect(rollup.p95DurationMs).toBe(500);
  });

  it("counts only assistant-role messages in a mixed batch", () => {
    const rollup = computeConversationRollup([
      makeEvent({
        type: "messages",
        messages: [
          { role: "user", content: "q" },
          { role: "assistant", content: "a1" },
          { role: "system", content: "s" },
          { role: "assistant", content: "a2" },
        ],
        response_id: null,
      }),
    ]);
    expect(rollup.assistantTurns).toBe(2);
  });

  it("leaves usage fields at zero when messages events have no usage block", () => {
    const rollup = computeConversationRollup([
      makeEvent({
        type: "messages",
        messages: [
          { role: "user", content: "hi" },
          { role: "assistant", content: "hello" },
        ],
        response_id: null,
      }),
    ]);
    expect(rollup).toEqual({
      assistantTurns: 1,
      inputTokens: 0,
      outputTokens: 0,
      totalCostUsd: null,
      p50DurationMs: null,
      p95DurationMs: null,
    });
  });

  it("accumulates very large token and cost totals without truncation", () => {
    const rollup = computeConversationRollup(
      Array.from({ length: 1000 }, () =>
        makeEvent({
          type: "messages",
          messages: [{ role: "assistant", content: "x" }],
          response_id: null,
          usage: {
            model: "big",
            prompt_tokens: 1_000_000,
            completion_tokens: 500_000,
            cost_usd: 0.01,
            duration_ms: 1000,
          },
        }),
      ),
    );
    expect(rollup.assistantTurns).toBe(1000);
    expect(rollup.inputTokens).toBe(1_000_000_000);
    expect(rollup.outputTokens).toBe(500_000_000);
    expect(rollup.totalCostUsd).toBeCloseTo(10);
    expect(rollup.p50DurationMs).toBe(1000);
    expect(rollup.p95DurationMs).toBe(1000);
  });
});

describe("formatRollupChips", () => {
  it("returns empty chips when rollup is all zero/null", () => {
    expect(
      formatRollupChips({
        assistantTurns: 0,
        inputTokens: 0,
        outputTokens: 0,
        totalCostUsd: null,
        p50DurationMs: null,
        p95DurationMs: null,
      }),
    ).toEqual([]);
  });

  it("formats singular turn and token/cost/duration chips", () => {
    const chips = formatRollupChips({
      assistantTurns: 1,
      inputTokens: 1200,
      outputTokens: 34,
      totalCostUsd: 0.000042,
      p50DurationMs: 850,
      p95DurationMs: 12_500,
    });
    expect(chips).toContain("1 turn");
    expect(chips.some((chip) => chip.includes("1,200 in"))).toBe(true);
    expect(chips.some((chip) => chip.startsWith("$"))).toBe(true);
    expect(chips.some((chip) => chip.startsWith("p50 "))).toBe(true);
    expect(chips.some((chip) => chip.startsWith("p95 "))).toBe(true);
  });

  it("formats huge token counts with locale grouping", () => {
    const chips = formatRollupChips({
      assistantTurns: 99,
      inputTokens: 12_345_678,
      outputTokens: 9_876_543,
      totalCostUsd: 123.456789,
      p50DurationMs: 90_000,
      p95DurationMs: 3_700_000,
    });
    expect(chips).toContain("99 turns");
    expect(chips.some((chip) => chip.includes("12,345,678 in"))).toBe(true);
    expect(chips.some((chip) => chip.includes("9,876,543 out"))).toBe(true);
    expect(chips).toContain("$123.456789");
    expect(chips).toContain("p50 1m 30s");
    expect(chips).toContain("p95 61m 40s");
  });

  it("omits token chip when both token totals are zero", () => {
    const chips = formatRollupChips({
      assistantTurns: 2,
      inputTokens: 0,
      outputTokens: 0,
      totalCostUsd: null,
      p50DurationMs: null,
      p95DurationMs: null,
    });
    expect(chips).toEqual(["2 turns"]);
  });

  it("treats zero cost_usd as a real cost total", () => {
    const rollup = computeConversationRollup([
      makeEvent({
        type: "messages",
        messages: [{ role: "assistant", content: "free" }],
        response_id: null,
        usage: {
          model: "free-tier",
          cost_usd: 0,
        },
      }),
    ]);
    expect(rollup.totalCostUsd).toBe(0);
    expect(formatRollupChips(rollup)).toContain("$0.000000");
  });

  it("returns the sole duration for both percentiles", () => {
    const rollup = computeConversationRollup([
      makeEvent({
        type: "messages",
        messages: [],
        response_id: null,
        usage: { model: "m", duration_ms: 42 },
      }),
    ]);
    expect(rollup.p50DurationMs).toBe(42);
    expect(rollup.p95DurationMs).toBe(42);
  });

  it("ignores non-finite durations but keeps finite negatives", () => {
    const rollup = computeConversationRollup([
      makeEvent({
        type: "messages",
        messages: [],
        response_id: null,
        usage: { model: "m", duration_ms: -5 },
      }),
      makeEvent({
        type: "messages",
        messages: [],
        response_id: null,
        usage: { model: "m", duration_ms: Number.POSITIVE_INFINITY },
      }),
      makeEvent({
        type: "messages",
        messages: [],
        response_id: null,
        usage: { model: "m", duration_ms: 250 },
      }),
    ]);
    expect(rollup.p50DurationMs).toBe(-5);
    expect(rollup.p95DurationMs).toBe(250);
  });

  it("formats both percentile chips when p50 and p95 are present", () => {
    const chips = formatRollupChips({
      assistantTurns: 1,
      inputTokens: 0,
      outputTokens: 0,
      totalCostUsd: null,
      p50DurationMs: 900,
      p95DurationMs: 900,
    });
    expect(chips).toContain("p50 900ms");
    expect(chips).toContain("p95 900ms");
  });
});
