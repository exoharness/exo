use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::{
    ModelClient, ModelRequest, ModelResponse, ModelResponseStream, PendingToolCall, SendRequest,
};
use anyhow::anyhow;
use async_trait::async_trait;
use exoharness::{
    BasicExoHarness, Binding, EventData, EventQuery, EventQueryDirection, ExoHarness,
    FileSystemMount, FileSystemMountMode, PutSecretRequest, Result, Secret, ToolRequest, Uuid7,
};
use lingua::universal::{AssistantContent, UserContent};
use lingua::{Message, UniversalStreamChunk};
use serde_json::{Map, Value};
use tempfile::TempDir;

use crate::{
    BasicHarness, BasicToolRuntime, ConversationModelConfig, CreateAgentRequest,
    CreateConversationRequest, Harness,
};

#[tokio::test(flavor = "current_thread")]
async fn creates_agents_and_conversations_with_persisted_config() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new_with_local_process_sandbox(tempdir.path().join("exoharness"))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = BasicHarness::new(
        exoharness,
        Arc::new(FakeModelClient::default()),
        Arc::new(BasicToolRuntime::default()),
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            library_tools: Vec::new(),
            enable_agent_tool_creation: true,
            sandbox_image: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: Some(512),
            max_tool_round_trips: Some(3),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest {
            slug: Some("session".to_string()),
            name: Some("Session".to_string()),
        })
        .await
        .expect("conversation should be created");

    let stored_agent = harness
        .get_agent("demo")
        .await
        .expect("get agent should succeed")
        .expect("agent should exist");
    let stored_conversation = stored_agent
        .get_conversation("session")
        .await
        .expect("get conversation should succeed")
        .expect("conversation should exist");

    assert_eq!(stored_agent.record().slug, "demo");
    assert_eq!(
        stored_agent.config().await.expect("agent config").model,
        "gpt-5.4"
    );
    assert_eq!(
        stored_conversation
            .config()
            .await
            .expect("conversation config")
            .shell_program,
        Some("/bin/bash".to_string())
    );
    assert_eq!(conversation.record().slug, "session");
}

#[tokio::test(flavor = "current_thread")]
async fn send_persists_messages_through_harness() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new_with_local_process_sandbox(tempdir.path().join("exoharness"))
            .await
            .expect("basic exoharness should initialize"),
    ) as Arc<dyn ExoHarness>;
    let harness = BasicHarness::new(
        exoharness,
        Arc::new(FakeModelClient::new(vec![ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("pong")],
            tool_calls: Vec::new(),
            usage: None,
        }])),
        Arc::new(BasicToolRuntime::default()),
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            library_tools: Vec::new(),
            enable_agent_tool_creation: true,
            sandbox_image: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    conversation
        .send(SendRequest {
            input: vec![user_message("ping")],
            session_id: None,
        })
        .await
        .expect("send should succeed");

    let messages = conversation.messages().await.expect("messages should load");
    assert_eq!(messages.len(), 2);
    assert!(matches!(messages[0], Message::User { .. }));
    assert!(matches!(messages[1], Message::Assistant { .. }));
}

