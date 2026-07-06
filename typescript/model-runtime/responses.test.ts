import { afterAll, describe, expect, it } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import type Anthropic from "@anthropic-ai/sdk";
import type { Response } from "openai/resources/responses/responses";
import type {
  ChatCompletion,
  ChatCompletionChunk,
} from "openai/resources/chat/completions";

import type { Message } from "../harness";
import { ensureTable } from "./cost";
import {
  AnthropicRuntime,
  anthropicMessageToResponse,
  anthropicUsageToResponseUsage,
  assistantToolCalls,
  buildAnthropicBody,
  ChatCompletionAccumulator,
  chatCompletionToResponse,
  ChatCompletionsRuntime,
  chatUsageToResponseUsage,
  isAnthropicModel,
  isOpenRouterBinding,
  messageToChatMessage,
  modelRequiresResponsesApi,
  responseToLinguaEvents,
  responseToolCalls,
  runtimeFromModelBinding,
  ResponsesRuntime,
  splitAnthropicMessages,
  toolResultContent,
} from "./responses";

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
      runtimeFromModelBinding(undefined, {
        model: "deepseek-chat",
        apiKey: "key",
      }),
    ).toBeInstanceOf(ChatCompletionsRuntime);
    expect(
      runtimeFromModelBinding(undefined, {
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
      runtimeFromModelBinding(undefined, {
        model: "claude-sonnet-4-6",
        apiKey: "key",
      }),
    ).toBeInstanceOf(AnthropicRuntime);
  });

  it("routes OpenRouter bindings through chat completions by base URL", () => {
    expect(
      isOpenRouterBinding({ baseUrl: "https://openrouter.ai/api/v1" }),
    ).toBe(true);
    expect(isOpenRouterBinding({ baseUrl: null })).toBe(false);
    // A Responses-looking model name over OpenRouter still uses chat completions.
    expect(
      runtimeFromModelBinding(undefined, {
        model: "openai/gpt-5-pro",
        apiKey: "key",
        baseUrl: "https://openrouter.ai/api/v1",
      }),
    ).toBeInstanceOf(ChatCompletionsRuntime);
  });
});

