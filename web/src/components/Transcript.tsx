import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type FormEvent,
  type KeyboardEvent,
  type ReactNode,
  type RefObject,
} from "react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";
import type { ExoClient } from "../api/exoClient";
import type {
  ConversationHandleInfo,
  Event,
  JsonValue,
  LinguaMessage,
  UsageRecord,
} from "../api/protocol";
import { formatJson, formatTime, shortId } from "../lib/rendering";
import {
  computeConversationRollup,
  formatRollupChips,
} from "../lib/conversationStats";
import {
  downloadConversationJson,
  downloadConversationMarkdown,
} from "../lib/exportConversation";
import {
  ArtifactContext,
  ArtifactView,
  ArtifactWrittenCard,
  findArtifactRef,
} from "./ArtifactViewer";
import { CopyButton } from "./CopyButton";
import {
  EventDetailDrawer,
  type EventDetailFocus,
  type EventDetailSelection,
} from "./EventDetailDrawer";
import { JsonPreview } from "./JsonPreview";
import { MarkdownContent } from "./Markdown";
import { SkeletonRows } from "./SkeletonRows";

// Render extra viewport height above/below the visible window so fast scrolling
// and find jumps land on already-mounted rows instead of blank space.
const VIEWPORT_OVERSCAN_PX = 800;
// Spacing between timeline rows, applied as item top padding (matches the gap
// the static grid used). Padding — not margin — keeps virtuoso's measurements
// stable (margins collapse and confuse its size observer).
const ROW_GAP_PX = 24;

interface TranscriptProps {
  conversation: ConversationHandleInfo | null;
  events: Event[];
  loading: boolean;
  error: string | null;
  agentLabel: string;
  canChat: boolean;
  selectionKey: string;
  // A turn is running server-side; events stream in while it does.
  turnCancelPending: boolean;
  turnElapsedSeconds: number;
  turnPending: boolean;
  // Read seam for the artifact viewer (read-only, may be null before selection).
  client: ExoClient | null;
  agentId: string | null;
  conversationId: string | null;
  onCancelChatTurn: () => Promise<void>;
  onSendChatMessage: (message: string) => Promise<void>;
}

interface ToolResultRecord {
  event: Event;
  output: unknown;
  raw: unknown;
  toolCallId: string;
  toolName: string | null;
}

type ShowEventDetails = (event: Event, focus?: EventDetailFocus) => void;

