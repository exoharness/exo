use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::{
    ModelClient, ModelRequest, ModelResponse, ModelResponseStream, PendingToolCall, SendRequest,
};
use anyhow::anyhow;
use async_trait::async_trait;
use exoharness::{
    AddEventsRequest, BasicExoHarness, Binding, EventData, EventKind, EventQuery,
    EventQueryDirection, ExoHarness, FileSystemMount, FileSystemMountMode, PutSecretRequest,
    Result, SandboxAttachment, SandboxProvider, Secret, ToolRequest, Uuid7,
};
use lingua::universal::{AssistantContent, UserContent};
use lingua::{Message, UniversalStreamChunk, UniversalUsage};
use serde_json::{Map, Value};
use tempfile::TempDir;

use crate::test_support::local_test_config;
use crate::{
    BasicToolRuntime, ConversationModelConfig, CreateAgentRequest, CreateConversationRequest,
    LocalProvider, Runtime, harness_tool::ensure_shell_sandbox,
};

#[tokio::test(flavor = "current_thread")]
async fn creates_agents_and_conversations_with_persisted_config() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: Some("agent-image".to_string()),
            sandbox_provider: SandboxProvider::Docker,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: Some(512),
            max_tool_round_trips: Some(3),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(
            &*agent,
            CreateConversationRequest {
                slug: Some("session".to_string()),
                name: Some("Session".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("conversation should be created");

    let stored_agent = harness
        .get_agent("demo")
        .await
        .expect("get agent should succeed")
        .expect("agent should exist");
    let stored_conversation = harness
        .get_conversation(stored_agent.as_ref(), "session")
        .await
        .expect("get conversation should succeed")
        .expect("conversation should exist");

    assert_eq!(stored_agent.record().slug, "demo");
    assert_eq!(
        crate::load_agent_config(&*stored_agent)
            .await
            .expect("agent config")
            .model,
        "gpt-5.4"
    );
    let stored_conversation_config = crate::load_conversation_config(&*stored_conversation)
        .await
        .expect("conversation config");
    assert_eq!(
        stored_conversation_config.shell_program,
        Some("/bin/bash".to_string())
    );
    assert_eq!(
        stored_conversation_config.sandbox_image,
        Some("agent-image".to_string())
    );
    assert_eq!(
        stored_conversation_config.sandbox_provider,
        Some(SandboxProvider::Docker)
    );
    assert_eq!(conversation.record().slug, "session");
}

#[tokio::test(flavor = "current_thread")]
async fn send_persists_messages_through_harness() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::new(FakeModelClient::new(vec![ModelResponse {
                provider_cost_usd: None,
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("pong")],
                tool_calls: Vec::new(),
                usage: None,
                model: None,
                ttft: None,
                duration: None,
            }])),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("ping")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    let messages = crate::materialize_conversation_messages(&*conversation)
        .await
        .expect("messages should load");
    assert_eq!(messages.len(), 2);
    assert!(matches!(messages[0], Message::User { .. }));
    assert!(matches!(messages[1], Message::Assistant { .. }));

    let sandbox_events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("sandbox events should load")
        .events;
    assert!(
        sandbox_events.is_empty(),
        "plain chat should not provision a sandbox"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn usage_record_is_persisted_with_computed_cost() {
    // Use an inline LiteLLM-schema fixture so the assertion is hermetic
    // and doesn't depend on whatever rates the upstream JSON happens to
    // ship today.
    const PRICING_FIXTURE: &str = r#"{
        "claude-sonnet-4-6": {
            "litellm_provider": "anthropic",
            "mode": "chat",
            "input_cost_per_token": 3e-06,
            "output_cost_per_token": 1.5e-05,
            "cache_read_input_token_cost": 3e-07,
            "cache_creation_input_token_cost": 3.75e-06
        }
    }"#;
    let pricing =
        Arc::new(cost::PricingTable::from_json_str(PRICING_FIXTURE).expect("fixture should parse"));

    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&exoharness),
            Arc::new(FakeModelClient::new(vec![ModelResponse {
                provider_cost_usd: None,
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("pong")],
                tool_calls: Vec::new(),
                usage: Some(UniversalUsage {
                    prompt_tokens: Some(1_000),
                    completion_tokens: Some(500),
                    prompt_cached_tokens: None,
                    prompt_cache_creation_tokens: None,
                    completion_reasoning_tokens: None,
                    ..Default::default()
                }),
                model: Some("claude-sonnet-4-6".to_string()),
                ttft: None,
                duration: None,
            }])),
            Arc::new(BasicToolRuntime),
            pricing,
        ),
        None,
    );

    let secret_id = exoharness::vault::global_vault(exoharness.as_ref())
        .await
        .expect("runtime vault")
        .put_secret(PutSecretRequest {
            target: None,
            name: "cost-test-key".to_string(),
            secret: Secret::Key {
                value: "test-key".to_string(),
            },
        })
        .await
        .expect("test secret should register");
    exoharness
        .put_binding(Binding::Llm {
            name: "claude-sonnet-4-6".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            base_url: None,
            secret: Some(exoharness::vault::SecretReference {
                vault_id: exoharness::vault::global_vault(exoharness.as_ref())
                    .await
                    .unwrap()
                    .record()
                    .id,
                secret_id,
            }),
        })
        .await
        .expect("binding should register");

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "cost-demo".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "claude-sonnet-4-6".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("ping")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await
        .expect("get events should succeed")
        .events;

    let assistant_usage = events
        .iter()
        .find_map(|event| match &event.data {
            EventData::Messages {
                messages,
                usage: Some(usage),
                ..
            } if messages
                .iter()
                .any(|m| matches!(m, Message::Assistant { .. })) =>
            {
                Some(usage)
            }
            _ => None,
        })
        .expect("assistant message event should carry a UsageRecord");

    assert_eq!(assistant_usage.model, "claude-sonnet-4-6");
    assert_eq!(assistant_usage.prompt_tokens, Some(1_000));
    assert_eq!(assistant_usage.completion_tokens, Some(500));
    // 1000 prompt @ $3/M + 500 completion @ $15/M = $0.003 + $0.0075 = $0.0105
    let cost = assistant_usage.cost_usd.expect("cost should be computed");
    assert!(
        (cost - 0.0105).abs() < 1e-9,
        "expected cost ~0.0105, got {cost}"
    );
    // Non-streaming path measures total duration.
    assert!(
        assistant_usage.duration_ms.is_some(),
        "duration should be recorded"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn usage_record_with_anthropic_cache_hits() {
    // Anthropic accounting is additive: prompt_tokens is the fresh slice,
    // and cache_read / cache_creation are billed separately on top. The
    // pricing math is unit-tested in exoharness::pricing; this test is the
    // end-to-end proof that cached counts (a) reach the persisted
    // UsageRecord and (b) hit the discounted cache-read rate when
    // compute_cost_usd is invoked through the executor.
    const PRICING_FIXTURE: &str = r#"{
        "claude-sonnet-4-6": {
            "litellm_provider": "anthropic",
            "mode": "chat",
            "input_cost_per_token": 3e-06,
            "output_cost_per_token": 1.5e-05,
            "cache_read_input_token_cost": 3e-07,
            "cache_creation_input_token_cost": 3.75e-06
        }
    }"#;
    let pricing =
        Arc::new(cost::PricingTable::from_json_str(PRICING_FIXTURE).expect("fixture should parse"));

    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&exoharness),
            Arc::new(FakeModelClient::new(vec![ModelResponse {
                provider_cost_usd: None,
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("pong")],
                tool_calls: Vec::new(),
                usage: Some(UniversalUsage {
                    prompt_tokens: Some(500),
                    completion_tokens: Some(200),
                    prompt_cached_tokens: Some(10_000),
                    prompt_cache_creation_tokens: Some(2_000),
                    completion_reasoning_tokens: None,
                    ..Default::default()
                }),
                model: Some("claude-sonnet-4-6".to_string()),
                ttft: None,
                duration: None,
            }])),
            Arc::new(BasicToolRuntime),
            pricing,
        ),
        None,
    );

    let secret_id = exoharness::vault::global_vault(exoharness.as_ref())
        .await
        .expect("runtime vault")
        .put_secret(PutSecretRequest {
            target: None,
            name: "anthropic-cache-key".to_string(),
            secret: Secret::Key {
                value: "test-key".to_string(),
            },
        })
        .await
        .expect("test secret should register");
    exoharness
        .put_binding(Binding::Llm {
            name: "claude-sonnet-4-6".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            base_url: None,
            secret: Some(exoharness::vault::SecretReference {
                vault_id: exoharness::vault::global_vault(exoharness.as_ref())
                    .await
                    .unwrap()
                    .record()
                    .id,
                secret_id,
            }),
        })
        .await
        .expect("binding should register");

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "anthropic-cache".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "claude-sonnet-4-6".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("ping")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    let usage = assistant_usage_record(&conversation).await;

    assert_eq!(usage.model, "claude-sonnet-4-6");
    assert_eq!(usage.prompt_tokens, Some(500));
    assert_eq!(usage.completion_tokens, Some(200));
    assert_eq!(usage.prompt_cached_tokens, Some(10_000));
    assert_eq!(usage.prompt_cache_creation_tokens, Some(2_000));
    // Anthropic-style additive:
    //   500    fresh prompt    @ $3/M     = 0.0015
    //   10000  cache read      @ $0.30/M  = 0.003
    //   2000   cache creation  @ $3.75/M  = 0.0075
    //   200    completion      @ $15/M    = 0.003
    // total = 0.015
    let cost = usage.cost_usd.expect("cost should be computed");
    assert!(
        (cost - 0.015).abs() < 1e-9,
        "expected cost ~0.015, got {cost}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn usage_record_with_openai_inclusive_accounting() {
    // OpenAI accounting is inclusive: prompt_tokens is the *total* input
    // including any cache hits, so the executor must subtract
    // prompt_cached_tokens before billing the fresh-input rate. Getting
    // this wrong silently double-bills cached tokens. This test pins the
    // behavior at the conversation-log level — the same accounting that
    // pricing.rs exercises in isolation now also has to survive the round
    // trip through ModelResponse → UsageRecord → persisted event.
    const PRICING_FIXTURE: &str = r#"{
        "gpt-4o-mini": {
            "litellm_provider": "openai",
            "mode": "chat",
            "input_cost_per_token": 1.5e-07,
            "output_cost_per_token": 6e-07,
            "cache_read_input_token_cost": 7.5e-08
        }
    }"#;
    let pricing =
        Arc::new(cost::PricingTable::from_json_str(PRICING_FIXTURE).expect("fixture should parse"));

    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&exoharness),
            Arc::new(FakeModelClient::new(vec![ModelResponse {
                provider_cost_usd: None,
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("pong")],
                tool_calls: Vec::new(),
                usage: Some(UniversalUsage {
                    // prompt_tokens here *includes* the 500 cached — OpenAI
                    // convention.
                    prompt_tokens: Some(2_000),
                    completion_tokens: Some(1_000),
                    prompt_cached_tokens: Some(500),
                    prompt_cache_creation_tokens: None,
                    completion_reasoning_tokens: None,
                    ..Default::default()
                }),
                model: Some("gpt-4o-mini".to_string()),
                ttft: None,
                duration: None,
            }])),
            Arc::new(BasicToolRuntime),
            pricing,
        ),
        None,
    );

    let secret_id = exoharness::vault::global_vault(exoharness.as_ref())
        .await
        .expect("runtime vault")
        .put_secret(PutSecretRequest {
            target: None,
            name: "openai-cache-key".to_string(),
            secret: Secret::Key {
                value: "test-key".to_string(),
            },
        })
        .await
        .expect("test secret should register");
    exoharness
        .put_binding(Binding::Llm {
            name: "gpt-4o-mini".to_string(),
            model: "gpt-4o-mini".to_string(),
            base_url: None,
            secret: Some(exoharness::vault::SecretReference {
                vault_id: exoharness::vault::global_vault(exoharness.as_ref())
                    .await
                    .unwrap()
                    .record()
                    .id,
                secret_id,
            }),
        })
        .await
        .expect("binding should register");

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "openai-cache".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-4o-mini".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("ping")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    let usage = assistant_usage_record(&conversation).await;

    assert_eq!(usage.model, "gpt-4o-mini");
    // Raw counts are preserved as the provider reported them — the
    // inclusive convention only matters for the cost computation, not for
    // the stored tokens.
    assert_eq!(usage.prompt_tokens, Some(2_000));
    assert_eq!(usage.completion_tokens, Some(1_000));
    assert_eq!(usage.prompt_cached_tokens, Some(500));
    // OpenAI-style inclusive:
    //   non_cached = 2000 - 500       = 1500
    //   1500   fresh prompt @ $0.15/M = 0.000225
    //   500    cache read   @ $0.075/M = 0.0000375
    //   1000   completion   @ $0.60/M = 0.0006
    // total = 0.0008625
    // If the executor mistakenly used the Anthropic-style additive
    // formula here, it would bill all 2000 prompt tokens at the fresh
    // rate and the total would be 0.0009375 — ~9% high — so this
    // assertion catches the provider-classification bug the PR
    // description calls out.
    let cost = usage.cost_usd.expect("cost should be computed");
    assert!(
        (cost - 0.0008625).abs() < 1e-9,
        "expected cost ~0.0008625, got {cost}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn close_session_appends_session_ended_event() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness: Arc<dyn ExoHarness> = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let harness = Runtime::new(
        LocalProvider::basic(
            Arc::clone(&exoharness),
            Arc::new(FakeModelClient::new(vec![ModelResponse {
                provider_cost_usd: None,
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("pong")],
                tool_calls: Vec::new(),
                usage: None,
                model: None,
                ttft: None,
                duration: None,
            }])),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    let result = harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("ping")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    conversation
        .end_session(result.session_id)
        .await
        .expect("close session should succeed");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: Some(result.session_id),
            turn_id: None,
            types: None,
        }))
        .await
        .expect("events should load")
        .events;

    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::SessionEnded))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn updating_agent_config_refreshes_executor_cache() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("pong-1")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("pong-2")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("first")],
                session_id: None,
            },
        )
        .await
        .expect("first send should succeed");

    let mut updated_config = crate::load_agent_config(&*agent)
        .await
        .expect("agent config should load");
    updated_config.model = "gpt-5.4-mini".to_string();
    harness
        .put_agent_config(&*agent, updated_config)
        .await
        .expect("agent config should update");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("second")],
                session_id: None,
            },
        )
        .await
        .expect("second send should succeed");

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].model, "gpt-5.4");
    assert_eq!(requests[1].model, "gpt-5.4-mini");
}

