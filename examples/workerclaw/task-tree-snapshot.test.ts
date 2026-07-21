import { describe, expect, it } from "vitest";

import {
  buildAutonomousContinueUserMessage,
  isTaskTreeFinished,
  type TaskTreeSnapshot,
} from "./task-tree-snapshot.js";

describe("task-tree-snapshot", () => {
  it("isTaskTreeFinished is false until complete_task persists taskComplete", () => {
    const open: TaskTreeSnapshot = {
      rootRef: "root",
      nodes: {},
      updatedAt: new Date().toISOString(),
    };
    expect(isTaskTreeFinished(open)).toBe(false);
    expect(
      isTaskTreeFinished({
        ...open,
        taskComplete: {
          summary: "Done",
          status: "completed",
          completedAt: new Date().toISOString(),
        },
      }),
    ).toBe(true);
  });

  it("buildAutonomousContinueUserMessage reminds agent to use tools", () => {
    const msg = buildAutonomousContinueUserMessage(
      "Let me install marp manually",
    );
    expect(msg).toContain("complete_task");
    expect(msg).toContain("Let me install marp manually");
    expect(msg).toContain("text-only");
  });
});
