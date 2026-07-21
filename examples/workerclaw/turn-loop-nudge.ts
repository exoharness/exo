import type { EventData, Message } from "@exo/harness";

/** Extra model rounds when the agent stops with text before complete_task. */
export const DEFAULT_MAX_TEXT_ONLY_NUDGES = 3;

export function resolveMaxTextOnlyNudges(): number {
  const raw = process.env.WORKERCLAW_MAX_TEXT_ONLY_NUDGES?.trim();
  if (!raw) return DEFAULT_MAX_TEXT_ONLY_NUDGES;
  const parsed = Number.parseInt(raw, 10);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return DEFAULT_MAX_TEXT_ONLY_NUDGES;
  }
  return parsed;
}

export function shouldExitOnTextOnly(
  completeTaskCalled: boolean,
  nudgesUsed: number,
  maxNudges: number,
): boolean {
  if (completeTaskCalled) return true;
  return nudgesUsed >= maxNudges;
}

/**
 * Reserve extra model rounds while complete_task has not been called so text-only
 * nudges are not cut off the moment maxToolRoundTrips is reached.
 */
export function resolveEffectiveMaxToolRoundTrips(
  maxToolRoundTrips: number | null | undefined,
  maxTextOnlyNudges: number,
  completeTaskCalled: boolean,
): number | null {
  if (maxToolRoundTrips === null || maxToolRoundTrips === undefined) {
    return null;
  }
  if (completeTaskCalled) {
    return maxToolRoundTrips;
  }
  return maxToolRoundTrips + maxTextOnlyNudges;
}

export function isRoundBudgetExhausted(
  round: number,
  maxToolRoundTrips: number | null | undefined,
  maxTextOnlyNudges: number,
  completeTaskCalled: boolean,
): boolean {
  const effective = resolveEffectiveMaxToolRoundTrips(
    maxToolRoundTrips,
    maxTextOnlyNudges,
    completeTaskCalled,
  );
  if (effective === null) {
    return false;
  }
  return round > effective;
}

export function buildTextOnlyNudgeMessage(
  nudgeIndex: number,
  lastAssistantText: string,
): string {
  const tail = lastAssistantText.trim().slice(0, 400);
  const lines = [
    "Your last reply was text-only and the task is NOT finished yet.",
    "",
    "Continue immediately in this turn:",
    "- Call a tool for the next step (do not reply with text-only plans).",
    "- When every TODO leaf is done and deliverables are reported via report_deliverable, call complete_task.",
    '- If you cannot finish, call complete_task with status "failed" and explain why.',
  ];
  if (tail) {
    lines.push("", "Your last message was:", tail);
  }
  if (nudgeIndex > 1) {
    lines.push("", `(Nudge ${nudgeIndex} — complete_task is still required.)`);
  }
  return lines.join("\n");
}

export function extractAssistantTextFromEvents(events: EventData[]): string {
  let text = "";
  for (const event of events) {
    if (event.type !== "messages" || !Array.isArray(event.messages)) continue;
    for (const msg of event.messages) {
      const extracted = extractAssistantText(msg);
      if (extracted) text = extracted;
    }
  }
  return text;
}

function extractAssistantText(message: Message): string {
  if (message.role !== "assistant") return "";
  if (typeof message.content === "string") return message.content.trim();
  if (!Array.isArray(message.content)) return "";
  const parts: string[] = [];
  for (const block of message.content) {
    if (!block || typeof block !== "object") continue;
    const b = block as { type?: string; text?: string };
    if (b.type === "text" && typeof b.text === "string") parts.push(b.text);
  }
  return parts.join("\n").trim();
}