export function Transcript({
  conversation,
  events,
  loading,
  error,
  agentLabel,
  canChat,
  selectionKey,
  turnCancelPending,
  turnElapsedSeconds,
  turnPending,
  client,
  agentId,
  conversationId,
  onCancelChatTurn,
  onSendChatMessage,
}: TranscriptProps) {
  const [showSystemEvents, setShowSystemEvents] = useState(false);
  const [showMessages, setShowMessages] = useState(true);
  const [showTools, setShowTools] = useState(true);
  const [showArtifacts, setShowArtifacts] = useState(true);
  const [pending, setPending] = useState<string | null>(null);
  const [sendError, setSendError] = useState<string | null>(null);
  const [sendStatus, setSendStatus] = useState<string | null>(null);
  const [showJumpToLatest, setShowJumpToLatest] = useState(false);
  const [liveAnnouncement, setLiveAnnouncement] = useState("");
  const [findQuery, setFindQuery] = useState("");
  const [detailSelection, setDetailSelection] =
    useState<EventDetailSelection | null>(null);

  const scrollRef = useRef<HTMLDivElement | null>(null);
  // The scroll element is also virtuoso's customScrollParent, so it has to live
  // in state — virtuoso needs the resolved node, not just a ref box.
  const [scrollEl, setScrollEl] = useState<HTMLDivElement | null>(null);
  const setScrollNode = useCallback((node: HTMLDivElement | null) => {
    scrollRef.current = node;
    setScrollEl(node);
  }, []);
  const timelineRef = useRef<HTMLDivElement | null>(null);
  const virtuosoRef = useRef<VirtuosoHandle | null>(null);
  const rowCountRef = useRef(0);
  const cancelRequestedRef = useRef(false);
  const prevTurnPendingRef = useRef(false);
  // Stick to the newest message unless the reader has scrolled up to read history.
  const stickRef = useRef(true);
  const lastScrollTopRef = useRef(0);

  // Pin to the bottom now and across the next two frames, so we still land on the
  // true end after markdown, avatars, and a tall reply finish laying out. With the
  // list virtualized, also ask virtuoso to render+measure the final row first so
  // scrollHeight reflects the real end rather than an estimate.
  function scrollToBottom() {
    const pin = () => {
      const el = scrollRef.current;
      if (el) {
        el.scrollTop = el.scrollHeight;
        lastScrollTopRef.current = el.scrollTop;
      }
    };
    const lastIndex = rowCountRef.current - 1;
    if (lastIndex >= 0) {
      virtuosoRef.current?.scrollToIndex({ index: lastIndex, align: "end" });
    }
    pin();
    requestAnimationFrame(() => {
      pin();
      requestAnimationFrame(pin);
    });
  }

  // Virtuoso owns stick-to-bottom natively (followOutput) and reports the
  // at-bottom state directly. We just mirror it into stickRef + the jump pill.
  // The old approach re-pinned scrollTop on every measured height change, which
  // fought virtuoso's own scroll anchoring and made scrolling up flicker/jump.
  const handleAtBottomChange = useCallback((atBottom: boolean) => {
    stickRef.current = atBottom;
    setShowJumpToLatest(!atBottom);
  }, []);

  const artifactLoader = useMemo(() => {
    if (!client || !agentId || !conversationId) {
      return null;
    }
    return {
      load: (artifactId: string, version?: number | null) =>
        client.readConversationArtifact(
          agentId,
          conversationId,
          artifactId,
          version,
        ),
    };
  }, [client, agentId, conversationId]);

  // The optimistic bubble is a stand-in for the user's message until the real
  // stored event arrives over the live poll; once it does, hand off to it.
  useEffect(() => {
    if (pending == null) {
      return;
    }
    const target = pending.trim();
    const landed = events.some(
      (event) =>
        event.data.type === "messages" &&
        event.data.messages.some(
          (message) =>
            typeof message.role === "string" &&
            message.role === "user" &&
            renderTextContent(
              "content" in message ? message.content : null,
            ).trim() === target,
        ),
    );
    if (landed) {
      setPending(null);
    }
  }, [events, pending]);

  // Switching conversations resets stick + clears any open detail.
  useEffect(() => {
    stickRef.current = true;
    setDetailSelection(null);
  }, [conversation?.record.id]);

  const showEventDetails = useCallback<ShowEventDetails>((event, focus) => {
    setDetailSelection({ event, focus });
  }, []);

  const closeEventDetails = useCallback(() => {
    setDetailSelection(null);
  }, []);

  const orderedEvents = useMemo(
    () => [...events].sort(compareEvents),
    [events],
  );
  // Pair tool calls with their results across the WHOLE history (the list is
  // virtualized, so there is no longer a windowed slice to scope this to).
  const toolCallIds = useMemo(
    () => collectToolCallIds(orderedEvents),
    [orderedEvents],
  );
  const toolResults = useMemo(
    () => buildToolResultIndex(orderedEvents),
    [orderedEvents],
  );

  // Flatten the ordered events into the exact set of logical rows the timeline
  // renders, applying the same filters the static list used. Each row carries a
  // plain-text projection so find can locate matches in rows that aren't mounted.
  const rows = useMemo<TimelineRowItem[]>(
    () =>
      buildTimelineRows(orderedEvents, {
        showArtifacts,
        showMessages,
        showSystemEvents,
        showTools,
        toolCallIds,
        toolResults,
      }),
    [
      orderedEvents,
      showArtifacts,
      showMessages,
      showSystemEvents,
      showTools,
      toolCallIds,
      toolResults,
    ],
  );
  rowCountRef.current = rows.length;

  // Land at the newest message once a conversation's events have loaded. Virtuoso
  // starts at the top and events arrive asynchronously, so we scroll the first
  // time rows appear for a conversation (once per conversation — live appends are
  // handled by followOutput). This is what makes opening the app land at the bottom.
  const scrolledConvRef = useRef<string | null>(null);
  useEffect(() => {
    const convId = conversation?.record.id ?? null;
    if (!convId) {
      scrolledConvRef.current = null;
      return;
    }
    if (rows.length > 0 && scrolledConvRef.current !== convId) {
      scrolledConvRef.current = convId;
      scrollToBottom();
    }
  }, [conversation?.record.id, rows.length]);

  const rollup = useMemo(() => computeConversationRollup(events), [events]);
  const rollupChips = useMemo(() => formatRollupChips(rollup), [rollup]);
  const exportStem = useMemo(
    () =>
      sanitizeFilename(
        conversation?.record.slug ||
          conversation?.record.name ||
          "conversation",
      ),
    [conversation],
  );

  const scrollToRow = useCallback((rowIndex: number) => {
    virtuosoRef.current?.scrollToIndex({ index: rowIndex, align: "center" });
  }, []);

  const find = useTranscriptFind({
    containerRef: timelineRef,
    query: findQuery,
    rows,
    scrollToRow,
  });

  function handleFindKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === "Enter") {
      event.preventDefault();
      if (event.shiftKey) {
        find.prev();
      } else {
        find.next();
      }
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      setFindQuery("");
      event.currentTarget.blur();
    }
  }

  useEffect(() => {
    if (prevTurnPendingRef.current && !turnPending) {
      setLiveAnnouncement("Reply complete");
    }
    prevTurnPendingRef.current = turnPending;
  }, [turnPending]);

  // New events are followed by virtuoso (followOutput). The optimistic bubble and
  // typing indicator render BELOW the virtualized list, so when they appear while
  // stuck to the bottom, nudge the scroll parent down to reveal them.
  useEffect(() => {
    if (stickRef.current && (pending != null || turnPending)) {
      const el = scrollRef.current;
      if (el) {
        el.scrollTop = el.scrollHeight;
      }
    }
  }, [pending, turnPending]);

  function jumpToLatest() {
    stickRef.current = true;
    scrollToBottom();
    setShowJumpToLatest(false);
  }

  async function handleSubmit(message: string) {
    cancelRequestedRef.current = false;
    setSendError(null);
    setSendStatus(null);
    setPending(message);
    stickRef.current = true;
    scrollToBottom();
    try {
      await onSendChatMessage(message);
    } catch (submitError) {
      setPending(null);
      if (cancelRequestedRef.current) {
        cancelRequestedRef.current = false;
        setSendStatus("Stopped.");
        return;
      }
      setSendError(
        submitError instanceof Error
          ? submitError.message
          : String(submitError),
      );
      throw submitError;
    }
  }

  async function handleCancelTurn() {
    cancelRequestedRef.current = true;
    setSendError(null);
    setSendStatus(null);
    try {
      await onCancelChatTurn();
      setPending(null);
      setSendStatus("Stopped.");
    } catch (cancelError) {
      cancelRequestedRef.current = false;
      setSendError(
        cancelError instanceof Error
          ? cancelError.message
          : String(cancelError),
      );
    }
  }

  // Render a single logical row on demand. Virtuoso only calls this for rows in
  // (or near) the viewport, so unmounted history costs nothing to display.
  const renderRow = useCallback(
    (index: number, row: TimelineRowItem) => (
      <div
        data-find-row={index}
        style={{ paddingTop: index === 0 ? 0 : ROW_GAP_PX }}
      >
        {row.event.data.type === "artifact_written" ? (
          <ArtifactWrittenCard
            artifactId={row.event.data.artifact_id}
            createdAt={row.event.created_at}
            path={row.event.data.path}
            version={row.event.data.version}
          />
        ) : row.kind === "system" ? (
          <SystemDivider event={row.event} />
        ) : (
          <ConversationEvent
            agentLabel={agentLabel}
            event={row.event}
            onShowDetails={showEventDetails}
            showSystemEvents={showSystemEvents}
            showTools={showTools}
            toolCallIds={toolCallIds}
            toolResults={toolResults}
          />
        )}
      </div>
    ),
    [
      agentLabel,
      showEventDetails,
      showSystemEvents,
      showTools,
      toolCallIds,
      toolResults,
    ],
  );

  const hasRows = rows.length > 0;

  return (
    <ArtifactContext.Provider value={artifactLoader}>
      <main className="main-panel" aria-label="Conversation transcript">
        <div className="transcript-scroll" ref={setScrollNode}>
          <div className="main-inner">
            {error ? <div className="error-banner">{error}</div> : null}

            {conversation ? (
              <header className="conversation-header">
                <div className="conversation-header-main">
                  <h1>
                    {conversation.record.name ||
                      conversation.record.slug ||
                      shortId(conversation.record.id)}
                  </h1>
                  {rollupChips.length > 0 ? (
                    <div
                      className="conversation-rollup"
                      aria-label="Conversation totals"
                    >
                      {rollupChips.map((chip) => (
                        <span className="metric-chip" key={chip}>
                          {chip}
                        </span>
                      ))}
                    </div>
                  ) : null}
                </div>
                <div className="conversation-header-actions">
                  <button
                    className="text-button"
                    disabled={events.length === 0}
                    onClick={() => downloadConversationJson(events, exportStem)}
                    type="button"
                  >
                    export json
                  </button>
                  <button
                    className="text-button"
                    disabled={events.length === 0}
                    onClick={() =>
                      downloadConversationMarkdown(events, exportStem)
                    }
                    type="button"
                  >
                    export md
                  </button>
                </div>
              </header>
            ) : null}

            <div className="timeline-toolbar">
              <div className="timeline-status">
                <span>
                  {loading
                    ? "loading events"
                    : `showing ${rows.length.toLocaleString()} of ${orderedEvents.length.toLocaleString()}`}
                </span>
                {conversation?.record.latest_event_id ? (
                  <span>
                    head {shortId(conversation.record.latest_event_id)}
                  </span>
                ) : null}
              </div>
              <div className="timeline-tools">
                <TranscriptSearchBox
                  activeIndex={find.activeIndex}
                  matchCount={find.matchCount}
                  onChange={setFindQuery}
                  onClear={() => setFindQuery("")}
                  onKeyDown={handleFindKeyDown}
                  onNext={find.next}
                  onPrev={find.prev}
                  query={findQuery}
                />
                <div
                  className="filter-chips"
                  role="group"
                  aria-label="Filter timeline by type"
                >
                  <FilterChip
                    active={showMessages}
                    label="messages"
                    onToggle={() => setShowMessages((value) => !value)}
                  />
                  <FilterChip
                    active={showTools}
                    label="tools"
                    onToggle={() => setShowTools((value) => !value)}
                  />
                  <FilterChip
                    active={showArtifacts}
                    label="artifacts"
                    onToggle={() => setShowArtifacts((value) => !value)}
                  />
                  <FilterChip
                    active={showSystemEvents}
                    label="system"
                    onToggle={() => setShowSystemEvents((value) => !value)}
                  />
                </div>
              </div>
            </div>

            <div
              className="timeline"
              ref={timelineRef}
              role="log"
              aria-live="polite"
              aria-relevant="additions text"
            >
              {!conversation ? (
                <EmptyState title="No conversation selected" />
              ) : null}
              {conversation && loading && events.length === 0 ? (
                <EventLoadingSkeleton />
              ) : null}
              {conversation && !loading && events.length === 0 && !pending ? (
                <EmptyState title="No events recorded" />
              ) : null}
              {hasRows && scrollEl ? (
                <Virtuoso
                  key={conversation?.record.id ?? "none"}
                  ref={virtuosoRef}
                  customScrollParent={scrollEl}
                  data={rows}
                  computeItemKey={(_index, row) => row.key}
                  itemContent={renderRow}
                  increaseViewportBy={VIEWPORT_OVERSCAN_PX}
                  initialTopMostItemIndex={Math.max(0, rows.length - 1)}
                  rangeChanged={find.refresh}
                  followOutput={(isAtBottom) => (isAtBottom ? "auto" : false)}
                  atBottomThreshold={80}
                  atBottomStateChange={handleAtBottomChange}
                />
              ) : null}
              {pending != null || turnPending ? (
                <div className="timeline-live-rows">
                  {pending != null ? <PendingMessage text={pending} /> : null}
                  {turnPending || pending != null ? (
                    <TypingIndicator agentLabel={agentLabel} />
                  ) : null}
                </div>
              ) : null}
            </div>
            <div className="sr-only" aria-live="polite">
              {liveAnnouncement}
            </div>
          </div>
          {showJumpToLatest ? (
            <button
              className="jump-to-latest"
              onClick={jumpToLatest}
              type="button"
            >
              jump to latest
            </button>
          ) : null}
        </div>
        <ChatComposer
          agentLabel={agentLabel}
          cancelPending={turnCancelPending}
          canChat={canChat}
          error={sendError}
          running={turnPending || pending != null}
          selectionKey={selectionKey}
          status={sendStatus}
          turnElapsedSeconds={turnElapsedSeconds}
          onCancel={handleCancelTurn}
          onSubmit={handleSubmit}
        />
        <EventDetailDrawer
          onClose={closeEventDetails}
          selection={detailSelection}
        />
      </main>
    </ArtifactContext.Provider>
  );
}