#[tokio::test(flavor = "current_thread")]
async fn send_executes_shell_tool_when_enabled() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: Vec::new(),
            tool_calls: vec![PendingToolCall {
                tool_call_id: "call-1".to_string(),
                request: ToolRequest {
                    namespace: None,
                    function_name: "shell".to_string(),
                    arguments: shell_command_arguments("printf hello"),
                },
            }],
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("done")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: Some("agent-image".to_string()),
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: true,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    let mut conversation_config = crate::load_conversation_config(&*conversation)
        .await
        .expect("conversation config should load");
    conversation_config.shell_program = Some("/bin/sh".to_string());
    conversation_config.sandbox_image = Some("conversation-image".to_string());
    conversation_config.sandbox_provider = Some(SandboxProvider::LocalProcess);
    harness
        .put_conversation_config(&*conversation, conversation_config)
        .await
        .expect("conversation config should update");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("run shell")],
                session_id: None,
            },
        )
        .await
        .expect("send should succeed");

    let messages = crate::materialize_conversation_messages(&*conversation)
        .await
        .expect("messages should load");
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::Tool { .. }))
    );
    assert!(matches!(messages.last(), Some(Message::Assistant { .. })));

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(requests[0].tools[0].name, "shell");

    let sandbox_events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("sandbox events should load")
        .events;
    assert!(matches!(
        &sandbox_events[0].data,
        EventData::SandboxCreated {
            provider,
            image,
            policy: Some(policy),
            enable_networking: true,
            ..
        } if provider == &SandboxProvider::LocalProcess && image == "conversation-image"
            && policy == &exoharness::SandboxNetworkPolicy::Unrestricted.into()
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn harness_exposes_raw_exoharness_handles() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    assert_eq!(
        harness
            .exoharness_handle()
            .list_agents()
            .await
            .expect("list agents through exoharness")
            .len(),
        1
    );
    assert_eq!(
        agent
            .list_conversations(exoharness::ListConversationsRequest::default())
            .await
            .expect("list conversations through agent handle")
            .conversations
            .len(),
        1
    );
    let events = conversation
        .get_events(None)
        .await
        .expect("get events through conversation handle")
        .events;
    assert!(
        events
            .iter()
            .all(|event| event.thread_id == conversation.record().id)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn updating_mounts_recreates_conversation_sandbox() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let mount_dir = tempdir.path().join("mount");
    std::fs::create_dir_all(&mount_dir).expect("mount dir should exist");

    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: Vec::new(),
            tool_calls: vec![PendingToolCall {
                tool_call_id: "call-1".to_string(),
                request: ToolRequest {
                    namespace: None,
                    function_name: "shell".to_string(),
                    arguments: shell_command_arguments("printf first"),
                },
            }],
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("done-1")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: Vec::new(),
            tool_calls: vec![PendingToolCall {
                tool_call_id: "call-2".to_string(),
                request: ToolRequest {
                    namespace: None,
                    function_name: "shell".to_string(),
                    arguments: shell_command_arguments("printf second"),
                },
            }],
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("done-2")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            model,
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: true,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(1),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    let mut conversation_config = crate::load_conversation_config(&*conversation)
        .await
        .expect("conversation config should load");
    conversation_config.shell_program = Some("/bin/sh".to_string());
    harness
        .put_conversation_config(&*conversation, conversation_config)
        .await
        .expect("conversation config should update");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("first")],
                session_id: None,
            },
        )
        .await
        .expect("first send should succeed");

    let first_sandboxes = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("get sandbox events")
        .events;
    assert_eq!(first_sandboxes.len(), 1);
    assert!(matches!(
        &first_sandboxes[0].data,
        EventData::SandboxCreated { default_workdir, .. } if default_workdir == "/"
    ));

    let mut updated_config = crate::load_conversation_config(&*conversation)
        .await
        .expect("conversation config should reload");
    updated_config.mounts = vec![FileSystemMount {
        host_path: mount_dir.display().to_string(),
        mount_path: "/mnt/project".to_string(),
        mode: FileSystemMountMode::ReadOnly,
        internal: Some(false),
    }];
    harness
        .put_conversation_config(&*conversation, updated_config)
        .await
        .expect("conversation config should update mounts");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("second")],
                session_id: None,
            },
        )
        .await
        .expect("second send should succeed");

    let second_sandboxes = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("get sandbox events after mount change")
        .events;
    assert_eq!(second_sandboxes.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn updating_sandbox_image_recreates_shell_sandbox_without_shell_program() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: true,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(1),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");
    let agent_config = crate::load_agent_config(&*agent)
        .await
        .expect("agent config should load");

    let mut conversation_config = crate::load_conversation_config(&*conversation)
        .await
        .expect("conversation config should load");
    conversation_config.shell_program = None;
    conversation_config.sandbox_image = Some("first-image".to_string());
    harness
        .put_conversation_config(&*conversation, conversation_config.clone())
        .await
        .expect("conversation config should update");

    let first_sandbox_id =
        ensure_shell_sandbox(conversation.as_ref(), &agent_config, &conversation_config)
            .await
            .expect("first sandbox should be created");

    conversation_config.sandbox_image = Some("second-image".to_string());
    harness
        .put_conversation_config(&*conversation, conversation_config.clone())
        .await
        .expect("conversation config should update again");

    let second_sandbox_id =
        ensure_shell_sandbox(conversation.as_ref(), &agent_config, &conversation_config)
            .await
            .expect("second sandbox should be created");

    assert_ne!(first_sandbox_id, second_sandbox_id);
    let sandbox_events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("get sandbox events")
        .events;
    assert_eq!(sandbox_events.len(), 2);
    assert!(matches!(
        &sandbox_events[0].data,
        EventData::SandboxCreated { image, .. } if image == "first-image"
    ));
    assert!(matches!(
        &sandbox_events[1].data,
        EventData::SandboxCreated { image, .. } if image == "second-image"
    ));

    let attached_sandbox_id = "borrowed-docker-sandbox".to_string();
    conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::SandboxAttached {
                sandbox_id: attached_sandbox_id.clone(),
                attachment: SandboxAttachment::DockerContainer {
                    container_id: "docker-container".to_string(),
                },
                default_workdir: "/workspace".to_string(),
            }],
        })
        .await
        .expect("sandbox attachment event should be recorded");
    assert_eq!(
        ensure_shell_sandbox(conversation.as_ref(), &agent_config, &conversation_config,)
            .await
            .expect("attached sandbox should be selected"),
        attached_sandbox_id
    );

    conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::SandboxDetached {
                sandbox_id: attached_sandbox_id,
                attachment: SandboxAttachment::DockerContainer {
                    container_id: "docker-container".to_string(),
                },
            }],
        })
        .await
        .expect("sandbox detachment event should be recorded");
    assert_eq!(
        ensure_shell_sandbox(conversation.as_ref(), &agent_config, &conversation_config,)
            .await
            .expect("previous sandbox should be selected after detachment"),
        second_sandbox_id
    );
}

