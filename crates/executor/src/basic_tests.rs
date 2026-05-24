use std::collections::VecDeque;
use std::ops::Bound;
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use async_trait::async_trait;
use exoharness::{
    AddEventsRequest, AddEventsResult, AgentHandle, AgentId, AgentRecord, Artifact,
    ArtifactVersion, BeginTurnRequest, Binding, BindingMetadata, BindingType, ConversationHandle,
    ConversationId, ConversationRecord, CreateSandboxRequest, Event, EventData, EventQuery,
    EventQueryDirection, EventStream, ExoHarness, ForkConversationRequest, GetEventsResult,
    NewAgentRequest, NewConversationRequest, PutSecretRequest, ReadArtifactRequest, Result,
    RunInSandboxRequest, SandboxId, SandboxProcess, SandboxProcessParts, Secret, SecretMetadata,
    SecretType, SessionId, SnapshotId, StartSandboxRequest, ToolRequest, ToolResult, TurnHandle,
    TurnId, TurnRecord, Uuid7, WriteArtifactRequest,
};
use futures::FutureExt;
use futures::io::Cursor;
use futures::stream;
use lingua::universal::{AssistantContent, UserContent};
use lingua::{Message, UniversalStreamChunk};
use serde_json::{Map, Value};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::harness_executor::{ExecutorStreamMode, HarnessExecutor};
use crate::*;

#[tokio::test(flavor = "current_thread")]
async fn send_appends_user_and_assistant_messages() {
    let agent_id = Uuid7::now();
    let conversation_id = Uuid7::now();
    let exoharness = Arc::new(FakeExoHarness::new(agent_id, conversation_id));
    let agent = exoharness
        .get_agent(&agent_id)
        .await
        .expect("get agent should succeed")
        .expect("agent should exist");
    let conversation = agent
        .get_conversation(&conversation_id)
        .await
        .expect("get conversation should succeed")
        .expect("conversation should exist");
    let executor = BasicExecutor::new(
        Arc::new(FakeModelClient::new(vec![ModelResponse {
            response_id: None,
            messages: vec![assistant_message("pong")],
            tool_calls: vec![],
            usage: None,
        }])),
        Arc::new(FakeToolRuntime::default()),
    );
    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: vec![user_message("ping")],
        })
        .await
        .expect("begin turn should succeed");

    executor
        .prepare_conversation(
            agent.as_ref(),
            conversation.as_ref(),
            &default_agent_config(),
            &ConversationConfig::default(),
        )
        .await
        .expect("prepare conversation should succeed");
    HarnessExecutor::run_turn(
        &executor,
        agent.as_ref(),
        conversation.as_ref(),
        Arc::clone(&turn),
        &default_agent_config(),
        &ConversationConfig::default(),
        &(),
        ExecutorStreamMode::Disabled,
        None,
    )
    .await
    .expect("execute turn should succeed");
    let latest_event_id = turn.finish().await.expect("turn should finish");

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

    assert_eq!(
        turn.record().session_id,
        events[0].session_id.expect("session id")
    );
    assert!(matches!(events[0].data, EventData::SessionStarted));
    assert!(matches!(events[1].data, EventData::TurnStarted));
    assert!(matches!(events[2].data, EventData::Messages { .. }));
    assert!(matches!(events[3].data, EventData::Messages { .. }));
    assert!(matches!(events[4].data, EventData::TurnEnded));
    assert_eq!(latest_event_id, events[4].id);
}

