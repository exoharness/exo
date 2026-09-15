import { describe, expect, it, vi } from "vitest";
import type { Response } from "openai/resources/responses/responses";
import {
  messagesEvent,
  type Event,
  type EventData,
  type TurnContext,
} from "@exo/harness";
import type {
  NativeResponsesRequest,
  ResponsesRuntimeLike,
} from "@exo/model-runtime/responses";
import { runGliaTurn } from "./glia-harness";

const { executePending } = vi.hoisted(() => ({ executePending: vi.fn() }));
vi.mock("@exo/model-runtime/turn-loop", () => ({
  createDefaultToolRegistry: async () => ({
    definitions: () => [
      { name: "shell", description: "shell", parameters: {} },
    ],
    executePending,
  }),
}));

function response(text: string, args?: string): Response {
  return {
    id: "response",
    model: "test-model",
    status: "completed",
    output: [
      {
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text, annotations: [] }],
      },
      ...(args === undefined
        ? []
        : [
            {
              type: "function_call",
              call_id: "call",
              name: "shell",
              arguments: args,
            },
          ]),
    ],
    usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15 },
  } as unknown as Response;
}

function review(decision: string, feedback = "Review guidance"): Response {
  return response(JSON.stringify({ decision, feedback }));
}

function setup(responses: Response[], prior?: Event[], streaming = false) {
  const events: Event[] = prior
    ? [...prior]
    : [
        {
          id: "input",
          conversationId: "conversation",
          createdAt: "2026-09-15T00:00:00Z",
          turnId: "turn",
          data: messagesEvent([
            {
              role: "user",
              content: "Optimize the benchmark; target latency 20 ms.",
            },
          ]),
        },
      ];
  const requests: NativeResponsesRequest[] = [];
  const complete = vi.fn(async (request: NativeResponsesRequest) => {
    requests.push(structuredClone(request));
    const next = responses.shift();
    if (!next) throw new Error("Unexpected model call");
    return next;
  });
  executePending.mockReset().mockImplementation(async (calls) => [
    {
      type: "tool_result",
      tool_call_id: calls[0].toolCallId,
      result: { stdout: "PRIVATE_RAW_METRICS", exit_code: 0 },
    },
  ]);
  const runtime = {
    complete,
    completeStream: vi.fn(async (request, handlers) => {
      await handlers?.onFirstChunk?.(1);
      await handlers?.onTextDelta?.("streamed research report");
      return complete(request);
    }),
    traceToolCall: vi.fn(async (_parent, _context, call, _round, execute) =>
      execute!(call),
    ),
    runTurn: vi.fn(),
  } satisfies ResponsesRuntimeLike;
  const context = {
    agentConfig: { instructions: [], model: "test-model" },
    exoharness: {
      current: {
        agent: { record: { id: "agent" } },
        conversation: {
          record: { id: "conversation" },
          getEvents: vi.fn(async () => ({ events: [...events], cursor: null })),
        },
        turn: {
          record: { id: "turn", sessionId: "session" },
          addEvents: vi.fn(async (data: EventData[]) => {
            const added = data.map((item, index) => ({
              id: String(events.length + index),
              conversationId: "conversation",
              createdAt: "2026-09-15T00:00:00Z",
              turnId: "turn",
              data: item,
            }));
            events.push(...added);
            return {
              eventIds: added.map((event) => event.id),
              latestEventId: added.at(-1)!.id,
            };
          }),
        },
      },
    },
    streaming,
    stream: { firstChunk: vi.fn(), text: vi.fn() },
  } as unknown as TurnContext;
  return { runtime, context, requests, events };
}

function payloads(events: Event[], eventType: string) {
  return events
    .filter((event) => event.data.event_type === eventType)
    .map((event) => event.data.payload);
}