#[tokio::test(flavor = "current_thread")]
async fn conversation_model_override_changes_effective_model() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness: Arc<dyn ExoHarness> = Arc::new(
        BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("first")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("second")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
        ModelResponse {
            provider_cost_usd: None,
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("third")],
            tool_calls: Vec::new(),
            usage: None,
            model: None,
            ttft: None,
            duration: None,
        },
    ]));
    let harness = Runtime::new(
        LocalProvider::basic(
            exoharness,
            Arc::clone(&model),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: Some(512),
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = harness
        .create_conversation(&*agent, CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("first")],
                session_id: None,
            },
        )
        .await
        .expect("first send should succeed");

    crate::put_conversation_model_override(
        &*conversation,
        Some(ConversationModelConfig {
            model: "claude-sonnet-4".to_string(),
            max_output_tokens: Some(2048),
        }),
    )
    .await
    .expect("model override should persist");

    assert_eq!(
        crate::get_conversation_model_override(&*conversation)
            .await
            .expect("model override should load"),
        Some(ConversationModelConfig {
            model: "claude-sonnet-4".to_string(),
            max_output_tokens: Some(2048),
        })
    );

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("second")],
                session_id: None,
            },
        )
        .await
        .expect("second send should succeed");

    crate::put_conversation_model_override(&*conversation, None)
        .await
        .expect("model override should clear");

    harness
        .send(
            Arc::clone(&agent),
            Arc::clone(&conversation),
            SendRequest {
                input: vec![user_message("third")],
                session_id: None,
            },
        )
        .await
        .expect("third send should succeed");

    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].model, "gpt-5.4");
    assert_eq!(requests[0].max_output_tokens, Some(512));
    assert_eq!(requests[1].model, "claude-sonnet-4");
    assert_eq!(requests[1].max_output_tokens, Some(2048));
    assert_eq!(requests[2].model, "gpt-5.4");
    assert_eq!(requests[2].max_output_tokens, Some(512));
}

