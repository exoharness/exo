import type {
  AssistantContent,
  JsonValue,
  LinguaMessage,
  SandboxProcessEvent,
  SandboxProcessStatus,
  ToolContent,
  UserContent,
} from "../api/protocol";

export function formatDateTime(value: string | null | undefined): string {
  if (!value) {
    return "unknown";
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return value;
  }
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "medium",
  }).format(date);
}

export function formatTime(value: string | null | undefined): string {
  if (!value) {
    return "unknown";
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return value;
  }
  return new Intl.DateTimeFormat(undefined, {
    hour: "2-digit",
    minute: "2-digit",
  }).format(date);
}

export function formatRecency(
  value: string | null | undefined,
  now = Date.now(),
): string {
  if (!value) {
    return "no activity";
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return "unknown";
  }
  const diffMs = now - date.getTime();
  if (diffMs < 0) {
    return formatTime(value);
  }
  const seconds = Math.floor(diffMs / 1000);
  if (seconds < 45) {
    return "just now";
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m ago`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours}h ago`;
  }
  const days = Math.floor(hours / 24);
  if (days < 7) {
    return `${days}d ago`;
  }
  return formatDateTime(value);
}

export function shortId(id: string | null | undefined): string {
  if (!id) {
    return "none";
  }
  return id.length > 14 ? `${id.slice(0, 8)}...${id.slice(-4)}` : id;
}

export function formatJson(value: JsonValue | unknown): string {
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

export function renderMessageContent(message: LinguaMessage): string {
  if (message.role === "assistant") {
    return isAssistantContent(message.content)
      ? renderAssistantContent(message.content)
      : formatJson(message.content);
  }
  if (
    message.role === "user" ||
    message.role === "system" ||
    message.role === "developer"
  ) {
    return isUserContent(message.content)
      ? renderUserContent(message.content)
      : formatJson(message.content);
  }
  if (message.role === "tool") {
    if (!isToolContent(message.content)) {
      return formatJson(message.content);
    }
    return message.content
      .map((part) => {
        if (part.type === "tool_result") {
          return `${part.tool_name}: ${formatJson(part.output)}`;
        }
        return formatJson(part);
      })
      .join("\n");
  }
  return "content" in message
    ? formatJson(message.content)
    : formatJson(message);
}

export function renderUserContent(content: UserContent): string {
  if (typeof content === "string") {
    return content;
  }
  return content
    .map((part) => {
      if (part.type === "text" && typeof part.text === "string") {
        return part.text;
      }
      return `[${part.type}] ${formatJson(part)}`;
    })
    .join("");
}

function isUserContent(content: unknown): content is UserContent {
  return typeof content === "string" || isContentPartArray(content);
}

function isAssistantContent(content: unknown): content is AssistantContent {
  return typeof content === "string" || isContentPartArray(content);
}

function isToolContent(content: unknown): content is ToolContent {
  return isContentPartArray(content);
}

function isContentPartArray(
  content: unknown,
): content is Array<{ type: string } & Record<string, JsonValue>> {
  return Array.isArray(content) && content.every(isContentPart);
}

function isContentPart(
  part: unknown,
): part is { type: string } & Record<string, JsonValue> {
  return (
    typeof part === "object" &&
    part !== null &&
    !Array.isArray(part) &&
    "type" in part &&
    typeof part.type === "string"
  );
}

export function renderAssistantContent(content: AssistantContent): string {
  if (typeof content === "string") {
    return content;
  }
  return content
    .map((part) => {
      if (part.type === "text" && typeof part.text === "string") {
        return part.text;
      }
      if (part.type === "reasoning" && typeof part.text === "string") {
        return `[reasoning] ${part.text}`;
      }
      if (part.type === "tool_call") {
        return `[tool_call ${part.tool_name}] ${formatJson(part.arguments)}`;
      }
      if (part.type === "tool_result") {
        return `[tool_result ${part.tool_name}] ${formatJson(part.output)}`;
      }
      if (part.type === "file") {
        return "[file]";
      }
      return `[${part.type}] ${formatJson(part)}`;
    })
    .join("");
}

export function statusText(status: SandboxProcessStatus): string {
  switch (status.type) {
    case "running":
      return "running";
    case "exited":
      return `exited ${status.exit_code}`;
    case "failed":
      return `failed: ${status.message}`;
    case "cancelled":
      return "cancelled";
  }
}

export function processEventSummary(event: SandboxProcessEvent): string {
  switch (event.type) {
    case "stdout":
    case "stderr":
      return `${event.type} ${decodeBytes(event.data)}`;
    case "exit":
      return `exit ${event.exit_code}`;
    case "error":
      return `error ${event.message}`;
    case "cancelled":
      return "cancelled";
  }
}

export function decodeBytes(bytes: number[]): string {
  if (bytes.length === 0) {
    return "";
  }
  try {
    return new TextDecoder().decode(new Uint8Array(bytes));
  } catch {
    return `[${bytes.length} bytes]`;
  }
}

export function clampText(text: string, max = 240): string {
  if (text.length <= max) {
    return text;
  }
  return `${text.slice(0, max)}...`;
}
