import { useEffect, useRef, useState } from "react";
import type { ExoClient } from "../api/exoClient";
import type { ConversationHandleInfo } from "../api/protocol";

// How many turn-count requests to keep in flight at once. The list info is a
// nicety, so it loads gently in the background rather than bursting one request
// per conversation the moment the list renders.
const CONCURRENCY = 4;

export type ConversationTurnCounts = Map<string, number>;

// Lazily fetch each conversation's turn count and cache it across renders and
// agent switches. A conversation is counted once: a ref tracks which keys have
// been scheduled, so reopening the list or a state update never refetches.
export function useConversationStats(
  client: ExoClient | null,
  agentId: string | null,
  conversations: ConversationHandleInfo[],
): ConversationTurnCounts {
  const [counts, setCounts] = useState<ConversationTurnCounts>(new Map());
  // Keys (`${agentId}:${conversationId}`) already counted or in flight.
  const scheduledRef = useRef<Set<string>>(new Set());

  useEffect(() => {
    if (!client || !agentId || conversations.length === 0) {
      return;
    }

    let cancelled = false;
    const exoClient = client;
    const agent = agentId;
    const scheduled = scheduledRef.current;

    // Empty conversations have no events, so skip the request and record 0.
    const pending: string[] = [];
    const immediate: Array<[string, number]> = [];
    for (const conversation of conversations) {
      const id = conversation.record.id;
      const key = `${agent}:${id}`;
      if (scheduled.has(key)) {
        continue;
      }
      scheduled.add(key);
      if (conversation.record.latest_event_id) {
        pending.push(id);
      } else {
        immediate.push([key, 0]);
      }
    }

    if (immediate.length > 0) {
      setCounts((previous) => {
        const next = new Map(previous);
        for (const [key, value] of immediate) {
          next.set(key, value);
        }
        return next;
      });
    }

    if (pending.length === 0) {
      return;
    }

    let cursor = 0;
    async function worker() {
      for (;;) {
        const conversationId = pending[cursor];
        cursor += 1;
        if (conversationId === undefined) {
          return;
        }
        const key = `${agent}:${conversationId}`;
        try {
          const count = await exoClient.countConversationTurns(
            agent,
            conversationId,
          );
          if (cancelled) {
            return;
          }
          setCounts((previous) => new Map(previous).set(key, count));
        } catch {
          // A failed count is non-fatal; let it be retried on a later mount.
          scheduled.delete(key);
        }
      }
    }

    void Promise.all(
      Array.from({ length: Math.min(CONCURRENCY, pending.length) }, () =>
        worker(),
      ),
    );

    return () => {
      cancelled = true;
    };
  }, [client, agentId, conversations]);

  return counts;
}
