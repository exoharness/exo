import { describe, expect, it, vi } from "vitest";
import type { Response } from "openai/resources/responses/responses";

import {
  AnthropicRuntime,
  buildChatNonStreamingBody,
  buildChatStreamingBody,
  buildNonStreamingBody,
  buildStreamingBody,
  ChatCompletionsRuntime,
  isAnthropicModel,
  isOpenRouterModel,
  modelRequiresResponsesApi,
  promptCacheKeyFromRequest,
  responseToLinguaEvents,
  responseToolCalls,
  runtimeFromModelConfig,
  ResponsesRuntime,
  type NativeResponsesRequest,
} from "./responses";

describe("model runtime cancellation", () => {
  it("aborts an in-flight model request through the SDK", async () => {
    let started!: () => void;
    const ready = new Promise<void>((resolve) => {
      started = resolve;
    });
    const fetch = vi.spyOn(globalThis, "fetch").mockImplementation(
      async (_input, init) =>
        new Promise((_resolve, reject) => {
          init?.signal?.addEventListener(
            "abort",
            () => reject(new DOMException("aborted", "AbortError")),
            { once: true },
          );
          started();
        }),
    );
    try {
      const controller = new AbortController();
      const runtime = new ResponsesRuntime({ apiKey: "synthetic-key" });
      const pending = runtime.complete(
        { model: "test-model", input: "hello" },
        { signal: controller.signal },
      );
      const rejected = expect(pending).rejects.toThrow(/abort/i);
      await ready;
      controller.abort();
      await rejected;
      expect(fetch).toHaveBeenCalledTimes(1);
    } finally {
      fetch.mockRestore();
    }
  });
});

describe("model runtime dispatch", () => {
  it("matches the Responses-required model families", () => {
    for (const model of [
      "o1-pro",
      "o3-pro",
      "gpt-5-pro",
      "gpt-5.3",
      "gpt-5.4",
      "gpt-5-codex",
      "gpt-5.1-codex-mini",
    ]) {
      expect(modelRequiresResponsesApi(model)).toBe(true);
    }

    for (const model of [
      "deepseek-chat",
      "gpt-4o",
      "gpt-5",
      "gpt-5.1",
      "gpt-5.2-chat-latest",
    ]) {
      expect(modelRequiresResponsesApi(model)).toBe(false);
    }
  });

  it("dispatches chat-only models away from Responses", () => {
    expect(
      runtimeFromModelConfig(undefined, {
        model: "deepseek-chat",
        apiKey: "key",
      }),
    ).toBeInstanceOf(ChatCompletionsRuntime);
    expect(
      runtimeFromModelConfig(undefined, {
        model: "gpt-5.4",
        apiKey: "key",
      }),
    ).toBeInstanceOf(ResponsesRuntime);
  });

  it("dispatches claude models to the native Anthropic runtime", () => {
    expect(isAnthropicModel("claude-sonnet-4-6")).toBe(true);
    expect(isAnthropicModel("gpt-5.4")).toBe(false);
    expect(isAnthropicModel("us.anthropic.claude-sonnet-4-6")).toBe(false);
    expect(
      runtimeFromModelConfig(undefined, {
        model: "claude-sonnet-4-6",
        apiKey: "key",
      }),
    ).toBeInstanceOf(AnthropicRuntime);
  });

  it("routes OpenRouter bindings through chat completions by base URL", () => {
    expect(isOpenRouterModel({ baseUrl: "https://openrouter.ai/api/v1" })).toBe(
      true,
    );
    expect(isOpenRouterModel({ baseUrl: null })).toBe(false);
    // A Responses-looking model name over OpenRouter still uses chat completions.
    expect(
      runtimeFromModelConfig(undefined, {
        model: "openai/gpt-5-pro",
        apiKey: "key",
        baseUrl: "https://openrouter.ai/api/v1",
      }),
    ).toBeInstanceOf(ChatCompletionsRuntime);
  });
});

