import type {
  AgentRecord,
  ConversationHandleInfo,
  Event,
} from "../api/protocol";

export interface LoadedConversationEvents {
  agentId: string;
  conversationId: string;
  events: Event[];
}

export interface OverviewEntry {
  agent: AgentRecord;
  conversation: ConversationHandleInfo;
}

export function countTurns(events: Event[]): number {
  let turns = 0;
  for (const event of events) {
    if (event.data.type === "turn_started") {
      turns += 1;
    }
  }
  return turns;
}

export function latestEventTimestamp(events: Event[]): string | null {
  if (events.length === 0) {
    return null;
  }
  return events[events.length - 1]?.created_at ?? null;
}

export function activitySortKey(
  entry: OverviewEntry,
  loadedEvents: LoadedConversationEvents | null,
): string {
  const { agent, conversation } = entry;
  const loaded =
    loadedEvents?.agentId === agent.id &&
    loadedEvents.conversationId === conversation.record.id
      ? loadedEvents.events
      : null;
  const timestamp = loaded ? latestEventTimestamp(loaded) : null;
  if (timestamp) {
    return timestamp;
  }
  return conversation.record.latest_event_id ?? "";
}

export function sortOverviewEntries(
  entries: OverviewEntry[],
  loadedEvents: LoadedConversationEvents | null,
): OverviewEntry[] {
  return [...entries].sort((left, right) => {
    const leftKey = activitySortKey(left, loadedEvents);
    const rightKey = activitySortKey(right, loadedEvents);
    return rightKey.localeCompare(leftKey);
  });
}

export interface ConversationCardStats {
  eventCount: number | null;
  turnCount: number | null;
  lastActivity: string | null;
}

export function deriveConversationCardStats(
  agentId: string,
  conversationId: string,
  loadedEvents: LoadedConversationEvents | null,
): ConversationCardStats {
  const loaded =
    loadedEvents?.agentId === agentId &&
    loadedEvents.conversationId === conversationId
      ? loadedEvents.events
      : null;
  return {
    eventCount: loaded?.length ?? null,
    turnCount: loaded ? countTurns(loaded) : null,
    lastActivity: loaded ? latestEventTimestamp(loaded) : null,
  };
}
