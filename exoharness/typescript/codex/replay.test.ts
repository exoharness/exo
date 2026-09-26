import { describe, expect, it, vi } from "vitest";
import { type JsonValue, type Message } from "../harness";
import {
  codexReplayChunks,
  codexReplayItems,
  replayCodexHistory,
} from "./replay";

describe("Codex history replay", () => {
  it("waits for native compaction before injecting the next batch", async () => {
    const operations: string[] = [];
    const server = {
      async request(method: string, _params: JsonValue) {
        operations.push(method);
      },
      async *events() {
        yield {
          method: "turn/completed",
          params: { threadId: "another-thread", turn: { status: "failed" } },
        };
        operations.push("compacted");
        yield {
          method: "turn/completed",
          params: { threadId: "thread", turn: { status: "completed" } },
        };
      },
    };
    await replayCodexHistory(
      server,
      "thread",
      codexReplayItems([
        { role: "user", content: "a".repeat(64_000) },
        { role: "assistant", content: "reply" },
      ]),
    );
    expect(operations).toEqual([
      "thread/inject_items",
      "thread/compact/start",
      "compacted",
      "thread/inject_items",
    ]);
  });

  it("stops replay when compaction fails", async () => {
    const request = vi.fn(async (_method: string, _params: JsonValue) => {});
    const server = {
      request,
      async *events() {
        yield {
          method: "error",
          params: {
            threadId: "thread",
            willRetry: false,
            error: { message: "invalid credentials" },
          },
        };
      },
    };
    await expect(
      replayCodexHistory(
        server,
        "thread",
        codexReplayItems([
          { role: "user", content: "a".repeat(64_000) },
          { role: "assistant", content: "reply" },
        ]),
      ),
    ).rejects.toThrow("invalid credentials");
    expect(request).toHaveBeenCalledTimes(2);
  });

  it("keeps long messages and tool calls structured", () => {
    const text = "support ticket evidence ".repeat(2_000);
    const messages: Message[] = [
      { role: "system", content: "You are the analyst." },
      { role: "user", content: text },
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call-1",
            tool_name: "shell",
            arguments: { type: "valid", value: { command: "cat ticket.md" } },
          },
        ],
      },
      {
        role: "tool",
        content: [
          {
            type: "tool_result",
            tool_call_id: "call-1",
            tool_name: "shell",
            output: { text: "ticket 42" },
          },
        ],
      },
      { role: "assistant", content: "The ticket describes an export bug." },
    ];
    const items = codexReplayItems(messages);
    expect(items[0]).toEqual({
      type: "message",
      role: "user",
      content: [{ type: "input_text", text }],
    });
    expect(items.at(-1)).toMatchObject({
      type: "message",
      role: "assistant",
      content: [
        { type: "output_text", text: "The ticket describes an export bug." },
      ],
    });
    expect(JSON.stringify(items)).toContain(text);
    expect(
      items.some(
        (item) => item.type === "function_call" && item.call_id === "call-1",
      ),
    ).toBe(true);
    expect(
      items.some(
        (item) =>
          item.type === "function_call_output" && item.call_id === "call-1",
      ),
    ).toBe(true);
    expect(JSON.stringify(items)).not.toContain("You are the analyst.");
    expect(codexReplayChunks(items, 100).flat()).toEqual(items);
    const toolChunk = codexReplayChunks(items, 100).find((chunk) =>
      chunk.some((item) => item.type === "function_call"),
    );
    expect(
      toolChunk?.some((item) => item.type === "function_call_output"),
    ).toBe(true);
  });

  it("doesn't split parallel calls from their results", () => {
    const chunks = codexReplayChunks(
      [
        { type: "function_call", call_id: "a", name: "first", arguments: "{}" },
        {
          type: "function_call",
          call_id: "b",
          name: "second",
          arguments: "{}",
        },
        { type: "function_call_output", call_id: "a", output: "first result" },
        { type: "function_call_output", call_id: "b", output: "second result" },
      ],
      1,
    );
    expect(chunks).toHaveLength(1);
    expect(chunks[0]).toHaveLength(4);
  });
});