describe("prompt cache key", () => {
  const baseRequest = (
    metadata?: Record<string, string>,
  ): NativeResponsesRequest => ({
    model: "gpt-5.4",
    input: "hello",
    ...(metadata ? { metadata } : {}),
  });

  it("derives a stable key from the conversation metadata", () => {
    const request = baseRequest({ conversation_id: "conv_1", turn_id: "t_1" });
    expect(promptCacheKeyFromRequest(request)).toBe("conv_1");
    expect(buildNonStreamingBody(request).prompt_cache_key).toBe("conv_1");
    expect(buildStreamingBody(request).prompt_cache_key).toBe("conv_1");
    expect(buildChatNonStreamingBody(request).prompt_cache_key).toBe("conv_1");
    expect(buildChatStreamingBody(request).prompt_cache_key).toBe("conv_1");
  });

  it("is stable across turns of the same session", () => {
    const turn1 = baseRequest({ conversation_id: "conv_1", turn_id: "t_1" });
    const turn2 = baseRequest({ conversation_id: "conv_1", turn_id: "t_2" });
    expect(buildNonStreamingBody(turn1).prompt_cache_key).toBe(
      buildNonStreamingBody(turn2).prompt_cache_key,
    );
    expect(buildChatStreamingBody(turn1).prompt_cache_key).toBe(
      buildChatStreamingBody(turn2).prompt_cache_key,
    );
  });

  it("falls back to session_id and omits the key when metadata is absent", () => {
    expect(promptCacheKeyFromRequest(baseRequest({ session_id: "s_1" }))).toBe(
      "s_1",
    );
    expect(buildNonStreamingBody(baseRequest()).prompt_cache_key).toBe(
      undefined,
    );
    expect(buildChatNonStreamingBody(baseRequest()).prompt_cache_key).toBe(
      undefined,
    );
  });
});

describe("response tool-call parsing", () => {
  it("keeps usage when a response has only tool calls", () => {
    const response = {
      model: "model",
      output: [
        {
          type: "function_call",
          call_id: "call",
          name: "shell",
          arguments: '{"command":"pwd"}',
        },
      ],
      usage: { input_tokens: 12, output_tokens: 3 },
    } as unknown as Response;
    expect(responseToLinguaEvents(response)).toMatchObject([
      {
        type: "messages",
        usage: { model: "model", prompt_tokens: 12, completion_tokens: 3 },
      },
      { type: "tool_requested", tool_call_id: "call" },
    ]);
  });

  it("attaches response usage to message events", () => {
    const response = {
      id: "resp_1",
      model: "gpt-5.4",
      output: [
        {
          type: "message",
          role: "assistant",
          content: [
            {
              type: "output_text",
              text: "hello",
              annotations: [],
            },
          ],
        },
      ],
      usage: {
        input_tokens: 12,
        output_tokens: 5,
        total_tokens: 17,
        input_tokens_details: {
          cached_tokens: 3,
        },
        output_tokens_details: {
          reasoning_tokens: 2,
        },
      },
    } as unknown as Response;

    expect(responseToLinguaEvents(response)).toContainEqual({
      type: "messages",
      messages: expect.any(Array),
      response_id: undefined,
      usage: expect.objectContaining({
        model: "gpt-5.4",
        prompt_tokens: 12,
        completion_tokens: 5,
        prompt_cached_tokens: 3,
        completion_reasoning_tokens: 2,
      }),
    });
  });

  it("turns malformed function arguments into tool result errors", () => {
    const response = {
      id: "resp_1",
      output: [
        {
          type: "function_call",
          call_id: "call_1",
          name: "shell",
          arguments: '{"command":',
        },
      ],
    } as unknown as Response;

    expect(responseToolCalls(response)).toEqual([]);
    expect(responseToLinguaEvents(response)).toContainEqual({
      type: "tool_result",
      tool_call_id: "call_1",
      result: {
        ok: false,
        error: expect.stringContaining("Invalid JSON arguments for shell"),
      },
    });
  });
});
