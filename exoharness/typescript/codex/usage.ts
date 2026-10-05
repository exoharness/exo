import { messagesEvent, type EventData } from "../harness";
import { type PricingTable } from "../model-runtime/cost";
import { modelUsageRecord } from "../model-runtime/usage";

export interface CodexTokenUsage {
  inputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
  cachedInputTokens?: number;
  cacheWriteInputTokens?: number;
  reasoningOutputTokens?: number;
}

export interface CodexTokenUsageUpdate {
  threadId?: string;
  turnId?: string;
  tokenUsage?: { last?: CodexTokenUsage; total?: CodexTokenUsage };
}

// Record only notifications already filtered to the active native turn. `last`
// is one model call; `total` includes earlier turns and also identifies repeated
// notifications. Raw response events are not emitted by resumed app-servers.
export class CodexUsageAccumulator {
  value: CodexTokenUsage | null = null;
  private lastThreadTotal: number | undefined;

  record(params: CodexTokenUsageUpdate | null): void {
    const { last, total } = params?.tokenUsage ?? {};
    if (!last || !total) return;
    const input = tokenCount(last.inputTokens);
    const output = tokenCount(last.outputTokens);
    const tokens = tokenCount(last.totalTokens);
    const threadTotal = tokenCount(total.totalTokens);
    if (
      input === undefined ||
      output === undefined ||
      tokens === undefined ||
      threadTotal === undefined ||
      (this.lastThreadTotal !== undefined &&
        threadTotal <= this.lastThreadTotal)
    )
      return;
    this.lastThreadTotal = threadTotal;
    const current = this.value;
    this.value = {
      inputTokens: add(current?.inputTokens, input),
      outputTokens: add(current?.outputTokens, output),
      totalTokens: add(current?.totalTokens, tokens),
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
