import { describe, expect, it } from "vitest";

import {
  claudeGatewayBaseUrl,
  claudeSandboxBaseEnv,
} from "../examples/typescript/claude-code-harness";

const binding = {
  name: "kimi-k3",
  model: "moonshotai/kimi-k3",
  apiKey: "sk-or-test",
};

describe("claude code model gateways", () => {
  it("treats a binding without a base URL as Anthropic itself", () => {
    expect(claudeGatewayBaseUrl(binding)).toBeNull();

    const env = claudeSandboxBaseEnv(binding);
    expect(env.ANTHROPIC_API_KEY).toBe("sk-or-test");
    expect(env.ANTHROPIC_BASE_URL).toBeUndefined();
    expect(env.ANTHROPIC_AUTH_TOKEN).toBeUndefined();
    expect(env.ANTHROPIC_DEFAULT_HAIKU_MODEL).toBeUndefined();
  });

  it("passes an Anthropic base URL through unchanged", () => {
    const env = claudeSandboxBaseEnv({
      ...binding,
      baseUrl: "https://api.anthropic.com/v1",
    });
    expect(env.ANTHROPIC_BASE_URL).toBe("https://api.anthropic.com/v1");
    expect(env.ANTHROPIC_AUTH_TOKEN).toBeUndefined();
    expect(env.ANTHROPIC_DEFAULT_SONNET_MODEL).toBeUndefined();
  });

  it("drops the OpenAI-style /v1 suffix exo's other harnesses use", () => {
    for (const baseUrl of [
      "https://openrouter.ai/api/v1",
      "https://openrouter.ai/api/v1/",
      "https://openrouter.ai/api",
    ]) {
      expect(claudeGatewayBaseUrl({ ...binding, baseUrl })).toBe(
        "https://openrouter.ai/api",
      );
    }
    expect(
      claudeGatewayBaseUrl({ ...binding, baseUrl: "http://litellm:4000" }),
    ).toBe("http://litellm:4000");
  });

  it("pins every model slot and adds bearer auth for a gateway", () => {
    const env = claudeSandboxBaseEnv({
      ...binding,
      baseUrl: "https://openrouter.ai/api/v1",
    });
    expect(env).toMatchObject({
      ANTHROPIC_API_KEY: "sk-or-test",
      ANTHROPIC_AUTH_TOKEN: "sk-or-test",
      ANTHROPIC_BASE_URL: "https://openrouter.ai/api",
      ANTHROPIC_DEFAULT_HAIKU_MODEL: "moonshotai/kimi-k3",
      ANTHROPIC_DEFAULT_SONNET_MODEL: "moonshotai/kimi-k3",
      ANTHROPIC_DEFAULT_OPUS_MODEL: "moonshotai/kimi-k3",
      CLAUDE_CODE_SUBAGENT_MODEL: "moonshotai/kimi-k3",
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
    });
  });

  it("leaves auth out when the binding has no key", () => {
    const env = claudeSandboxBaseEnv({
      name: "local",
      model: "qwen3-coder",
      baseUrl: "http://ollama:11434",
    });
    expect(env.ANTHROPIC_BASE_URL).toBe("http://ollama:11434");
    expect(env.ANTHROPIC_API_KEY).toBeUndefined();
    expect(env.ANTHROPIC_AUTH_TOKEN).toBeUndefined();
  });
});