#[derive(Default)]
struct FakeModelClient {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl FakeModelClient {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().expect("model client poisoned").clone()
    }
}

#[async_trait]
impl ModelClient for FakeModelClient {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.requests
            .lock()
            .expect("model client poisoned")
            .push(request);
        let mut responses = self.responses.lock().expect("model client poisoned");
        responses
            .pop_front()
            .ok_or_else(|| anyhow!("no more model responses configured"))
    }

    async fn complete_stream(
        &self,
        _request: ModelRequest,
    ) -> Result<Box<dyn ModelResponseStream>> {
        Ok(Box::new(FakeModelResponseStream))
    }
}

struct FakeModelResponseStream;

#[async_trait]
impl ModelResponseStream for FakeModelResponseStream {
    async fn next_chunk(&mut self) -> Result<Option<UniversalStreamChunk>> {
        Ok(None)
    }

    async fn finish(self: Box<Self>) -> Result<ModelResponse> {
        Err(anyhow!("streaming not configured"))
    }
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant {
        content: AssistantContent::String(text.to_string()),
        id: None,
    }
}

/// Fetch the UsageRecord attached to the first Messages event that
/// contains an assistant message. Mirrors the pattern in
/// `usage_record_is_persisted_with_computed_cost` so the new tests stay
/// readable.
async fn assistant_usage_record(
    conversation: &Arc<dyn crate::ConversationHandle>,
) -> exoharness::UsageRecord {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await
        .expect("get events should succeed")
        .events;

    events
        .into_iter()
        .find_map(|event| match event.data {
            EventData::Messages {
                messages,
                usage: Some(usage),
                ..
            } if messages
                .iter()
                .any(|m| matches!(m, Message::Assistant { .. })) =>
            {
                Some(*usage)
            }
            _ => None,
        })
        .expect("assistant message event should carry a UsageRecord")
}