describe("response tool-call parsing", () => {
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

describe("anthropic message normalization", () => {
  it("normalizes text and tool_use content into Responses output", () => {
    const message = {
      id: "msg_1",
      model: "claude-sonnet-4-6",
      content: [
        { type: "text", text: "hello " },
        {
          type: "tool_use",
          id: "toolu_1",
          name: "shell",
          input: { command: "ls" },
        },
        { type: "text", text: "world" },
      ],
      usage: {
        input_tokens: 100,
        output_tokens: 20,
        cache_read_input_tokens: 40,
      },
    } as unknown as Anthropic.Message;

    const response = anthropicMessageToResponse(message);

    expect(response.id).toBe("msg_1");
    expect(response.model).toBe("claude-sonnet-4-6");
    expect(response.status).toBe("completed");
    // Text blocks concatenate into a single assistant message; tool_use blocks
    // become function_call items with JSON-encoded arguments.
    expect(response.output).toEqual([
      {
        id: "msg_1_message",
        type: "message",
        role: "assistant",
        status: "completed",
        content: [
          {
            type: "output_text",
            text: "hello world",
            annotations: [],
          },
        ],
      },
      {
        id: "toolu_1_item",
        type: "function_call",
        call_id: "toolu_1",
        name: "shell",
        arguments: '{"command":"ls"}',
        status: "completed",
      },
    ]);
    expect(response.usage).toEqual({
      input_tokens: 100,
      output_tokens: 20,
      total_tokens: 120,
      input_tokens_details: { cached_tokens: 40 },
      output_tokens_details: { reasoning_tokens: 0 },
    });
  });

  it("omits the message output when there is no text", () => {
    const message = {
      id: "msg_2",
      model: "claude-sonnet-4-6",
      content: [
        { type: "tool_use", id: "toolu_1", name: "shell", input: null },
      ],
      usage: null,
    } as unknown as Anthropic.Message;

    const response = anthropicMessageToResponse(message);
    expect(response.output).toEqual([
      expect.objectContaining({
        type: "function_call",
        call_id: "toolu_1",
        arguments: "{}",
      }),
    ]);
    expect(response.usage).toBeNull();
  });

  it("maps cache_read_input_tokens into cached_tokens and sums the total", () => {
    expect(
      anthropicUsageToResponseUsage({
        input_tokens: 7,
        output_tokens: 3,
        cache_read_input_tokens: 5,
      } as unknown as Anthropic.Usage),
    ).toEqual({
      input_tokens: 7,
      output_tokens: 3,
      total_tokens: 10,
      input_tokens_details: { cached_tokens: 5 },
      output_tokens_details: { reasoning_tokens: 0 },
    });
    expect(anthropicUsageToResponseUsage(null)).toBeNull();
    expect(
      anthropicUsageToResponseUsage({} as unknown as Anthropic.Usage),
    ).toEqual({
      input_tokens: 0,
      output_tokens: 0,
      total_tokens: 0,
      input_tokens_details: { cached_tokens: 0 },
      output_tokens_details: { reasoning_tokens: 0 },
    });
  });
});

describe("anthropic request body", () => {
  const messages = [
    { role: "system", content: "be terse" },
    { role: "developer", content: "reply in English" },
    { role: "user", content: "hi" },
  ] as Message[];

  it("lifts system and developer messages to top-level system", () => {
    const { system, messages: conversation } = splitAnthropicMessages(messages);
    expect(system).toBe("be terse\n\nreply in English");
    expect(conversation).toHaveLength(1);
    expect(conversation[0]).toMatchObject({ role: "user" });
  });

  it("defaults max_tokens to 4096 and omits empty system/tools", () => {
    const body = buildAnthropicBody({
      model: "claude-sonnet-4-6",
      messages: [{ role: "user", content: "hi" }] as Message[],
    });
    expect(body.model).toBe("claude-sonnet-4-6");
    expect(body.max_tokens).toBe(4096);
    expect(body.system).toBeUndefined();
    expect(body.tools).toBeUndefined();
  });

  it("uses the configured output token limit and maps tools", () => {
    const body = buildAnthropicBody({
      model: "claude-sonnet-4-6",
      messages,
      maxOutputTokens: 512,
      tools: [
        {
          name: "shell",
          description: "Run a shell command.",
          parameters: {
            type: "object",
            additionalProperties: false,
            properties: { command: { type: "string" } },
            required: ["command"],
          },
        },
      ],
    });
    expect(body.max_tokens).toBe(512);
    expect(body.system).toBe("be terse\n\nreply in English");
    expect(body.tools).toEqual([
      {
        name: "shell",
        description: "Run a shell command.",
        input_schema: {
          type: "object",
          additionalProperties: false,
          properties: { command: { type: "string" } },
          required: ["command"],
        },
      },
    ]);
  });
});

describe("ChatCompletionAccumulator", () => {
  it("concatenates fragmented tool-call arguments and orders by index", () => {
    const accumulator = new ChatCompletionAccumulator();
    // Index 1 starts before index 0 and both argument payloads arrive split
    // across chunks; finalize must reassemble and sort by index.
    const chunks = [
      {
        id: "chatcmpl_1",
        created: 1700000000,
        model: "gpt-4o",
        choices: [
          {
            delta: {
              content: "Hel",
              tool_calls: [
                {
                  index: 1,
                  id: "call_b",
                  function: { name: "beta", arguments: '{"b":' },
                },
              ],
            },
          },
        ],
      },
      {
        choices: [
          {
            delta: {
              tool_calls: [
                {
                  index: 0,
                  id: "call_a",
                  function: { name: "alpha", arguments: '{"a":1' },
                },
                { index: 1, function: { arguments: "2}" } },
              ],
            },
          },
        ],
      },
      {
        choices: [
          {
            delta: {
              content: "lo",
              tool_calls: [
                { index: 0, function: { arguments: "}" } },
                // No id or name ever arrives for index 2, so it is dropped.
                { index: 2, function: { arguments: '{"c":3}' } },
              ],
            },
          },
        ],
        usage: {
          prompt_tokens: 10,
          completion_tokens: 5,
          total_tokens: 15,
        },
      },
    ] as unknown as ChatCompletionChunk[];

    for (const chunk of chunks) {
      accumulator.push(chunk);
    }
    const response = accumulator.finalize();

    expect(response.id).toBe("chatcmpl_1");
    expect(response.model).toBe("gpt-4o");
    expect(response.output).toEqual([
      expect.objectContaining({
        type: "message",
        content: [
          expect.objectContaining({ type: "output_text", text: "Hello" }),
        ],
      }),
      expect.objectContaining({
        type: "function_call",
        call_id: "call_a",
        name: "alpha",
        arguments: '{"a":1}',
      }),
      expect.objectContaining({
        type: "function_call",
        call_id: "call_b",
        name: "beta",
        arguments: '{"b":2}',
      }),
    ]);
    expect(response.usage).toEqual({
      input_tokens: 10,
      output_tokens: 5,
      total_tokens: 15,
      input_tokens_details: { cached_tokens: 0 },
      output_tokens_details: { reasoning_tokens: 0 },
    });
  });
});

describe("chat completion normalization", () => {
  it("maps a non-streaming completion including cached prompt tokens", () => {
    const completion = {
      id: "chatcmpl_2",
      created: 1700000001,
      model: "gpt-4o",
      choices: [
        {
          message: {
            role: "assistant",
            content: "running",
            tool_calls: [
              {
                type: "function",
                id: "call_1",
                function: { name: "shell", arguments: '{"command":"ls"}' },
              },
            ],
          },
        },
      ],
      usage: {
        prompt_tokens: 100,
        completion_tokens: 10,
        total_tokens: 110,
        prompt_tokens_details: { cached_tokens: 25 },
        completion_tokens_details: { reasoning_tokens: 3 },
      },
    } as unknown as ChatCompletion;

    const response = chatCompletionToResponse(completion);
    expect(response.id).toBe("chatcmpl_2");
    expect(response.created_at).toBe(1700000001);
    expect(response.output).toEqual([
      expect.objectContaining({
        type: "message",
        content: [
          expect.objectContaining({ type: "output_text", text: "running" }),
        ],
      }),
      expect.objectContaining({
        type: "function_call",
        call_id: "call_1",
        name: "shell",
        arguments: '{"command":"ls"}',
      }),
    ]);
    expect(response.usage).toEqual({
      input_tokens: 100,
      output_tokens: 10,
      total_tokens: 110,
      input_tokens_details: { cached_tokens: 25 },
      output_tokens_details: { reasoning_tokens: 3 },
    });
  });

  it("defaults missing token details to zero and null usage to null", () => {
    expect(
      chatUsageToResponseUsage({
        prompt_tokens: 4,
        completion_tokens: 2,
        total_tokens: 6,
      } as ChatCompletion["usage"]),
    ).toEqual({
      input_tokens: 4,
      output_tokens: 2,
      total_tokens: 6,
      input_tokens_details: { cached_tokens: 0 },
      output_tokens_details: { reasoning_tokens: 0 },
    });
    expect(chatUsageToResponseUsage(null)).toBeNull();
  });
});

describe("chat message conversion", () => {
  it("maps tool messages to tool_call_id plus JSON content", () => {
    expect(
      messageToChatMessage({
        role: "tool",
        content: [
          {
            type: "tool_result",
            tool_call_id: "call_1",
            tool_name: "shell",
            output: { ok: true, stdout: "hi\n" },
          },
        ],
      } as Message),
    ).toEqual({
      role: "tool",
      tool_call_id: "call_1",
      content: '{"ok":true,"stdout":"hi\\n"}',
    });
  });

  it("maps system and developer roles to chat system messages", () => {
    expect(
      messageToChatMessage({ role: "developer", content: "rules" } as Message),
    ).toEqual({ role: "system", content: "rules" });
  });

  it("extracts assistant tool_calls alongside text content", () => {
    expect(
      messageToChatMessage({
        role: "assistant",
        content: [
          { type: "text", text: "run" },
          {
            type: "tool_call",
            tool_call_id: "call_1",
            tool_name: "shell",
            arguments: { command: "ls" },
          },
        ],
      } as Message),
    ).toEqual({
      role: "assistant",
      content: "run",
      tool_calls: [
        {
          id: "call_1",
          type: "function",
          function: { name: "shell", arguments: '{"command":"ls"}' },
        },
      ],
    });
  });

  it("skips malformed assistant tool_call parts", () => {
    expect(assistantToolCalls("just text")).toEqual([]);
    expect(
      assistantToolCalls([
        { type: "text", text: "no calls" },
        { type: "tool_call", tool_name: "shell" }, // missing tool_call_id
        { type: "tool_call", tool_call_id: "call_1" }, // missing tool_name
      ]),
    ).toEqual([]);
    // Non-object arguments serialize as an empty object.
    expect(
      assistantToolCalls([
        {
          type: "tool_call",
          tool_call_id: "call_1",
          tool_name: "shell",
          arguments: "not-an-object",
        },
      ]),
    ).toEqual([
      {
        id: "call_1",
        type: "function",
        function: { name: "shell", arguments: "{}" },
      },
    ]);
  });

  it("throws when a tool message lacks a tool_result content part", () => {
    expect(() => toolResultContent("plain string")).toThrow(
      "tool message must contain a tool_result content part",
    );
    expect(() =>
      toolResultContent([{ type: "text", text: "not a result" }]),
    ).toThrow("tool message must contain a tool_result content part");
    expect(() =>
      toolResultContent([{ type: "tool_result", output: {} }]),
    ).toThrow("tool message must contain a tool_result content part");
  });
});

describe("usage cost annotation", () => {
  // Mirrors cost.test.ts's fixture; claude-sonnet-4-6 bills additively.
  const FIXTURE = `{
    "sample_spec": { "comment": "ignored" },
    "claude-sonnet-4-6": {
      "litellm_provider": "anthropic", "input_cost_per_token": 3e-06,
      "output_cost_per_token": 1.5e-05, "cache_read_input_token_cost": 3e-07
    }
  }`;
  let tempdir: string | null = null;

  afterAll(async () => {
    if (tempdir) {
      await fs.rm(tempdir, { recursive: true, force: true });
    }
  });

  it("attaches cost_usd to messages events from the shared price table", async () => {
    tempdir = await fs.mkdtemp(path.join(os.tmpdir(), "exo-prices-"));
    const pricesPath = path.join(tempdir, "prices.json");
    await fs.writeFile(pricesPath, FIXTURE, "utf8");

    const previous = process.env.EXO_LITELLM_PRICES_PATH;
    process.env.EXO_LITELLM_PRICES_PATH = pricesPath;
    try {
      // ensureTable memoizes globally, so the fixture table persists for the
      // rest of this worker process. That is safe here: no other test in this
      // file asserts on cost for a model present in the fixture.
      await ensureTable();
    } finally {
      if (previous === undefined) {
        delete process.env.EXO_LITELLM_PRICES_PATH;
      } else {
        process.env.EXO_LITELLM_PRICES_PATH = previous;
      }
    }

    const response = {
      id: "resp_1",
      model: "claude-sonnet-4-6",
      output: [
        {
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: "hello", annotations: [] }],
        },
      ],
      usage: {
        input_tokens: 500,
        output_tokens: 200,
        total_tokens: 10_700,
        input_tokens_details: { cached_tokens: 10_000 },
        output_tokens_details: { reasoning_tokens: 0 },
      },
    } as unknown as Response;

    const events = responseToLinguaEvents(response);
    const messages = events.find((event) => event.type === "messages");
    const usage = (messages as { usage?: Record<string, unknown> }).usage;
    // 500 fresh * 3e-6 + 10k cached * 3e-7 + 200 out * 1.5e-5
    expect(usage?.cost_usd).toBeCloseTo(0.0075, 12);
    expect(usage?.prompt_tokens).toBe(500);
    expect(usage?.prompt_cached_tokens).toBe(10_000);
  });

  it("omits cost_usd for models missing from the table", () => {
    const response = {
      id: "resp_2",
      model: "acme-llm-9000",
      output: [
        {
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: "hi", annotations: [] }],
        },
      ],
      usage: {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
        input_tokens_details: { cached_tokens: 0 },
        output_tokens_details: { reasoning_tokens: 0 },
      },
    } as unknown as Response;

    const events = responseToLinguaEvents(response);
    const messages = events.find((event) => event.type === "messages");
    const usage = (messages as { usage?: Record<string, unknown> }).usage;
    expect(usage).toBeDefined();
    expect(usage).not.toHaveProperty("cost_usd");
  });
});
