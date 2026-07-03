export interface ActiveChatTurn {
  cancelRequested: boolean;
  requestId: string;
  startedAt: number;
}

export function makeChatRequestId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `chat-${Date.now().toString(36)}-${Math.random()
    .toString(36)
    .slice(2)}`;
}

export function parseJsonObject(text: string): Record<string, unknown> | null {
  if (!text.trim()) {
    return null;
  }

  try {
    const value = JSON.parse(text) as unknown;
    return value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

export function extractChatError(
  payload: Record<string, unknown> | null,
): string | null {
  if (
    !payload ||
    typeof payload.error !== "string" ||
    payload.error.trim() === ""
  ) {
    return null;
  }
  return payload.error;
}

export function clearActiveChatTurnIfMatch(
  current: ActiveChatTurn | null,
  requestId: string,
): ActiveChatTurn | null {
  return current?.requestId === requestId ? null : current;
}

export function markChatCancelRequested(
  current: ActiveChatTurn | null,
  requestId: string,
): ActiveChatTurn | null {
  return current?.requestId === requestId
    ? { ...current, cancelRequested: true }
    : current;
}

export function createActiveChatTurn(requestId: string): ActiveChatTurn {
  return {
    cancelRequested: false,
    requestId,
    startedAt: Date.now(),
  };
}