fn shell_command_arguments(command: &str) -> Map<String, Value> {
    Map::from_iter([(String::from("command"), Value::String(command.to_string()))])
}

async fn register_test_models(exoharness: &dyn ExoHarness) {
    let secret_id = exoharness::vault::global_vault(exoharness)
        .await
        .expect("runtime vault")
        .put_secret(PutSecretRequest {
            target: None,
            name: "test-openai".to_string(),
            secret: Secret::Key {
                value: "test-key".to_string(),
            },
        })
        .await
        .expect("test secret should register");

    for model in ["gpt-5.4", "gpt-5.4-mini", "claude-sonnet-4"] {
        exoharness
            .put_binding(Binding::Llm {
                name: model.to_string(),
                model: model.to_string(),
                base_url: None,
                secret: Some(exoharness::vault::SecretReference {
                    vault_id: exoharness::vault::global_vault(exoharness)
                        .await
                        .unwrap()
                        .record()
                        .id,
                    secret_id,
                }),
            })
            .await
            .expect("test model should register");
    }
}

#[tokio::test]
async fn remote_threads_paginate_and_failed_creation_only_deletes_the_new_thread() -> Result<()> {
    use exoharness::protocol::{
        ClientMessage, ConversationHandleInfo, Request, Response, ServerMessage,
    };
    use exoharness::{HttpExoHarness, ListConversationsResult};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

    let temp = TempDir::new()?;
    let local = Runtime::new(
        LocalProvider::basic(
            Arc::new(BasicExoHarness::new(local_test_config(temp.path())).await?),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    let agent = local
        .create_agent(CreateAgentRequest {
            slug: "support".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: false,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            sandbox_scope: None,
            enable_networking: false,
            model: "gpt-test".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        })
        .await?;
    let config = local.get_agent_config(agent.as_ref()).await?;
    let artifact = agent.list_artifacts().await?.remove(0);
    let agent_record = agent.record().clone();
    let agent_id = agent_record.id;
    let mut threads = Vec::new();
    for slug in ["failed", "older", "middle", "recent"] {
        let thread = local
            .create_conversation(
                agent.as_ref(),
                CreateConversationRequest {
                    slug: Some(slug.to_string()),
                    ..Default::default()
                },
            )
            .await?;
        threads.push(ConversationHandleInfo {
            agent_id,
            record: thread.record().clone(),
        });
    }
    threads.reverse();
    let recent_id = threads[0].record.id;
    let failed_id = threads[3].record.id;
    let invalid_cursor = Arc::new(Mutex::new(None::<Uuid7>));
    let cursor_override = invalid_cursor.clone();
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let deletions = deleted.clone();
    let server = MockServer::start().await;
    Mock::given(path("/request"))
        .respond_with(move |request: &wiremock::Request| {
            let ClientMessage::Request { id, request } =
                serde_json::from_slice(&request.body).unwrap();
            let response = match request {
                Request::GetAgent { .. } => Some(Response::Agent {
                    agent: Some(agent_record.clone()),
                }),
                Request::AgentWriteArtifact { .. } => Some(Response::ArtifactVersion {
                    artifact: artifact.clone(),
                }),
                Request::ListConversations { request, .. } => {
                    let index = request.cursor.map_or(0, |cursor| {
                        threads
                            .iter()
                            .position(|thread| thread.record.id == cursor)
                            .unwrap()
                            + 1
                    });
                    let mut next_cursor = (index < 2).then_some(threads[index].record.id);
                    if request.cursor.is_some() {
                        next_cursor = cursor_override.lock().unwrap().or(next_cursor);
                    }
                    Some(Response::Conversations {
                        result: ListConversationsResult {
                            conversations: vec![threads[index].clone()],
                            next_cursor,
                        },
                    })
                }
                Request::NewConversation { .. } => Some(Response::Conversation {
                    conversation: Some(threads[3].clone()),
                }),
                Request::ConversationWriteArtifact { .. } => None,
                Request::DeleteConversation {
                    conversation_id, ..
                } => {
                    deletions.lock().unwrap().push(conversation_id);
                    Some(Response::Bool { value: true })
                }
                other => panic!("unexpected request: {other:?}"),
            };
            ResponseTemplate::new(200).set_body_json(ServerMessage::Response {
                id,
                ok: response.is_some(),
                error: response
                    .is_none()
                    .then(|| "configuration write failed".to_string()),
                response,
            })
        })
        .mount(&server)
        .await;
    let remote = Runtime::new(
        LocalProvider::basic(
            Arc::new(HttpExoHarness::new(server.uri(), None)?),
            Arc::new(FakeModelClient::default()),
            Arc::new(BasicToolRuntime),
            Arc::new(cost::PricingTable::empty()),
        ),
        None,
    );
    let agent = remote.get_agent(&agent_id.to_string()).await?.unwrap();
    assert_eq!(
        crate::harness_helpers::list_conversation_handles(agent.as_ref())
            .await?
            .len(),
        3
    );
    assert_eq!(
        remote
            .get_conversation(agent.as_ref(), "older")
            .await?
            .unwrap()
            .record()
            .slug,
        "older"
    );
    // Prime the runtime cache so creation reaches the failing thread-config write.
    remote.put_agent_config(agent.as_ref(), config).await?;
    let error = remote
        .create_conversation(agent.as_ref(), CreateConversationRequest::default())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("configuration write failed"));
    assert_eq!(*deleted.lock().unwrap(), vec![failed_id]);
    for cursor in [recent_id, Uuid7::now()] {
        *invalid_cursor.lock().unwrap() = Some(cursor);
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::harness_helpers::list_conversation_handles(agent.as_ref()),
        )
        .await?
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains("thread listing cursor did not advance")
        );
    }
    Ok(())
}

