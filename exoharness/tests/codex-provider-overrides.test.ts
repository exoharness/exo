import { describe, expect, it } from "vitest";

import {
  codexModelProvider,
  codexProviderOverrides,
} from "../examples/typescript/codex-harness";

const binding = {
  name: "claude",
  model: "claude-sonnet-4-6",
  apiKey: "sk-test",
};

describe("codex model provider overrides", () => {
  it("leaves Codex on its built-in provider without a base URL", () => {
    expect(codexProviderOverrides(binding)).toEqual([]);
    expect(codexModelProvider(binding)).toBe("openai");
  });

  it("starts threads on the binding's provider when it has a base URL", () => {
    expect(
      codexModelProvider({ ...binding, baseUrl: "http://172.17.0.1:4000/v1" }),
    ).toBe("exo");
  });

  it("routes a gateway base URL through a Responses provider", () => {
    const overrides = codexProviderOverrides({
      ...binding,
      baseUrl: "http://172.17.0.1:4000/v1",
    });
    expect(overrides).toEqual([
      "-c",
      'model_provider="exo"',
      "-c",
      'model_providers.exo.name="exo model binding"',
      "-c",
      'model_providers.exo.base_url="http://172.17.0.1:4000/v1"',
      "-c",
      'model_providers.exo.env_key="OPENAI_API_KEY"',
      "-c",
      'model_providers.exo.wire_api="responses"',
    ]);
  });

  it("gives OpenRouter Chat Completions, which is all it serves", () => {
    const overrides = codexProviderOverrides({
      ...binding,
      baseUrl: "https://openrouter.ai/api/v1",
    });
    expect(overrides).toContain('model_providers.exo.wire_api="chat"');
  });
});