function PendingMessage({ text }: { text: string }) {
  // The message is posted the instant the turn-runner starts, so it reads as
  // already sent. What the reader is actually waiting on is the agent's reply —
  // that is what the typing indicator below this bubble represents.
  return (
    <article className="message-block role-user message-pending">
      <div className="bubble">
        <MarkdownContent text={text} />
      </div>
    </article>
  );
}

function TypingIndicator({ agentLabel }: { agentLabel: string }) {
  return (
    <div
      className="message-block role-assistant typing-row"
      aria-label={`${agentLabel || "agent"} is replying`}
    >
      <div className="message-header">
        <span className="message-identity">
          <RoleAvatar label={agentLabel || "assistant"} role="assistant" />
          <span className="role-label">{agentLabel || "assistant"}</span>
        </span>
      </div>
      <div className="typing-indicator" aria-hidden="true">
        <span />
        <span />
        <span />
      </div>
    </div>
  );
}

function ChatComposer({
  agentLabel,
  cancelPending,
  canChat,
  error,
  running,
  selectionKey,
  status,
  turnElapsedSeconds,
  onCancel,
  onSubmit,
}: {
  agentLabel: string;
  cancelPending: boolean;
  canChat: boolean;
  error: string | null;
  running: boolean;
  selectionKey: string;
  status: string | null;
  turnElapsedSeconds: number;
  onCancel: () => Promise<void>;
  onSubmit: (message: string) => Promise<void>;
}) {
  const [draft, setDraft] = useState("");
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const trimmedDraft = draft.trim();
  const disabled = !canChat || running;
  const placeholder = canChat
    ? `Message ${agentLabel || "agent"}`
    : "Select a conversation to chat";

  useEffect(() => {
    setDraft("");
  }, [selectionKey]);

  useEffect(() => {
    if (canChat) {
      textareaRef.current?.focus();
    }
  }, [canChat, selectionKey]);

  useEffect(() => {
    if (!running && canChat) {
      textareaRef.current?.focus();
    }
  }, [canChat, running]);

  // Grow the input with its content instead of starting as a tall fixed box.
  useEffect(() => {
    const el = textareaRef.current;
    if (!el) {
      return;
    }
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 180)}px`;
  }, [draft]);

  function submitMessage(event?: FormEvent<HTMLFormElement>) {
    event?.preventDefault();
    if (disabled || !trimmedDraft) {
      return;
    }
    const message = draft;
    // Clear instantly so the box never sits full while the turn runs; the parent
    // shows the optimistic bubble and the "taking a turn" state.
    setDraft("");
    void onSubmit(message).catch(() => {
      setDraft(message);
    });
  }

  function cancelTurn() {
    if (!running || cancelPending) {
      return;
    }
    void onCancel();
  }

  function handleKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      submitMessage();
    }
  }

  return (
    <section className="chat-composer-shell" aria-label="Chat composer">
      <form className="chat-composer" onSubmit={submitMessage}>
        <div className="chat-input-wrap">
          <label className="sr-only" htmlFor="chat-message">
            Message
          </label>
          <textarea
            aria-describedby="chat-composer-status"
            disabled={disabled}
            id="chat-message"
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={placeholder}
            ref={textareaRef}
            rows={1}
            value={draft}
          />
        </div>
        {running ? (
          <button disabled={cancelPending} onClick={cancelTurn} type="button">
            {cancelPending ? "Stopping" : "Stop"}
          </button>
        ) : (
          <button disabled={disabled || !trimmedDraft} type="submit">
            Send
          </button>
        )}
      </form>
      <div
        className="chat-composer-status"
        id="chat-composer-status"
        aria-live="polite"
      >
        {running ? (
          <span>
            {formatRunningStatus(
              agentLabel || "agent",
              turnElapsedSeconds,
              cancelPending,
            )}
          </span>
        ) : null}
        {!running && error ? <span className="chat-error">{error}</span> : null}
        {!running && !error && status ? <span>{status}</span> : null}
        {!running && !error && !canChat ? (
          <span>Select a conversation to send a message.</span>
        ) : null}
      </div>
    </section>
  );
}

function formatRunningStatus(
  agentLabel: string,
  elapsedSeconds: number,
  cancelPending: boolean,
): string {
  if (cancelPending) {
    return "Stopping turn…";
  }
  if (elapsedSeconds >= 240) {
    return `${agentLabel} is still working… (${elapsedSeconds}s); timeout is near`;
  }
  if (elapsedSeconds >= 10) {
    return `${agentLabel} is still working… (${elapsedSeconds}s)`;
  }
  return `${agentLabel} is taking a turn…`;
}

function ConversationEvent({
  agentLabel,
  event,
  onShowDetails,
  showSystemEvents,
  showTools,
  toolCallIds,
  toolResults,
}: {
  agentLabel: string;
  event: Event;
  onShowDetails: ShowEventDetails;
  showSystemEvents: boolean;
  showTools: boolean;
  toolCallIds: Set<string>;
  toolResults: Map<string, ToolResultRecord[]>;
}) {
  const data = event.data;

  if (data.type === "messages") {
    return (
      <MessagesEvent
        agentLabel={agentLabel}
        event={event}
        messages={data.messages}
        onShowDetails={onShowDetails}
        showSystemEvents={showSystemEvents}
        showTools={showTools}
        toolCallIds={toolCallIds}
        toolResults={toolResults}
        usage={data.usage}
      />
    );
  }

  if (data.type === "tool_requested") {
    return (
      <section
        className="conversation-event tool-thread"
        id={`event-${event.id}`}
      >
        <ToolCallRow
          event={event}
          onShowDetails={onShowDetails}
          raw={data}
          results={toolResults.get(data.tool_call_id) ?? []}
          toolArguments={data.request.arguments}
          toolCallId={data.tool_call_id}
          toolName={data.request.function_name}
        />
        <RawDisclosure value={event} />
      </section>
    );
  }

  if (data.type === "tool_result") {
    if (toolCallIds.has(data.tool_call_id)) {
      return null;
    }
    return (
      <section
        className="conversation-event tool-thread"
        id={`event-${event.id}`}
      >
        <OrphanToolResult
          event={event}
          onShowDetails={onShowDetails}
          output={data.result}
          raw={data}
          toolCallId={data.tool_call_id}
        />
        <RawDisclosure value={event} />
      </section>
    );
  }

  return null;
}

function MessagesEvent({
  agentLabel,
  event,
  messages,
  onShowDetails,
  showSystemEvents,
  showTools,
  toolCallIds,
  toolResults,
  usage,
}: {
  agentLabel: string;
  event: Event;
  messages: LinguaMessage[];
  onShowDetails: ShowEventDetails;
  showSystemEvents: boolean;
  showTools: boolean;
  toolCallIds: Set<string>;
  toolResults: Map<string, ToolResultRecord[]>;
  usage: UsageRecord | null | undefined;
}) {
  // "raw event" in the per-message actions reveals this single shared disclosure
  // rather than minting another copy of the same JSON.
  const [rawOpen, setRawOpen] = useState(false);
  const rawId = `raw-${event.id}`;

  function showRaw() {
    setRawOpen(true);
    requestAnimationFrame(() => {
      document.getElementById(rawId)?.scrollIntoView({ block: "nearest" });
    });
  }

  const rendered = messages
    .map((message, index) => (
      <MessageBlock
        agentLabel={agentLabel}
        event={event}
        key={`${event.id}-${index}`}
        message={message}
        onShowDetails={onShowDetails}
        onShowRaw={showRaw}
        showSystemEvents={showSystemEvents}
        showTools={showTools}
        toolCallIds={toolCallIds}
        toolResults={toolResults}
        usage={usage}
      />
    ))
    .filter(Boolean);

  if (rendered.length === 0) {
    return null;
  }

  return (
    <section className="conversation-event" id={`event-${event.id}`}>
      {rendered}
      <RawDisclosure
        id={rawId}
        onToggle={setRawOpen}
        open={rawOpen}
        value={event}
      />
    </section>
  );
}

function MessageBlock({
  agentLabel,
  event,
  message,
  onShowDetails,
  onShowRaw,
  showSystemEvents,
  showTools,
  toolCallIds,
  toolResults,
  usage,
}: {
  agentLabel: string;
  event: Event;
  message: LinguaMessage;
  onShowDetails: ShowEventDetails;
  onShowRaw: () => void;
  showSystemEvents: boolean;
  showTools: boolean;
  toolCallIds: Set<string>;
  toolResults: Map<string, ToolResultRecord[]>;
  usage: UsageRecord | null | undefined;
}) {
  const role = typeof message.role === "string" ? message.role : "message";
  const content = "content" in message ? message.content : null;

  if (role === "user") {
    const text = renderTextContent(content);
    return (
      <article className="message-block role-user">
        <MessageActions
          markdown={text}
          eventId={event.id}
          onShowDetails={() => onShowDetails(event)}
          onShowRaw={onShowRaw}
        />
        <div className="bubble">
          <MarkdownContent text={text} />
        </div>
        <time className="bubble-time">{formatTime(event.created_at)}</time>
      </article>
    );
  }

  if (role === "assistant") {
    const text = renderAssistantPlainText(content);
    return (
      <article className="message-block role-assistant">
        <MessageActions
          markdown={text}
          eventId={event.id}
          onShowDetails={() => onShowDetails(event)}
          onShowRaw={onShowRaw}
        />
        <MessageHeader
          label={agentLabel || "assistant"}
          role="assistant"
          time={event.created_at}
        />
        <AssistantContentView
          content={content}
          event={event}
          onShowDetails={onShowDetails}
          showTools={showTools}
          toolResults={toolResults}
        />
        <AssistantMetadataFooter usage={usage} />
      </article>
    );
  }

  if (role === "tool") {
    if (!showTools) {
      return null;
    }
    const rows = renderToolMessageContent(
      content,
      event,
      toolCallIds,
      onShowDetails,
    );
    if (rows.length === 0) {
      return null;
    }
    return <div className="message-block role-tool">{rows}</div>;
  }

  if (!showSystemEvents) {
    return null;
  }

  return (
    <SystemInlineMessage
      event={event}
      label={`${role} message`}
      onShowDetails={onShowDetails}
      raw={message}
      text={renderTextContent(content)}
    />
  );
}

function AssistantContentView({
  content,
  event,
  onShowDetails,
  showTools,
  toolResults,
}: {
  content: unknown;
  event: Event;
  onShowDetails: ShowEventDetails;
  showTools: boolean;
  toolResults: Map<string, ToolResultRecord[]>;
}) {
  if (typeof content === "string") {
    return <AssistantText text={content} />;
  }

  if (!isContentPartArray(content)) {
    return <MutedLine>unsupported assistant content</MutedLine>;
  }

  return (
    <div className="assistant-flow">
      {content.map((part, index) => {
        if (part.type === "text" && typeof part.text === "string") {
          return <AssistantText key={index} text={part.text} />;
        }

        if (part.type === "reasoning" && typeof part.text === "string") {
          return (
            <details className="thinking-disclosure" key={index}>
              <summary>thinking</summary>
              <MarkdownContent text={part.text} />
            </details>
          );
        }

        // Tool activity embedded in the assistant turn answers to the tools
        // filter just like standalone tool events do.
        if (
          !showTools &&
          (part.type === "tool_call" || part.type === "tool_result")
        ) {
          return null;
        }

        if (part.type === "tool_call") {
          const toolCallId =
            typeof part.tool_call_id === "string" ? part.tool_call_id : null;
          const toolName =
            typeof part.tool_name === "string" ? part.tool_name : "tool";
          return (
            <ToolCallRow
              event={event}
              key={index}
              onShowDetails={onShowDetails}
              raw={part}
              results={toolCallId ? (toolResults.get(toolCallId) ?? []) : []}
              toolArguments={"arguments" in part ? part.arguments : null}
              toolCallId={toolCallId}
              toolName={toolName}
            />
          );
        }

        if (part.type === "tool_result") {
          const toolCallId =
            typeof part.tool_call_id === "string" ? part.tool_call_id : null;
          if (toolCallId && toolResults.has(toolCallId)) {
            return null;
          }
          return (
            <OrphanToolResult
              event={event}
              key={index}
              onShowDetails={onShowDetails}
              output={"output" in part ? part.output : null}
              raw={part}
              toolCallId={toolCallId}
            />
          );
        }

        if (part.type === "file") {
          return <MutedLine key={index}>file attachment</MutedLine>;
        }

        return <MutedLine key={index}>[{part.type}]</MutedLine>;
      })}
    </div>
  );
}

function ToolCallRow({
  event,
  onShowDetails,
  raw,
  results,
  toolArguments,
  toolCallId,
  toolName,
}: {
  event: Event;
  onShowDetails?: ShowEventDetails;
  raw: unknown;
  results: ToolResultRecord[];
  toolArguments: unknown;
  toolCallId: string | null;
  toolName: string;
}) {
  const status = toolCardStatus(results);
  const durationMs = findDurationMs([
    raw,
    ...results.map((result) => result.raw),
    ...results.map((result) => result.output),
  ]);

  return (
    <article className={`tool-card tool-call-card tool-status-${status}`}>
      <details className="tool-card-details" open={status === "error"}>
        <summary className="tool-card-header">
          <span className="tool-glyph" aria-hidden="true">
            <ToolIcon />
          </span>
          <span className="tool-title-group">
            <code>{toolName}</code>
            <span>{summarizeValue(toolArguments)}</span>
          </span>
          <span className="tool-status-group">
            <span className="status-dot" aria-label={status} />
            <span>{status}</span>
            {durationMs != null ? (
              <span>{formatDuration(durationMs)}</span>
            ) : null}
            {onShowDetails ? (
              <button
                className="tool-detail-button"
                onClick={(clickEvent) => {
                  clickEvent.preventDefault();
                  clickEvent.stopPropagation();
                  onShowDetails(event, {
                    type: "tool_request",
                    raw,
                    request: {
                      function_name: toolName,
                      arguments: toolArguments,
                    },
                    results,
                    toolCallId,
                    toolName,
                  });
                }}
                type="button"
              >
                details
              </button>
            ) : null}
          </span>
          <time>{formatTime(event.created_at)}</time>
        </summary>
        <div className="tool-card-body">
          <ToolPayload label="arguments">
            <JsonPreview
              value={toolArguments}
              label="arguments"
              maxCollapsedLength={900}
            />
          </ToolPayload>
          {toolCallId ? (
            <div className="tool-call-id">call {shortId(toolCallId)}</div>
          ) : null}
          {results.length === 0 ? (
            <div className="tool-empty-result">result pending</div>
          ) : null}
          {results.map((result, index) => (
            <ToolResultPanel
              fallbackToolName={toolName}
              key={`${result.toolCallId}-${result.event.id}-${index}`}
              result={result}
            />
          ))}
          <RawDisclosure value={raw} />
        </div>
      </details>
    </article>
  );
}

function ToolResultPanel({
  fallbackToolName,
  result,
}: {
  fallbackToolName: string;
  result: ToolResultRecord;
}) {
  const isError =
    isToolResultError(result.output) || isToolResultError(result.raw);
  const artifactRef =
    findArtifactRef(result.output) ?? findArtifactRef(result.raw);

  return (
    <details
      className={`tool-result-panel ${isError ? "tool-result-error" : "tool-result-ok"}`}
      open
    >
      <summary>
        <span className="payload-label">result</span>
        <span className="tool-result-meta">
          <span className="status-dot" aria-label={isError ? "error" : "ok"} />
          <code>{result.toolName ?? fallbackToolName}</code>
          <time>{formatTime(result.event.created_at)}</time>
        </span>
      </summary>
      <JsonPreview
        value={result.output}
        label="result"
        maxCollapsedLength={1200}
      />
      {artifactRef ? (
        <ArtifactView
          artifactId={artifactRef.artifactId}
          path={artifactRef.path}
          version={artifactRef.version}
        />
      ) : null}
    </details>
  );
}

function OrphanToolResult({
  event,
  onShowDetails,
  output,
  raw,
  toolCallId,
}: {
  event: Event;
  onShowDetails?: ShowEventDetails;
  output: unknown;
  raw: unknown;
  toolCallId: string | null;
}) {
  const isError = isToolResultError(output) || isToolResultError(raw);
  const artifactRef = findArtifactRef(output) ?? findArtifactRef(raw);

  return (
    <article
      className={`tool-card tool-result-card orphan-result ${isError ? "tool-status-error" : "tool-status-ok"}`}
    >
      <details className="tool-card-details" open>
        <summary className="tool-card-header">
          <span className="tool-glyph" aria-hidden="true">
            <ResultIcon />
          </span>
          <span className="tool-title-group">
            <code>tool result</code>
            <span>
              {toolCallId
                ? `unpaired ${shortId(toolCallId)}`
                : "unpaired result"}
            </span>
          </span>
          <span className="tool-status-group">
            <span
              className="status-dot"
              aria-label={isError ? "error" : "ok"}
            />
            <span>{isError ? "error" : "ok"}</span>
            {onShowDetails ? (
              <button
                className="tool-detail-button"
                onClick={(clickEvent) => {
                  clickEvent.preventDefault();
                  clickEvent.stopPropagation();
                  onShowDetails(event, {
                    type: "tool_result",
                    raw,
                    output,
                    toolCallId,
                  });
                }}
                type="button"
              >
                details
              </button>
            ) : null}
          </span>
          <time>{formatTime(event.created_at)}</time>
        </summary>
        <div className="tool-card-body">
          <ToolPayload label="result">
            <JsonPreview
              value={output}
              label="result"
              maxCollapsedLength={1200}
            />
          </ToolPayload>
          {artifactRef ? (
            <ArtifactView
              artifactId={artifactRef.artifactId}
              path={artifactRef.path}
              version={artifactRef.version}
            />
          ) : null}
          <RawDisclosure value={raw} />
        </div>
      </details>
    </article>
  );
}

function SystemInlineMessage({
  event,
  label,
  onShowDetails,
  raw,
  text,
}: {
  event: Event;
  label: string;
  onShowDetails: ShowEventDetails;
  raw: unknown;
  text: string;
}) {
  return (
    <div className="system-inline">
      <span className="system-chip">
        <SystemIcon />
        <span>{label}</span>
        <time>{formatTime(event.created_at)}</time>
        <button
          className="system-detail-button"
          onClick={() => onShowDetails(event)}
          type="button"
        >
          details
        </button>
      </span>
      {text ? <MarkdownContent text={text} /> : null}
      <RawDisclosure value={raw} />
    </div>
  );
}

function MessageHeader({
  label,
  role,
  time,
}: {
  label: string;
  role: string;
  time: string;
}) {
  return (
    <div className="message-header">
      <span className="message-identity">
        <RoleAvatar label={label} role={role} />
        <span className="role-label">{label}</span>
      </span>
      <time>{formatTime(time)}</time>
    </div>
  );
}

function SystemDivider({ event }: { event: Event }) {
  return (
    <div className="system-divider">
      <span className="system-chip">
        <SystemIcon />
        <span>{systemEventLabel(event)}</span>
        <time>{formatTime(event.created_at)}</time>
      </span>
      <RawDisclosure value={event} />
    </div>
  );
}

function AssistantMetadataFooter({
  usage,
}: {
  usage: UsageRecord | null | undefined;
}) {
  const chips = assistantMetricChips(usage);
  if (chips.length === 0) {
    return null;
  }

  return (
    <footer className="message-meta" aria-label="Assistant response metrics">
      {chips.map((chip) => (
        <span className="metric-chip" key={chip}>
          {chip}
        </span>
      ))}
    </footer>
  );
}

function ToolPayload({
  children,
  label,
}: {
  children: ReactNode;
  label: string;
}) {
  return (
    <section className="tool-payload">
      <div className="payload-label">{label}</div>
      {children}
    </section>
  );
}

function RawDisclosure({
  value,
  open,
  onToggle,
  id,
}: {
  value: unknown;
  open?: boolean;
  onToggle?: (open: boolean) => void;
  id?: string;
}) {
  const rawText = formatJson(value);
  const controlled = open !== undefined;
  return (
    <details
      className="raw-disclosure"
      id={id}
      open={controlled ? open : undefined}
      onToggle={
        controlled ? (event) => onToggle?.(event.currentTarget.open) : undefined
      }
    >
      <summary>
        raw
        <CopyButton className="copy-button raw-copy-button" text={rawText} />
      </summary>
      <JsonPreview value={value} label="raw" maxCollapsedLength={900} />
    </details>
  );
}

function MessageActions({
  markdown,
  eventId,
  onShowDetails,
  onShowRaw,
}: {
  markdown: string;
  eventId: string;
  onShowDetails: () => void;
  onShowRaw: () => void;
}) {
  return (
    <div className="message-actions">
      {markdown ? (
        <CopyButton
          className="copy-button message-action-button"
          label="copy md"
          text={markdown}
        />
      ) : null}
      <button
        className="copy-button message-action-button"
        onClick={onShowRaw}
        type="button"
      >
        raw event
      </button>
      <button
        className="copy-button message-action-button"
        onClick={onShowDetails}
        type="button"
      >
        details
      </button>
      <a
        aria-label="Link to this message"
        className="copy-button message-action-button message-anchor"
        href={`#event-${eventId}`}
        title="Link to this message"
      >
        #
      </a>
    </div>
  );
}

