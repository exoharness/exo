import { describe, expect, it } from "vitest";
import {
  clampText,
  decodeBytes,
  formatJson,
  formatRecency,
  formatTime,
  processEventSummary,
  renderAssistantContent,
  renderMessageContent,
  renderUserContent,
  shortId,
  statusText,
} from "./rendering";

describe("formatTime", () => {
  it('returns "unknown" for nullish values', () => {
    expect(formatTime(null)).toBe("unknown");
    expect(formatTime(undefined)).toBe("unknown");
    expect(formatTime("")).toBe("unknown");
  });

  it("returns the raw string for invalid dates", () => {
    expect(formatTime("not-a-date")).toBe("not-a-date");
  });

  it("formats valid ISO timestamps as hour:minute", () => {
    const formatted = formatTime("2025-06-01T14:05:00.000Z");
    expect(formatted).toMatch(/\d{1,2}:\d{2}/);
    expect(formatted).not.toBe("unknown");
  });
});

describe("formatRecency", () => {
  const now = Date.parse("2025-06-20T12:00:00.000Z");

  it('returns "no activity" for nullish timestamps', () => {
    expect(formatRecency(null, now)).toBe("no activity");
    expect(formatRecency(undefined, now)).toBe("no activity");
    expect(formatRecency("", now)).toBe("no activity");
  });

  it('returns "unknown" for invalid timestamps', () => {
    expect(formatRecency("not-a-date", now)).toBe("unknown");
  });

  it("uses relative buckets for recent activity", () => {
    expect(formatRecency("2025-06-20T11:59:30.000Z", now)).toBe("just now");
    expect(formatRecency("2025-06-20T11:45:00.000Z", now)).toBe("15m ago");
    expect(formatRecency("2025-06-20T08:00:00.000Z", now)).toBe("4h ago");
    expect(formatRecency("2025-06-18T12:00:00.000Z", now)).toBe("2d ago");
  });

  it("falls back to clock time for future timestamps", () => {
    const future = "2025-06-20T13:00:00.000Z";
    expect(formatRecency(future, now)).toBe(formatTime(future));
  });

  it("uses absolute date formatting after seven days", () => {
    const old = "2025-06-01T12:00:00.000Z";
    const formatted = formatRecency(old, now);
    expect(formatted).not.toContain("ago");
    expect(formatted).not.toBe("unknown");
  });
});

describe("shortId", () => {
  it('returns "none" for empty ids', () => {
    expect(shortId(null)).toBe("none");
    expect(shortId(undefined)).toBe("none");
    expect(shortId("")).toBe("none");
  });

  it("leaves short ids unchanged", () => {
    expect(shortId("evt-short")).toBe("evt-short");
  });

  it("abbreviates long ids with head and tail", () => {
    expect(shortId("abcdefghijklmnopqrstuvwxyz")).toBe("abcdefgh...wxyz");
  });
});

describe("formatJson", () => {
  it("pretty-prints json values", () => {
    expect(formatJson({ a: 1 })).toBe('{\n  "a": 1\n}');
  });

  it("falls back to String for circular structures", () => {
    const circular: { self?: unknown } = {};
    circular.self = circular;
    expect(formatJson(circular)).toBe("[object Object]");
  });
});

describe("renderUserContent", () => {
  it("returns plain strings as-is", () => {
    expect(renderUserContent("hello")).toBe("hello");
  });

  it("concatenates text parts and labels unknown part types", () => {
    expect(
      renderUserContent([
        { type: "text", text: "hi " },
        { type: "image", url: "x" },
      ]),
    ).toBe('hi [image] {\n  "type": "image",\n  "url": "x"\n}');
  });
});

describe("renderAssistantContent", () => {
  it("renders reasoning, tool_call, and text parts", () => {
    const text = renderAssistantContent([
      { type: "text", text: "answer" },
      { type: "reasoning", text: "thinking" },
      {
        type: "tool_call",
        tool_name: "grep",
        arguments: { pattern: "foo" },
      },
    ]);
    expect(text).toContain("answer");
    expect(text).toContain("[reasoning] thinking");
    expect(text).toContain("[tool_call grep]");
    expect(text).toContain('"pattern": "foo"');
  });
});

describe("renderMessageContent", () => {
  it("routes by role to the appropriate renderer", () => {
    expect(renderMessageContent({ role: "user", content: "ping" })).toBe(
      "ping",
    );
    expect(renderMessageContent({ role: "assistant", content: "pong" })).toBe(
      "pong",
    );
    expect(
      renderMessageContent({
        role: "tool",
        content: [
          { type: "tool_result", tool_name: "read", output: { bytes: 3 } },
        ],
      }),
    ).toContain("read:");
  });
});

describe("decodeBytes and clampText", () => {
  it("decodes utf-8 byte arrays", () => {
    expect(decodeBytes([104, 105])).toBe("hi");
    expect(decodeBytes([])).toBe("");
  });

  it("clamps long text with an ellipsis suffix", () => {
    expect(clampText("abcdef", 4)).toBe("abcd...");
    expect(clampText("short", 10)).toBe("short");
  });
});

describe("statusText", () => {
  it("describes each sandbox process status variant", () => {
    expect(statusText({ type: "running" })).toBe("running");
    expect(statusText({ type: "exited", exit_code: 2 })).toBe("exited 2");
    expect(statusText({ type: "failed", message: "oom" })).toBe("failed: oom");
    expect(statusText({ type: "cancelled" })).toBe("cancelled");
  });
});

describe("processEventSummary", () => {
  it("summarizes stdout/stderr and terminal events", () => {
    expect(
      processEventSummary({ type: "stdout", cursor: 0, data: [65, 66] }),
    ).toBe("stdout AB");
    expect(processEventSummary({ type: "exit", cursor: 1, exit_code: 1 })).toBe(
      "exit 1",
    );
    expect(
      processEventSummary({ type: "error", cursor: 2, message: "boom" }),
    ).toBe("error boom");
  });
});
