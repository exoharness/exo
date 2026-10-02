import { describe, expect, it } from "vitest";
import {
  assertNativeToolSafety,
  nativeItemComplete,
  nativeTurnSnapshot,
} from "./recovery";

describe("Codex native turn recovery", () => {
  it("finds the interrupted turn rather than a later turn", () => {
    const snapshot = nativeTurnSnapshot(
      {
        thread: {
          turns: [
            {
              id: "saved",
              status: "inProgress",
              itemsView: "full",
              items: [
                { id: "done", type: "agentMessage", text: "working" },
                {
                  id: "running",
                  type: "commandExecution",
                  status: "inProgress",
                },
              ],
            },
            { id: "later", status: "completed", itemsView: "full", items: [] },
          ],
        },
      },
      "saved",
    );
    expect(snapshot?.status).toBe("inProgress");
    expect(snapshot?.items.map((item) => item.id)).toEqual(["done", "running"]);
    expect(snapshot?.items.map(nativeItemComplete)).toEqual([true, false]);
  });

  it("rejects a partial snapshot before reconstructing output", () => {
    expect(() =>
      nativeTurnSnapshot(
        {
          thread: {
            turns: [
              {
                id: "saved",
                status: "completed",
                itemsView: "summary",
                items: [],
              },
            ],
          },
        },
        "saved",
      ),
    ).toThrow("incomplete item history");
  });

  it("reattaches a live tool but rejects replay when its result is unknown", () => {
    const snapshot = nativeTurnSnapshot(
      {
        thread: {
          turns: [
            {
              id: "saved",
              status: "inProgress",
              itemsView: "full",
              items: [
                {
                  id: "shell-1",
                  type: "commandExecution",
                  status: "inProgress",
                },
              ],
            },
          ],
        },
      },
      "saved",
    );
    expect(snapshot).not.toBeNull();
    expect(() =>
      assertNativeToolSafety(snapshot, new Set(["shell-1"]), true),
    ).not.toThrow();
    expect(() =>
      assertNativeToolSafety(snapshot, new Set(["shell-1"]), false),
    ).toThrow("cannot safely resume unresolved native Codex tool call");
    expect(() =>
      assertNativeToolSafety(null, new Set(["shell-1"]), false),
    ).toThrow("cannot safely replay unresolved native Codex tool call");
  });
});