function FilterChip({
  active,
  label,
  onToggle,
}: {
  active: boolean;
  label: string;
  onToggle: () => void;
}) {
  return (
    <button
      aria-pressed={active}
      className={`filter-chip ${active ? "is-active" : ""}`}
      onClick={onToggle}
      type="button"
    >
      {label}
    </button>
  );
}

function TranscriptSearchBox({
  query,
  matchCount,
  activeIndex,
  onChange,
  onClear,
  onKeyDown,
  onNext,
  onPrev,
}: {
  query: string;
  matchCount: number;
  activeIndex: number;
  onChange: (value: string) => void;
  onClear: () => void;
  onKeyDown: (event: KeyboardEvent<HTMLInputElement>) => void;
  onNext: () => void;
  onPrev: () => void;
}) {
  const hasQuery = query.trim().length > 0;
  const position = matchCount > 0 ? `${activeIndex + 1}/${matchCount}` : "0/0";

  return (
    <div className="timeline-search">
      <SearchIcon />
      <input
        aria-label="Find in conversation"
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={onKeyDown}
        placeholder="Find…"
        spellCheck={false}
        type="search"
        value={query}
      />
      {hasQuery ? (
        <>
          <span className="search-count" aria-live="polite">
            {position}
          </span>
          <span className="search-nav">
            <button
              aria-label="Previous match"
              className="search-nav-button"
              disabled={matchCount === 0}
              onClick={onPrev}
              type="button"
            >
              ↑
            </button>
            <button
              aria-label="Next match"
              className="search-nav-button"
              disabled={matchCount === 0}
              onClick={onNext}
              type="button"
            >
              ↓
            </button>
          </span>
          <button
            aria-label="Clear search"
            className="search-close"
            onClick={onClear}
            type="button"
          >
            ✕
          </button>
        </>
      ) : null}
    </div>
  );
}

