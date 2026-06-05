use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Bound;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use futures::future::BoxFuture;
use futures::io::{AsyncRead, AsyncWrite};
use lingua::{Message, universal::UniversalStreamChunk};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Result, Uuid7};

#[async_trait]
pub trait ExoHarness: Send + Sync {
    async fn list_agents(&self) -> Result<Vec<Arc<dyn AgentHandle>>>;
    async fn get_agent(&self, id: &AgentId) -> Result<Option<Arc<dyn AgentHandle>>>;
    async fn new_agent(&self, request: NewAgentRequest) -> Result<Arc<dyn AgentHandle>>;
    async fn delete_agent(&self, id: &AgentId) -> Result<bool>;

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>>;
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

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>>;
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
    /// Rebuilds the local TurnHandle facade for an already-created turn.
    /// The durable identity is the agent, conversation, session, and turn ids;
    /// this method only bundles those ids back into the trait object API.
    async fn turn_handle(&self, record: TurnRecord) -> Result<Arc<dyn TurnHandle>>;

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
    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord>;
    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()>;
    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()>;
    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult>;
    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus>;
    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus>;
    async fn run_in_sandbox(&self, request: RunInSandboxRequest)
    -> Result<Box<dyn SandboxProcess>>;

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>>;
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
    pub types: Option<Vec<EventKind>>,
}

/// Tag identifying an `EventData` variant — used by `EventQuery::types` to
/// filter events by kind without stringly comparing snake_case names. The
/// constants below cover every built-in variant; `EventKind::custom(name)`
/// is the escape hatch for `EventData::Custom { event_type, .. }`.
///
/// Wire format is the same single-string-per-kind shape the underlying
/// `EventData` serde tag uses (`#[serde(transparent)]`), so changing
/// `EventQuery::types` from `Vec<String>` to `Vec<EventKind>` is a
/// source-level change only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventKind(Cow<'static, str>);

