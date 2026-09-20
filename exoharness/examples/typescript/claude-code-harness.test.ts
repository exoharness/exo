import { afterEach, describe, expect, it, vi } from "vitest";

import type { ResolvedLlmBinding } from "@exo/model-runtime/shared";
import { claudeSandboxBaseEnv, claudeSandboxEnv } from "./claude-code-harness";

function binding(
  overrides: Partial<ResolvedLlmBinding> = {},
): ResolvedLlmBinding {
  return {
    name: "claude",
    model: "claude-opus-5",
    apiKey: undefined,
    baseUrl: null,
    ...overrides,
  };
}

describe("claudeSandboxBaseEnv", () => {
  afterEach(() => {
    vi.unstubAllEnvs();
  });

  it("passes through a subscription token and sets no ANTHROPIC_API_KEY when the binding has no secret", () => {
    vi.stubEnv("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat-test-token");

    const env = claudeSandboxBaseEnv(binding({ apiKey: undefined }));

    expect(env.CLAUDE_CODE_OAUTH_TOKEN).toBe("sk-ant-oat-test-token");
    expect(env.ANTHROPIC_API_KEY).toBeUndefined();
  });

  it("sets ANTHROPIC_API_KEY when the binding has a key secret and no token is present", () => {
    const env = claudeSandboxBaseEnv(
      binding({ apiKey: "sk-ant-api-test-key" }),
    );

    expect(env.ANTHROPIC_API_KEY).toBe("sk-ant-api-test-key");
    expect(env.CLAUDE_CODE_OAUTH_TOKEN).toBeUndefined();
  });

  it("throws rather than letting an API key silently win when both a key secret and a subscription token are present", () => {
    vi.stubEnv("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat-test-token");

    expect(() =>
      claudeSandboxBaseEnv(binding({ apiKey: "sk-ant-api-test-key" })),
    ).toThrow(/ambiguous/i);
  });
});

describe("claudeSandboxEnv", () => {
  it("forwards CLAUDE_ and ANTHROPIC_ prefixed vars and fills in HOME/CLAUDE_CONFIG_DIR defaults", () => {
    const env = claudeSandboxEnv({
      CLAUDE_CODE_OAUTH_TOKEN: "sk-ant-oat-test-token",
      UNRELATED_VAR: "should-not-appear",
    });

    expect(env.CLAUDE_CODE_OAUTH_TOKEN).toBe("sk-ant-oat-test-token");
    expect(env.UNRELATED_VAR).toBeUndefined();
    expect(env.HOME).toBe("/home/exo");
    expect(env.CLAUDE_CONFIG_DIR).toBe("/home/exo/.claude");
  });

  it("never forwards ANTHROPIC_API_KEY when it was never set upstream", () => {
    const env = claudeSandboxEnv({
      CLAUDE_CODE_OAUTH_TOKEN: "sk-ant-oat-test-token",
    });

    expect(env.ANTHROPIC_API_KEY).toBeUndefined();
  });
});
