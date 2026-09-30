import {
  messagesEvent,
  toolRequestedEvent,
  type EventData,
  type JsonObject,
  type Message,
  type PendingToolCall,
} from "../harness";
import { computeCostUsd, getTable, type PricingTable } from "./cost";

export interface ModelUsage {
  promptTokens?: number;
  completionTokens?: number;
  promptCachedTokens?: number;
  promptCacheCreationTokens?: number;
  completionReasoningTokens?: number;
  providerCostUsd?: number;
  ttftMs?: number;
  durationMs?: number;
}

export interface ModelOutput {
  messages: Message[];
  responseId?: string;
  toolCalls?: PendingToolCall[];
  usage?: JsonObject;
}

export function modelUsageRecord(
  model: string,
  usage: ModelUsage,
  pricing: PricingTable | null = getTable(),
): JsonObject {
  const cost =
    usage.providerCostUsd ??
    (pricing && (usage.promptTokens != null || usage.completionTokens != null)
      ? computeCostUsd(pricing, model, {
          prompt: usage.promptTokens,
          completion: usage.completionTokens,
          cached: usage.promptCachedTokens,
          cacheCreation: usage.promptCacheCreationTokens,
        })
      : null);
  const record: JsonObject = { model };
  const fields: Array<[string, number | undefined | null]> = [
    ["prompt_tokens", usage.promptTokens],
    ["completion_tokens", usage.completionTokens],
    ["prompt_cached_tokens", usage.promptCachedTokens],
    ["prompt_cache_creation_tokens", usage.promptCacheCreationTokens],
    ["completion_reasoning_tokens", usage.completionReasoningTokens],
    ["cost_usd", cost],
    ["ttft_ms", usage.ttftMs],
    ["duration_ms", usage.durationMs],
  ];
  for (const [name, value] of fields) {
    if (value != null) record[name] = value;
  }
  return record;
}

export function modelResponseEvents(output: ModelOutput): EventData[] {
  const events: EventData[] = [];
  if (output.messages.length > 0 || output.usage) {
    events.push(
      messagesEvent(output.messages, output.responseId, output.usage),
    );
  }
  for (const toolCall of output.toolCalls ?? []) {
    events.push(toolRequestedEvent(toolCall, output.responseId));
  }
  return events;
}