#[tokio::test(flavor = "current_thread")]
async fn send_executes_tool_round_trip() {
    let agent_id = Uuid7::now();
    let conversation_id = Uuid7::now();
    let exoharness = Arc::new(FakeExoHarness::new(agent_id, conversation_id));
    let agent = exoharness
        .get_agent(&agent_id)
        .await
        .expect("get agent should succeed")
        .expect("agent should exist");
    let conversation = agent
        .get_conversation(&conversation_id)
        .await
        .expect("get conversation should succeed")
        .expect("conversation should exist");
    let tool_call_id = "call-1".to_string();
    let model = Arc::new(FakeModelClient::new(vec![
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![],
            tool_calls: vec![PendingToolCall {
                tool_call_id: tool_call_id.clone(),
                request: ToolRequest {
                    function_name: "shell".to_string(),
                    arguments: Map::new(),
                },
            }],
            usage: None,
        },
        ModelResponse {
            response_id: Some(Uuid7::now()),
            messages: vec![assistant_message("done")],
            tool_calls: vec![],
            usage: None,
        },
    ]));
    let executor = BasicExecutor::new(
        Arc::clone(&model),
        Arc::new(FakeToolRuntime::with_result(Value::String(
            "ok".to_string(),
        ))),
    );
    let agent_config = default_agent_config();
    let conversation_config = ConversationConfig {
        enable_networking: true,
        shell_program: Some("bash".to_string()),
        mounts: Vec::new(),
        sandbox_scope: None,
    };
    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: vec![user_message("run it")],
        })
        .await
        .expect("begin turn should succeed");

    HarnessExecutor::run_turn(
        &executor,
        agent.as_ref(),
        conversation.as_ref(),
        Arc::clone(&turn),
        &agent_config,
        &conversation_config,
        &(),
        ExecutorStreamMode::Disabled,
        None,
    )
    .await
    .expect("execute turn should succeed");
    turn.finish().await.expect("turn should finish");

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

    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::ToolRequested { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::ToolResult { .. }))
    );
    assert!(events.iter().any(|event| {
        match &event.data {
            EventData::Messages { messages, .. } => messages
                .iter()
                .any(|message| matches!(message, Message::Assistant { .. })),
            _ => false,
        }
    }));

    let requests = model.observed_requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| matches!(message, Message::Tool { .. }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn send_stream_emits_chunks_and_persists_final_response() {
    let agent_id = Uuid7::now();
    let conversation_id = Uuid7::now();
    let exoharness = Arc::new(FakeExoHarness::new(agent_id, conversation_id));
    let agent = exoharness
        .get_agent(&agent_id)
        .await
        .expect("get agent should succeed")
        .expect("agent should exist");
    let conversation = agent
        .get_conversation(&conversation_id)
        .await
        .expect("get conversation should succeed")
        .expect("conversation should exist");
    let executor = BasicExecutor::new(
        Arc::new(FakeModelClient::with_streams(vec![FakeStreamResponse {
            chunks: vec![
                UniversalStreamChunk::text_delta(0, "hel"),
                UniversalStreamChunk::text_delta(0, "lo"),
                UniversalStreamChunk::finish(0, "stop"),
            ],
            final_response: ModelResponse {
                response_id: Some(Uuid7::now()),
                messages: vec![assistant_message("hello")],
                tool_calls: vec![],
                usage: None,
            },
        }])),
        Arc::new(FakeToolRuntime::default()),
    );
    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: vec![user_message("stream it")],
        })
        .await
        .expect("begin turn should succeed");
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

    executor
        .prepare_conversation(
            agent.as_ref(),
            conversation.as_ref(),
            &default_agent_config(),
            &ConversationConfig::default(),
        )
        .await
        .expect("prepare conversation should succeed");
    HarnessExecutor::run_turn(
        &executor,
        agent.as_ref(),
        conversation.as_ref(),
        Arc::clone(&turn),
        &default_agent_config(),
        &ConversationConfig::default(),
        &(),
        ExecutorStreamMode::Enabled(&event_tx),
        None,
    )
    .await
    .expect("execute turn stream should succeed");
    let latest_event_id = turn.finish().await.expect("turn should finish");
    drop(event_tx);

    let mut stream = ExecutionStreamHandle::new(UnboundedReceiverStream::new(event_rx));

    let first_event = stream
        .next()
        .await
        .expect("first event should exist")
        .expect("first event should succeed");
    assert!(matches!(
        first_event,
        ExecutionStreamEvent::FirstChunk { .. }
    ));

    let mut chunk_text = String::new();
    while let Some(event) = stream.next().await {
        match event.expect("stream event should succeed") {
            ExecutionStreamEvent::FirstChunk { .. } => {}
            ExecutionStreamEvent::Chunk(chunk) => {
                for choice in chunk.choices {
                    if let Some(delta) = choice.delta_view()
                        && let Some(content) = delta.content
                    {
                        chunk_text.push_str(&content);
                    }
                }
            }
            ExecutionStreamEvent::ToolCall { .. } => {}
            ExecutionStreamEvent::ToolResult { .. } => {}
            ExecutionStreamEvent::Completed(_) => {
                panic!("executor stream should not emit completion")
            }
        }
    }

    assert_eq!(chunk_text, "hello");

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
    assert_eq!(latest_event_id, events.last().expect("turn ended event").id);
    assert!(events.iter().any(|event| {
        match &event.data {
            EventData::Messages { messages, .. } => messages
                .iter()
                .any(|message| matches!(message, Message::Assistant { .. })),
            _ => false,
        }
    }));
}

