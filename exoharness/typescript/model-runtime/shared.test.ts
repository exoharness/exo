import { describe, expect, it } from "vitest";

import {
  authDiagnostic,
  tracingOnlyModelBinding,
  type ResolvedLlmBinding,
} from "./shared";

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

describe("authDiagnostic", () => {
  it("never includes credential material, only the mode and a boolean", () => {
    const diagnostic = authDiagnostic(
      binding({ apiKey: "sk-super-secret-value", authMode: "api-key" }),
    );

    expect(diagnostic).toEqual({
      authMode: "api-key",
      hasKeySecret: true,
      warning: null,
    });
    expect(JSON.stringify(diagnostic)).not.toContain("sk-super-secret-value");
  });

  it("reports subscription mode with no key secret and no warning by default", () => {
    const diagnostic = authDiagnostic(
      binding({ apiKey: undefined, authMode: "subscription" }),
    );

    expect(diagnostic).toEqual({
      authMode: "subscription",
      hasKeySecret: false,
      warning: null,
    });
  });

  it("carries a warning when the caller detects a competing credential, without naming it", () => {
    const diagnostic = authDiagnostic(
      binding({ apiKey: "sk-test-key", authMode: "api-key" }),
      { competingCredentialPresent: true },
    );

    expect(diagnostic.warning).toBeTruthy();
    expect(diagnostic.warning).not.toContain("sk-test-key");
  });
});
