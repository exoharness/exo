use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use exoharness::{
    AgentHandle, ConversationHandle, EventId, FileSystemMount, ResponseId, Result, SessionId,
    ToolArguments, ToolCallId, ToolRequest, ToolResult, TurnId,
};
use lingua::{Message, UniversalStreamChunk, UniversalUsage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_stream::{Stream, wrappers::UnboundedReceiverStream};

use crate::braintrust::BraintrustTracingConfig;

#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct AgentConfig {
    pub instructions: Vec<Message>,
    #[serde(default)]
    pub harness: AgentHarnessKind,
    #[serde(default)]
    pub typescript: Option<TypeScriptHarnessConfig>,
    #[serde(default)]
    pub library_tools: Vec<ToolManifestEntry>,
    #[serde(default = "default_enable_agent_tool_creation")]
    pub enable_agent_tool_creation: bool,
    #[serde(default)]
    pub sandbox_image: Option<String>,
    #[serde(default)]
    pub enable_networking: bool,
    pub model: String,
    pub max_output_tokens: Option<i64>,
    pub max_tool_round_trips: Option<u32>,
    pub braintrust: Option<BraintrustTracingConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentHarnessKind {
    #[default]
    Basic,
    Rlm,
    #[serde(rename = "typescript")]
    TypeScript,
    Exoclaw,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeScriptHarnessConfig {
    pub module_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolManifestEntry {
    pub module_path: String,
    pub initialization: Value,
}

pub fn default_enable_agent_tool_creation() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ConversationConfig {
    pub enable_networking: bool,
    pub shell_program: Option<String>,
    #[serde(default)]
    pub mounts: Vec<FileSystemMount>,
    #[serde(default)]
    pub sandbox_scope: Option<SandboxScope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxScope {
    Agent,
    Conversation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationModelConfig {
    pub model: String,
    pub max_output_tokens: Option<i64>,
}

impl fmt::Display for ConversationModelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "model={}, max_output_tokens={}",
            self.model,
            self.max_output_tokens
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string())
        )
    }
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            enable_networking: false,
            shell_program: Some("/bin/bash".to_string()),
            mounts: Vec::new(),
            sandbox_scope: None,
        }
    }
}

pub fn effective_sandbox_scope(
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
) -> SandboxScope {
    conversation_config
        .sandbox_scope
        .unwrap_or(match agent_config.harness {
            AgentHarnessKind::Exoclaw => SandboxScope::Agent,
            _ => SandboxScope::Conversation,
        })
}

#[async_trait]
pub trait ModelClient: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse>;
    async fn complete_stream(&self, request: ModelRequest) -> Result<Box<dyn ModelResponseStream>>;
}

#[async_trait]
pub trait ModelResponseStream: Send {
    async fn next_chunk(&mut self) -> Result<Option<UniversalStreamChunk>>;
    async fn finish(self: Box<Self>) -> Result<ModelResponse>;
}

#[async_trait]
pub trait ToolRuntime: Send + Sync {
    async fn prepare_conversation(
        &self,
        _agent: &dyn AgentHandle,
        _conversation: &dyn ConversationHandle,
        _agent_config: &AgentConfig,
        _config: &ConversationConfig,
    ) -> Result<()> {
        Ok(())
    }

    async fn execute(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
        request: &ToolRequest,
    ) -> Result<ToolResult>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub response_id: Option<ResponseId>,
    pub messages: Vec<Message>,
    pub tool_calls: Vec<PendingToolCall>,
    pub usage: Option<UniversalUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingToolCall {
    pub tool_call_id: ToolCallId,
    pub request: ToolRequest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendRequest {
    pub input: Vec<Message>,
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone)]
pub struct SendResult {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub latest_event_id: EventId,
}

pub struct ExecutionStreamHandle {
    event_stream: UnboundedReceiverStream<Result<ExecutionStreamEvent>>,
}

impl ExecutionStreamHandle {
    pub fn new(event_stream: UnboundedReceiverStream<Result<ExecutionStreamEvent>>) -> Self {
        Self { event_stream }
    }
}

#[derive(Debug, Clone)]
pub enum ExecutionStreamEvent {
    FirstChunk {
        ttft: Duration,
    },
    Chunk(UniversalStreamChunk),
    ToolCall {
        tool_call_id: ToolCallId,
        tool_name: String,
        arguments: ToolArguments,
    },
    ToolResult {
        tool_call_id: ToolCallId,
        result: ToolResult,
    },
    Completed(SendResult),
}

impl Stream for ExecutionStreamHandle {
    type Item = Result<ExecutionStreamEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.event_stream).poll_next(cx)
    }
}