struct FakeModelClient {
    responses: Mutex<VecDeque<ModelResponse>>,
    streams: Mutex<VecDeque<FakeStreamResponse>>,
    observed_requests: Mutex<Vec<ModelRequest>>,
}

impl FakeModelClient {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            streams: Mutex::new(VecDeque::new()),
            observed_requests: Mutex::new(Vec::new()),
        }
    }

    fn with_streams(streams: Vec<FakeStreamResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            streams: Mutex::new(VecDeque::from(streams)),
            observed_requests: Mutex::new(Vec::new()),
        }
    }

    fn observed_requests(&self) -> Vec<ModelRequest> {
        self.observed_requests
            .lock()
            .expect("model client poisoned")
            .clone()
    }
}

#[async_trait]
impl ModelClient for FakeModelClient {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.observed_requests
            .lock()
            .expect("model client poisoned")
            .push(request);
        let mut responses = self.responses.lock().expect("model client poisoned");
        responses
            .pop_front()
            .ok_or_else(|| anyhow!("no more model responses configured"))
    }

    async fn complete_stream(&self, request: ModelRequest) -> Result<Box<dyn ModelResponseStream>> {
        self.observed_requests
            .lock()
            .expect("model client poisoned")
            .push(request);
        let mut streams = self.streams.lock().expect("model client poisoned");
        let stream = streams
            .pop_front()
            .ok_or_else(|| anyhow!("no more model streams configured"))?;
        Ok(Box::new(FakeModelResponseStream {
            chunks: VecDeque::from(stream.chunks),
            final_response: Some(stream.final_response),
        }))
    }
}

struct FakeStreamResponse {
    chunks: Vec<UniversalStreamChunk>,
    final_response: ModelResponse,
}

struct FakeModelResponseStream {
    chunks: VecDeque<UniversalStreamChunk>,
    final_response: Option<ModelResponse>,
}

#[async_trait]
impl ModelResponseStream for FakeModelResponseStream {
    async fn next_chunk(&mut self) -> Result<Option<UniversalStreamChunk>> {
        Ok(self.chunks.pop_front())
    }

    async fn finish(mut self: Box<Self>) -> Result<ModelResponse> {
        self.final_response
            .take()
            .ok_or_else(|| anyhow!("stream already finished"))
    }
}

#[derive(Default)]
struct FakeToolRuntime {
    result: Mutex<Option<Value>>,
}

impl FakeToolRuntime {
    fn with_result(result: Value) -> Self {
        Self {
            result: Mutex::new(Some(result)),
        }
    }
}

#[async_trait]
impl ToolRuntime for FakeToolRuntime {
    async fn execute(
        &self,
        _agent: &dyn AgentHandle,
        _conversation: &dyn ConversationHandle,
        _agent_config: &AgentConfig,
        _config: &ConversationConfig,
        _request: &ToolRequest,
    ) -> Result<ToolResult> {
        let guard = self.result.lock().expect("tool runtime poisoned");
        Ok(guard.clone().unwrap_or(Value::Null))
    }
}

