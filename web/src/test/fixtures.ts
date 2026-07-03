import type { Event, EventData } from "../api/protocol";

export function makeEvent(
  data: EventData,
  overrides: Partial<Omit<Event, "data">> = {},
): Event {
  return {
    id: overrides.id ?? "evt-1",
    conversation_id: overrides.conversation_id ?? "conv-1",
    session_id: overrides.session_id ?? null,
    turn_id: overrides.turn_id ?? null,
    created_at: overrides.created_at ?? "2025-06-01T12:00:00.000Z",
    data,
  };
}
