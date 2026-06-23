import { useEffect } from "react";
import { createPortal } from "react-dom";
import type { Event } from "../api/protocol";
import { formatTime, shortId } from "../lib/rendering";
import { JsonPreview } from "./JsonPreview";

export interface EventDetailToolResult {
  event: Event;
  output: unknown;
  raw: unknown;
  toolCallId: string;
  toolName: string | null;
}

export type EventDetailFocus =
  | {
      type: "tool_request";
      raw: unknown;
      request: unknown;
      results: EventDetailToolResult[];
      toolCallId: string | null;
      toolName: string;
    }
  | {
      type: "tool_result";
      raw: unknown;
      output: unknown;
      toolCallId: string | null;
      toolName?: string | null;
    };

export interface EventDetailSelection {
  event: Event;
  focus?: EventDetailFocus;
}

interface EventDetailDrawerProps {
  selection: EventDetailSelection | null;
  onClose: () => void;
}

const FULL_JSON_LENGTH = Number.MAX_SAFE_INTEGER;

export function EventDetailDrawer({
  selection,
  onClose,
}: EventDetailDrawerProps) {
  useEffect(() => {
    if (!selection) {
      return;
    }

    function handleKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") {
        onClose();
      }
    }

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [onClose, selection]);

  if (!selection || typeof document === "undefined") {
    return null;
  }

  const { event } = selection;

  return createPortal(
    <div
      className="event-detail-backdrop"
      onMouseDown={onClose}
      role="presentation"
    >
      <aside
        aria-labelledby="event-detail-title"
        aria-modal="true"
        className="event-detail-drawer"
        onMouseDown={(mouseEvent) => mouseEvent.stopPropagation()}
        role="dialog"
      >
        <header className="event-detail-header">
          <div className="event-detail-title-group">
            <span className="event-detail-kicker">event details</span>
            <h2 id="event-detail-title">{event.data.type}</h2>
          </div>
          <button
            aria-label="Close event details"
            className="event-detail-close"
            onClick={onClose}
            type="button"
          >
            x
          </button>
        </header>

        <div className="event-detail-body">
          <section className="event-detail-section">
            <div className="section-header">
              <h2>Metadata</h2>
              <span>{shortId(event.id)}</span>
            </div>
            <dl className="kv-grid event-detail-meta">
              <dt>id</dt>
              <dd>
                <code>{event.id}</code>
              </dd>
              <dt>type</dt>
              <dd>
                <code>{event.data.type}</code>
              </dd>
              <dt>created_at</dt>
              <dd>
                <time dateTime={event.created_at}>
                  {formatTime(event.created_at)}
                </time>
              </dd>
              {event.session_id ? (
                <>
                  <dt>session</dt>
                  <dd>
                    <code>{event.session_id}</code>
                  </dd>
                </>
              ) : null}
              {event.turn_id ? (
                <>
                  <dt>turn</dt>
                  <dd>
                    <code>{event.turn_id}</code>
                  </dd>
                </>
              ) : null}
            </dl>
          </section>

          <ToolDetailSplit selection={selection} />

          <section className="event-detail-section">
            <div className="section-header">
              <h2>Raw Event</h2>
              <span>full</span>
            </div>
            <JsonPreview
              value={event}
              label="raw event"
              maxCollapsedLength={FULL_JSON_LENGTH}
            />
          </section>
        </div>
      </aside>
    </div>,
    document.body,
  );
}

function ToolDetailSplit({ selection }: { selection: EventDetailSelection }) {
  const focus = selection.focus ?? inferToolFocus(selection.event);
  if (!focus) {
    return null;
  }

  if (focus.type === "tool_result") {
    return (
      <section className="event-detail-section">
        <div className="section-header">
          <h2>Tool Result</h2>
          <span>
            {focus.toolCallId ? shortId(focus.toolCallId) : "unpaired"}
          </span>
        </div>
        <div className="event-detail-tool-split">
          <div className="event-detail-empty">request not available</div>
          <ToolJsonBlock label="result" value={focus.output} />
          <ToolJsonBlock label="raw result" value={focus.raw} />
        </div>
      </section>
    );
  }

  return (
    <section className="event-detail-section">
      <div className="section-header">
        <h2>Request / Result</h2>
        <span>{focus.toolCallId ? shortId(focus.toolCallId) : "tool"}</span>
      </div>
      <div className="event-detail-tool-split">
        <div className="event-detail-result-head">
          <span className="payload-label">request</span>
          <code>{focus.toolName}</code>
        </div>
        <JsonPreview
          value={focus.request}
          label="request"
          maxCollapsedLength={FULL_JSON_LENGTH}
        />
        {focus.results.length === 0 ? (
          <div className="event-detail-empty">result pending</div>
        ) : (
          focus.results.map((result, index) => (
            <div
              className="event-detail-result"
              key={`${result.toolCallId}-${result.event.id}-${index}`}
            >
              <div className="event-detail-result-head">
                <span className="payload-label">result</span>
                <span>
                  {result.toolName ? <code>{result.toolName}</code> : null}
                  <time dateTime={result.event.created_at}>
                    {formatTime(result.event.created_at)}
                  </time>
                </span>
              </div>
              <JsonPreview
                value={result.output}
                label="result"
                maxCollapsedLength={FULL_JSON_LENGTH}
              />
            </div>
          ))
        )}
      </div>
    </section>
  );
}

function ToolJsonBlock({ label, value }: { label: string; value: unknown }) {
  return (
    <div className="event-detail-result">
      <span className="payload-label">{label}</span>
      <JsonPreview
        value={value}
        label={label}
        maxCollapsedLength={FULL_JSON_LENGTH}
      />
    </div>
  );
}

function inferToolFocus(event: Event): EventDetailFocus | null {
  const data = event.data;
  if (data.type === "tool_requested") {
    return {
      type: "tool_request",
      raw: data,
      request: data.request,
      results: [],
      toolCallId: data.tool_call_id,
      toolName: data.request.function_name,
    };
  }

  if (data.type === "tool_result") {
    return {
      type: "tool_result",
      raw: data,
      output: data.result,
      toolCallId: data.tool_call_id,
    };
  }

  return null;
}