struct FakeExoHarness {
    state: Arc<Mutex<FakeState>>,
}

struct FakeState {
    agent: AgentRecord,
    conversation: FakeConversationState,
}

struct FakeConversationState {
    record: ConversationRecord,
    events: Vec<Event>,
}

impl FakeExoHarness {
    fn new(agent_id: AgentId, conversation_id: ConversationId) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                agent: AgentRecord {
                    id: agent_id,
                    slug: "agent".to_string(),
                    name: "Agent".to_string(),
                },
                conversation: FakeConversationState {
                    record: ConversationRecord {
                        id: conversation_id,
                        slug: "conversation".to_string(),
                        name: "Conversation".to_string(),
                        latest_event_id: None,
                    },
                    events: Vec::new(),
                },
            })),
        }
    }
}

#[async_trait]
impl ExoHarness for FakeExoHarness {
    async fn list_agents(&self) -> Result<Vec<Arc<dyn AgentHandle>>> {
        let state = self.state.lock().expect("state poisoned");
        Ok(vec![Arc::new(FakeAgentHandle {
            state: Arc::clone(&self.state),
            record: state.agent.clone(),
        })])
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<Arc<dyn AgentHandle>>> {
        let state = self.state.lock().expect("state poisoned");
        if &state.agent.id != id {
            return Ok(None);
        }
        Ok(Some(Arc::new(FakeAgentHandle {
            state: Arc::clone(&self.state),
            record: state.agent.clone(),
        })))
    }

    async fn new_agent(&self, _request: NewAgentRequest) -> Result<Arc<dyn AgentHandle>> {
        Err(anyhow!("not implemented"))
    }

    async fn delete_agent(&self, _id: &AgentId) -> Result<bool> {
        Err(anyhow!("not implemented"))
    }

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>> {
        Ok(vec![test_model_binding_metadata()])
    }

    async fn put_binding(&self, _binding: Binding) -> Result<exoharness::BindingId> {
        Err(anyhow!("not implemented"))
    }

    async fn get_binding(&self, _id: &exoharness::BindingId) -> Result<Option<Binding>> {
        Ok(Some(test_model_binding()))
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        Ok(vec![test_secret_metadata()])
    }

    async fn put_secret(&self, _secret: PutSecretRequest) -> Result<exoharness::SecretId> {
        Err(anyhow!("not implemented"))
    }

    async fn get_secret(&self, _id: &exoharness::SecretId) -> Result<Option<Secret>> {
        Ok(Some(Secret::Key {
            value: "test-key".to_string(),
        }))
    }
}

struct FakeAgentHandle {
    state: Arc<Mutex<FakeState>>,
    record: AgentRecord,
}

#[async_trait]
impl AgentHandle for FakeAgentHandle {
    fn record(&self) -> &AgentRecord {
        &self.record
    }

    async fn list_conversations(&self) -> Result<Vec<Arc<dyn ConversationHandle>>> {
        let state = self.state.lock().expect("state poisoned");
        Ok(vec![Arc::new(FakeConversationHandle {
            state: Arc::clone(&self.state),
            record: state.conversation.record.clone(),
        })])
    }

    async fn get_conversation(
        &self,
        id: &ConversationId,
    ) -> Result<Option<Arc<dyn ConversationHandle>>> {
        let state = self.state.lock().expect("state poisoned");
        if &state.conversation.record.id != id {
            return Ok(None);
        }
        Ok(Some(Arc::new(FakeConversationHandle {
            state: Arc::clone(&self.state),
            record: state.conversation.record.clone(),
        })))
    }

    async fn new_conversation(
        &self,
        _request: NewConversationRequest,
    ) -> Result<Arc<dyn ConversationHandle>> {
        Err(anyhow!("not implemented"))
    }

    async fn delete_conversation(&self, _id: &ConversationId) -> Result<bool> {
        Err(anyhow!("not implemented"))
    }

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>> {
        Ok(vec![test_model_binding_metadata()])
    }

