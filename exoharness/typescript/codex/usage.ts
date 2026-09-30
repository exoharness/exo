import { type CodexProtocolLogEntry } from "./app-server";
import { messagesEvent, type EventData } from "../harness";
import { type PricingTable } from "../model-runtime/cost";
import { modelUsageRecord } from "../model-runtime/usage";
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
  return messagesEvent(
    [],
    undefined,
    modelUsageRecord(
      model,
      {
        prompt_tokens: usage.inputTokens,
        completion_tokens: usage.outputTokens,
        prompt_cached_tokens: usage.cachedInputTokens,
        prompt_cache_creation_tokens: usage.cacheWriteInputTokens,
        completion_reasoning_tokens: usage.reasoningOutputTokens,
      },
      table,
    ),
  );
}
