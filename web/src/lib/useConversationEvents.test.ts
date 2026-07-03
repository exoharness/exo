import { describe, expect, it } from "vitest";
import { processEventPage } from "./useConversationEvents";
import { makeEvent } from "../test/fixtures";

function messagesEvent(id: string) {
  return makeEvent(
    { type: "messages", messages: [], response_id: null },
    { id },
  );
}

describe("processEventPage", () => {
  it("returns empty fresh list and null cursor for an empty page", () => {
    const seen = new Set<string>();
    expect(processEventPage([], seen)).toEqual({ fresh: [], cursor: null });
    expect(seen.size).toBe(0);
  });

  it("returns all events as fresh when none were seen", () => {
    const seen = new Set<string>();
    const page = [messagesEvent("a"), messagesEvent("b")];
    const result = processEventPage(page, seen);

    expect(result.fresh.map((event) => event.id)).toEqual(["a", "b"]);
    expect(result.cursor).toBe("b");
    expect([...seen]).toEqual(["a", "b"]);
  });

  it("deduplicates events already in seen but still advances cursor", () => {
    const seen = new Set(["a", "b"]);
    const page = [messagesEvent("a"), messagesEvent("b"), messagesEvent("c")];
    const result = processEventPage(page, seen);

    expect(result.fresh.map((event) => event.id)).toEqual(["c"]);
    expect(result.cursor).toBe("c");
    expect([...seen].sort()).toEqual(["a", "b", "c"]);
  });

  it("returns no fresh events when the entire page is duplicate", () => {
    const seen = new Set(["x", "y"]);
    const page = [messagesEvent("x"), messagesEvent("y")];
    const result = processEventPage(page, seen);

    expect(result.fresh).toEqual([]);
    expect(result.cursor).toBe("y");
    expect(seen.size).toBe(2);
  });
});