    async fn put_binding(&self, _binding: Binding) -> Result<exoharness::BindingId> {
        Err(anyhow!("not implemented"))
    }

    async fn get_binding(&self, _id: &exoharness::BindingId) -> Result<Option<Binding>> {
        Ok(Some(test_model_binding()))
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        Ok(vec![test_secret_metadata()])
    }

    async fn put_secret(&self, _secret: PutSecretRequest) -> Result<exoharness::SecretId> {
        Err(anyhow!("not implemented"))
    }

    async fn get_secret(&self, _id: &exoharness::SecretId) -> Result<Option<Secret>> {
        Ok(Some(Secret::Key {
            value: "test-key".to_string(),
        }))
    }

    async fn write_artifact(&self, _request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        Err(anyhow!("not implemented"))
    }

    async fn read_artifact(&self, _request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        Ok(None)
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        Ok(Vec::new())
    }
}

struct FakeConversationHandle {
    state: Arc<Mutex<FakeState>>,
    record: ConversationRecord,
}

#[async_trait]
impl ConversationHandle for FakeConversationHandle {
    fn record(&self) -> &ConversationRecord {
        &self.record
    }

    async fn start_session(&self) -> Result<SessionId> {
        let session_id = Uuid7::now();
        append_event(&self.state, session_id, None, EventData::SessionStarted);
        Ok(session_id)
    }

    async fn end_session(&self, id: SessionId) -> Result<()> {
        append_event(&self.state, id, None, EventData::SessionEnded);
        Ok(())
    }

    async fn begin_turn(&self, request: BeginTurnRequest) -> Result<Arc<dyn TurnHandle>> {
        let session_id = match request.session_id {
            Some(session_id) => session_id,
            None => self.start_session().await?,
        };
        let turn_id = Uuid7::now();
        let mut latest_event_id = Some(append_event(
            &self.state,
            session_id,
            Some(turn_id),
            EventData::TurnStarted,
        ));
        if !request.input.is_empty() {
            latest_event_id = Some(append_event(
                &self.state,
                session_id,
                Some(turn_id),
                EventData::Messages {
                    messages: request.input,
                    response_id: None,
                },
            ));
        }
        Ok(Arc::new(FakeTurnHandle {
            state: Arc::clone(&self.state),
            record: TurnRecord {
                id: turn_id,
                session_id,
            },
            latest_event_id: Mutex::new(latest_event_id),
        }))
    }

    async fn get_events(&self, query: Option<EventQuery>) -> Result<GetEventsResult> {
        let state = self.state.lock().expect("state poisoned");
        let mut events = state.conversation.events.clone();

        if let Some(query) = query {
            if let Some(session_id) = query.session_id {
                events.retain(|event| event.session_id == Some(session_id));
            }
            if let Some(turn_id) = query.turn_id {
                events.retain(|event| event.turn_id == Some(turn_id));
            }
            if let Some(types) = query.types {
                events.retain(|event| types.iter().any(|ty| event_type(event) == ty.as_str()));
            }
            match query.direction.unwrap_or(EventQueryDirection::Asc) {
                EventQueryDirection::Asc => {
                    if let Some(cursor) = query.cursor {
                        events.retain(|event| event.id > cursor);
                    }
                }
                EventQueryDirection::Desc => {
                    events.reverse();
                    if let Some(cursor) = query.cursor {
                        events.retain(|event| event.id < cursor);
                    }
                }
            }
            if let Some(limit) = query.limit {
                events.truncate(limit as usize);
            }
        }

        let cursor = events.last().map(|event| event.id);
        Ok(GetEventsResult { events, cursor })
    }

    async fn watch_events(
        &self,
        _after_exclusive: Bound<exoharness::EventId>,
    ) -> Result<EventStream> {
        Ok(Box::pin(stream::empty()))
    }

    async fn get_event(&self, id: exoharness::EventId) -> Result<Option<Event>> {
        let state = self.state.lock().expect("state poisoned");
        Ok(state
            .conversation
            .events
            .iter()
            .find(|event| event.id == id)
            .cloned())
    }

