use std::collections::BTreeMap;

use exoharness::{
    AgentRecord, EventId, EventQueryDirection, SessionId, ThreadId, ThreadRecord, ToolCallId,
    ToolResult, TurnRecord,
};
use lingua::{Message, universal::UserContent};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn into_vec(self) -> Vec<T> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnAttention {
    #[default]
    Wake,
    Interrupt,
}

impl TurnAttention {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wake => "wake",
            Self::Interrupt => "interrupt",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadVisibility {
    #[default]
    Owner,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadSource {
    Automation { automation_id: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentsQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderIdentity {
    pub account_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadsQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<EventId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateThreadBody {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vaults: Vec<exoharness::vault::VaultId>,
    #[serde(alias = "conversation_id", skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    #[serde(alias = "conversation_slug", skip_serializing_if = "Option::is_none")]
    pub thread_slug: Option<String>,
    #[serde(alias = "conversation_name", skip_serializing_if = "Option::is_none")]
    pub thread_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub visibility: ThreadVisibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ThreadSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FrontendToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    #[serde(
        default = "default_frontend_tool_defer_loading",
        skip_serializing_if = "frontend_tool_defer_loading_is_default"
    )]
    pub defer_loading: bool,
}

fn default_frontend_tool_defer_loading() -> bool {
    true
}

fn frontend_tool_defer_loading_is_default(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnDeliveryCallback {
    pub url: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The runtime collects canonical events for at least this long before
    /// posting them to the receiver. Tool-call responses are still immediate.
    #[serde(default = "default_delivery_batch_interval_ms")]
    pub batch_interval_ms: u64,
}

fn default_delivery_batch_interval_ms() -> u64 {
    1_000
}

impl TurnDeliveryCallback {
    pub fn executes(&self, function_name: &str) -> bool {
        self.tools.iter().any(|tool| tool == function_name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendToolExecutionError {
    pub r#type: FrontendToolExecutionErrorType,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendToolExecutionErrorType {
    ExecutionError,
    UserRejected,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendToolExecutionResult {
    FrontendToolSuccess {
        output: ToolResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_input: Option<UserContent>,
    },
    FrontendToolError {
        output: ToolResult,
        error: FrontendToolExecutionError,
    },
}

impl FrontendToolExecutionResult {
    pub fn into_parts(
        self,
    ) -> (
        ToolResult,
        Option<FrontendToolExecutionError>,
        Option<UserContent>,
    ) {
        match self {
            Self::FrontendToolSuccess {
                output,
                model_input,
            } => (output, None, model_input),
            Self::FrontendToolError { output, error } => (output, Some(error), None),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubmitTurnBody<PageScope = ()> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub attention: TurnAttention,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<OneOrMany<Message>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontend_tools: Option<OneOrMany<FrontendToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_approve_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_scope: Option<PageScope>,
    #[serde(default)]
    pub reset_history: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_callback: Option<TurnDeliveryCallback>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponseBody {
    pub session_id: SessionId,
    pub approval_id: String,
    pub approved: bool,
    #[serde(default)]
    pub allow_for_tool: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendToolResultBody {
    pub session_id: SessionId,
    pub tool_call_id: ToolCallId,
    pub result: FrontendToolExecutionResult,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventsQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<exoharness::TurnId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<EventId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<EventQueryDirection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatchQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<EventId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListAgentsResult {
    pub agents: Vec<AgentRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListThreadsResult<T = ThreadRecord> {
    pub agent: AgentRecord,
    pub threads: Vec<T>,
    pub next_cursor: Option<EventId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateThreadResult {
    pub agent: AgentRecord,
    pub thread: ThreadRecord,
    pub harness: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitTurnResult {
    pub agent: AgentRecord,
    pub thread: ThreadRecord,
    pub turn: TurnRecord,
    pub harness: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteThreadResult {
    pub agent: AgentRecord,
    pub thread_id: ThreadId,
    pub deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStatusResult {
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelTurnResult {
    pub canceled_active_turn: bool,
    pub finished_event_id: Option<EventId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForkThreadBody {
    #[serde(alias = "conversation_name", skip_serializing_if = "Option::is_none")]
    pub thread_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadResult<T = ThreadRecord> {
    pub agent: AgentRecord,
    pub thread: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventResult {
    pub event_id: EventId,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ProviderError {
    ContextRequired {
        message: String,
        context: BTreeMap<String, String>,
    },
}
