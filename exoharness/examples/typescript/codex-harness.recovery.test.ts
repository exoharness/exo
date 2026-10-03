import { describe, expect, it, vi } from "vitest";
import type { Event, EventData, TurnContext } from "@exo/harness";
import { FakeCodexAppServer } from "../../typescript/codex/fixtures/fake-app-server";

vi.mock("@exo/model-runtime/cost", () => ({
  ensureTable: async () => {},
  getTable: () => ({}),
}));
vi.mock("@exo/model-runtime/responses", async (importOriginal) => ({
  ...(await importOriginal<object>()),
  traceExecutorTurn: async (
    _context: unknown,
    run: (parent: unknown) => Promise<unknown>,
  ) => run({}),
  tracedUnderParent: async (
    _parent: unknown,
    run: (span: { log: (value: unknown) => void }) => Promise<unknown>,
  ) => run({ log: () => {} }),
}));
vi.mock("@exo/model-runtime/shared", async (importOriginal) => ({
  ...(await importOriginal<object>()),
  resolveSandboxModel: () => ({
    model: "test-model",
    baseUrl: "https://example.invalid",
  }),
}));

import harness from "./codex-harness";

function testContext(
  server: FakeCodexAppServer,
  id: string,
  options: {
    savedTurn?: boolean;
    startIntent?: boolean;
    approvalRequested?: boolean;
  } = {},
): { context: TurnContext; events: Event[] } {
  const events: Event[] = [];
  const add = (data: EventData) => {
    events.push({
      id: String(events.length + 1),
      conversationId: id,
      sessionId: "session-1",
      turnId: "exo-turn",
      createdAt: new Date().toISOString(),
      data,
    });
  };
  add({
    type: "messages",
    messages: [{ role: "user", content: "do the work" }],
  });
  if (options.startIntent) {
    add({
      type: "custom",
      event_type: "codex_turn_start_intent",
      payload: {
        codex_thread_id: "old-thread",
      },
    });
  }
  if (options.savedTurn) {
    add({
      type: "custom",
      event_type: "codex_turn_started",
      payload: {
        codex_thread_id: "old-thread",
        codex_turn_id: "old-turn",
      },
    });
  }
  if (options.approvalRequested) {
    add({
      type: "custom",
      event_type: "agent_runtime.approval_requested",
      payload: { approval_id: "approval-1" },
    });
  }
  const context = {
    mcpServers: [],
    tools: [],
    agentConfig: {
      instructions: [],
      harness: "typescript",
      enableAgentToolCreation: false,
      sandbox: {
        provider: "local_process",
        mounts: [],
        enableNetworking: true,
        scope: "conversation",
      },
      model: "test-model",
    },
    conversationConfig: { mounts: [], workdir: "/workspace" },
    request: { input: [{ role: "user", content: "do the work" }] },
    streaming: false,
    exoharness: {
      current: {
        agent: { record: { id: `agent-${id}`, slug: "codex" } },
        conversation: {
          record: { id },
          getEvents: async (query?: { turnId?: string; types?: string[] }) => ({
            events: events.filter(
              (event) =>
                (!query?.turnId || event.turnId === query.turnId) &&
                (!query?.types ||
                  query.types.includes(
                    event.data.type === "custom"
                      ? String(event.data.event_type)
                      : event.data.type,
                  )),
            ),
            cursor: null,
          }),
        },
        turn: {
          record: { id: "exo-turn", sessionId: "session-1" },
          addEvents: async (data: EventData[]) => {
            data.forEach(add);
            return { eventIds: [], latestEventId: String(events.length) };
          },
        },
      },
    },
    startSandboxProcess: async () => server.process,
    stream: {
      firstChunk: async () => {},
      text: async () => {},
    },
  } as unknown as TurnContext;
  return { context, events };
}

describe("Codex harness recovery", () => {
  it("continues a saved turn on a resumed native thread with empty input", async () => {
    const server = new FakeCodexAppServer({ resumeAvailable: true });
    const { context, events } = testContext(server, "resumed", {
      savedTurn: true,
    });
    await harness.resumeTurn!(context);
    expect(
      server.requests.some((request) => request.method === "thread/resume"),
    ).toBe(true);
    expect(
      server.requests.find((request) => request.method === "turn/start")?.params
        ?.input,
    ).toEqual([]);
    expect(
      events.some(
        (event) =>
          event.data.type === "messages" &&
          JSON.stringify(event.data.messages).includes("done"),
      ),
    ).toBe(true);
  });

  it("replays Exo history into a new thread when native resume fails", async () => {
    const server = new FakeCodexAppServer({ resumeAvailable: false });
    const { context } = testContext(server, "replayed", { savedTurn: true });
    await harness.resumeTurn!(context);
    expect(
      server.requests.some((request) => request.method === "thread/start"),
    ).toBe(true);
    expect(
      JSON.stringify(
        server.requests.find(
          (request) => request.method === "thread/inject_items",
        )?.params?.items,
      ),
    ).toContain("do the work");
    expect(
      server.requests.find((request) => request.method === "turn/start")?.params
        ?.input,
    ).toEqual([]);
  });

  it("uses the original input when no native turn was started", async () => {
    const server = new FakeCodexAppServer({ resumeAvailable: false });
    const { context, events } = testContext(server, "before-start");
    await harness.resumeTurn!(context);
    expect(
      server.requests.find((request) => request.method === "turn/start")?.params
        ?.input,
    ).toEqual([{ type: "text", text: "do the work", text_elements: [] }]);
    expect(
      events.some(
        (event) =>
          event.data.type === "custom" &&
          event.data.event_type === "codex_turn_start_intent",
      ),
    ).toBe(true);
  });

  it("fails if turn/start may have run without saving its native ID", async () => {
    const server = new FakeCodexAppServer({ resumeAvailable: false });
    const { context } = testContext(server, "ambiguous-start", {
      startIntent: true,
    });
    await expect(harness.resumeTurn!(context)).rejects.toThrow(
      "turn/start was attempted",
    );
    expect(server.requests).toEqual([]);
  });

  it("fails a live native approval state instead of waiting forever", async () => {
    const server = new FakeCodexAppServer({
      resumeAvailable: true,
      reused: true,
      oldTurn: {
        id: "old-turn",
        status: "inProgress",
        itemsView: "full",
        items: [
          { id: "shell-1", type: "commandExecution", status: "inProgress" },
        ],
      },
    });
    const { context } = testContext(server, "pending-approval", {
      savedTurn: true,
    });
    await expect(harness.resumeTurn!(context)).rejects.toThrow(
      "approval request may have been missed",
    );
    expect(
      server.requests.some((request) => request.method === "turn/start"),
    ).toBe(false);
  });

  it("fails an unanswered Exo approval even if the native snapshot has no tool item", async () => {
    const server = new FakeCodexAppServer({
      resumeAvailable: true,
      reused: true,
      oldTurn: {
        id: "old-turn",
        status: "inProgress",
        itemsView: "full",
        items: [],
      },
    });
    const { context } = testContext(server, "saved-approval", {
      savedTurn: true,
      approvalRequested: true,
    });
    await expect(harness.resumeTurn!(context)).rejects.toThrow(
      "unanswered approval",
    );
    expect(
      server.requests.some((request) => request.method === "turn/start"),
    ).toBe(false);
  });
});