impl EventKind {
    pub const CONVERSATION_CREATED: EventKind = EventKind(Cow::Borrowed("conversation_created"));
    pub const CONVERSATION_UPDATED: EventKind = EventKind(Cow::Borrowed("conversation_updated"));
    pub const CONVERSATION_DELETED: EventKind = EventKind(Cow::Borrowed("conversation_deleted"));
    pub const CONVERSATION_FORKED: EventKind = EventKind(Cow::Borrowed("conversation_forked"));
    pub const SESSION_STARTED: EventKind = EventKind(Cow::Borrowed("session_started"));
    pub const SESSION_ENDED: EventKind = EventKind(Cow::Borrowed("session_ended"));
    pub const TURN_STARTED: EventKind = EventKind(Cow::Borrowed("turn_started"));
    pub const TURN_ENDED: EventKind = EventKind(Cow::Borrowed("turn_ended"));
    pub const MESSAGES: EventKind = EventKind(Cow::Borrowed("messages"));
    pub const TOOL_REQUESTED: EventKind = EventKind(Cow::Borrowed("tool_requested"));
    pub const TOOL_RESULT: EventKind = EventKind(Cow::Borrowed("tool_result"));
    pub const LINGUA_STREAM_CHUNK: EventKind = EventKind(Cow::Borrowed("lingua_stream_chunk"));
    pub const ARTIFACT_WRITTEN: EventKind = EventKind(Cow::Borrowed("artifact_written"));
    pub const SANDBOX_CREATED: EventKind = EventKind(Cow::Borrowed("sandbox_created"));
    pub const SANDBOX_STARTED: EventKind = EventKind(Cow::Borrowed("sandbox_started"));
    pub const SANDBOX_STOPPED: EventKind = EventKind(Cow::Borrowed("sandbox_stopped"));
    pub const SANDBOX_SNAPSHOTTED: EventKind = EventKind(Cow::Borrowed("sandbox_snapshotted"));
    pub const SANDBOX_PROCESS_STARTED: EventKind =
        EventKind(Cow::Borrowed("sandbox_process_started"));
    pub const SANDBOX_PROCESS_STATE_UPDATED: EventKind =
        EventKind(Cow::Borrowed("sandbox_process_state_updated"));
    pub const SANDBOX_PROCESS_EVENT: EventKind = EventKind(Cow::Borrowed("sandbox_process_event"));

    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
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
    ConversationCreated {
        slug: String,
        name: String,
    },
    ConversationUpdated {
        slug: Option<String>,
        name: Option<String>,
    },
    ConversationDeleted,
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
    LinguaStreamChunk {
        chunk: UniversalStreamChunk,
    },
    ArtifactWritten {
        artifact_id: ArtifactId,
        path: String,
        version: u64,
    },
    SandboxCreated {
        sandbox_id: SandboxId,
        provider: SandboxProvider,
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
    SandboxProcessStarted {
        sandbox_id: SandboxId,
        process_id: SandboxProcessId,
        command: Vec<String>,
        cwd: Option<String>,
        mode: SandboxProcessMode,
        stdin: SandboxProcessStdin,
        output: SandboxProcessOutput,
        lifecycle: SandboxProcessLifecycle,
        status: SandboxProcessStatus,
        provider_state: Option<Value>,
    },
    SandboxProcessStateUpdated {
        sandbox_id: SandboxId,
        process_id: SandboxProcessId,
        status: SandboxProcessStatus,
        provider_state: Option<Value>,
    },
    SandboxProcessEvent {
        sandbox_id: SandboxId,
        process_id: SandboxProcessId,
        event: SandboxProcessEvent,
    },
    Custom {
        event_type: String,
        payload: Value,
    },
}

impl EventData {
    /// Tag identifying this event's variant. Source of truth for the
    /// `EventQuery::types` filter on `get_events`.
    pub fn kind(&self) -> EventKind {
        match self {
            Self::ConversationCreated { .. } => EventKind::CONVERSATION_CREATED,
            Self::ConversationUpdated { .. } => EventKind::CONVERSATION_UPDATED,
            Self::ConversationDeleted => EventKind::CONVERSATION_DELETED,
            Self::ConversationForked { .. } => EventKind::CONVERSATION_FORKED,
            Self::SessionStarted => EventKind::SESSION_STARTED,
            Self::SessionEnded => EventKind::SESSION_ENDED,
            Self::TurnStarted => EventKind::TURN_STARTED,
            Self::TurnEnded => EventKind::TURN_ENDED,
            Self::Messages { .. } => EventKind::MESSAGES,
            Self::ToolRequested { .. } => EventKind::TOOL_REQUESTED,
            Self::ToolResult { .. } => EventKind::TOOL_RESULT,
            Self::LinguaStreamChunk { .. } => EventKind::LINGUA_STREAM_CHUNK,
            Self::ArtifactWritten { .. } => EventKind::ARTIFACT_WRITTEN,
            Self::SandboxCreated { .. } => EventKind::SANDBOX_CREATED,
            Self::SandboxStarted { .. } => EventKind::SANDBOX_STARTED,
            Self::SandboxStopped { .. } => EventKind::SANDBOX_STOPPED,
            Self::SandboxSnapshotted { .. } => EventKind::SANDBOX_SNAPSHOTTED,
            Self::SandboxProcessStarted { .. } => EventKind::SANDBOX_PROCESS_STARTED,
            Self::SandboxProcessStateUpdated { .. } => EventKind::SANDBOX_PROCESS_STATE_UPDATED,
            Self::SandboxProcessEvent { .. } => EventKind::SANDBOX_PROCESS_EVENT,
            Self::Custom { event_type, .. } => EventKind::custom(event_type.clone()),
        }
    }
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
    pub provider: SandboxProvider,
    pub image: String,
    pub default_workdir: Option<String>,
    pub file_system_mounts: Option<Vec<FileSystemMount>>,
    pub enable_networking: Option<bool>,
    pub idle_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProvider {
    #[default]
    Daytona,
    AppleContainer,
    Docker,
    #[serde(alias = "local")]
    LocalProcess,
}

impl SandboxProvider {
    pub fn is_local(self) -> bool {
        matches!(
            self,
            Self::AppleContainer | Self::Docker | Self::LocalProcess
        )
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartSandboxProcessRequest {
    pub sandbox_id: SandboxId,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub mode: SandboxProcessMode,
    #[serde(default)]
    pub stdin: SandboxProcessStdin,
    #[serde(default)]
    pub output: SandboxProcessOutput,
    #[serde(default)]
    pub lifecycle: SandboxProcessLifecycle,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProcessMode {
    #[default]
    Exec,
    Pty,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProcessStdin {
    None,
    #[default]
    Open,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProcessOutput {
    Buffered,
    #[default]
    Stream,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProcessLifecycle {
    #[default]
    Attached,
    Detached,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxProcessRecord {
    pub id: SandboxProcessId,
    pub sandbox_id: SandboxId,
    pub status: SandboxProcessStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SandboxProcessStatus {
    Running,
    Exited { exit_code: i32 },
    Failed { message: String },
    Cancelled,
}

impl SandboxProcessStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxProcessEventQuery {
    pub sandbox_id: SandboxId,
    pub process_id: SandboxProcessId,
    pub after: Option<u64>,
    pub limit: Option<u32>,
    pub follow: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetSandboxProcessEventsResult {
    pub events: Vec<SandboxProcessEvent>,
    pub cursor: Option<u64>,
    pub status: SandboxProcessStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SandboxProcessEvent {
    Stdout { cursor: u64, data: Vec<u8> },
    Stderr { cursor: u64, data: Vec<u8> },
    Exit { cursor: u64, exit_code: i32 },
    Error { cursor: u64, message: String },
    Cancelled { cursor: u64 },
}

impl SandboxProcessEvent {
    pub fn cursor(&self) -> u64 {
        match self {
            Self::Stdout { cursor, .. }
            | Self::Stderr { cursor, .. }
            | Self::Exit { cursor, .. }
            | Self::Error { cursor, .. }
            | Self::Cancelled { cursor } => *cursor,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteSandboxProcessInputRequest {
    pub sandbox_id: SandboxId,
    pub process_id: SandboxProcessId,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloseSandboxProcessInputRequest {
    pub sandbox_id: SandboxId,
    pub process_id: SandboxProcessId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaitSandboxProcessRequest {
    pub sandbox_id: SandboxId,
    pub process_id: SandboxProcessId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelSandboxProcessRequest {
    pub sandbox_id: SandboxId,
    pub process_id: SandboxProcessId,
    pub signal: Option<String>,
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
pub struct BindingRecord {
    pub id: BindingId,
    pub r#type: BindingType,
    pub name: String,
    pub created_at: DateTimeUtc,
    pub binding: Binding,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BindingType {
    Env,
    Mcp,
    Llm,
    Sandbox,
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
    /// How to reach a remote sandbox provider.
    Sandbox {
        name: String,
        config: SandboxProviderConfig,
    },
}

/// Per-provider sandbox config for a `Binding::Sandbox`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum SandboxProviderConfig {
    Docker {
        #[serde(default = "default_docker_image")]
        default_image: String,
    },
    Daytona {
        /// Secret-store id of the API key.
        api_key_secret_id: SecretId,
        /// Daytona `target` (`us` / `eu` / `experimental`; Note: CRIU snapshot support requires `experimental` as of 6/5/2026).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        organization_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_url: Option<String>,
        #[serde(default = "default_daytona_image")]
        default_image: String,
    },
    // To be implemented in followup PRs:
    // E2b {
    //     api_key_secret_id: SecretId,
    //     api_url: Option<String>,
    //     default_image: String,  // #[serde(default = "default_e2b_image")]
    // },
    // ExeDev {
    //     token_secret_id: SecretId,     // bearer token (SSH-key-signed), not an api key
    //     region: Option<String>,        // datacenter: LAX | NYC | FRA | ...
    //     api_url: Option<String>,
    //     default_image: String,   // #[serde(default = "default_exedev_image")]
    // },
}

pub fn default_daytona_image() -> String {
    "daytonaio/sandbox:0.8.0".to_string()
}
pub fn default_docker_image() -> String {
    "docker.io/library/ubuntu:24.04".to_string()
}

impl SandboxProviderConfig {
    pub fn provider(&self) -> SandboxProvider {
        match self {
            Self::Daytona { .. } => SandboxProvider::Daytona,
            Self::Docker { .. } => SandboxProvider::Docker,
        }
    }

    /// The binding's configured default base image.
    pub fn default_image(&self) -> &str {
        match self {
            Self::Daytona { default_image, .. } | Self::Docker { default_image, .. } => {
                default_image
            }
        }
    }
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
pub type SandboxProcessId = String;
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
crate::impl_has_uuid7_id!(BindingRecord, id);
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
    fn serializes_mount_modes_as_ro_rw() {
        let ro =
            serde_json::to_value(FileSystemMountMode::ReadOnly).expect("mode should serialize");
        let rw =
            serde_json::to_value(FileSystemMountMode::ReadWrite).expect("mode should serialize");
        assert_eq!(ro, Value::String("ro".to_string()));
        assert_eq!(rw, Value::String("rw".to_string()));
    }
}
