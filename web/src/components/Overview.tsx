import { useEffect, useMemo, useState } from "react";
import type { ExoClient } from "../api/exoClient";
import type { AgentRecord, ConversationHandleInfo } from "../api/protocol";
import {
  deriveConversationCardStats,
  sortOverviewEntries,
  type LoadedConversationEvents,
} from "../lib/overviewStats";
import { formatRecency, shortId } from "../lib/rendering";
import { SkeletonRows } from "./SkeletonRows";

export type { LoadedConversationEvents };

export interface OverviewProps {
  client: ExoClient | null;
  agents: AgentRecord[];
  refreshToken: number;
  loadedEvents: LoadedConversationEvents | null;
  onSelectConversation: (agentId: string, conversationId: string) => void;
}

interface OverviewEntry {
  agent: AgentRecord;
  conversation: ConversationHandleInfo;
}

export function Overview({
  client,
  agents,
  refreshToken,
  loadedEvents,
  onSelectConversation,
}: OverviewProps) {
  const [entries, setEntries] = useState<OverviewEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!client || agents.length === 0) {
      setEntries([]);
      setLoading(false);
      setError(null);
      return;
    }

    const exoClient = client;
    let cancelled = false;
    setLoading(true);
    setError(null);

    async function loadOverview() {
      const results = await Promise.allSettled(
        agents.map(async (agent) => {
          const conversations = await exoClient.listConversations(agent.id);
          return conversations.map(
            (conversation): OverviewEntry => ({ agent, conversation }),
          );
        }),
      );

      if (cancelled) {
        return;
      }

      const nextEntries: OverviewEntry[] = [];
      const failures: string[] = [];

      for (const result of results) {
        if (result.status === "fulfilled") {
          nextEntries.push(...result.value);
          continue;
        }
        failures.push(
          result.reason instanceof Error
            ? result.reason.message
            : String(result.reason),
        );
      }

      setEntries(nextEntries);
      setError(failures.length > 0 ? failures[0] : null);
      setLoading(false);
    }

    void loadOverview();

    return () => {
      cancelled = true;
    };
  }, [agents, client, refreshToken]);

  const sortedEntries = useMemo(
    () => sortOverviewEntries(entries, loadedEvents),
    [entries, loadedEvents],
  );

  return (
    <div className="overview-shell">
      <div className="overview-inner">
        <header className="overview-header">
          <div>
            <h1>Activity overview</h1>
            <p>
              {loading
                ? "Loading conversations…"
                : `${sortedEntries.length} conversation${sortedEntries.length === 1 ? "" : "s"} across ${agents.length} agent${agents.length === 1 ? "" : "s"}`}
            </p>
          </div>
        </header>

        {error ? <div className="error-banner">{error}</div> : null}

        {loading ? (
          <div className="overview-grid overview-grid-loading">
            {Array.from({ length: 6 }, (_, index) => (
              <div className="overview-card overview-card-skeleton" key={index}>
                <SkeletonRows className="overview-skeleton" count={4} />
              </div>
            ))}
          </div>
        ) : sortedEntries.length === 0 ? (
          <div className="overview-empty">
            {agents.length === 0 ? "No agents." : "No conversations yet."}
          </div>
        ) : (
          <div className="overview-grid">
            {sortedEntries.map(({ agent, conversation }) => {
              const record = conversation.record;
              const { eventCount, turnCount, lastActivity } =
                deriveConversationCardStats(agent.id, record.id, loadedEvents);
              const latestEventId = record.latest_event_id;

              return (
                <button
                  className="overview-card"
                  key={`${agent.id}:${record.id}`}
                  onClick={() => onSelectConversation(agent.id, record.id)}
                  type="button"
                >
                  <div className="overview-card-head">
                    <strong>
                      {record.name || record.slug || shortId(record.id)}
                    </strong>
                    <span className="overview-recency">
                      {lastActivity
                        ? formatRecency(lastActivity)
                        : latestEventId
                          ? shortId(latestEventId)
                          : "idle"}
                    </span>
                  </div>
                  <div className="overview-card-agent">
                    <span>{agent.name || agent.slug || shortId(agent.id)}</span>
                    <code>{agent.slug || shortId(agent.id)}</code>
                  </div>
                  <dl className="overview-meta">
                    <div>
                      <dt>Slug</dt>
                      <dd>{record.slug || shortId(record.id)}</dd>
                    </div>
                    <div>
                      <dt>Latest event</dt>
                      <dd>{latestEventId ? shortId(latestEventId) : "none"}</dd>
                    </div>
                    {eventCount != null ? (
                      <div>
                        <dt>Events</dt>
                        <dd>{eventCount.toLocaleString()}</dd>
                      </div>
                    ) : null}
                    {turnCount != null && turnCount > 0 ? (
                      <div>
                        <dt>Turns</dt>
                        <dd>{turnCount.toLocaleString()}</dd>
                      </div>
                    ) : null}
                    {lastActivity ? (
                      <div>
                        <dt>Last activity</dt>
                        <dd>
                          <time dateTime={lastActivity}>
                            {formatRecency(lastActivity)}
                          </time>
                        </dd>
                      </div>
                    ) : null}
                  </dl>
                </button>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
