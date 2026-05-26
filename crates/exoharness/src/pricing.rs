//! Pure data + computation for per-message LLM cost.
//!
//! This module deserializes the
//! [LiteLLM pricing database](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json)
//! (used here as the canonical, frequently-updated source of model rates
//! across providers) and computes per-call cost given a model name and
//! token counts.
//!
//! It is intentionally network-free; loading the JSON (from cache, env
//! override, or HTTP fetch) is the responsibility of the consuming crate.
//!
//! ## Why per-provider math is necessary
//!
//! Providers disagree on what `prompt_tokens` *includes*:
//!
//! - **Anthropic-family** (`anthropic`, `bedrock_converse`, `vertex_ai-*`,
//!   `azure_ai` for Claude models): `prompt_tokens` is the *fresh*
//!   (non-cached) input only. `prompt_cached_tokens` and
//!   `prompt_cache_creation_tokens` are reported separately. Cost is
//!   additive across the three.
//!
//! - **OpenAI-family** (`openai`, `mistral`, et al.): `prompt_tokens` is
//!   the *total* input including any cache hits. `prompt_cached_tokens`
//!   is a subset that must be subtracted to avoid double-billing.
//!
//! Mixing these conventions silently over- or under-counts cost by a
//! large factor on cached responses (cache hits are typically 10× cheaper
//! than fresh input).

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelEntry {
    /// Provider classification used to pick the cost formula. Examples:
    /// `"anthropic"`, `"openai"`, `"bedrock_converse"`,
    /// `"vertex_ai-anthropic_models"`, `"azure_ai"`, `"mistral"`.
    #[serde(default)]
    pub litellm_provider: Option<String>,
    /// `chat`, `embedding`, `image_generation`, etc. We only price `chat`
    /// entries here; others are loaded but lookups for non-chat usage
    /// return `None`.
    #[serde(default)]
    pub mode: Option<String>,

    #[serde(default)]
    pub input_cost_per_token: Option<f64>,
    #[serde(default)]
    pub output_cost_per_token: Option<f64>,
    /// Per-token rate for cache *reads* (Anthropic discounted tier;
    /// OpenAI cached input).
    #[serde(default)]
    pub cache_read_input_token_cost: Option<f64>,
    /// Per-token rate for cache *writes* — the surcharge for the request
    /// that populates a cache entry. Anthropic-specific; absent on OpenAI.
    #[serde(default)]
    pub cache_creation_input_token_cost: Option<f64>,
    /// Anthropic 1-hour cache tier (more expensive than the default 5-min
    /// write rate). Currently unused — we default to the 5-minute rate
    /// because lingua does not surface which tier was hit.
    #[serde(default)]
    pub cache_creation_input_token_cost_above_1hr: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    entries: HashMap<String, ModelEntry>,
}

