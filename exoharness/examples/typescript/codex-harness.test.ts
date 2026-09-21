import { describe, expect, it } from "vitest";

import type { ResolvedLlmBinding } from "@exo/model-runtime/shared";
import type { TurnContext } from "@exo/harness";
import { codexSandboxCommand, codexSandboxEnv } from "./codex-harness";

function binding(
  overrides: Partial<ResolvedLlmBinding> = {},
): ResolvedLlmBinding {
  return {
    name: "codex",
    model: "gpt-5.6-luna",
    apiKey: undefined,
    baseUrl: null,
    authMode: "api-key",
    ...overrides,
  };
}

function context(shellProgram: string | null = null): TurnContext {
  return {
    conversationConfig: { shellProgram },
  } as unknown as TurnContext;
}

describe("codexSandboxEnv", () => {
  it("sets OPENAI_API_KEY when the binding has a key secret (api-key mode)", () => {
    const env = codexSandboxEnv(
      binding({ apiKey: "sk-test-api-key", authMode: "api-key" }),
    );

    expect(env.OPENAI_API_KEY).toBe("sk-test-api-key");
  });

  it("never sets OPENAI_API_KEY under subscription mode", () => {
    const env = codexSandboxEnv(
      binding({ apiKey: undefined, authMode: "subscription" }),
    );

    expect(env.OPENAI_API_KEY).toBeUndefined();
  });
});

describe("codexSandboxCommand", () => {
  it("does not require auth.json to pre-exist under api-key mode", () => {
    const command = codexSandboxCommand(
      context(),
      binding({ apiKey: "sk-test-api-key", authMode: "api-key" }),
    ).join(" ");

    expect(command).not.toMatch(/auth-mode subscription/);
    expect(command).toContain("exec codex app-server");
  });

  it("fails fast with an actionable message under subscription mode when auth.json is missing", () => {
    const command = codexSandboxCommand(
      context(),
      binding({ apiKey: undefined, authMode: "subscription" }),
    ).join(" ");

    expect(command).toContain("auth-mode subscription");
    expect(command).toContain("auth.json");
    expect(command).toContain("exit 1");
  });
});
