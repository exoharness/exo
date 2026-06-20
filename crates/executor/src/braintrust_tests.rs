use std::time::Duration;

use lingua::UniversalUsage;

use crate::braintrust::llm_metrics;

#[test]
fn llm_metrics_use_braintrust_metric_names() {
    let usage = UniversalUsage {
        prompt_tokens: Some(11),
        completion_tokens: Some(7),
        prompt_cached_tokens: Some(3),
        prompt_cache_creation_tokens: Some(2),
        completion_reasoning_tokens: Some(5),
        ..Default::default()
    };

    let metrics = llm_metrics(Some(&usage), Some(Duration::from_millis(250)));

    assert_eq!(metrics.get("prompt_tokens"), Some(&11.0));
    assert_eq!(metrics.get("completion_tokens"), Some(&7.0));
    assert_eq!(metrics.get("tokens"), Some(&18.0));
    assert_eq!(metrics.get("prompt_cached_tokens"), Some(&3.0));
    assert_eq!(metrics.get("prompt_cache_creation_tokens"), Some(&2.0));
    assert_eq!(metrics.get("completion_reasoning_tokens"), Some(&5.0));
    assert_eq!(metrics.get("time_to_first_token"), Some(&0.25));
    assert!(!metrics.contains_key("time_to_first_token_ms"));
    assert!(!metrics.contains_key("duration_ms"));
}

#[test]
fn llm_metrics_do_not_emit_duration_when_usage_is_absent() {
    let metrics = llm_metrics(None, None);

    assert!(metrics.is_empty());
}