function SearchIcon() {
  return (
    <svg
      aria-hidden="true"
      className="search-icon"
      focusable="false"
      viewBox="0 0 16 16"
    >
      <circle cx="7" cy="7" r="4.2" />
      <path d="m10.2 10.2 3 3" />
    </svg>
  );
}

interface TranscriptFind {
  matchCount: number;
  activeIndex: number;
  next: () => void;
  prev: () => void;
  // Re-scan the mounted rows and repaint highlights. Called after the virtualizer
  // renders a new range so highlights track what's actually on screen.
  refresh: () => void;
}

interface FindMatch {
  rowIndex: number;
  ordinalInRow: number;
}

// Find-in-conversation across a virtualized list. Matches are enumerated from the
// plain-text projection of EVERY row (including rows that aren't mounted), so the
// count and next/prev order are stable. Jumping asks the virtualizer to render the
// target row (scrollToIndex) and then paints the live DOM via the CSS Custom
// Highlight API — never mutating React-owned nodes. Only matches inside currently
// mounted rows can be painted; that is inherent to virtualization.
function useTranscriptFind({
  containerRef,
  query,
  rows,
  scrollToRow,
}: {
  containerRef: RefObject<HTMLElement | null>;
  query: string;
  rows: TimelineRowItem[];
  scrollToRow: (rowIndex: number) => void;
}): TranscriptFind {
  const needle = query.trim();

  const matches = useMemo<FindMatch[]>(() => {
    if (needle.length === 0) {
      return [];
    }
    const lower = needle.toLowerCase();
    const out: FindMatch[] = [];
    for (let i = 0; i < rows.length; i += 1) {
      const haystack = rows[i].text.toLowerCase();
      if (haystack.length === 0) {
        continue;
      }
      let from = haystack.indexOf(lower);
      let ordinal = 0;
      while (from !== -1) {
        out.push({ rowIndex: i, ordinalInRow: ordinal });
        ordinal += 1;
        from = haystack.indexOf(lower, from + lower.length);
      }
    }
    return out;
  }, [rows, needle]);

  const matchCount = matches.length;
  const [activeIndex, setActiveIndex] = useState(0);

  const matchesRef = useRef(matches);
  matchesRef.current = matches;
  const activeIndexRef = useRef(0);
  const needleRef = useRef(needle);
  needleRef.current = needle;

  const clearHighlights = useCallback(() => {
    if (typeof CSS !== "undefined" && "highlights" in CSS) {
      CSS.highlights.delete("exo-find");
      CSS.highlights.delete("exo-find-active");
    }
  }, []);

  useEffect(() => {
    setActiveIndex((current) =>
      matchCount === 0 ? 0 : Math.min(current, matchCount - 1),
    );
  }, [matchCount]);

  const refresh = useCallback(() => {
    const container = containerRef.current;
    const activeNeedle = needleRef.current;
    if (!container || activeNeedle.length === 0) {
      clearHighlights();
      return;
    }
    const target = matchesRef.current[activeIndexRef.current] ?? null;
    paintFindHighlights(container, activeNeedle, target);
  }, [clearHighlights, containerRef]);

  // Jump to the active match (scrolling the virtualizer if it's off-screen) and
  // repaint once the target row has had a chance to mount.
  useEffect(() => {
    activeIndexRef.current = activeIndex;
    if (matchCount === 0) {
      clearHighlights();
      return;
    }
    const target = matchesRef.current[activeIndex];
    if (target) {
      scrollToRow(target.rowIndex);
    }
    refresh();
    let raf2 = 0;
    const raf1 = requestAnimationFrame(() => {
      refresh();
      raf2 = requestAnimationFrame(refresh);
    });
    return () => {
      cancelAnimationFrame(raf1);
      cancelAnimationFrame(raf2);
    };
  }, [activeIndex, matchCount, needle, refresh, scrollToRow, clearHighlights]);

  useEffect(() => clearHighlights, [clearHighlights]);

  const next = useCallback(() => {
    setActiveIndex((current) => {
      const total = matchesRef.current.length;
      return total === 0 ? 0 : (current + 1) % total;
    });
  }, []);

  const prev = useCallback(() => {
    setActiveIndex((current) => {
      const total = matchesRef.current.length;
      return total === 0 ? 0 : (current - 1 + total) % total;
    });
  }, []);

  return { matchCount, activeIndex, next, prev, refresh };
}