#[tokio::test(flavor = "current_thread")]
async fn close_session_appends_session_ended_event() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness: Arc<dyn ExoHarness> = Arc::new(
        BasicExoHarness::new_with_local_process_sandbox(tempdir.path().join("exoharness"))
            .await
            .expect("basic exoharness should initialize"),
    );
    let harness = BasicHarness::new(
        Arc::clone(&exoharness),
        Arc::new(FakeModelClient::new(vec![ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("pong")],
            tool_calls: Vec::new(),
            usage: None,
        }])),
        Arc::new(BasicToolRuntime::default()),
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            library_tools: Vec::new(),
            enable_agent_tool_creation: true,
            sandbox_image: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    let result = conversation
        .send(SendRequest {
            input: vec![user_message("ping")],
            session_id: None,
        })
        .await
        .expect("send should succeed");

    conversation
        .close_session(result.session_id)
        .await
        .expect("close session should succeed");

    let events = conversation
        .exoharness_handle()
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
        BasicExoHarness::new_with_local_process_sandbox(tempdir.path().join("exoharness"))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("pong-1")],
            tool_calls: Vec::new(),
            usage: None,
        },
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("pong-2")],
            tool_calls: Vec::new(),
            usage: None,
        },
    ]));
    let harness = BasicHarness::new(
        exoharness,
        Arc::clone(&model),
        Arc::new(BasicToolRuntime::default()),
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            library_tools: Vec::new(),
            enable_agent_tool_creation: true,
            sandbox_image: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    conversation
        .send(SendRequest {
            input: vec![user_message("first")],
            session_id: None,
        })
        .await
        .expect("first send should succeed");

    let mut updated_config = agent.config().await.expect("agent config should load");
    updated_config.model = "gpt-5.4-mini".to_string();
    agent
        .put_config(updated_config)
        .await
        .expect("agent config should update");

    conversation
        .send(SendRequest {
            input: vec![user_message("second")],
            session_id: None,
        })
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
        BasicExoHarness::new_with_local_process_sandbox(tempdir.path().join("exoharness"))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: Vec::new(),
            tool_calls: vec![PendingToolCall {
                tool_call_id: "call-1".to_string(),
                request: ToolRequest {
                    function_name: "shell".to_string(),
                    arguments: shell_command_arguments("printf hello"),
                },
            }],
            usage: None,
        },
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("done")],
            tool_calls: Vec::new(),
            usage: None,
        },
    ]));
    let harness = BasicHarness::new(exoharness, Arc::clone(&model), Arc::new(BasicToolRuntime));
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            library_tools: Vec::new(),
            enable_agent_tool_creation: true,
            sandbox_image: None,
            enable_networking: true,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    let mut conversation_config = conversation
        .config()
        .await
        .expect("conversation config should load");
    conversation_config.shell_program = Some("/bin/sh".to_string());
    conversation
        .put_config(conversation_config)
        .await
        .expect("conversation config should update");

    conversation
        .send(SendRequest {
            input: vec![user_message("run shell")],
            session_id: None,
        })
        .await
        .expect("send should succeed");

    let messages = conversation.messages().await.expect("messages should load");
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
        .exoharness_handle()
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec!["sandbox_created".to_string()]),
        }))
        .await
        .expect("sandbox events should load")
        .events;
    assert!(matches!(
        &sandbox_events[0].data,
        EventData::SandboxCreated {
            enable_networking: true,
            ..
        }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn harness_exposes_raw_exoharness_handles() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness = Arc::new(
        BasicExoHarness::new_with_local_process_sandbox(tempdir.path().join("exoharness"))
            .await
            .expect("basic exoharness should initialize"),
    );
    let harness = BasicHarness::new(
        exoharness,
        Arc::new(FakeModelClient::default()),
        Arc::new(BasicToolRuntime::default()),
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            library_tools: Vec::new(),
            enable_agent_tool_creation: true,
            sandbox_image: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
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
            .exoharness_handle()
            .list_conversations()
            .await
            .expect("list conversations through agent handle")
            .len(),
        1
    );
    let events = conversation
        .exoharness_handle()
        .get_events(None)
        .await
        .expect("get events through conversation handle")
        .events;
    assert!(
        events
            .iter()
            .all(|event| event.conversation_id == conversation.record().id)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn updating_mounts_recreates_shell_sandbox() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let mount_dir = tempdir.path().join("mount");
    std::fs::create_dir_all(&mount_dir).expect("mount dir should exist");

    let exoharness = Arc::new(
        BasicExoHarness::new_with_local_process_sandbox(tempdir.path().join("exoharness"))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("done-1")],
            tool_calls: Vec::new(),
            usage: None,
        },
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("done-2")],
            tool_calls: Vec::new(),
            usage: None,
        },
    ]));
    let harness = BasicHarness::new(exoharness, model, Arc::new(BasicToolRuntime));
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: Some("Demo".to_string()),
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            library_tools: Vec::new(),
            enable_agent_tool_creation: true,
            sandbox_image: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: Some(1),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    let mut conversation_config = conversation
        .config()
        .await
        .expect("conversation config should load");
    conversation_config.shell_program = Some("/bin/sh".to_string());
    conversation
        .put_config(conversation_config)
        .await
        .expect("conversation config should update");

    conversation
        .send(SendRequest {
            input: vec![user_message("first")],
            session_id: None,
        })
        .await
        .expect("first send should succeed");

    let first_sandboxes = conversation
        .exoharness_handle()
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec!["sandbox_created".to_string()]),
        }))
        .await
        .expect("get sandbox events")
        .events;
    assert_eq!(first_sandboxes.len(), 1);
    assert!(matches!(
        &first_sandboxes[0].data,
        EventData::SandboxCreated { default_workdir, .. } if default_workdir == "/"
    ));

    let mut updated_config = conversation
        .config()
        .await
        .expect("conversation config should reload");
    updated_config.mounts = vec![FileSystemMount {
        host_path: mount_dir.display().to_string(),
        mount_path: "/mnt/project".to_string(),
        mode: FileSystemMountMode::ReadOnly,
        internal: Some(false),
    }];
    conversation
        .put_config(updated_config)
        .await
        .expect("conversation config should update mounts");

    conversation
        .send(SendRequest {
            input: vec![user_message("second")],
            session_id: None,
        })
        .await
        .expect("second send should succeed");

    let second_sandboxes = conversation
        .exoharness_handle()
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec!["sandbox_created".to_string()]),
        }))
        .await
        .expect("get sandbox events after mount change")
        .events;
    assert_eq!(second_sandboxes.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn conversation_model_override_changes_effective_model() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let exoharness: Arc<dyn ExoHarness> = Arc::new(
        BasicExoHarness::new_with_local_process_sandbox(tempdir.path().join("exoharness"))
            .await
            .expect("basic exoharness should initialize"),
    );
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("first")],
            tool_calls: Vec::new(),
            usage: None,
        },
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("second")],
            tool_calls: Vec::new(),
            usage: None,
        },
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("third")],
            tool_calls: Vec::new(),
            usage: None,
        },
    ]));
    let harness = BasicHarness::new(
        exoharness,
        Arc::clone(&model),
        Arc::new(BasicToolRuntime::default()),
    );
    register_test_models(harness.exoharness_handle().as_ref()).await;

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "demo".to_string(),
            name: None,
            harness: crate::AgentHarnessKind::Basic,
            typescript: None,
            library_tools: Vec::new(),
            enable_agent_tool_creation: true,
            sandbox_image: None,
            enable_networking: false,
            model: "gpt-5.4".to_string(),
            max_output_tokens: Some(512),
            max_tool_round_trips: Some(2),
            braintrust: None,
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .create_conversation(CreateConversationRequest::default())
        .await
        .expect("conversation should be created");

    conversation
        .send(SendRequest {
            input: vec![user_message("first")],
            session_id: None,
        })
        .await
        .expect("first send should succeed");

    conversation
        .put_model_override(Some(ConversationModelConfig {
            model: "claude-sonnet-4".to_string(),
            max_output_tokens: Some(2048),
        }))
        .await
        .expect("model override should persist");

    assert_eq!(
        conversation
            .model_override()
            .await
            .expect("model override should load"),
        Some(ConversationModelConfig {
            model: "claude-sonnet-4".to_string(),
            max_output_tokens: Some(2048),
        })
    );

    conversation
        .send(SendRequest {
            input: vec![user_message("second")],
            session_id: None,
        })
        .await
        .expect("second send should succeed");

    conversation
        .put_model_override(None)
        .await
        .expect("model override should clear");

    conversation
        .send(SendRequest {
            input: vec![user_message("third")],
            session_id: None,
        })
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

fn shell_command_arguments(command: &str) -> Map<String, Value> {
    Map::from_iter([(String::from("command"), Value::String(command.to_string()))])
}

async fn register_test_models(exoharness: &dyn ExoHarness) {
    let secret_id = exoharness
        .put_secret(PutSecretRequest {
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
                secret_id: Some(secret_id),
            })
            .await
            .expect("test model should register");
    }
}
