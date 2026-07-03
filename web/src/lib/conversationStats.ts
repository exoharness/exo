import type { Event, UsageRecord } from "../api/protocol";

export interface ConversationRollup {
  assistantTurns: number;
  inputTokens: number;
  outputTokens: number;
  totalCostUsd: number | null;
  p50DurationMs: number | null;
  p95DurationMs: number | null;
}

export function computeConversationRollup(events: Event[]): ConversationRollup {
  let assistantTurns = 0;
  let inputTokens = 0;
  let outputTokens = 0;
  let totalCostUsd = 0;
  let hasCost = false;
  const durations: number[] = [];

  for (const event of events) {
    const data = event.data;
    if (data.type !== "messages") {
      continue;
    }

    const usage = data.usage;
    if (usage) {
      accumulateUsage(usage, durations);
      if (usage.prompt_tokens != null) {
        inputTokens += usage.prompt_tokens;
      }
      if (usage.completion_tokens != null) {
        outputTokens += usage.completion_tokens;
      }
      if (usage.cost_usd != null) {
        totalCostUsd += usage.cost_usd;
        hasCost = true;
      }
    }

    for (const message of data.messages) {
      if (typeof message.role === "string" && message.role === "assistant") {
        assistantTurns += 1;
      }
    }
  }

  durations.sort((left, right) => left - right);

  return {
    assistantTurns,
    inputTokens,
    outputTokens,
    totalCostUsd: hasCost ? totalCostUsd : null,
    p50DurationMs: percentile(durations, 0.5),
    p95DurationMs: percentile(durations, 0.95),
  };
}

function accumulateUsage(usage: UsageRecord, durations: number[]) {
  if (usage.duration_ms != null && Number.isFinite(usage.duration_ms)) {
    durations.push(usage.duration_ms);
  }
}

function percentile(sorted: number[], p: number): number | null {
  if (sorted.length === 0) {
    return null;
  }
  const index = Math.min(
    sorted.length - 1,
    Math.max(0, Math.ceil(p * sorted.length) - 1),
  );
  return sorted[index] ?? null;
}

export function formatRollupChips(rollup: ConversationRollup): string[] {
  const chips: string[] = [];
  if (rollup.assistantTurns > 0) {
    chips.push(
      `${rollup.assistantTurns} turn${rollup.assistantTurns === 1 ? "" : "s"}`,
    );
  }
  if (rollup.inputTokens > 0 || rollup.outputTokens > 0) {
    chips.push(
      `${rollup.inputTokens.toLocaleString()} in / ${rollup.outputTokens.toLocaleString()} out`,
    );
  }
  if (rollup.totalCostUsd != null) {
    chips.push(`$${rollup.totalCostUsd.toFixed(6)}`);
  }
  if (rollup.p50DurationMs != null) {
    chips.push(`p50 ${formatDuration(rollup.p50DurationMs)}`);
  }
  if (rollup.p95DurationMs != null) {
    chips.push(`p95 ${formatDuration(rollup.p95DurationMs)}`);
  }
  return chips;
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