interface ScannedRange {
  range: Range;
  rowIndex: number;
  ordinal: number;
}

function paintFindHighlights(
  container: HTMLElement,
  needle: string,
  target: FindMatch | null,
) {
  if (typeof CSS === "undefined" || !("highlights" in CSS)) {
    // No highlight API: still bring the active row into view if it's mounted.
    if (target) {
      const el = container.querySelector(
        `[data-find-row="${target.rowIndex}"]`,
      );
      if (el instanceof HTMLElement) {
        el.scrollIntoView({ block: "center", inline: "nearest" });
      }
    }
    return;
  }
  const scanned = collectFindRanges(container, needle);
  if (scanned.length === 0) {
    CSS.highlights.delete("exo-find");
    CSS.highlights.delete("exo-find-active");
    return;
  }
  let activeRange: Range | null = null;
  if (target) {
    activeRange =
      scanned.find(
        (entry) =>
          entry.rowIndex === target.rowIndex &&
          entry.ordinal === target.ordinalInRow,
      )?.range ??
      scanned.find((entry) => entry.rowIndex === target.rowIndex)?.range ??
      null;
  }
  const rest = scanned
    .map((entry) => entry.range)
    .filter((range) => range !== activeRange);
  CSS.highlights.set("exo-find", new Highlight(...rest));
  if (activeRange) {
    // Positioning is owned by scrollToRow(align:"center") on the jump. A second
    // scrollIntoView here fought virtuoso's own scroll — leaving the active match
    // off-centre — and, because this repaints on every rangeChanged, it would
    // also yank the view back whenever the reader scrolled. Paint only.
    CSS.highlights.set("exo-find-active", new Highlight(activeRange));
  } else {
    CSS.highlights.delete("exo-find-active");
  }
}

function collectFindRanges(root: HTMLElement, query: string): ScannedRange[] {
  const needle = query.toLowerCase();
  const ranges: ScannedRange[] = [];
  const perRow = new Map<number, number>();
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let node = walker.nextNode();
  while (node) {
    const text = node.nodeValue ?? "";
    if (text.length > 0) {
      const haystack = text.toLowerCase();
      let from = haystack.indexOf(needle);
      if (from !== -1) {
        const rowIndex = findRowIndexOf(node);
        while (from !== -1) {
          const range = document.createRange();
          range.setStart(node, from);
          range.setEnd(node, from + needle.length);
          const ordinal = perRow.get(rowIndex) ?? 0;
          perRow.set(rowIndex, ordinal + 1);
          ranges.push({ range, rowIndex, ordinal });
          from = haystack.indexOf(needle, from + needle.length);
        }
      }
    }
    node = walker.nextNode();
  }
  return ranges;
}

function findRowIndexOf(node: Node): number {
  const el = node.parentElement?.closest("[data-find-row]");
  const attr = el?.getAttribute("data-find-row");
  return attr ? Number.parseInt(attr, 10) : -1;
}

interface TimelineRowItem {
  key: string;
  kind: "artifact" | "system" | "conversation";
  event: Event;
  text: string;
}

interface TimelineFilterOptions {
  showArtifacts: boolean;
  showMessages: boolean;
  showSystemEvents: boolean;
  showTools: boolean;
  toolCallIds: Set<string>;
  toolResults: Map<string, ToolResultRecord[]>;
}

// Flatten ordered events into the exact rows the timeline renders, mirroring the
// filter rules the static list used. Each row gets a plain-text projection used
// only by find — it never affects what is rendered.
function buildTimelineRows(
  orderedEvents: Event[],
  options: TimelineFilterOptions,
): TimelineRowItem[] {
  const {
    showArtifacts,
    showMessages,
    showSystemEvents,
    showTools,
    toolCallIds,
    toolResults,
  } = options;
  const rows: TimelineRowItem[] = [];

  for (const event of orderedEvents) {
    if (event.data.type === "artifact_written") {
      if (!showArtifacts) {
        continue;
      }
      rows.push({
        key: event.id,
        kind: "artifact",
        event,
        text: eventSearchText(event, toolResults),
      });
      continue;
    }

    if (!isConversationEvent(event)) {
      if (showSystemEvents) {
        rows.push({
          key: event.id,
          kind: "system",
          event,
          text: eventSearchText(event, toolResults),
        });
      }
      continue;
    }

    const kind = conversationEventKind(event);
    if (kind === "tool" && !showTools) {
      continue;
    }
    if (kind === "message" && !showMessages) {
      continue;
    }
    if (
      !hasRenderableConversationContent(event, showSystemEvents, toolCallIds)
    ) {
      continue;
    }

    rows.push({
      key: event.id,
      kind: "conversation",
      event,
      text: eventSearchText(event, toolResults),
    });
  }

  return rows;
}

function clipSearchText(value: string, max = 2000): string {
  return value.length > max ? value.slice(0, max) : value;
}

