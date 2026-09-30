import { describe, expect, it } from "vitest";
import type { UniversalUsage } from "@braintrust/lingua-types";

import { modelUsageRecord } from "./events";

describe("model usage records", () => {
  it("preserves Lingua token details and omits undefined fields", () => {
    const usage: UniversalUsage = {
      prompt_tokens: 12,
      completion_tokens: 3,
      total_tokens: 15,
      prompt_tokens_exclude_cache: true,
      input_details: { cached: { total_tokens: 2 } },
      completion_reasoning_tokens: undefined,
    };
    expect(modelUsageRecord("model", usage, null)).toEqual({
      model: "model",
      prompt_tokens: 12,
      completion_tokens: 3,
      total_tokens: 15,
      prompt_tokens_exclude_cache: true,
      input_details: { cached: { total_tokens: 2 } },
    });
  });

  it("prefers the provider's cost, including zero", () => {
    const pricing = new Map([
      ["model", { input_cost_per_token: 1, output_cost_per_token: 1 }],
    ]);
    for (const cost_usd of [0, 0.5]) {
      expect(
        modelUsageRecord(
          "model",
          { prompt_tokens: 100, completion_tokens: 10, cost_usd },
          pricing,
        ).cost_usd,
      ).toBe(cost_usd);
    }
    expect(modelUsageRecord("model", {}, pricing)).toEqual({
      model: "model",
    });
  });
});
