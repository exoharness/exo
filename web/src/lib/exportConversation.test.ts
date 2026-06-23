import { describe, expect, it } from "vitest";
import { buildConversationMarkdown } from "./exportConversation";
import { makeEvent } from "../test/fixtures";

describe("buildConversationMarkdown", () => {
  it("returns header only for empty events", () => {
    expect(buildConversationMarkdown([])).toBe("# Conversation export\n");
  });

  it("orders events by created_at then id", () => {
    const markdown = buildConversationMarkdown([
      makeEvent(
        {
          type: "messages",
          messages: [{ role: "user", content: "second" }],
          response_id: null,
        },
        { id: "evt-b", created_at: "2025-06-01T12:00:01.000Z" },
      ),
      makeEvent(
        {
          type: "messages",
          messages: [{ role: "user", content: "first" }],
          response_id: null,
        },
        { id: "evt-a", created_at: "2025-06-01T12:00:00.000Z" },
      ),
      makeEvent(
        {
          type: "messages",
          messages: [{ role: "user", content: "tie-break" }],
          response_id: null,
        },
        { id: "evt-c", created_at: "2025-06-01T12:00:00.000Z" },
      ),
    ]);

    const firstIdx = markdown.indexOf("first");
    const tieIdx = markdown.indexOf("tie-break");
    const secondIdx = markdown.indexOf("second");
    expect(firstIdx).toBeGreaterThan(-1);
    expect(tieIdx).toBeGreaterThan(firstIdx);
    expect(secondIdx).toBeGreaterThan(tieIdx);
  });

  it("includes user and assistant messages but skips other roles", () => {
    const markdown = buildConversationMarkdown([
      makeEvent({
        type: "messages",
        messages: [
          { role: "user", content: "question" },
          { role: "assistant", content: "answer" },
          { role: "system", content: "hidden" },
          {
            role: "tool",
            content: [{ type: "tool_result", tool_name: "t", output: "x" }],
          },
        ],
        response_id: null,
      }),
    ]);

    expect(markdown).toContain("## user ·");
    expect(markdown).toContain("question");
    expect(markdown).toContain("## assistant ·");
    expect(markdown).toContain("answer");
    expect(markdown).not.toContain("hidden");
    expect(markdown).not.toContain("## tool ·");
  });

  it("renders tool_requested and tool_result sections as json fences", () => {
    const markdown = buildConversationMarkdown([
      makeEvent(
        {
          type: "tool_requested",
          tool_call_id: "tc-1",
          response_id: null,
          request: {
            function_name: "read_file",
            arguments: { path: "/tmp/a" },
          },
        },
        { created_at: "2025-06-01T12:00:00.000Z" },
      ),
      makeEvent(
        {
          type: "tool_result",
          tool_call_id: "tc-1",
          result: { ok: true },
        },
        { created_at: "2025-06-01T12:00:01.000Z" },
      ),
    ]);

    expect(markdown).toContain("## tool · read_file ·");
    expect(markdown).toContain('"path": "/tmp/a"');
    expect(markdown).toContain("## tool result ·");
    expect(markdown).toContain('"ok": true');
  });

  it("preserves markdown metacharacters in message bodies", () => {
    const markdown = buildConversationMarkdown([
      makeEvent({
        type: "messages",
        messages: [
          {
            role: "user",
            content: "# not a heading\n`code` and | pipe |",
          },
          {
            role: "assistant",
            content: "**bold** _italic_",
          },
        ],
        response_id: null,
      }),
    ]);

    expect(markdown).toContain("# not a heading");
    expect(markdown).toContain("`code` and | pipe |");
    expect(markdown).toContain("**bold** _italic_");
    expect(markdown.match(/^## user/m)).not.toBeNull();
    expect(markdown.match(/^## assistant/m)).not.toBeNull();
  });

  it("escapes nothing in tool json fences but keeps valid json", () => {
    const markdown = buildConversationMarkdown([
      makeEvent({
        type: "tool_requested",
        tool_call_id: "tc-2",
        response_id: null,
        request: {
          function_name: "echo",
          arguments: {
            note: 'say "hi" and \\ backslash',
            nested: { tags: "<script>" },
          },
        },
      }),
    ]);

    expect(markdown).toContain('"note": "say \\"hi\\" and \\\\ backslash"');
    expect(markdown).toContain('"tags": "<script>"');
    expect(markdown).toMatch(/```json[\s\S]*```/);
  });

  it("handles many events without dropping sections", () => {
    const events = Array.from({ length: 200 }, (_, index) =>
      makeEvent(
        {
          type: "messages",
          messages: [{ role: "user", content: `msg-${index}` }],
          response_id: null,
        },
        {
          id: `evt-${String(index).padStart(4, "0")}`,
          created_at: `2025-06-01T12:00:${String(index % 60).padStart(2, "0")}.000Z`,
        },
      ),
    );
    const markdown = buildConversationMarkdown(events);
    expect(markdown).toContain("msg-0");
    expect(markdown).toContain("msg-199");
    expect(markdown.match(/## user ·/g)?.length).toBe(200);
  });

  it("skips unknown event types and empty message batches", () => {
    const markdown = buildConversationMarkdown([
      makeEvent({ type: "turn_started" }),
      makeEvent({ type: "session_ended" }),
      makeEvent({
        type: "messages",
        messages: [],
        response_id: null,
      }),
    ]);
    expect(markdown).toBe("# Conversation export\n");
  });

  it("exports standalone tool_result events without a matching request", () => {
    const markdown = buildConversationMarkdown([
      makeEvent({
        type: "tool_result",
        tool_call_id: "orphan",
        result: { warning: "late result" },
      }),
    ]);
    expect(markdown).toContain("## tool result ·");
    expect(markdown).toContain('"warning": "late result"');
  });

  it("renders non-string assistant content as json", () => {
    const markdown = buildConversationMarkdown([
      makeEvent({
        type: "messages",
        messages: [
          {
            role: "assistant",
            content: [{ type: "text", text: "structured" }],
          },
        ],
        response_id: null,
      }),
    ]);
    expect(markdown).toContain("structured");
    expect(markdown).toContain("## assistant ·");
  });

  it("ends with a trailing newline", () => {
    const markdown = buildConversationMarkdown([
      makeEvent({
        type: "messages",
        messages: [{ role: "user", content: "one" }],
        response_id: null,
      }),
    ]);
    expect(markdown.endsWith("\n")).toBe(true);
  });
});