function eventSearchText(
  event: Event,
  toolResults: Map<string, ToolResultRecord[]>,
): string {
  const data = event.data;
  const parts: string[] = [];

  if (data.type === "artifact_written") {
    return ["artifact written", data.path, data.artifact_id].join(" ");
  }

  if (data.type === "tool_requested") {
    parts.push(data.request.function_name);
    parts.push(clipSearchText(formatJson(data.request.arguments)));
    for (const result of toolResults.get(data.tool_call_id) ?? []) {
      parts.push(clipSearchText(formatJson(result.output)));
    }
    return parts.join(" ");
  }

  if (data.type === "tool_result") {
    return clipSearchText(formatJson(data.result));
  }

  if (data.type === "messages") {
    for (const message of data.messages) {
      const role = typeof message.role === "string" ? message.role : "message";
      const content = "content" in message ? message.content : null;
      if (role === "assistant") {
        parts.push(renderAssistantPlainText(content));
      } else if (role === "tool") {
        if (isContentPartArray(content)) {
          for (const part of content) {
            if (part.type === "tool_result") {
              parts.push(
                clipSearchText(
                  formatJson("output" in part ? part.output : null),
                ),
              );
            }
          }
        }
      } else {
        parts.push(renderTextContent(content));
      }
      if (isContentPartArray(content)) {
        for (const part of content) {
          if (part.type === "tool_call") {
            const name =
              typeof part.tool_name === "string" ? part.tool_name : "tool";
            parts.push(name);
            parts.push(
              clipSearchText(
                summarizeValue("arguments" in part ? part.arguments : null),
              ),
            );
          }
        }
      }
    }
    return parts.join(" ");
  }

  return systemEventLabel(event);
}

function EventLoadingSkeleton() {
  return (
    <div aria-label="Loading events" className="event-loading-skeleton">
      <SkeletonRows count={4} />
    </div>
  );
}

function EmptyState({ title }: { title: string }) {
  return <div className="empty-state">{title}</div>;
}

function MutedLine({ children }: { children: ReactNode }) {
  return <div className="muted-line">{children}</div>;
}

function RoleAvatar({ label, role }: { label: string; role: string }) {
  const normalizedRole = role.toLowerCase();
  const initial = firstInitial(label);

  if (normalizedRole === "assistant") {
    return (
      <span className="role-avatar avatar-assistant" aria-hidden="true">
        {initial || <AssistantIcon />}
      </span>
    );
  }

  if (normalizedRole === "user") {
    return (
      <span className="role-avatar avatar-user" aria-hidden="true">
        <UserIcon />
      </span>
    );
  }

  return (
    <span className="role-avatar avatar-system" aria-hidden="true">
      <SystemIcon />
    </span>
  );
}

function firstInitial(label: string): string {
  const trimmed = label.trim();
  return trimmed ? trimmed[0].toUpperCase() : "";
}

function AssistantIcon() {
  return (
    <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
      <path d="M8 2.4 9.2 6l3.4 1.2-3.4 1.2L8 12l-1.2-3.6-3.4-1.2L6.8 6 8 2.4Z" />
    </svg>
  );
}

function UserIcon() {
  return (
    <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
      <path d="M8 8.1a2.7 2.7 0 1 0 0-5.4 2.7 2.7 0 0 0 0 5.4Zm-4.8 5.1c.6-2.4 2.3-3.6 4.8-3.6s4.2 1.2 4.8 3.6" />
    </svg>
  );
}

function SystemIcon() {
  return (
    <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
      <path d="M8 2.2v2M8 11.8v2M2.2 8h2M11.8 8h2M3.9 3.9l1.4 1.4M10.7 10.7l1.4 1.4M12.1 3.9l-1.4 1.4M5.3 10.7l-1.4 1.4" />
      <circle cx="8" cy="8" r="2.2" />
    </svg>
  );
}

function ToolIcon() {
  return (
    <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
      <path d="M6.2 3.3 2.8 6.7l3.4 3.4M9.8 3.3l3.4 3.4-3.4 3.4M8.8 2.6 7.2 13.4" />
    </svg>
  );
}

function ResultIcon() {
  return (
    <svg viewBox="0 0 16 16" focusable="false" aria-hidden="true">
      <path d="M3 8.4 6.2 12 13 4" />
    </svg>
  );
}

function assistantMetricChips(usage: UsageRecord | null | undefined): string[] {
  if (!usage) {
    return [];
  }

  const chips: string[] = [];
  if (usage.model) {
    chips.push(usage.model);
  }

  const tokenUsage = formatTokenUsage(usage);
  if (tokenUsage) {
    chips.push(tokenUsage);
  }

  if (usage.duration_ms != null) {
    chips.push(formatDuration(usage.duration_ms));
  }

  if (usage.ttft_ms != null) {
    chips.push(`${formatDuration(usage.ttft_ms)} ttft`);
  }

  if (usage.cost_usd != null) {
    chips.push(`$${usage.cost_usd.toFixed(6)}`);
  }

  return chips;
}

function formatTokenUsage(usage: UsageRecord): string | null {
  const input = usage.prompt_tokens;
  const output = usage.completion_tokens;
  const reasoning = usage.completion_reasoning_tokens;

  if (input == null && output == null && reasoning == null) {
    return null;
  }

  const parts: string[] = [];
  if (input != null) {
    parts.push(`${formatInteger(input)} in`);
  }
  if (output != null) {
    parts.push(`${formatInteger(output)} out`);
  }
  if (reasoning != null) {
    parts.push(`${formatInteger(reasoning)} reasoning`);
  }

  return parts.join(" / ");
}

function formatInteger(value: number): string {
  return value.toLocaleString();
}

function formatDuration(valueMs: number): string {
  if (valueMs < 1000) {
    return `${Math.round(valueMs)}ms`;
  }
  if (valueMs < 60_000) {
    return `${(valueMs / 1000).toFixed(valueMs < 10_000 ? 1 : 0)}s`;
  }
  const minutes = Math.floor(valueMs / 60_000);
  const seconds = Math.round((valueMs % 60_000) / 1000);
  return `${minutes}m ${seconds}s`;
}

function toolCardStatus(
  results: ToolResultRecord[],
): "pending" | "ok" | "error" {
  if (results.length === 0) {
    return "pending";
  }
  return results.some(
    (result) =>
      isToolResultError(result.output) || isToolResultError(result.raw),
  )
    ? "error"
    : "ok";
}

function isToolResultError(value: unknown): boolean {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }

  const record = value as Record<string, unknown>;
  const type = typeof record.type === "string" ? record.type.toLowerCase() : "";
  const status =
    typeof record.status === "string" ? record.status.toLowerCase() : "";
  const outcome =
    typeof record.outcome === "string" ? record.outcome.toLowerCase() : "";
  return (
    type === "error" ||
    status === "error" ||
    status === "failed" ||
    outcome === "error" ||
    "error" in record ||
    ("stderr" in record &&
      typeof record.stderr === "string" &&
      record.stderr.trim().length > 0)
  );
}

function findDurationMs(values: unknown[]): number | null {
  for (const value of values) {
    const found = findDurationInValue(value, 0);
    if (found != null) {
      return found;
    }
  }
  return null;
}

function findDurationInValue(value: unknown, depth: number): number | null {
  if (!value || typeof value !== "object" || depth > 3) {
    return null;
  }

  if (Array.isArray(value)) {
    for (const item of value) {
      const found = findDurationInValue(item, depth + 1);
      if (found != null) {
        return found;
      }
    }
    return null;
  }

  const record = value as Record<string, unknown>;
  for (const key of ["duration_ms", "elapsed_ms", "latency_ms", "runtime_ms"]) {
    const metric = record[key];
    if (typeof metric === "number" && Number.isFinite(metric)) {
      return metric;
    }
  }

  for (const entryValue of Object.values(record)) {
    const found = findDurationInValue(entryValue, depth + 1);
    if (found != null) {
      return found;
    }
  }

  return null;
}

function compareEvents(left: Event, right: Event): number {
  const timeCompare = left.created_at.localeCompare(right.created_at);
  return timeCompare === 0 ? left.id.localeCompare(right.id) : timeCompare;
}

function isConversationEvent(event: Event): boolean {
  return (
    event.data.type === "messages" ||
    event.data.type === "tool_requested" ||
    event.data.type === "tool_result"
  );
}

// Split conversation events into the two filter buckets the toolbar exposes:
// plain message turns vs. tool activity. A messages event that carries only
// tool-role results reads as tool activity, not a message.
function conversationEventKind(event: Event): "message" | "tool" {
  const data = event.data;
  if (data.type === "tool_requested" || data.type === "tool_result") {
    return "tool";
  }
  if (data.type === "messages") {
    const hasMessageRole = data.messages.some((message) => {
      const role = typeof message.role === "string" ? message.role : "message";
      return role !== "tool";
    });
    return hasMessageRole ? "message" : "tool";
  }
  return "message";
}

