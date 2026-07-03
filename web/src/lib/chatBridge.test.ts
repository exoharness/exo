import { afterEach, describe, expect, it, vi } from "vitest";
import {
  clearActiveChatTurnIfMatch,
  createActiveChatTurn,
  extractChatError,
  makeChatRequestId,
  markChatCancelRequested,
  parseJsonObject,
  type ActiveChatTurn,
} from "./chatBridge";

describe("makeChatRequestId", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("uses crypto.randomUUID when available", () => {
    const randomUUID = vi.fn(() => "uuid-test-1234");
    vi.stubGlobal("crypto", { randomUUID });
    expect(makeChatRequestId()).toBe("uuid-test-1234");
    expect(randomUUID).toHaveBeenCalledOnce();
  });

  it("falls back to a chat-prefixed id when randomUUID is missing", () => {
    vi.stubGlobal("crypto", {});
    const id = makeChatRequestId();
    expect(id.startsWith("chat-")).toBe(true);
    expect(id.length).toBeGreaterThan("chat-".length);
  });
});

describe("parseJsonObject", () => {
  it("returns null for empty or non-object payloads", () => {
    expect(parseJsonObject("")).toBeNull();
    expect(parseJsonObject("   ")).toBeNull();
    expect(parseJsonObject("[]")).toBeNull();
    expect(parseJsonObject('"text"')).toBeNull();
    expect(parseJsonObject("not-json")).toBeNull();
  });

  it("parses object payloads", () => {
    expect(parseJsonObject('{"ok":true,"error":"x"}')).toEqual({
      ok: true,
      error: "x",
    });
  });
});

describe("extractChatError", () => {
  it("returns trimmed error strings and ignores invalid payloads", () => {
    expect(extractChatError({ error: "turn failed" })).toBe("turn failed");
    expect(extractChatError({ error: "  " })).toBeNull();
    expect(extractChatError({ error: 500 })).toBeNull();
    expect(extractChatError(null)).toBeNull();
  });
});

describe("active chat turn bookkeeping", () => {
  const turn: ActiveChatTurn = {
    cancelRequested: false,
    requestId: "req-1",
    startedAt: 1_700_000_000_000,
  };

  it("creates a fresh turn with cancelRequested false", () => {
    const created = createActiveChatTurn("req-new");
    expect(created.requestId).toBe("req-new");
    expect(created.cancelRequested).toBe(false);
    expect(created.startedAt).toBeGreaterThan(0);
  });

  it("clears only the matching active turn", () => {
    expect(clearActiveChatTurnIfMatch(turn, "req-1")).toBeNull();
    expect(clearActiveChatTurnIfMatch(turn, "req-other")).toBe(turn);
    expect(clearActiveChatTurnIfMatch(null, "req-1")).toBeNull();
  });

  it("marks cancel requested only for the matching request id", () => {
    expect(markChatCancelRequested(turn, "req-1")).toEqual({
      ...turn,
      cancelRequested: true,
    });
    expect(markChatCancelRequested(turn, "req-other")).toBe(turn);
    expect(markChatCancelRequested(null, "req-1")).toBeNull();
  });

  it("preserves a newer turn when an older request finishes", () => {
    const newer: ActiveChatTurn = {
      cancelRequested: false,
      requestId: "req-2",
      startedAt: turn.startedAt + 1,
    };
    expect(clearActiveChatTurnIfMatch(newer, "req-1")).toBe(newer);
    expect(markChatCancelRequested(newer, "req-1")).toBe(newer);
  });
});
