import { useState } from "react";
import { formatJson } from "../lib/rendering";

interface JsonPreviewProps {
  value: unknown;
  label?: string;
  defaultOpen?: boolean;
  maxCollapsedLength?: number;
}

export function JsonPreview({
  value,
  label = "json",
  defaultOpen = false,
  maxCollapsedLength = 1200,
}: JsonPreviewProps) {
  const text = formatJson(redactSecrets(value));
  const isLong =
    text.length > maxCollapsedLength || text.split("\n").length > 16;
  const body = <ExpandablePre text={text} maxLength={maxCollapsedLength * 2} />;

  if (!isLong) {
    return body;
  }

  return (
    <details className="json-disclosure" open={defaultOpen}>
      <summary>
        {label} ({text.length.toLocaleString()} chars)
      </summary>
      {body}
    </details>
  );
}

const SENSITIVE_KEY_PATTERN =
  /(^|[_-])(api[_-]?key|access[_-]?token|refresh[_-]?token|auth[_-]?token|token|authorization|password|passwd|secret|private[_-]?key|credential|cookie|bearer)($|[_-])/i;

function redactSecrets(value: unknown, seen = new WeakSet<object>()): unknown {
  if (value == null) {
    return value;
  }

  if (typeof value === "string") {
    return looksLikeSecret(value) ? "[redacted]" : value;
  }

  if (typeof value !== "object") {
    return value;
  }

  if (seen.has(value)) {
    return "[circular]";
  }
  seen.add(value);

  if (Array.isArray(value)) {
    return value.map((item) => redactSecrets(item, seen));
  }

  return Object.fromEntries(
    Object.entries(value as Record<string, unknown>).map(
      ([key, entryValue]) => [
        key,
        isSensitiveKey(key) ? "[redacted]" : redactSecrets(entryValue, seen),
      ],
    ),
  );
}

function isSensitiveKey(key: string): boolean {
  const normalized = key.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase();
  return SENSITIVE_KEY_PATTERN.test(normalized);
}

function looksLikeSecret(value: string): boolean {
  const trimmed = value.trim();
  return (
    /^Bearer\s+\S{12,}$/i.test(trimmed) ||
    /^sk-[A-Za-z0-9_-]{16,}$/.test(trimmed) ||
    /^xox[abprs]-[A-Za-z0-9-]{16,}$/.test(trimmed) ||
    /^AIza[A-Za-z0-9_-]{20,}$/.test(trimmed) ||
    /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]{16,}$/.test(trimmed) ||
    /^gh[pousr]_[A-Za-z0-9_]{20,}$/.test(trimmed) ||
    /^github_pat_[A-Za-z0-9_]{20,}$/.test(trimmed)
  );
}

function ExpandablePre({
  text,
  maxLength,
}: {
  text: string;
  maxLength: number;
}) {
  const [expanded, setExpanded] = useState(false);
  const shouldClamp = text.length > maxLength;
  const visibleText =
    !shouldClamp || expanded ? text : `${text.slice(0, maxLength)}\n...`;

  return (
    <div className="pre-block">
      <pre className="json-preview">{visibleText}</pre>
      {shouldClamp ? (
        <button
          className="text-button"
          type="button"
          onClick={() => setExpanded((value) => !value)}
        >
          {expanded ? "show less" : "show more"}
        </button>
      ) : null}
    </div>
  );
}

interface TextPreviewProps {
  text: string;
  maxCollapsedLength?: number;
}

export function TextPreview({
  text,
  maxCollapsedLength = 1600,
}: TextPreviewProps) {
  const isLong =
    text.length > maxCollapsedLength || text.split("\n").length > 24;
  if (!isLong) {
    return <pre className="message-text">{text || "(empty)"}</pre>;
  }

  return (
    <details className="json-disclosure">
      <summary>text ({text.length.toLocaleString()} chars)</summary>
      <pre className="message-text">{text}</pre>
    </details>
  );
}
