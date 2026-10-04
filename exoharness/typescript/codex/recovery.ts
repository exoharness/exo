import type { JsonValue } from "../harness";
import { asRecord } from "../model-runtime/shared";

export interface NativeTurnSnapshot {
  id: string;
  status: "completed" | "interrupted" | "failed" | "inProgress";
  items: Record<string, unknown>[];
  error: unknown;
}

export function nativeTurnSnapshot(
  response: JsonValue,
  turnId: string,
): NativeTurnSnapshot | null {
  const turns = asRecord(asRecord(response).thread).turns;
  if (!Array.isArray(turns)) return null;
  const turn = turns.find((candidate) => asRecord(candidate).id === turnId);
  if (!turn) return null;
  const record = asRecord(turn);
  if (
    record.status !== "completed" &&
    record.status !== "interrupted" &&
    record.status !== "failed" &&
    record.status !== "inProgress"
  ) {
    throw new Error(`Codex turn ${turnId} has an unknown status`);
  }
  if (!Array.isArray(record.items)) {
    throw new Error(`Codex turn ${turnId} has no items`);
  }
  if (record.itemsView !== "full") {
    throw new Error(`Codex turn ${turnId} has incomplete item history`);
  }
  return {
    id: turnId,
    status: record.status,
    items: record.items.map(asRecord),
    error: record.error,
  };
}

export function nativeItemComplete(item: Record<string, unknown>): boolean {
  return item.status !== "inProgress";
}

export function assertNativeToolSafety(
  snapshot: NativeTurnSnapshot | null,
  unresolvedToolIds: Set<string>,
): void {
  if (!snapshot) {
    if (unresolvedToolIds.size > 0) {
      throw new Error("cannot safely replay unresolved native Codex tool call");
    }
    return;
  }
  for (const id of unresolvedToolIds) {
    const item = snapshot.items.find((candidate) => candidate.id === id);
    if (!item || !nativeItemComplete(item)) {
      throw new Error(
        `cannot safely resume unresolved native Codex tool call ${id}`,
      );
    }
  }
  if (
    snapshot.items.some(
      (item) =>
        !nativeItemComplete(item) &&
        (item.type === "commandExecution" ||
          item.type === "mcpToolCall" ||
          item.type === "dynamicToolCall" ||
          item.type === "webSearch" ||
          item.type === "fileChange"),
    )
  ) {
    throw new Error(
      "cannot safely reattach an in-progress native Codex tool call; its approval request may have been missed",
    );
  }
}
