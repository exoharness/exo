import { describe, expect, it } from "vitest";

import {
  buildTextOnlyNudgeMessage,
  extractAssistantTextFromEvents,
  isRoundBudgetExhausted,
  resolveEffectiveMaxToolRoundTrips,
  shouldExitOnTextOnly,
} from "./turn-loop-nudge.js";

describe("turn-loop-nudge", () => {
  it("exits when complete_task was called", () => {
    expect(shouldExitOnTextOnly(true, 0, 3)).toBe(true);
  });

  it("continues while nudge budget remains", () => {
    expect(shouldExitOnTextOnly(false, 0, 3)).toBe(false);
    expect(shouldExitOnTextOnly(false, 2, 3)).toBe(false);
  });

  it("exits when nudge budget is exhausted", () => {
    expect(shouldExitOnTextOnly(false, 3, 3)).toBe(true);
  });

  it("builds nudge with last assistant text", () => {
    const msg = buildTextOnlyNudgeMessage(
      1,
      "The listFiles tool with a nested wrapper just returns `/home/user/` root. Let me correctly call it:",
    );
    expect(msg).toMatch(/complete_task/);
    expect(msg).toMatch(/text-only/);
    expect(msg).toMatch(/listFiles tool/);
  });

  it("extends round budget while complete_task is pending", () => {
    expect(resolveEffectiveMaxToolRoundTrips(40, 3, false)).toBe(43);
    expect(resolveEffectiveMaxToolRoundTrips(40, 3, true)).toBe(40);
    expect(isRoundBudgetExhausted(41, 40, 3, false)).toBe(false);
    expect(isRoundBudgetExhausted(44, 40, 3, false)).toBe(true);
  });

  it("extracts assistant text from message events", () => {
    const text = extractAssistantTextFromEvents([
      {
        type: "messages",
        messages: [
          {
            role: "assistant",
            content: [{ type: "text", text: "Let me install marp manually:" }],
          },
        ],
      },
    ]);
    expect(text).toBe("Let me install marp manually:");
  });
});
