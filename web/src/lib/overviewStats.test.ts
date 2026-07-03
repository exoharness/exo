import { describe, expect, it } from "vitest";
import type { AgentRecord, ConversationHandleInfo } from "../api/protocol";
import { makeEvent } from "../test/fixtures";
import {
  activitySortKey,
  countTurns,
  deriveConversationCardStats,
  latestEventTimestamp,
  sortOverviewEntries,
  type OverviewEntry,
} from "./overviewStats";

const agent: AgentRecord = {
  id: "agent-1",
  slug: "alpha",
  name: "Alpha",
};

function makeEntry(
  conversationId: string,
  latestEventId: string | null,
  agentOverride: AgentRecord = agent,
): OverviewEntry {
  const conversation: ConversationHandleInfo = {
    agent_id: agentOverride.id,
    record: {
      id: conversationId,
      slug: conversationId,
      name: conversationId,
      latest_event_id: latestEventId,
    },
  };
  return { agent: agentOverride, conversation };
}

describe("countTurns", () => {
  it("counts only turn_started events", () => {
    const events = [
      makeEvent({ type: "turn_started" }),
      makeEvent({ type: "messages", messages: [], response_id: null }),
      makeEvent({ type: "turn_started" }),
      makeEvent({ type: "session_ended" }),
    ];
    expect(countTurns(events)).toBe(2);
  });

  it("returns zero for empty input", () => {
    expect(countTurns([])).toBe(0);
  });
});

describe("latestEventTimestamp", () => {
  it("returns the last event created_at in array order", () => {
    const events = [
      makeEvent(
        { type: "turn_started" },
        { created_at: "2025-06-01T10:00:00.000Z" },
      ),
      makeEvent(
        { type: "messages", messages: [], response_id: null },
        { created_at: "2025-06-01T11:30:00.000Z" },
      ),
    ];
    expect(latestEventTimestamp(events)).toBe("2025-06-01T11:30:00.000Z");
  });

  it("returns null for empty events", () => {
    expect(latestEventTimestamp([])).toBeNull();
  });
});

describe("activitySortKey", () => {
  it("prefers loaded event timestamps over latest_event_id metadata", () => {
    const entry = makeEntry("conv-a", "evt-old");
    const loaded = {
      agentId: agent.id,
      conversationId: "conv-a",
      events: [
        makeEvent(
          { type: "messages", messages: [], response_id: null },
          { created_at: "2025-06-02T08:00:00.000Z" },
        ),
      ],
    };
    expect(activitySortKey(entry, loaded)).toBe("2025-06-02T08:00:00.000Z");
  });

  it("falls back to latest_event_id when events are not loaded for that card", () => {
    const entry = makeEntry("conv-b", "evt-newer");
    const loaded = {
      agentId: agent.id,
      conversationId: "conv-other",
      events: [
        makeEvent(
          { type: "messages", messages: [], response_id: null },
          { created_at: "2025-06-02T08:00:00.000Z" },
        ),
      ],
    };
    expect(activitySortKey(entry, loaded)).toBe("evt-newer");
  });

  it("returns empty string when no activity metadata exists", () => {
    expect(activitySortKey(makeEntry("conv-c", null), null)).toBe("");
  });
});

describe("sortOverviewEntries", () => {
  it("orders conversations by descending activity key", () => {
    const older = makeEntry("older", "2025-06-01T10:00:00.000Z");
    const newer = makeEntry("newer", "2025-06-03T12:00:00.000Z");
    const sorted = sortOverviewEntries([older, newer], null);
    expect(sorted.map((entry) => entry.conversation.record.id)).toEqual([
      "newer",
      "older",
    ]);
  });

  it("prefers loaded timestamps over stale latest_event_id metadata when sorting", () => {
    const stale = makeEntry("stale", "2025-06-01T10:00:00.000Z");
    const freshMeta = makeEntry("fresh-meta", "2025-06-02T10:00:00.000Z");
    const loaded = {
      agentId: agent.id,
      conversationId: "stale",
      events: [
        makeEvent(
          { type: "messages", messages: [], response_id: null },
          { created_at: "2025-06-04T08:00:00.000Z" },
        ),
      ],
    };
    const sorted = sortOverviewEntries([freshMeta, stale], loaded);
    expect(sorted.map((entry) => entry.conversation.record.id)).toEqual([
      "stale",
      "fresh-meta",
    ]);
  });
});

describe("deriveConversationCardStats", () => {
  it("returns null stats when events are not loaded for the card", () => {
    expect(
      deriveConversationCardStats("agent-1", "conv-1", {
        agentId: "agent-1",
        conversationId: "conv-other",
        events: [makeEvent({ type: "turn_started" })],
      }),
    ).toEqual({
      eventCount: null,
      turnCount: null,
      lastActivity: null,
    });
  });

  it("derives counts and last activity for the matching loaded conversation", () => {
    const events = [
      makeEvent(
        { type: "turn_started" },
        { created_at: "2025-06-01T09:00:00.000Z" },
      ),
      makeEvent(
        { type: "messages", messages: [], response_id: null },
        { created_at: "2025-06-01T09:05:00.000Z" },
      ),
      makeEvent(
        { type: "turn_started" },
        { created_at: "2025-06-01T09:10:00.000Z" },
      ),
    ];
    expect(
      deriveConversationCardStats("agent-1", "conv-1", {
        agentId: "agent-1",
        conversationId: "conv-1",
        events,
      }),
    ).toEqual({
      eventCount: 3,
      turnCount: 2,
      lastActivity: "2025-06-01T09:10:00.000Z",
    });
  });

  it("reports zero turns when loaded events contain none", () => {
    const events = [
      makeEvent(
        { type: "messages", messages: [], response_id: null },
        { created_at: "2025-06-01T09:00:00.000Z" },
      ),
    ];
    expect(
      deriveConversationCardStats("agent-1", "conv-1", {
        agentId: "agent-1",
        conversationId: "conv-1",
        events,
      }).turnCount,
    ).toBe(0);
  });
});
