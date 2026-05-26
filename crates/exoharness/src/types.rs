use std::collections::HashMap;
use std::ops::Bound;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncWrite};
use lingua::Message;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Result, Uuid7};

#[async_trait]
pub trait ExoHarness: Send + Sync {
    async fn list_agents(&self) -> Result<Vec<Arc<dyn AgentHandle>>>;
    async fn get_agent(&self, id: &AgentId) -> Result<Option<Arc<dyn AgentHandle>>>;
    async fn new_agent(&self, request: NewAgentRequest) -> Result<Arc<dyn AgentHandle>>;
    async fn delete_agent(&self, id: &AgentId) -> Result<bool>;

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>>;
    async fn put_binding(&self, binding: Binding) -> Result<BindingId>;
    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>>;

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>>;
    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId>;
    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>>;
}

#[async_trait]
pub trait AgentHandle: Send + Sync {
    fn record(&self) -> &AgentRecord;

    async fn list_conversations(&self) -> Result<Vec<Arc<dyn ConversationHandle>>>;
    async fn get_conversation(
        &self,
        id: &ConversationId,
    ) -> Result<Option<Arc<dyn ConversationHandle>>>;
    async fn new_conversation(
        &self,
        request: NewConversationRequest,
    ) -> Result<Arc<dyn ConversationHandle>>;
    async fn delete_conversation(&self, id: &ConversationId) -> Result<bool>;

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>>;
    async fn put_binding(&self, binding: Binding) -> Result<BindingId>;
    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>>;

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>>;
    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId>;
    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>>;

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion>;
    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>>;
    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>>;
}

#[async_trait]
pub trait ConversationHandle: Send + Sync {
    fn record(&self) -> &ConversationRecord;

    async fn start_session(&self) -> Result<SessionId>;
    async fn end_session(&self, id: SessionId) -> Result<()>;
    async fn begin_turn(&self, request: BeginTurnRequest) -> Result<Arc<dyn TurnHandle>>;

    async fn get_events(&self, query: Option<EventQuery>) -> Result<GetEventsResult>;
    async fn watch_events(&self, after_exclusive: Bound<EventId>) -> Result<EventStream>;
    async fn get_event(&self, id: EventId) -> Result<Option<Event>>;
    async fn add_events(&self, request: AddEventsRequest) -> Result<AddEventsResult>;
    async fn fork(&self, request: ForkConversationRequest) -> Result<Arc<dyn ConversationHandle>>;

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion>;
    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>>;
    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>>;

    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId>;
    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId>;
    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()>;
    async fn stop_sandbox(&self, id: SandboxId) -> Result<()>;
    async fn run_in_sandbox(&self, request: RunInSandboxRequest)
    -> Result<Box<dyn SandboxProcess>>;

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>>;
    async fn put_binding(&self, binding: Binding) -> Result<BindingId>;
    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>>;

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>>;
    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId>;
    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>>;
}

#[async_trait]
pub trait TurnHandle: Send + Sync {
    fn record(&self) -> &TurnRecord;