impl PricingTable {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a LiteLLM `model_prices_and_context_window.json` document.
    ///
    /// Unknown fields and the `sample_spec` doc entry are silently
    /// ignored; entries that don't deserialize into [`ModelEntry`]
    /// (e.g., embedding-only entries with no per-token rates) are
    /// included but will simply produce `None` from [`Self::compute_cost_usd`].
    pub fn from_json_str(s: &str) -> anyhow::Result<Self> {
        let raw: HashMap<String, serde_json::Value> = serde_json::from_str(s)?;
        let mut entries = HashMap::with_capacity(raw.len());
        for (key, value) in raw {
            if key == "sample_spec" {
                continue;
            }
            if let Ok(entry) = serde_json::from_value::<ModelEntry>(value) {
                entries.insert(key, entry);
            }
        }
        Ok(Self { entries })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve a model name to an entry. Matches exact key first; falls
    /// back to the longest entry-key that is a prefix of `model`, so
    /// dated revisions like `claude-sonnet-4-6-20251022` resolve to
    /// `claude-sonnet-4-6`.
    pub fn lookup(&self, model: &str) -> Option<&ModelEntry> {
        if let Some(entry) = self.entries.get(model) {
            return Some(entry);
        }
        self.entries
            .iter()
            .filter(|(key, _)| model.starts_with(key.as_str()))
            .max_by_key(|(key, _)| key.len())
            .map(|(_, entry)| entry)
    }

    /// Compute USD cost for a single call. Returns `None` when the model
    /// is unknown or the entry has no `input_cost_per_token` (e.g., a
    /// non-chat entry).
    pub fn compute_cost_usd(&self, model: &str, tokens: TokenCounts) -> Option<f64> {
        let entry = self.lookup(model)?;
        let input_rate = entry.input_cost_per_token?;
        let output_rate = entry.output_cost_per_token.unwrap_or(0.0);
        // For cache rates, fall back to the input rate if the provider
        // doesn't publish a separate cached tier.
        let cache_read_rate = entry.cache_read_input_token_cost.unwrap_or(input_rate);
        let cache_creation_rate = entry.cache_creation_input_token_cost.unwrap_or(input_rate);

        let prompt = tokens.prompt.unwrap_or(0).max(0) as f64;
        let completion = tokens.completion.unwrap_or(0).max(0) as f64;
        let cached = tokens.prompt_cached.unwrap_or(0).max(0) as f64;
        let cache_creation = tokens.prompt_cache_creation.unwrap_or(0).max(0) as f64;

        let style = ProviderStyle::for_litellm_provider(entry.litellm_provider.as_deref());

        let cost = match style {
            ProviderStyle::Additive => {
                prompt * input_rate
                    + cached * cache_read_rate
                    + cache_creation * cache_creation_rate
                    + completion * output_rate
            }
            ProviderStyle::Inclusive => {
                let non_cached = (prompt - cached).max(0.0);
                non_cached * input_rate
                    + cached * cache_read_rate
                    + cache_creation * cache_creation_rate
                    + completion * output_rate
            }
        };
        Some(cost)
    }
}

/// Token counts from a single API response, in lingua's
/// provider-agnostic shape.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenCounts {
    pub prompt: Option<i64>,
    pub completion: Option<i64>,
    pub prompt_cached: Option<i64>,
    pub prompt_cache_creation: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderStyle {
    /// Anthropic-family: `prompt_tokens` excludes cached. Bill all three
    /// buckets separately.
    Additive,
    /// OpenAI-family: `prompt_tokens` includes cached. Subtract cached
    /// from prompt before billing fresh-input rate.
    Inclusive,
}

impl ProviderStyle {
    fn for_litellm_provider(provider: Option<&str>) -> Self {
        // LiteLLM uses these strings (see model_prices_and_context_window.json
        // values for litellm_provider). Anything not classified here defaults
        // to Inclusive — same convention as OpenAI, which is the safer of the
        // two when cached==0 (the typical case).
        match provider {
            Some(p) if p.starts_with("anthropic") => Self::Additive,
            Some(p) if p.starts_with("bedrock") => Self::Additive,
            Some(p) if p.starts_with("vertex_ai-anthropic") => Self::Additive,
            Some("azure_ai") => Self::Additive, // azure_ai is used for Claude on Azure
            _ => Self::Inclusive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny vendored slice of LiteLLM's JSON, just enough to exercise the
    // accounting formulas. Rates are taken from real entries so the
    // assertions correspond to numbers you can verify on each provider's
    // pricing page.
    const FIXTURE: &str = r#"{
        "sample_spec": {
            "comment": "doc entry, must be ignored"
        },
        "claude-sonnet-4-6": {
            "litellm_provider": "anthropic",
            "mode": "chat",
            "input_cost_per_token": 3e-06,
            "output_cost_per_token": 1.5e-05,
            "cache_read_input_token_cost": 3e-07,
            "cache_creation_input_token_cost": 3.75e-06
        },
        "gpt-4o-mini": {
            "litellm_provider": "openai",
            "mode": "chat",
            "input_cost_per_token": 1.5e-07,
            "output_cost_per_token": 6e-07,
            "cache_read_input_token_cost": 7.5e-08
        },
        "us.anthropic.claude-sonnet-4-6": {
            "litellm_provider": "bedrock_converse",
            "mode": "chat",
            "input_cost_per_token": 3.3e-06,
            "output_cost_per_token": 1.65e-05,
            "cache_read_input_token_cost": 3.3e-07,
            "cache_creation_input_token_cost": 4.125e-06
        }
    }"#;

    fn approx(a: f64, b: f64) {
        let rel = (a - b).abs() / b.abs().max(1e-12);
        assert!(rel < 1e-9, "expected {b}, got {a} (rel diff {rel})");
    }

    fn table() -> PricingTable {
        PricingTable::from_json_str(FIXTURE).expect("fixture parses")
    }

    #[test]
    fn empty_table_returns_none() {
        let cost = PricingTable::empty().compute_cost_usd(
            "claude-sonnet-4-6",
            TokenCounts {
                prompt: Some(100),
                completion: Some(50),
                ..Default::default()
            },
        );
        assert!(cost.is_none());
    }

    #[test]
    fn skips_sample_spec_doc_entry() {
        let t = table();
        assert!(t.lookup("sample_spec").is_none());
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn longest_prefix_match() {
        let t = table();
        let entry = t.lookup("claude-sonnet-4-6-20251022").expect("prefix");
        // Should match the direct anthropic entry, not the bedrock one
        // (because "claude-sonnet-4-6" is a prefix of the input but
        // "us.anthropic.claude-sonnet-4-6" is not).
        assert_eq!(entry.litellm_provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn anthropic_additive_no_cache() {
        // 1000 prompt @ $3/M + 500 completion @ $15/M = $0.003 + $0.0075 = $0.0105
        let cost = table()
            .compute_cost_usd(
                "claude-sonnet-4-6",
                TokenCounts {
                    prompt: Some(1_000),
                    completion: Some(500),
                    ..Default::default()
                },
            )
            .expect("cost");
        approx(cost, 0.0105);
    }

    #[test]
    fn anthropic_additive_with_cache_hits() {
        // For Anthropic, prompt_tokens excludes cached. So 500 fresh + 10k
        // cached read + 0 cache_creation + 200 completion:
        //   500   * 3e-06  = 0.0015
        //   10000 * 3e-07  = 0.003
        //   200   * 1.5e-05 = 0.003
        // total = 0.0075
        let cost = table()
            .compute_cost_usd(
                "claude-sonnet-4-6",
                TokenCounts {
                    prompt: Some(500),
                    completion: Some(200),
                    prompt_cached: Some(10_000),
                    prompt_cache_creation: None,
                },
            )
            .expect("cost");
        approx(cost, 0.0075);
    }

    #[test]
    fn anthropic_additive_with_cache_creation() {
        // 0 fresh + 0 cached + 5000 cache_creation + 100 completion:
        //   5000 * 3.75e-06 = 0.01875
        //   100  * 1.5e-05  = 0.0015
        // total = 0.02025
        let cost = table()
            .compute_cost_usd(
                "claude-sonnet-4-6",
                TokenCounts {
                    prompt: Some(0),
                    completion: Some(100),
                    prompt_cached: None,
                    prompt_cache_creation: Some(5_000),
                },
            )
            .expect("cost");
        approx(cost, 0.02025);
    }

    #[test]
    fn openai_inclusive_subtracts_cached_from_prompt() {
        // For OpenAI, prompt_tokens=2000 includes the 500 cached. So:
        //   non_cached = 2000 - 500 = 1500
        //   1500 * 1.5e-07 = 0.000225
        //   500  * 7.5e-08 = 0.0000375
        //   1000 * 6e-07   = 0.0006
        // total = 0.0008625
        let cost = table()
            .compute_cost_usd(
                "gpt-4o-mini",
                TokenCounts {
                    prompt: Some(2_000),
                    completion: Some(1_000),
                    prompt_cached: Some(500),
                    prompt_cache_creation: None,
                },
            )
            .expect("cost");
        approx(cost, 0.0008625);
    }

    #[test]
    fn openai_inclusive_collapses_to_naive_when_no_cache() {
        // 1000 prompt @ $0.15/M + 500 completion @ $0.60/M
        //   = 1.5e-4 + 3e-4 = 4.5e-4
        let cost = table()
            .compute_cost_usd(
                "gpt-4o-mini",
                TokenCounts {
                    prompt: Some(1_000),
                    completion: Some(500),
                    ..Default::default()
                },
            )
            .expect("cost");
        approx(cost, 0.00045);
    }

    #[test]
    fn bedrock_uses_additive_with_regional_surcharge() {
        // us.anthropic.claude-sonnet-4-6 has 10% higher rates than the direct
        // Anthropic entry: 1000 prompt @ $3.30/M + 500 completion @ $16.50/M.
        let cost = table()
            .compute_cost_usd(
                "us.anthropic.claude-sonnet-4-6",
                TokenCounts {
                    prompt: Some(1_000),
                    completion: Some(500),
                    ..Default::default()
                },
            )
            .expect("cost");
        approx(cost, 0.01155);
    }

    #[test]
    fn unknown_model_returns_none() {
        assert!(
            table()
                .compute_cost_usd(
                    "acme-llm-9000",
                    TokenCounts {
                        prompt: Some(100),
                        completion: Some(50),
                        ..Default::default()
                    },
                )
                .is_none()
        );
    }

    #[test]
    fn provider_style_classification() {
        assert_eq!(
            ProviderStyle::for_litellm_provider(Some("anthropic")),
            ProviderStyle::Additive
        );
        assert_eq!(
            ProviderStyle::for_litellm_provider(Some("bedrock_converse")),
            ProviderStyle::Additive
        );
        assert_eq!(
            ProviderStyle::for_litellm_provider(Some("vertex_ai-anthropic_models")),
            ProviderStyle::Additive
        );
        assert_eq!(
            ProviderStyle::for_litellm_provider(Some("openai")),
            ProviderStyle::Inclusive
        );
        assert_eq!(
            ProviderStyle::for_litellm_provider(Some("mistral")),
            ProviderStyle::Inclusive
        );
        assert_eq!(
            ProviderStyle::for_litellm_provider(None),
            ProviderStyle::Inclusive
        );
    }
}