function hasRenderableConversationContent(
  event: Event,
  showSystemEvents: boolean,
  toolCallIds: Set<string>,
): boolean {
  const data = event.data;
  if (data.type === "tool_requested") {
    return true;
  }
  if (data.type === "tool_result") {
    return !toolCallIds.has(data.tool_call_id);
  }
  if (data.type !== "messages") {
    return false;
  }
  return data.messages.some((message) => {
    const role = typeof message.role === "string" ? message.role : "message";
    const content = "content" in message ? message.content : null;
    if (role === "user" || role === "assistant") {
      return true;
    }
    if (role === "tool") {
      return renderToolMessageContent(content, event, toolCallIds).length > 0;
    }
    return showSystemEvents;
  });
}

function collectToolCallIds(events: Event[]): Set<string> {
  const ids = new Set<string>();
  for (const event of events) {
    const data = event.data;
    if (data.type === "tool_requested") {
      ids.add(data.tool_call_id);
      continue;
    }
    if (data.type !== "messages") {
      continue;
    }
    for (const message of data.messages) {
      const content = "content" in message ? message.content : null;
      if (!isContentPartArray(content)) {
        continue;
      }
      for (const part of content) {
        if (
          part.type === "tool_call" &&
          typeof part.tool_call_id === "string"
        ) {
          ids.add(part.tool_call_id);
        }
      }
    }
  }
  return ids;
}

function buildToolResultIndex(
  events: Event[],
): Map<string, ToolResultRecord[]> {
  const results = new Map<string, ToolResultRecord[]>();

  function addResult(record: ToolResultRecord) {
    const existing = results.get(record.toolCallId) ?? [];
    existing.push(record);
    results.set(record.toolCallId, existing);
  }

  for (const event of events) {
    const data = event.data;
    if (data.type === "tool_result") {
      addResult({
        event,
        output: data.result,
        raw: data,
        toolCallId: data.tool_call_id,
        toolName: null,
      });
      continue;
    }

    if (data.type !== "messages") {
      continue;
    }

    for (const message of data.messages) {
      const content = "content" in message ? message.content : null;
      if (!isContentPartArray(content)) {
        continue;
      }
      for (const part of content) {
        if (
          part.type !== "tool_result" ||
          typeof part.tool_call_id !== "string"
        ) {
          continue;
        }
        addResult({
          event,
          output: "output" in part ? part.output : null,
          raw: part,
          toolCallId: part.tool_call_id,
          toolName: typeof part.tool_name === "string" ? part.tool_name : null,
        });
      }
    }
  }

  return results;
}

function renderToolMessageContent(
  content: unknown,
  event: Event,
  toolCallIds: Set<string>,
  onShowDetails?: ShowEventDetails,
): ReactNode[] {
  if (!isContentPartArray(content)) {
    return [];
  }

  return content
    .map((part, index) => {
      if (part.type !== "tool_result") {
        return <MutedLine key={index}>[{part.type}]</MutedLine>;
      }
      const toolCallId =
        typeof part.tool_call_id === "string" ? part.tool_call_id : null;
      if (toolCallId && toolCallIds.has(toolCallId)) {
        return null;
      }
      return (
        <OrphanToolResult
          event={event}
          key={index}
          onShowDetails={onShowDetails}
          output={"output" in part ? part.output : null}
          raw={part}
          toolCallId={toolCallId}
        />
      );
    })
    .filter(Boolean);
}

function renderTextContent(content: unknown): string {
  if (typeof content === "string") {
    return content;
  }
  if (!isContentPartArray(content)) {
    return "";
  }
  return content
    .map((part) => {
      if (part.type === "text" && typeof part.text === "string") {
        return part.text;
      }
      return `[${part.type}]`;
    })
    .join("\n");
}

function isContentPartArray(
  content: unknown,
): content is Array<Record<string, unknown> & { type: string }> {
  return (
    Array.isArray(content) &&
    content.every(
      (part) =>
        typeof part === "object" &&
        part !== null &&
        !Array.isArray(part) &&
        "type" in part &&
        typeof part.type === "string",
    )
  );
}

function summarizeValue(value: unknown): string {
  if (value == null) {
    return "no arguments";
  }
  if (typeof value === "string") {
    return clip(value);
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (Array.isArray(value)) {
    return `${value.length} item${value.length === 1 ? "" : "s"}`;
  }
  if (typeof value === "object") {
    const entries = Object.entries(value as Record<string, JsonValue>);
    if (entries.length === 0) {
      return "{}";
    }
    const summary = entries
      .slice(0, 3)
      .map(([key, entryValue]) => `${key}: ${summarizePrimitive(entryValue)}`)
      .join(", ");
    return entries.length > 3 ? `${summary}, ...` : summary;
  }
  return clip(formatJson(value));
}

function summarizePrimitive(value: unknown): string {
  if (value == null) {
    return "null";
  }
  if (typeof value === "string") {
    return `"${clip(value, 44)}"`;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (Array.isArray(value)) {
    return `[${value.length}]`;
  }
  if (typeof value === "object") {
    return "{...}";
  }
  return clip(String(value), 44);
}

function clip(value: string, max = 120): string {
  return value.length > max ? `${value.slice(0, max)}...` : value;
}

function systemEventLabel(event: Event): string {
  const data = event.data;
  switch (data.type) {
    case "conversation_created":
      return "conversation created";
    case "conversation_updated":
      return "conversation updated";
    case "conversation_deleted":
      return "conversation deleted";
    case "conversation_forked":
      return "conversation forked";
    case "session_started":
      return "session started";
    case "session_ended":
      return "session ended";
    case "turn_started":
      return "turn started";
    case "turn_ended":
      return "turn ended";
    case "lingua_stream_chunk":
      return "stream chunk";
    case "artifact_written":
      return `artifact written · ${data.path}`;
    case "sandbox_created":
      return "sandbox created";
    case "sandbox_started":
      return "sandbox started";
    case "sandbox_stopped":
      return "sandbox stopped";
    case "sandbox_snapshotted":
      return "sandbox snapshotted";
    case "sandbox_process_started":
      return "process started";
    case "sandbox_process_state_updated":
      return "process state updated";
    case "sandbox_process_event":
      return "process output";
    case "custom":
      return data.event_type;
    case "error":
      return "error";
    default:
      return data.type;
  }
}

// MiniMax-style models emit their reasoning as literal <think>...</think> inside
// the text, not as a structured reasoning part. Fold those into the quiet
// thinking disclosure, render the rest as the message, and never leak raw tags.
export function splitThinking(
  text: string,
): { type: "text" | "think"; text: string }[] {
  const out: { type: "text" | "think"; text: string }[] = [];
  const re = /<think>([\s\S]*?)<\/think>/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) {
      out.push({ type: "text", text: text.slice(last, m.index) });
    }
    out.push({ type: "think", text: m[1] });
    last = re.lastIndex;
  }
  const tail = text.slice(last);
  const open = tail.indexOf("<think>");
  if (open !== -1) {
    if (open > 0) out.push({ type: "text", text: tail.slice(0, open) });
    out.push({ type: "think", text: tail.slice(open + "<think>".length) });
  } else if (tail.length > 0) {
    out.push({ type: "text", text: tail });
  }
  return out
    .map((s) => ({
      type: s.type,
      text: s.text.replace(/<\/?think>/g, "").trim(),
    }))
    .filter((s) => s.text.length > 0);
}

function AssistantText({ text }: { text: string }) {
  const segments = splitThinking(text);
  if (segments.length === 0) {
    return null;
  }
  return (
    <>
      {segments.map((segment, index) =>
        segment.type === "think" ? (
          <details className="thinking-disclosure" key={index}>
            <summary>thinking</summary>
            <MarkdownContent text={segment.text} />
          </details>
        ) : (
          <MarkdownContent key={index} text={segment.text} />
        ),
      )}
    </>
  );
}

function renderAssistantPlainText(content: unknown): string {
  if (typeof content === "string") {
    return content;
  }
  if (!isContentPartArray(content)) {
    return "";
  }
  return content
    .map((part) => {
      if (part.type === "text" && typeof part.text === "string") {
        return part.text;
      }
      if (part.type === "reasoning" && typeof part.text === "string") {
        return part.text;
      }
      return "";
    })
    .filter(Boolean)
    .join("\n\n");
}

function sanitizeFilename(value: string): string {
  const trimmed = value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-");
  return trimmed || "conversation";
}
