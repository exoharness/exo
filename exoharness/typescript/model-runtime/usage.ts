import type { UniversalUsage } from "@braintrust/lingua-types";

import { toJsonObject, type JsonObject } from "../harness";
import { computeCostUsd, getTable, type PricingTable } from "./cost";

export function modelUsageRecord(
  model: string,
  usage: UniversalUsage & { cost_usd?: number },
  pricing: PricingTable | null = getTable(),
): JsonObject {
  const cost =
    usage.cost_usd ??
    (pricing && (usage.prompt_tokens != null || usage.completion_tokens != null)
      ? computeCostUsd(pricing, model, {
          prompt: usage.prompt_tokens,
          completion: usage.completion_tokens,
          cached: usage.prompt_cached_tokens,
          cacheCreation: usage.prompt_cache_creation_tokens,
        })
      : null);
  return toJsonObject({
    model,
    ...usage,
    ...(cost != null ? { cost_usd: cost } : {}),
  });
}