    async fn add_events(&self, request: AddEventsRequest) -> Result<AddEventsResult> {
        let mut state = self.state.lock().expect("state poisoned");
        if request.expected_head != state.conversation.record.latest_event_id {
            return Err(anyhow!("head mismatch"));
        }

        let mut event_ids = Vec::new();
        let mut latest_event_id = state.conversation.record.latest_event_id;

        for data in request.data {
            let event_id = Uuid7::now();
            let created_at = event_id.timestamp().expect("uuid7 timestamp");
            let event = Event {
                id: event_id,
                conversation_id: state.conversation.record.id,
                session_id: request.session_id,
                turn_id: request.turn_id,
                created_at,
                data,
            };
            latest_event_id = Some(event_id);
            event_ids.push(event_id);
            state.conversation.events.push(event);
        }

        let latest_event_id = latest_event_id.expect("at least one event");
        state.conversation.record.latest_event_id = Some(latest_event_id);
        Ok(AddEventsResult {
            event_ids,
            latest_event_id,
        })
    }

    async fn fork(&self, _request: ForkConversationRequest) -> Result<Arc<dyn ConversationHandle>> {
        Err(anyhow!("not implemented"))
    }

    async fn write_artifact(&self, _request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        Err(anyhow!("not implemented"))
    }

    async fn read_artifact(&self, _request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        Ok(None)
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        Ok(Vec::new())
    }

    async fn create_sandbox(&self, _request: CreateSandboxRequest) -> Result<SandboxId> {
        Err(anyhow!("not implemented"))
    }

    async fn snapshot_sandbox(&self, _id: SandboxId) -> Result<SnapshotId> {
        Err(anyhow!("not implemented"))
    }

    async fn start_sandbox(&self, _request: StartSandboxRequest) -> Result<()> {
        Err(anyhow!("not implemented"))
    }

    async fn stop_sandbox(&self, _id: SandboxId) -> Result<()> {
        Err(anyhow!("not implemented"))
    }

    async fn run_in_sandbox(
        &self,
        _request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        Ok(Box::new(FakeSandboxProcess))
    }

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>> {
        Ok(vec![test_model_binding_metadata()])
    }

    async fn put_binding(&self, _binding: Binding) -> Result<exoharness::BindingId> {
        Err(anyhow!("not implemented"))
    }

    async fn get_binding(&self, _id: &exoharness::BindingId) -> Result<Option<Binding>> {
        Ok(Some(test_model_binding()))
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        Ok(vec![test_secret_metadata()])
    }

    async fn put_secret(&self, _secret: PutSecretRequest) -> Result<exoharness::SecretId> {
        Err(anyhow!("not implemented"))
    }

    async fn get_secret(&self, _id: &exoharness::SecretId) -> Result<Option<Secret>> {
        Ok(Some(Secret::Key {
            value: "test-key".to_string(),
        }))
    }
}

struct FakeTurnHandle {
    state: Arc<Mutex<FakeState>>,
    record: TurnRecord,
    latest_event_id: Mutex<Option<exoharness::EventId>>,
}

#[async_trait]
impl TurnHandle for FakeTurnHandle {
    fn record(&self) -> &TurnRecord {
        &self.record
    }

    async fn add_events(&self, data: Vec<EventData>) -> Result<AddEventsResult> {
        let expected_head = *self
            .latest_event_id
            .lock()
            .expect("turn latest event id poisoned");
        let add_result = FakeConversationHandle {
            state: Arc::clone(&self.state),
            record: {
                let state = self.state.lock().expect("state poisoned");
                state.conversation.record.clone()
            },
        }
        .add_events(AddEventsRequest {
            session_id: Some(self.record.session_id),
            turn_id: Some(self.record.id),
            expected_head,
            data,
        })
        .await?;
        let mut latest_event_id = self
            .latest_event_id
            .lock()
            .expect("turn latest event id poisoned");
        *latest_event_id = Some(add_result.latest_event_id);
        Ok(add_result)
    }

