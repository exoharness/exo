import { describe, expect, it } from "vitest";

import {
  assistantTextMessage,
  materializeEventsToMessages,
  messagesEvent,
  projectAnthropicMessageToolEvents,
  toolRequestedEvent,
  toolResultEvent,
  userTextMessage,
  type Event,
} from "../typescript/harness";
import {
  codexReplayChunks,
  codexReplayItems,
} from "../typescript/codex/replay";

describe("agent harness canonical events", () => {
  it("replays message and tool events into portable conversation messages", () => {
    const events: Event[] = [
      event("e1", messagesEvent([userTextMessage("inspect the repo")])),
      event(
        "e2",
        toolRequestedEvent({
          toolCallId: "tool-1",
          request: {
            functionName: "codex.shell",
            arguments: { command: "pwd" },
          },
        }),
      ),
      event("usage", messagesEvent([])),
      event(
        "e3",
        toolResultEvent("tool-1", {
          exit_code: 0,
          stdout: "/workspace\n",
          stderr: "",
        }),
      ),
      event("e4", messagesEvent([assistantTextMessage("done")])),
    ];

    expect(materializeEventsToMessages(events)).toEqual([
      userTextMessage("inspect the repo"),
      {
        role: "assistant",
        content: [
          {
            type: "tool_call",
            tool_call_id: "tool-1",
            tool_name: "codex.shell",
            arguments: { type: "valid", value: { command: "pwd" } },
          },
        ],
      },
      {
        role: "tool",
        content: [
          {
            type: "tool_result",
            tool_call_id: "tool-1",
            tool_name: "codex.shell",
            output: {
              exit_code: 0,
              stdout: "/workspace\n",
              stderr: "",
            },
          },
        ],
      },
      assistantTextMessage("done"),
    ]);
    const replay = codexReplayItems(materializeEventsToMessages(events));
    expect(replay[1]).toMatchObject({
      type: "function_call",
      call_id: "tool-1",
      name: "codex_shell",
      arguments: '{"command":"pwd"}',
    });
    expect(replay[2]).toMatchObject({
      type: "function_call_output",
      call_id: "tool-1",
    });
  });

  it("bounds replay chunks after a cancelled embedded tool call", () => {
    const messages = materializeEventsToMessages([
      event(
        "call",
        messagesEvent([
          {
            role: "assistant",
            content: [
              {
                type: "tool_call",
                tool_call_id: "cancelled",
                tool_name: "shell",
                arguments: { type: "valid", value: {} },
              },
            ],
          },
        ]),
      ),
      ...Array.from({ length: 20 }, (_, i) =>
        event(`text-${i}`, messagesEvent([userTextMessage("a".repeat(700))])),
      ),
    ]);
    const chunks = codexReplayChunks(codexReplayItems(messages), 1024);
    expect(chunks.length).toBeGreaterThan(1);
    expect(chunks.every((chunk) => JSON.stringify(chunk).length <= 1024)).toBe(
      true,
    );
    expect(chunks[0]?.map((item) => item.type)).toEqual([
      "function_call",
      "function_call_output",
    ]);
  });

  it("projects Claude/Cursor-style tool_use and tool_result blocks", () => {
    const requested = projectAnthropicMessageToolEvents(
      {
        type: "assistant",
        message: {
          role: "assistant",
          content: [
            {
              type: "tool_use",
              id: "tool-1",
              name: "Bash",
              input: { command: "ls" },
            },
          ],
        },
      },
      { toolNamePrefix: "claude." },
    );
    const result = projectAnthropicMessageToolEvents({
      type: "user",
      message: {
        role: "user",
        content: [
          {
            type: "tool_result",
            tool_use_id: "tool-1",
            content: "README.md\n",
          },
        ],
      },
    });

    expect(requested).toEqual([
      toolRequestedEvent({
        toolCallId: "tool-1",
        request: {
          functionName: "claude.Bash",
          arguments: { command: "ls" },
        },
      }),
    ]);
    expect(result).toEqual([
      toolResultEvent("tool-1", {
        content: "README.md\n",
        is_error: false,
      }),
    ]);
  });
});

function event(id: string, data: Event["data"]): Event {
  return {
    id,
    conversationId: "conversation-1",
    sessionId: "session-1",
    turnId: "turn-1",
    createdAt: "2026-05-03T00:00:00Z",
    data,
  };
}
