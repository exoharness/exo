import { describe, expect, it } from "vitest";

import { toolResultMessage, type Event, type Message } from "@exo/harness";

import {
  materializeWorkerclawEventsToMessages,
  repairLinguaToolPairing,
} from "./message-materialize.js";

describe("materializeWorkerclawEventsToMessages", () => {
  it("keeps parallel tool results when names only appear in messages events", () => {
    const events: Event[] = [
      {
        id: "1",
        conversationId: "conversation",
        createdAt: "2026-01-01T00:00:00Z",
        data: {
          type: "messages",
          messages: [
            {
              role: "assistant",
              content: [
                {
                  type: "tool_call",
                  tool_call_id: "call_a",
                  tool_name: "task_tree_update_status",
                  arguments: {},
                },
                {
                  type: "tool_call",
                  tool_call_id: "call_b",
                  tool_name: "task_tree_update_status",
                  arguments: {},
                },
              ],
            },
          ],
        },
      },
      {
        id: "2",
        conversationId: "conversation",
        createdAt: "2026-01-01T00:00:01Z",
        data: {
          type: "tool_result",
          tool_call_id: "call_a",
          result: { ok: true, value: { status: "in_progress" } },
        },
      },
      {
        id: "3",
        conversationId: "conversation",
        createdAt: "2026-01-01T00:00:02Z",
        data: {
          type: "tool_result",
          tool_call_id: "call_b",
          result: { ok: true, value: { status: "completed" } },
        },
      },
    ];

    expect(materializeWorkerclawEventsToMessages(events)).toEqual([
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
          {
            type: "tool_call",
            tool_call_id: "call_b",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_a", "task_tree_update_status", {
        ok: true,
        value: { status: "in_progress" },
      }),
      toolResultMessage("call_b", "task_tree_update_status", {
        ok: true,
        value: { status: "completed" },
      }),
    ]);
  });
});

describe("repairLinguaToolPairing", () => {
  it("coalesces split assistant rows before pairing tool results", () => {
    const messages: Message[] = [
      {
        role: "assistant",
        content: [{ type: "text", text: "Running tools in parallel." }],
      },
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_b",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_a", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
      toolResultMessage("call_b", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
    ];

    expect(repairLinguaToolPairing(messages)).toEqual([
      {
        role: "assistant",
        content: [
          { type: "text", text: "Running tools in parallel." },
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
          {
            type: "tool_call",
            tool_call_id: "call_b",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_a", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
      toolResultMessage("call_b", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
    ]);
  });

  it("synthesizes missing tool results after parallel assistant tool calls", () => {
    const messages: Message[] = [
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "task_tree_init",
            arguments: {},
          },
          {
            type: "tool_call",
            tool_call_id: "call_b",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_b", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
    ];

    expect(repairLinguaToolPairing(messages)).toEqual([
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "call_a",
            tool_name: "task_tree_init",
            arguments: {},
          },
          {
            type: "tool_call",
            tool_call_id: "call_b",
            tool_name: "task_tree_update_status",
            arguments: {},
          },
        ],
      },
      toolResultMessage("call_b", "task_tree_update_status", {
        ok: true,
        value: {},
      }),
      toolResultMessage("call_a", "task_tree_init", {
        ok: false,
        error: "tool result missing from event log; synthesized by WorkerClaw",
      }),
    ]);
  });
});
