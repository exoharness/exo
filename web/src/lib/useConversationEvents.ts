import { useCallback, useEffect, useRef, useState } from "react";
import { EVENT_PAGE_SIZE, type ExoClient } from "../api/exoClient";
import type { Event, EventId } from "../api/protocol";

// Cursor polling stands in for the substrate's eventual `watch_events` endpoint:
// poll `conversation_get_events` after the latest event id, append + dedupe, and
// vary the cadence by activity. A streaming endpoint would swap in behind this
// same hook without touching the transcript.
const ACTIVE_INTERVAL_MS = 1000;
const IDLE_BASE_INTERVAL_MS = 2000;
const IDLE_MAX_INTERVAL_MS = 15000;
const IDLE_BACKOFF = 1.6;

interface UseConversationEventsOptions {
  client: ExoClient | null;
  agentId: string | null;
  conversationId: string | null;
  // While a turn is pending the conversation is changing fast, so poll fast.
  turnPending: boolean;
  // Bumping this forces a clean reload (e.g. the inspector's manual refresh).
  reloadKey: number;
}

interface UseConversationEventsResult {
  events: Event[];
  loading: boolean;
  error: string | null;
  // Fetch immediately instead of waiting for the next scheduled poll.
  poll: () => void;
}

export function useConversationEvents({
  client,
  agentId,
  conversationId,
  turnPending,
  reloadKey,
}: UseConversationEventsOptions): UseConversationEventsResult {
  const [events, setEvents] = useState<Event[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const turnPendingRef = useRef(turnPending);
  turnPendingRef.current = turnPending;

  const pollNowRef = useRef<() => void>(() => {});
  const poll = useCallback(() => pollNowRef.current(), []);

  useEffect(() => {
    if (!client || !agentId || !conversationId) {
      setEvents([]);
      setLoading(false);
      setError(null);
      pollNowRef.current = () => {};
      return;
    }

    const exoClient = client;
    const agent = agentId;
    const conversation = conversationId;

    let cancelled = false;
    let running = false;
    let timer: number | undefined;
    let cursor: EventId | null = null;
    let emptyStreak = 0;
    const seen = new Set<string>();

    setEvents([]);
    setError(null);
    setLoading(true);

    // Pull every page newer than the cursor; returns whether anything landed.
    async function drain(): Promise<boolean> {
      let added = false;
      for (;;) {
        const page = await exoClient.getEventsPage(agent, conversation, cursor);
        if (cancelled) {
          return added;
        }
        if (page.events.length === 0) {
          break;
        }
        const { fresh, cursor: pageCursor } = processEventPage(
          page.events,
          seen,
        );
        if (pageCursor) {
          cursor = pageCursor;
        }
        if (fresh.length > 0) {
          setEvents((previous) => previous.concat(fresh));
          added = true;
        }
        if (page.events.length < EVENT_PAGE_SIZE) {
          break;
        }
      }
      return added;
    }

    function schedule(delay: number) {
      if (cancelled) {
        return;
      }
      window.clearTimeout(timer);
      timer = window.setTimeout(() => void run(), delay);
    }

    async function run() {
      if (running) {
        return;
      }
      running = true;
      try {
        const added = await drain();
        if (cancelled) {
          return;
        }
        setError(null);
        if (turnPendingRef.current) {
          emptyStreak = 0;
          schedule(ACTIVE_INTERVAL_MS);
        } else if (added) {
          emptyStreak = 0;
          schedule(IDLE_BASE_INTERVAL_MS);
        } else {
          emptyStreak += 1;
          schedule(
            Math.min(
              IDLE_BASE_INTERVAL_MS * IDLE_BACKOFF ** emptyStreak,
              IDLE_MAX_INTERVAL_MS,
            ),
          );
        }
      } catch (caught) {
        if (cancelled) {
          return;
        }
        setError(caught instanceof Error ? caught.message : String(caught));
        schedule(IDLE_MAX_INTERVAL_MS);
      } finally {
        running = false;
      }
    }

    pollNowRef.current = () => {
      if (!cancelled) {
        void run();
      }
    };

    void (async () => {
      running = true;
      try {
        await drain();
        if (cancelled) {
          return;
        }
        setLoading(false);
      } catch (caught) {
        if (cancelled) {
          return;
        }
        setLoading(false);
        setError(caught instanceof Error ? caught.message : String(caught));
      } finally {
        running = false;
      }
      schedule(
        turnPendingRef.current ? ACTIVE_INTERVAL_MS : IDLE_BASE_INTERVAL_MS,
      );
    })();

    return () => {
      cancelled = true;
      window.clearTimeout(timer);
      pollNowRef.current = () => {};
    };
  }, [client, agentId, conversationId, reloadKey]);

  // A starting turn should wake an idle poller into its fast cadence at once.
  useEffect(() => {
    if (turnPending) {
      poll();
    }
  }, [turnPending, poll]);

  return { events, loading, error, poll };
}

export function processEventPage(
  pageEvents: Event[],
  seen: Set<string>,
): { fresh: Event[]; cursor: EventId | null } {
  if (pageEvents.length === 0) {
    return { fresh: [], cursor: null };
  }
  const fresh = pageEvents.filter((event) => !seen.has(event.id));
  const cursor = pageEvents[pageEvents.length - 1]!.id;
  for (const event of fresh) {
    seen.add(event.id);
  }
  return { fresh, cursor };
}
