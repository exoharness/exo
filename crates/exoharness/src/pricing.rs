//! Per-model price table and cost computation.
//!
//! Rates are stored as USD per 1M tokens and the cost is computed at the
//! moment the API call completes, so the recorded value reflects the price
//! that applied then — not whatever the rate happens to be when the event
//! log is later read.
//!
//! Upstream provider APIs (Anthropic, OpenAI, Google, Bedrock) return token
//! counts in their `usage` block but not dollar amounts; cost is always a
//! local computation. The table here is intentionally small and easy to
//! grow; unknown models simply produce `None` for cost (tokens are still
//! recorded).

#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cached_input_per_million: Option<f64>,
    pub cache_creation_per_million: Option<f64>,
}

/// Pricing entries keyed by canonical model id. Lookups also accept any
/// model name that starts with one of these keys (the longest matching
/// prefix wins), which covers dated revisions like
/// `claude-sonnet-4-6-20251022`.
const PRICING_TABLE: &[(&str, ModelPricing)] = &[
    // Anthropic Claude 4.x
    (
        "claude-opus-4-7",
        ModelPricing {
            input_per_million: 15.0,
            output_per_million: 75.0,
            cached_input_per_million: Some(1.50),
            cache_creation_per_million: Some(18.75),
        },
    ),
    (
        "claude-sonnet-4-6",
        ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.30),
            cache_creation_per_million: Some(3.75),
        },
    ),
    (
        "claude-haiku-4-5",
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cached_input_per_million: Some(0.10),
            cache_creation_per_million: Some(1.25),
        },
    ),
    // OpenAI
    (
        "gpt-4o-mini",
        ModelPricing {
            input_per_million: 0.15,
            output_per_million: 0.60,
            cached_input_per_million: Some(0.075),
            cache_creation_per_million: None,
        },
    ),
    (
        "gpt-4o",
        ModelPricing {
            input_per_million: 2.50,
            output_per_million: 10.0,
            cached_input_per_million: Some(1.25),
            cache_creation_per_million: None,
        },
    ),
    (
        "o3-mini",
        ModelPricing {
            input_per_million: 1.10,
            output_per_million: 4.40,
            cached_input_per_million: Some(0.55),
            cache_creation_per_million: None,
        },
    ),
    (
        "o1",
        ModelPricing {
            input_per_million: 15.0,
            output_per_million: 60.0,
            cached_input_per_million: Some(7.50),
            cache_creation_per_million: None,
        },
    ),
];

pub fn lookup(model: &str) -> Option<&'static ModelPricing> {
    if let Some((_, pricing)) = PRICING_TABLE.iter().find(|(name, _)| *name == model) {
        return Some(pricing);
    }
    PRICING_TABLE
        .iter()
        .filter(|(name, _)| model.starts_with(name))
        .max_by_key(|(name, _)| name.len())
        .map(|(_, pricing)| pricing)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCounts {
    pub prompt: Option<i64>,
    pub completion: Option<i64>,
    pub prompt_cached: Option<i64>,
    pub prompt_cache_creation: Option<i64>,
}

/// Compute the dollar cost of a single API call from its token counts.
///
/// Returns `None` if the model is not in the pricing table.
///
/// Accounting assumption: `prompt`, `prompt_cached`, and
/// `prompt_cache_creation` are treated as additive line items, each billed
/// at its own rate. This matches Anthropic's published billing model
/// (where `prompt_tokens` reports only fresh, non-cached input). Providers
/// that instead report cached tokens as a subset of `prompt_tokens` (some
/// OpenAI accountings) will slightly over-count by `cached × input_rate`;
/// this is acceptable for now and can be refined per-provider later.
pub fn compute_cost_usd(model: &str, tokens: TokenCounts) -> Option<f64> {
    let pricing = lookup(model)?;
    let prompt = tokens.prompt.unwrap_or(0).max(0) as f64;
    let completion = tokens.completion.unwrap_or(0).max(0) as f64;
    let cached = tokens.prompt_cached.unwrap_or(0).max(0) as f64;
    let cache_creation = tokens.prompt_cache_creation.unwrap_or(0).max(0) as f64;

    let mut cost = prompt / 1_000_000.0 * pricing.input_per_million
        + completion / 1_000_000.0 * pricing.output_per_million;

    let cached_rate = pricing
        .cached_input_per_million
        .unwrap_or(pricing.input_per_million);
    cost += cached / 1_000_000.0 * cached_rate;

    let creation_rate = pricing
        .cache_creation_per_million
        .unwrap_or(pricing.input_per_million);
    cost += cache_creation / 1_000_000.0 * creation_rate;

    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn lookup_finds_exact_match() {
        let pricing = lookup("claude-sonnet-4-6").expect("known model");
        assert!(approx(pricing.input_per_million, 3.0));
        assert!(approx(pricing.output_per_million, 15.0));
    }

    #[test]
    fn lookup_finds_longest_prefix() {
        // dated revision should match the un-dated entry
        let pricing = lookup("claude-sonnet-4-6-20251022").expect("dated revision");
        assert!(approx(pricing.input_per_million, 3.0));
    }

    #[test]
    fn lookup_prefers_longer_prefix() {
        // gpt-4o-mini should not match the bare gpt-4o entry
        let pricing = lookup("gpt-4o-mini-2024-07-18").expect("mini revision");
        assert!(approx(pricing.input_per_million, 0.15));
    }

    #[test]
    fn lookup_returns_none_for_unknown_model() {
        assert!(lookup("acme-llm-7000").is_none());
    }

    #[test]
    fn compute_cost_basic() {
        let cost = compute_cost_usd(
            "claude-sonnet-4-6",
            TokenCounts {
                prompt: Some(1_000_000),
                completion: Some(1_000_000),
                prompt_cached: None,
                prompt_cache_creation: None,
            },
        )
        .expect("known model");
        // 1M prompt @ $3 + 1M completion @ $15 = $18
        assert!(approx(cost, 18.0));
    }

    #[test]
    fn compute_cost_includes_cached_at_discounted_rate() {
        let cost = compute_cost_usd(
            "claude-sonnet-4-6",
            TokenCounts {
                prompt: Some(0),
                completion: Some(0),
                prompt_cached: Some(1_000_000),
                prompt_cache_creation: Some(1_000_000),
            },
        )
        .expect("known model");
        // 1M cached @ $0.30 + 1M cache_creation @ $3.75 = $4.05
        assert!(approx(cost, 4.05));
    }

    #[test]
    fn compute_cost_handles_missing_cached_rate() {
        // gpt-4o has no cache_creation_per_million; should fall back to input rate.
        let cost = compute_cost_usd(
            "gpt-4o",
            TokenCounts {
                prompt: Some(0),
                completion: Some(0),
                prompt_cached: Some(0),
                prompt_cache_creation: Some(1_000_000),
            },
        )
        .expect("known model");
        // 1M cache_creation @ input rate $2.50 = $2.50
        assert!(approx(cost, 2.50));
    }

    #[test]
    fn compute_cost_returns_none_for_unknown_model() {
        assert!(
            compute_cost_usd(
                "acme-llm-7000",
                TokenCounts {
                    prompt: Some(100),
                    completion: Some(50),
                    prompt_cached: None,
                    prompt_cache_creation: None,
                }
            )
            .is_none()
        );
    }
}