    async fn finish(&self) -> Result<exoharness::EventId> {
        let event_id = append_event(
            &self.state,
            self.record.session_id,
            Some(self.record.id),
            EventData::TurnEnded,
        );
        let mut latest_event_id = self
            .latest_event_id
            .lock()
            .expect("turn latest event id poisoned");
        *latest_event_id = Some(event_id);
        Ok(event_id)
    }
}

struct FakeSandboxProcess;

#[async_trait]
impl SandboxProcess for FakeSandboxProcess {
    fn into_parts(self: Box<Self>) -> SandboxProcessParts {
        SandboxProcessParts {
            stdout: Box::pin(Cursor::new(Vec::new())),
            stderr: Box::pin(Cursor::new(Vec::new())),
            stdin: Box::pin(Cursor::new(Vec::new())),
            wait: async { Ok(0) }.boxed(),
        }
    }
}

fn append_event(
    state: &Arc<Mutex<FakeState>>,
    session_id: SessionId,
    turn_id: Option<TurnId>,
    data: EventData,
) -> exoharness::EventId {
    let mut state = state.lock().expect("state poisoned");
    let event_id = Uuid7::now();
    let created_at = event_id.timestamp().expect("uuid7 timestamp");
    let conversation_id = state.conversation.record.id;
    state.conversation.record.latest_event_id = Some(event_id);
    state.conversation.events.push(Event {
        id: event_id,
        conversation_id,
        session_id: Some(session_id),
        turn_id,
        created_at,
        data,
    });
    event_id
}

fn event_type(event: &Event) -> String {
    match &event.data {
        EventData::ConversationForked { .. } => "conversation_forked".to_string(),
        EventData::SessionStarted => "session_started".to_string(),
        EventData::SessionEnded => "session_ended".to_string(),
        EventData::TurnStarted => "turn_started".to_string(),
        EventData::TurnEnded => "turn_ended".to_string(),
        EventData::Messages { .. } => "messages".to_string(),
        EventData::ToolRequested { .. } => "tool_requested".to_string(),
        EventData::ToolResult { .. } => "tool_result".to_string(),
        EventData::ArtifactWritten { .. } => "artifact_written".to_string(),
        EventData::SandboxCreated { .. } => "sandbox_created".to_string(),
        EventData::SandboxStarted { .. } => "sandbox_started".to_string(),
        EventData::SandboxStopped { .. } => "sandbox_stopped".to_string(),
        EventData::SandboxSnapshotted { .. } => "sandbox_snapshotted".to_string(),
        EventData::Custom { event_type, .. } => event_type.clone(),
    }
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant {
        id: None,
        content: AssistantContent::String(text.to_string()),
    }
}

fn test_model_binding_metadata() -> BindingMetadata {
    let id = Uuid7::now();
    BindingMetadata {
        id,
        r#type: BindingType::Llm,
        name: "test-model".to_string(),
        created_at: id.timestamp().expect("uuid7 timestamp"),
    }
}

fn test_model_binding() -> Binding {
    Binding::Llm {
        name: "test-model".to_string(),
        model: "test-model".to_string(),
        base_url: None,
        secret_id: Some(Uuid7::now()),
    }
}

fn test_secret_metadata() -> SecretMetadata {
    let id = Uuid7::now();
    SecretMetadata {
        id,
        r#type: SecretType::Key,
        name: "test-secret".to_string(),
        created_at: id.timestamp().expect("uuid7 timestamp"),
    }
}

fn default_agent_config() -> AgentConfig {
    AgentConfig {
        instructions: Vec::new(),
        harness: crate::AgentHarnessKind::Basic,
        typescript: None,
        library_tools: Vec::new(),
        enable_agent_tool_creation: true,
        sandbox_image: None,
        enable_networking: false,
        model: "test-model".to_string(),
        max_output_tokens: None,
        max_tool_round_trips: Some(4),
        braintrust: None,
    }
}
