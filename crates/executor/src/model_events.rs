use cost::{PricingTable, TokenCounts};
use exoharness::{EventData, UsageRecord};

use crate::ModelResponse;

pub(crate) fn model_response_events(
    response: ModelResponse,
    pricing: &PricingTable,
) -> Vec<EventData> {
    let mut events = Vec::new();

    let usage = build_usage_record(&response, pricing);
    if !response.messages.is_empty() || usage.is_some() {
        events.push(EventData::Messages {
            messages: response.messages,
            response_id: response.response_id,
            usage,
        });
    }

    for tool_call in response.tool_calls {
        events.push(EventData::ToolRequested {
            tool_call_id: tool_call.tool_call_id,
            response_id: response.response_id,
            request: tool_call.request,
        });
    }

    events
}

fn build_usage_record(
    response: &ModelResponse,
    pricing: &PricingTable,
) -> Option<Box<UsageRecord>> {
    // Production completions always include duration; fakes may omit all metadata.
    let usage = response.usage.as_ref();
    let has_timing = response.ttft.is_some() || response.duration.is_some();
    if usage.is_none() && !has_timing && response.provider_cost_usd.is_none() {
        return None;
    }

    let model = response.model.clone().unwrap_or_default();
    // Prefer the provider-reported cost (e.g. OpenRouter's `usage.cost`); fall
    // back to the local price-table estimate when the provider doesn't send one.
    let cost_usd = response.provider_cost_usd.or_else(|| {
        let usage = usage.filter(|_| !model.is_empty())?;
        pricing.compute_cost_usd(
            &model,
            TokenCounts {
                prompt: usage.prompt_tokens,
                completion: usage.completion_tokens,
                prompt_cached: usage.prompt_cached_tokens,
                prompt_cache_creation: usage.prompt_cache_creation_tokens,
            },
        )
    });

    Some(Box::new(UsageRecord {
        model,
        prompt_tokens: usage.and_then(|u| u.prompt_tokens),
        completion_tokens: usage.and_then(|u| u.completion_tokens),
        prompt_cached_tokens: usage.and_then(|u| u.prompt_cached_tokens),
        prompt_cache_creation_tokens: usage.and_then(|u| u.prompt_cache_creation_tokens),
        completion_reasoning_tokens: usage.and_then(|u| u.completion_reasoning_tokens),
        cost_usd,
        ttft_ms: response.ttft.map(|d| d.as_millis() as u64),
        duration_ms: response.duration.map(|d| d.as_millis() as u64),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_only_responses_retain_usage_and_provider_cost() {
        let response = ModelResponse {
            messages: vec![],
            response_id: None,
            tool_calls: vec![crate::PendingToolCall {
                tool_call_id: "call".into(),
                request: exoharness::ToolRequest {
                    function_name: "shell".into(),
                    arguments: Default::default(),
                    namespace: None,
                },
            }],
            usage: Some(lingua::UniversalUsage {
                prompt_tokens: Some(12),
                completion_tokens: Some(3),
                ..Default::default()
            }),
            model: Some("model".into()),
            ttft: None,
            duration: None,
            provider_cost_usd: Some(0.0),
        };
        let events = model_response_events(response, &PricingTable::empty());
        assert_eq!(events.len(), 2);
        let EventData::Messages {
            messages,
            usage: Some(usage),
            ..
        } = &events[0]
        else {
            panic!("tool-only responses must retain usage");
        };
        assert!(messages.is_empty());
        assert_eq!(usage.prompt_tokens, Some(12));
        assert_eq!(usage.cost_usd, Some(0.0));
        assert!(
            matches!(&events[1], EventData::ToolRequested { tool_call_id, .. } if tool_call_id == "call")
        );
    }
}