describe("single-context Glia", () => {
  it("executes research tools, restricts the Supervisor view, and feeds revisions back", async () => {
    const state = setup([
      response(
        "Baseline is 40 ms; testing the queueing hypothesis.",
        '{"command":"PRIVATE_SOURCE_CODE"}',
      ),
      ...Array.from({ length: 4 }, () =>
        response("Collecting evidence", '{"command":"benchmark"}'),
      ),
      review(
        "revise",
        "What evidence distinguishes queueing from service time?",
      ),
      response("Measured 18 ms. Artifacts and reproduction command recorded."),
      review("finish"),
    ]);
    await runGliaTurn(state.runtime, state.context, "parent", "test-model");
    expect(executePending).toHaveBeenCalledTimes(5);
    const supervisor = state.requests[5];
    expect(supervisor.tools).toEqual([]);
    expect(JSON.stringify(supervisor.messages)).toContain("Baseline is 40 ms");
    expect(JSON.stringify(supervisor.messages)).not.toMatch(
      /PRIVATE_SOURCE_CODE|PRIVATE_RAW_METRICS/,
    );
    expect(JSON.stringify(state.requests[6].messages)).toContain(
      "What evidence distinguishes queueing",
    );
    expect(JSON.stringify(state.requests[6].messages)).toContain(
      "PRIVATE_RAW_METRICS",
    );
    expect(payloads(state.events, "glia_run_finished")).toEqual([
      { reason: "supervisor_approved", researcher_rounds: 6 },
    ]);
    // Both roles retain usage in canonical message events.
    expect(state.events.filter((event) => event.data.usage)).toHaveLength(8);
  });

  it("always reviews proposed completion and continues after a rejection", async () => {
    const state = setup([
      response("Done without a measured baseline."),
      review("revise", "Establish the baseline first."),
      response("Baseline and candidate were measured."),
      review("finish"),
    ]);
    await runGliaTurn(state.runtime, state.context, "parent", "test-model");
    expect(state.requests).toHaveLength(4);
    expect(JSON.stringify(state.requests[2])).toContain(
      "Establish the baseline first.",
    );
  });

  it("reviews periodically between experiments", async () => {
    const state = setup([
      response("Running baseline", '{"command":"baseline"}'),
      ...Array.from({ length: 4 }, () =>
        response("Testing candidate", '{"command":"benchmark"}'),
      ),
      review("continue"),
      response("Results"),
      review("finish"),
    ]);
    await runGliaTurn(state.runtime, state.context, "parent", "test-model");
    expect(
      state.requests.map((request) => request.metadata?.glia_role),
    ).toEqual([
      "researcher",
      "researcher",
      "researcher",
      "researcher",
      "researcher",
      "supervisor",
      "researcher",
      "supervisor",
    ]);
  });

  it("fails on malformed tool arguments instead of treating them as completion", async () => {
    const state = setup([response("Running baseline", '{"command":')]);
    await expect(
      runGliaTurn(state.runtime, state.context, "parent", "test-model"),
    ).rejects.toThrow("invalid tool arguments");
    expect(executePending).not.toHaveBeenCalled();
    expect(payloads(state.events, "glia_run_finished")).toEqual([]);
  });

  it("stops within budget, reserves a tool-free report, and streams the stop notice", async () => {
    const state = setup(
      [response("Incomplete"), review("revise")],
      undefined,
      true,
    );
    state.context.agentConfig.maxToolRoundTrips = 0;
    await runGliaTurn(state.runtime, state.context, "parent", "test-model");
    expect(state.requests).toHaveLength(2);
    expect(state.requests[0].tools).toEqual([]);
    expect(state.runtime.completeStream).toHaveBeenCalledOnce();
    expect(state.context.stream.text).toHaveBeenCalledWith(
      expect.stringContaining("has not approved"),
    );
    expect(payloads(state.events, "glia_run_finished")).toEqual([
      { reason: "round_budget", researcher_rounds: 1 },
    ]);
  });

  it("replays durable reviews across turns and reads all event pages", async () => {
    const previous = setup([
      response("Incomplete"),
      review("revise", "Recall the memory-pressure finding."),
    ]);
    previous.context.agentConfig.maxToolRoundTrips = 0;
    await runGliaTurn(
      previous.runtime,
      previous.context,
      "parent",
      "test-model",
    );
    const next = setup(
      [response("Measured completion"), review("finish")],
      previous.events,
    );
    const getEvents = vi.mocked(
      next.context.exoharness.current.conversation.getEvents,
    );
    getEvents.mockResolvedValueOnce({
      events: previous.events.slice(0, 2),
      cursor: "next-page",
    });
    getEvents.mockResolvedValueOnce({
      events: previous.events.slice(2),
      cursor: null,
    });
    await runGliaTurn(next.runtime, next.context, "parent", "test-model");
    expect(getEvents).toHaveBeenNthCalledWith(2, {
      direction: "asc",
      cursor: "next-page",
    });
    expect(JSON.stringify(next.requests[0])).toContain(
      "Recall the memory-pressure finding.",
    );
    expect(JSON.stringify(next.requests[1])).toContain(
      "Recall the memory-pressure finding.",
    );
  });

  it("does not disclose stored reasoning or tool content blocks to the Supervisor", async () => {
    const state = setup([response("Complete"), review("finish")]);
    state.events.push({
      id: "prior",
      conversationId: "conversation",
      createdAt: "2026-09-15T00:00:00Z",
      data: messagesEvent([
        {
          role: "assistant",
          content: [
            { type: "reasoning", text: "PRIVATE_REASONING" },
            { type: "text", text: "Public finding" },
            {
              type: "tool_call",
              tool_name: "shell",
              arguments: { command: "PRIVATE_CODE" },
            },
          ],
        },
      ]),
    });
    await runGliaTurn(state.runtime, state.context, "parent", "test-model");
    const supervisorMessages = JSON.stringify(state.requests[1].messages);
    expect(supervisorMessages).toContain("Public finding");
    expect(supervisorMessages).not.toMatch(/PRIVATE_REASONING|PRIVATE_CODE/);
  });

  it.each([
    "not json",
    '{"decision":"invalid","feedback":"x"}',
    '{"decision":"finish","feedback":""}',
  ])("fails explicitly on an invalid Supervisor response: %s", async (text) => {
    const state = setup([response("Done"), response(text)]);
    await expect(
      runGliaTurn(state.runtime, state.context, "parent", "test-model"),
    ).rejects.toThrow();
    expect(payloads(state.events, "glia_run_finished")).toEqual([]);
  });

  it("never executes Supervisor tool requests", async () => {
    const state = setup([
      response("Done"),
      response("", '{"command":"forbidden"}'),
    ]);
    await expect(
      runGliaTurn(state.runtime, state.context, "parent", "test-model"),
    ).rejects.toThrow("Supervisor must not request tools");
    expect(executePending).not.toHaveBeenCalled();
  });

  it("rejects premature Supervisor termination during active experimentation", async () => {
    const state = setup([
      ...Array.from({ length: 5 }, () =>
        response("Experiment", '{"command":"run"}'),
      ),
      review("finish"),
    ]);
    await expect(
      runGliaTurn(state.runtime, state.context, "parent", "test-model"),
    ).rejects.toThrow("cannot finish");
  });

  it("does not approve empty or truncated Researcher reports", async () => {
    for (const report of [
      response(""),
      { ...response("Partial report"), status: "incomplete" as const },
    ]) {
      const state = setup([report]);
      await expect(
        runGliaTurn(state.runtime, state.context, "parent", "test-model"),
      ).rejects.toThrow();
      expect(state.requests).toHaveLength(1);
      expect(payloads(state.events, "glia_run_finished")).toEqual([]);
    }
  });

  it("does not accept a truncated Supervisor decision", async () => {
    const state = setup([
      response("Done"),
      { ...review("finish"), status: "incomplete" },
    ]);
    await expect(
      runGliaTurn(state.runtime, state.context, "parent", "test-model"),
    ).rejects.toThrow("Supervisor response did not complete");
    expect(payloads(state.events, "glia_run_finished")).toEqual([]);
  });
});
