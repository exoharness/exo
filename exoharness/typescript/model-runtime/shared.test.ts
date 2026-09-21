import { describe, expect, it } from "vitest";

import { tracingOnlyModelBinding, type ResolvedLlmBinding } from "./shared";

function binding(
  overrides: Partial<ResolvedLlmBinding> = {},
): ResolvedLlmBinding {
  return {
    name: "model",
    model: "some-model",
    apiKey: undefined,
    baseUrl: null,
    authMode: "api-key",
    ...overrides,
  };
}

describe("tracingOnlyModelBinding", () => {
  it("returns the binding unchanged when an apiKey is already present", () => {
    const original = binding({ apiKey: "sk-test-key" });

    expect(tracingOnlyModelBinding(original)).toBe(original);
  });

  it("fills in a non-empty placeholder apiKey under subscription auth so tracing clients can construct", () => {
    const result = tracingOnlyModelBinding(
      binding({ apiKey: undefined, authMode: "subscription" }),
    );

    expect(result.apiKey).toBeTruthy();
    expect(result.apiKey).not.toBe("sk-test-key");
  });
});