    async fn add_events(&self, data: Vec<EventData>) -> Result<AddEventsResult>;
    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion>;
    async fn finish(&self) -> Result<EventId>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRecord {
    pub id: AgentId,
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewAgentRequest {
    pub slug: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationRecord {
    pub id: ConversationId,
    pub slug: String,
    pub name: String,
    pub latest_event_id: Option<EventId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewConversationRequest {
    pub slug: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnRecord {
    pub id: TurnId,
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BeginTurnRequest {
    pub session_id: Option<SessionId>,
    pub input: Vec<Message>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventQuery {
    pub cursor: Option<EventId>,
    pub direction: Option<EventQueryDirection>,
    pub limit: Option<u32>,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub types: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventQueryDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetEventsResult {
    pub events: Vec<Event>,
    pub cursor: Option<EventId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddEventsRequest {
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub expected_head: Option<EventId>,
    pub data: Vec<EventData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AddEventsResult {
    pub event_ids: Vec<EventId>,
    pub latest_event_id: EventId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkConversationRequest {
    pub up_to_inclusive: Option<EventId>,
    pub slug: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageRecord {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cached_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_creation_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_reasoning_tokens: Option<i64>,
    /// Cost in USD, computed at call time from the price table baked into
    /// this binary. `None` if the model is not in the table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Time to first token (streaming only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Wall-clock duration from request start to end of response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Server-reported processing time, if the provider surfaces one. Not
    /// currently populated — lingua does not yet expose this field — but
    /// reserved so the event format is stable when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub conversation_id: ConversationId,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub created_at: DateTimeUtc,
    pub data: EventData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    ConversationForked {
        source_conversation_id: ConversationId,
        up_to_inclusive: Option<EventId>,
    },
    SessionStarted,
    SessionEnded,
    TurnStarted,
    TurnEnded,
    Messages {
        messages: Vec<Message>,
        response_id: Option<ResponseId>,
        // Boxed to keep `EventData` small: `UsageRecord` is ~170 bytes and
        // most events don't carry it, so inlining it would bloat every
        // variant (and every enum that embeds `EventData`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Box<UsageRecord>>,
    },
    ToolRequested {
        tool_call_id: ToolCallId,
        response_id: Option<ResponseId>,
        request: ToolRequest,
    },
    ToolResult {
        tool_call_id: ToolCallId,
        result: ToolResult,
    },
    ArtifactWritten {
        artifact_id: ArtifactId,
        path: String,
        version: u64,
    },
    SandboxCreated {
        sandbox_id: SandboxId,
        image: String,
        default_workdir: String,
        file_system_mounts: Vec<FileSystemMount>,
        enable_networking: bool,
        idle_seconds: u64,
    },
    SandboxStarted {
        sandbox_id: SandboxId,
        snapshot_id: Option<SnapshotId>,
    },
    SandboxStopped {
        sandbox_id: SandboxId,
    },
    SandboxSnapshotted {
        sandbox_id: SandboxId,
        snapshot_id: SnapshotId,
    },
    Custom {
        event_type: String,
        payload: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolRequest {
    pub function_name: String,
    pub arguments: ToolArguments,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactVersion {
    pub artifact_id: ArtifactId,
    pub path: String,
    pub version: u64,
    pub created_at: DateTimeUtc,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    #[serde(flatten)]
    pub version: ArtifactVersion,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteArtifactRequest {
    pub path: String,
    pub contents: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadArtifactRequest {
    pub artifact_id: ArtifactId,
    pub version: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum FileSystemMountMode {
    #[serde(rename = "ro")]
    ReadOnly,
    #[serde(rename = "rw")]
    ReadWrite,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSystemMount {
    pub host_path: String,
    pub mount_path: String,
    pub mode: FileSystemMountMode,
    pub internal: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateSandboxRequest {
    pub image: String,
    pub default_workdir: Option<String>,
    pub file_system_mounts: Option<Vec<FileSystemMount>>,
    pub enable_networking: Option<bool>,
    pub idle_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartSandboxRequest {
    pub id: SandboxId,
    pub snapshot_id: SnapshotId,
    pub idle_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunInSandboxRequest {
    pub id: SandboxId,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[async_trait]
pub trait SandboxProcess: Send {
    fn into_parts(self: Box<Self>) -> SandboxProcessParts;
}

pub struct SandboxProcessParts {
    pub stdout: BoxAsyncRead,
    pub stderr: BoxAsyncRead,
    pub stdin: BoxAsyncWrite,
    pub wait: BoxFuture<'static, Result<i32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BindingMetadata {
    pub id: BindingId,
    pub r#type: BindingType,
    pub name: String,
    pub created_at: DateTimeUtc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BindingType {
    Env,
    Mcp,
    Llm,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretMetadata {
    pub id: SecretId,
    pub r#type: SecretType,
    pub name: String,
    pub created_at: DateTimeUtc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PutSecretRequest {
    pub name: String,
    pub secret: Secret,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecretType {
    Key,
    Oauth,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Binding {
    Env {
        name: String,
        env_var: String,
        secret_id: SecretId,
    },
    Mcp {
        name: String,
        server_url: String,
        secret_id: Option<SecretId>,
    },
    Llm {
        name: String,
        model: String,
        base_url: Option<String>,
        secret_id: Option<SecretId>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Secret {
    Key {
        value: String,
    },
    Oauth {
        access_token: String,
        refresh_token: Option<String>,
    },
}

pub type AgentId = Uuid7;
pub type ConversationId = Uuid7;
pub type SessionId = Uuid7;
pub type TurnId = Uuid7;
pub type EventId = Uuid7;
pub type ResponseId = Uuid7;
pub type ToolCallId = String;
pub type ArtifactId = Uuid7;
pub type SandboxId = String;
pub type SnapshotId = Uuid7;
pub type BindingId = Uuid7;
pub type SecretId = Uuid7;
pub type DateTimeUtc = DateTime<Utc>;
pub type ToolResult = Value;
pub type ToolArguments = Map<String, Value>;
pub type BoxAsyncRead = Pin<Box<dyn AsyncRead + Send + Unpin>>;
pub type BoxAsyncWrite = Pin<Box<dyn AsyncWrite + Send + Unpin>>;
pub type EventStream = Pin<Box<dyn Stream<Item = Result<Event>> + Send>>;

crate::impl_has_uuid7_id!(AgentRecord, id);
crate::impl_has_uuid7_id!(ConversationRecord, id);
crate::impl_has_uuid7_id!(TurnRecord, id);
crate::impl_has_uuid7_id!(Event, id);
crate::impl_has_uuid7_id!(BindingMetadata, id);
crate::impl_has_uuid7_id!(SecretMetadata, id);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_event_types_as_snake_case() {
        let event = EventData::SessionStarted;
        let value = serde_json::to_value(event).expect("event should serialize");
        assert_eq!(
            value.get("type").and_then(Value::as_str),
            Some("session_started")
        );
    }

    #[test]
    fn messages_event_deserializes_without_usage_field() {
        // Older logs predate per-message cost tracking and have no `usage`
        // field on Messages events; serde(default) must keep them readable.
        let json = serde_json::json!({
            "type": "messages",
            "messages": [],
            "response_id": null,
        });
        let event: EventData = serde_json::from_value(json).expect("legacy event should parse");
        match event {
            EventData::Messages { usage, .. } => assert!(usage.is_none()),
            _ => panic!("expected Messages variant"),
        }
    }

    #[test]
    fn messages_event_serializes_usage_when_present() {
        let event = EventData::Messages {
            messages: vec![],
            response_id: None,
            usage: Some(Box::new(UsageRecord {
                model: "claude-sonnet-4-6".to_string(),
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                cost_usd: Some(0.001),
                ..Default::default()
            })),
        };
        let value = serde_json::to_value(&event).expect("event should serialize");
        let usage = value.get("usage").expect("usage field present");
        assert_eq!(
            usage.get("model").and_then(Value::as_str),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            usage.get("prompt_tokens").and_then(Value::as_i64),
            Some(100)
        );
        assert!((usage.get("cost_usd").and_then(Value::as_f64).unwrap() - 0.001).abs() < 1e-9);
        // Round-trip back through the legacy-event parser
        let parsed: EventData = serde_json::from_value(value).expect("round-trip parse");
        assert!(matches!(parsed, EventData::Messages { usage: Some(_), .. }));
    }

    #[test]
    fn serializes_mount_modes_as_ro_rw() {
        let ro =
            serde_json::to_value(FileSystemMountMode::ReadOnly).expect("mode should serialize");
        let rw =
            serde_json::to_value(FileSystemMountMode::ReadWrite).expect("mode should serialize");
        assert_eq!(ro, Value::String("ro".to_string()));
        assert_eq!(rw, Value::String("rw".to_string()));
    }
}
