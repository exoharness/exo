import { type CodexProtocolLogEntry } from "./app-server";
import { messagesEvent, type EventData, type JsonObject } from "../harness";
import { computeCostUsd, type PricingTable } from "../model-runtime/cost";
import { isRecord } from "../model-runtime/shared";

export interface CodexTokenUsage {
  inputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
  cachedInputTokens?: number;
  cacheWriteInputTokens?: number;
  reasoningOutputTokens?: number;
}

export function accumulateCodexUsage(
  current: CodexTokenUsage | null,
  entry: CodexProtocolLogEntry,
): CodexTokenUsage | null {
  const message = entry.message;
  if (
    entry.direction !== "server_to_client" ||
    !isRecord(message) ||
    message.method !== "rawResponse/completed" ||
    !isRecord(message.params)
  )
    return current;
  const last = message.params.usage;
  if (!isRecord(last)) return current;
  const input = tokenCount(last.inputTokens);
  const total = tokenCount(last.totalTokens);
  const output = tokenCount(last.outputTokens);
  if (input === undefined || total === undefined || output === undefined)
    return current;
  return {
    inputTokens: add(current?.inputTokens, input),
    outputTokens: add(current?.outputTokens, output),
    totalTokens: add(current?.totalTokens, total),
    cachedInputTokens: add(
      current?.cachedInputTokens,
      tokenCount(last.cachedInputTokens),
    ),
    cacheWriteInputTokens: add(
      current?.cacheWriteInputTokens,
      tokenCount(last.cacheWriteInputTokens),
    ),
    reasoningOutputTokens: add(
      current?.reasoningOutputTokens,
      tokenCount(last.reasoningOutputTokens),
    ),
  };
}

function tokenCount(value: unknown): number | undefined {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0
    ? value
    : undefined;
}

function add(
  current: number | undefined,
  next: number | undefined,
): number | undefined {
  return next === undefined ? current : (current ?? 0) + next;
}

export function codexUsageEvent(
  model: string,
  usage: CodexTokenUsage,
  table: PricingTable | null,
): EventData {
  const record: JsonObject = { model };
  if (usage.inputTokens !== undefined) record.prompt_tokens = usage.inputTokens;
  if (usage.outputTokens !== undefined)
    record.completion_tokens = usage.outputTokens;
  if (usage.cachedInputTokens !== undefined) {
    record.prompt_cached_tokens = usage.cachedInputTokens;
  }
  if (usage.cacheWriteInputTokens !== undefined) {
    record.prompt_cache_creation_tokens = usage.cacheWriteInputTokens;
  }
  if (usage.reasoningOutputTokens !== undefined) {
    record.completion_reasoning_tokens = usage.reasoningOutputTokens;
  }
  const cost =
    table && usage.inputTokens !== undefined && usage.outputTokens !== undefined
      ? computeCostUsd(table, model, {
          prompt: usage.inputTokens,
          completion: usage.outputTokens,
          cached: usage.cachedInputTokens,
          cacheCreation: usage.cacheWriteInputTokens,
        })
      : null;
  if (cost !== null) record.cost_usd = cost;
  return messagesEvent([], undefined, record);
}
