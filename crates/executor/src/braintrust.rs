use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use async_trait::async_trait;
use braintrust_sdk_rust::{BraintrustClient, ParentSpanInfo, SpanHandle, SpanLog, SpanType};
use exoharness::{
    AgentRecord, ConversationRecord, Result, SessionId, ToolRequest, ToolResult, TurnId,
};
use lingua::UniversalUsage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::execution_tracing::{
    ExecutionTracer, LlmExecutionTrace, ToolExecutionTrace, TurnExecutionTrace,
};
use crate::{AgentConfig, ModelRequest, ModelResponse};

#[derive(Debug, Clone)]
pub struct BraintrustRuntimeConfig {
    pub api_key: String,
    pub app_url: Option<String>,
    pub api_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BraintrustTracingConfig {
    pub org_name: Option<String>,
    pub project: BraintrustProject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum BraintrustProject {
    Name(String),
    Id(String),
}

#[derive(Default)]
pub struct BraintrustTracer {
    runtime_config: Option<BraintrustRuntimeConfig>,
    clients: Mutex<HashMap<BraintrustClientKey, BraintrustClient>>,
    sessions: Mutex<HashMap<SessionId, TraceSession>>,
}

impl BraintrustTracer {
    pub fn new(runtime_config: Option<BraintrustRuntimeConfig>) -> Self {
        Self {
            runtime_config,
            clients: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    async fn session_for(
        &self,
        config: &BraintrustTracingConfig,
        agent: &AgentRecord,
        conversation: &ConversationRecord,
        agent_config: &AgentConfig,
        session_id: SessionId,
    ) -> Option<TraceSession> {
        if let Some(existing) = self.sessions.lock().await.get(&session_id).cloned() {
            return Some(existing);
        }

        let client = self.client_for(config).await?;
        let mut span_builder = client.span_builder().await.ok()?;

        if let Some(org_name) = &config.org_name {
            span_builder = span_builder.org_name(org_name);
        }
        span_builder = match &config.project {
            BraintrustProject::Name(project_name) => span_builder
                .project_name(project_name.clone())
                .parent_info(ParentSpanInfo::ProjectName {
                    project_name: project_name.clone(),
                }),
            BraintrustProject::Id(project_id) => {
                span_builder.parent_info(ParentSpanInfo::ProjectLogs {
                    object_id: project_id.clone(),
                })
            }
        };

        let span = span_builder
            .span_type(SpanType::Task)
            .purpose("executor_session")
            .build();
        span.log(
            SpanLog::builder()
                .name("executor_session")
                .metadata(metadata_object(json!({
                    "agent_id": agent.id,
                    "agent_slug": agent.slug,
                    "conversation_id": conversation.id,
                    "conversation_slug": conversation.slug,
                    "session_id": session_id,
                    "model": agent_config.model,
                })))
                .build()
                .ok()?,
        );

        let session = TraceSession { client, span };
        self.sessions
            .lock()
            .await
            .insert(session_id, session.clone());
        Some(session)
    }

    async fn client_for(&self, config: &BraintrustTracingConfig) -> Option<BraintrustClient> {
        let runtime_config = self.runtime_config.as_ref()?;
        let default_project = match &config.project {
            BraintrustProject::Name(project_name) => Some(project_name.clone()),
            BraintrustProject::Id(_) => None,
        };

        let key = BraintrustClientKey {
            api_key_hash: hash_secret(&runtime_config.api_key),
            app_url: runtime_config.app_url.clone(),
            api_url: runtime_config.api_url.clone(),
            org_name: config.org_name.clone(),
            default_project: default_project.clone(),
        };

        if let Some(existing) = self.clients.lock().await.get(&key).cloned() {
            return Some(existing);
        }

        let mut builder = BraintrustClient::builder()
            .api_key(runtime_config.api_key.clone())
            .blocking_login(true);
        if let Some(app_url) = &runtime_config.app_url {
            builder = builder.app_url(app_url.clone());
        }
        if let Some(api_url) = &runtime_config.api_url {
            builder = builder.api_url(api_url.clone());
        }
        if let Some(org_name) = &config.org_name {
            builder = builder.org_name(org_name);
        }
        if let Some(default_project) = default_project {
            builder = builder.default_project(default_project);
        }

        let client = builder.build().await.ok()?;
        self.clients.lock().await.insert(key, client.clone());
        Some(client)
    }
}

#[async_trait]
impl ExecutionTracer for BraintrustTracer {
    async fn flush(&self) -> Result<()> {
        let sessions = self
            .sessions
            .lock()
            .await
            .drain()
            .map(|(_, session)| session)
            .collect::<Vec<_>>();
        for session in sessions {
            session.finish();
        }

        let clients = self
            .clients
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for client in clients {
            client.flush().await?;
        }
        Ok(())
    }

    async fn start_turn(
        &self,
        config: Option<&BraintrustTracingConfig>,
        agent: &AgentRecord,
        conversation: &ConversationRecord,
        agent_config: &AgentConfig,
        session_id: SessionId,
        turn_id: TurnId,
        streamed: bool,
    ) -> Option<Box<dyn TurnExecutionTrace>> {
        let config = config?;
        let session = self
            .session_for(config, agent, conversation, agent_config, session_id)
            .await?;
        let mut span_builder = session.client.span_builder().await.ok()?;
        span_builder = span_builder.parent_info(parent_info_for_span(&session.span).await?);
        let span = span_builder
            .span_type(SpanType::Task)
            .purpose("executor_turn")
            .build();
        span.log(
            SpanLog::builder()
                .name("executor_turn")
                .metadata(metadata_object(json!({
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "model": agent_config.model,
                    "streamed": streamed,
                })))
                .build()
                .ok()?,
        );

        Some(Box::new(TurnTrace {
            client: session.client.clone(),
            span,
        }))
    }
}

#[derive(Clone)]
struct TraceSession {
    client: BraintrustClient,
    span: SpanHandle<BraintrustClient>,
}

impl TraceSession {
    fn finish(self) {
        self.span.end();
    }
}

pub struct TurnTrace {
    client: BraintrustClient,
    span: SpanHandle<BraintrustClient>,
}

impl TurnTrace {
    async fn start_llm_round_inner(
        &self,
        request: &ModelRequest,
        round_index: usize,
    ) -> Option<LlmRoundTrace> {
        let mut span_builder = self.client.span_builder().await.ok()?;
        span_builder = span_builder.parent_info(parent_info_for_span(&self.span).await?);
        let span = span_builder.span_type(SpanType::Llm).build();

        span.log(
            SpanLog::builder()
                .name(format!("responses:{}", request.model))
                .metadata(metadata_object(json!({
                    "round_index": round_index,
                    "runtime": "responses",
                    "model": request.model,
                    "max_output_tokens": request.max_output_tokens,
                    "tool_count": request.tools.len(),
                    "tools": request
                        .tools
                        .iter()
                        .map(|tool| tool.name.clone())
                        .collect::<Vec<_>>(),
                })))
                .input(serialize_value(&request.messages)?)
                .build()
                .ok()?,
        );

        Some(LlmRoundTrace { span })
    }

    async fn start_tool_call_inner(
        &self,
        request: &ToolRequest,
        round_index: usize,
    ) -> Option<ToolTrace> {
        let mut span_builder = self.client.span_builder().await.ok()?;
        span_builder = span_builder.parent_info(parent_info_for_span(&self.span).await?);
        let span = span_builder
            .span_type(SpanType::Tool)
            .purpose("tool_call")
            .build();

        span.log(
            SpanLog::builder()
                .name(request.function_name.clone())
                .metadata(metadata_object(json!({
                    "round_index": round_index,
                })))
                .input(serialize_value(request)?)
                .build()
                .ok()?,
        );

        Some(ToolTrace { span })
    }

    async fn finish(self) {
        self.span.end();
    }
}

#[async_trait]
impl TurnExecutionTrace for TurnTrace {
    fn export_parent(&self) -> Option<String> {
        self.span
            .export()
            .ok()
            .map(|components| components.to_str())
    }

    async fn start_llm_round(
        &self,
        request: &ModelRequest,
        round_index: usize,
    ) -> Option<Box<dyn LlmExecutionTrace>> {
        self.start_llm_round_inner(request, round_index)
            .await
            .map(|trace| Box::new(trace) as Box<dyn LlmExecutionTrace>)
    }

    async fn start_tool_call(
        &self,
        request: &ToolRequest,
        round_index: usize,
    ) -> Option<Box<dyn ToolExecutionTrace>> {
        self.start_tool_call_inner(request, round_index)
            .await
            .map(|trace| Box::new(trace) as Box<dyn ToolExecutionTrace>)
    }

    async fn finish_success(self: Box<Self>, latest_event_id: Option<exoharness::EventId>) {
        self.span.log(
            SpanLog::builder()
                .metadata(metadata_object(json!({
                    "status": "ok",
                    "latest_event_id": latest_event_id,
                })))
                .build()
                .expect("span log should build"),
        );
        (*self).finish().await;
    }

    async fn finish_error(self: Box<Self>, error: &anyhow::Error) {
        self.span.log(
            SpanLog::builder()
                .metadata(metadata_object(json!({ "status": "error" })))
                .error(error.to_string())
                .build()
                .expect("span log should build"),
        );
        (*self).finish().await;
    }
}

pub struct LlmRoundTrace {
    span: SpanHandle<BraintrustClient>,
}

impl LlmRoundTrace {
    async fn finish_success_inner(self, response: &ModelResponse, ttft: Option<Duration>) {
        let mut builder = SpanLog::builder()
            .output(
                serialize_value(&response.messages)
                    .unwrap_or(Value::String("failed to serialize response".to_string())),
            )
            .metadata(metadata_object(json!({
                "response_id": response.response_id,
            })));
        let metrics = llm_metrics(response.usage.as_ref(), ttft);
        if !metrics.is_empty() {
            builder = builder.metrics(metrics);
        }
        self.span
            .log(builder.build().expect("span log should build"));
        self.span.end();
    }

    async fn finish_error_inner(self, error: &anyhow::Error) {
        self.span.log(
            SpanLog::builder()
                .error(error.to_string())
                .build()
                .expect("span log should build"),
        );
        self.span.end();
    }
}

#[async_trait]
impl LlmExecutionTrace for LlmRoundTrace {
    async fn finish_success(self: Box<Self>, response: &ModelResponse, ttft: Option<Duration>) {
        (*self).finish_success_inner(response, ttft).await;
    }

    async fn finish_error(self: Box<Self>, error: &anyhow::Error) {
        (*self).finish_error_inner(error).await;
    }
}

pub struct ToolTrace {
    span: SpanHandle<BraintrustClient>,
}

impl ToolTrace {
    async fn finish_success_inner(self, result: &ToolResult) {
        self.span.log(
            SpanLog::builder()
                .output(result.clone())
                .build()
                .expect("span log should build"),
        );
        self.span.end();
    }

    async fn finish_error_inner(self, error: &anyhow::Error) {
        self.span.log(
            SpanLog::builder()
                .error(error.to_string())
                .build()
                .expect("span log should build"),
        );
        self.span.end();
    }
}

#[async_trait]
impl ToolExecutionTrace for ToolTrace {
    async fn finish_success(self: Box<Self>, result: &ToolResult) {
        (*self).finish_success_inner(result).await;
    }

    async fn finish_error(self: Box<Self>, error: &anyhow::Error) {
        (*self).finish_error_inner(error).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BraintrustClientKey {
    api_key_hash: u64,
    app_url: Option<String>,
    api_url: Option<String>,
    org_name: Option<String>,
    default_project: Option<String>,
}

fn hash_secret(secret: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    secret.hash(&mut hasher);
    hasher.finish()
}

async fn parent_info_for_span(span: &SpanHandle<BraintrustClient>) -> Option<ParentSpanInfo> {
    span.export().ok()?.to_parent_span_info().ok()
}

fn metadata_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

pub(crate) fn llm_metrics(
    usage: Option<&UniversalUsage>,
    ttft: Option<Duration>,
) -> HashMap<String, f64> {
    let mut metrics = response_usage_metrics(usage);
    if let Some(ttft) = ttft {
        metrics.insert("time_to_first_token".to_string(), ttft.as_secs_f64());
    }
    metrics
}

fn response_usage_metrics(usage: Option<&UniversalUsage>) -> HashMap<String, f64> {
    let Some(usage) = usage else {
        return HashMap::new();
    };

    let mut metrics = HashMap::new();
    if let Some(prompt_tokens) = usage.prompt_tokens {
        metrics.insert("prompt_tokens".to_string(), prompt_tokens as f64);
    }
    if let Some(completion_tokens) = usage.completion_tokens {
        metrics.insert("completion_tokens".to_string(), completion_tokens as f64);
    }
    if let (Some(prompt_tokens), Some(completion_tokens)) =
        (usage.prompt_tokens, usage.completion_tokens)
    {
        metrics.insert(
            "tokens".to_string(),
            (prompt_tokens + completion_tokens) as f64,
        );
    }
    if let Some(prompt_cached_tokens) = usage.prompt_cached_tokens {
        metrics.insert(
            "prompt_cached_tokens".to_string(),
            prompt_cached_tokens as f64,
        );
    }
    if let Some(prompt_cache_creation_tokens) = usage.prompt_cache_creation_tokens {
        metrics.insert(
            "prompt_cache_creation_tokens".to_string(),
            prompt_cache_creation_tokens as f64,
        );
    }
    if let Some(completion_reasoning_tokens) = usage.completion_reasoning_tokens {
        metrics.insert(
            "completion_reasoning_tokens".to_string(),
            completion_reasoning_tokens as f64,
        );
    }
    metrics
}

fn serialize_value<T: Serialize>(value: &T) -> Option<Value> {
    serde_json::to_value(value).ok()
}