#[tokio::test]
async fn basic_and_rlm_models_receive_mcp_errors_and_can_continue() -> Result<()> {
    use exo_mcp::{McpCredentials, McpServerConfig, McpToolSet};
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method},
    };

    #[derive(serde::Deserialize)]
    struct Rpc {
        id: u64,
    }
    let server = MockServer::start().await;
    for (method_name, result) in [
        (
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
        ),
        (
            "tools/list",
            json!({"tools":[{"name":"search","inputSchema":{"type":"object"}}]}),
        ),
        (
            "tools/call",
            json!({"isError":true,"content":[{"type":"text","text":"Please supply a query"}],"structuredContent":{"reason":"missing_query"}}),
        ),
    ] {
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method":method_name})))
            .respond_with(move |request: &wiremock::Request| {
                let rpc: Rpc = request.body_json().unwrap();
                ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc":"2.0","id":rpc.id,"result":result}))
            })
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method":"notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    let mcp = Arc::new(
        McpToolSet::connect(
            &[McpServerConfig {
                name: "fixture".into(),
                url: server.uri(),
                allowed_tools: None,
                blocked_tools: vec![],
            }],
            McpCredentials::default(),
        )
        .await?,
    );
    for kind in [crate::AgentHarnessKind::Basic, crate::AgentHarnessKind::Rlm] {
        let temp = TempDir::new()?;
        let root =
            Arc::new(BasicExoHarness::new(local_test_config(temp.path().join("state"))).await?);
        let model = Arc::new(FakeModelClient::new(vec![
            serde_json::from_value(
                json!({"messages":[],"tool_calls":[{"tool_call_id":"search-1","request":{"function_name":"exo_mcp__fixture__search","arguments":{}}}]}),
            )?,
            serde_json::from_value(
                json!({"messages":[{"role":"assistant","content":"Please supply a query."}],"tool_calls":[]}),
            )?,
        ]));
        let tools = Arc::new(crate::McpToolRuntime::new(
            BasicToolRuntime,
            Arc::clone(&mcp),
        ));
        let provider = match kind {
            crate::AgentHarnessKind::Basic => LocalProvider::basic(
                root,
                Arc::clone(&model),
                tools,
                Arc::new(cost::PricingTable::empty()),
            ),
            _ => LocalProvider::rlm(root, Arc::clone(&model), tools),
        };
        let harness = Runtime::new(provider, None);
        register_test_models(harness.exoharness_handle().as_ref()).await;
        let agent = harness
            .create_agent(CreateAgentRequest {
                slug: "mcp-errors".into(),
                name: None,
                harness: kind,
                typescript: None,
                enable_agent_tool_creation: false,
                sandbox_image: None,
                sandbox_provider: SandboxProvider::LocalProcess,
                sandbox_scope: None,
                enable_networking: true,
                model: "gpt-5.4".into(),
                max_output_tokens: None,
                max_tool_round_trips: Some(1),
                braintrust: None,
            })
            .await?;
        let thread = harness
            .create_conversation(agent.as_ref(), CreateConversationRequest::default())
            .await?;
        harness
            .send(
                agent,
                thread,
                SendRequest {
                    input: vec![user_message("Search")],
                    session_id: None,
                },
            )
            .await?;
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        let output = requests[1]
            .messages
            .iter()
            .find_map(|message| {
                if let Message::Tool { content } = message {
                    let lingua::universal::ToolContentPart::ToolResult(result) = &content[0];
                    Some(&result.output)
                } else {
                    None
                }
            })
            .expect("model should receive the MCP error result");
        assert_eq!(
            lingua::serde_json::to_string(output)?,
            json!({
                "isError":true,"content":[{"type":"text","text":"Please supply a query"}],
                "structuredContent":{"reason":"missing_query"}
            })
            .to_string()
        );
        harness.shutdown().await?;
    }
    mcp.close().await?;
    Ok(())
}
