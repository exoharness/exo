use serde::{Deserialize, Serialize};

use crate::{
    AddEventsRequest, AddEventsResult, AgentId, AgentRecord, Artifact, ArtifactVersion,
    BeginTurnRequest, Binding, BindingId, BindingRecord, CancelSandboxProcessRequest,
    CloseSandboxProcessInputRequest, ConversationId, ConversationRecord, CreateSandboxRequest,
    Event, EventData, EventId, EventQuery, ForkConversationRequest, GetEventsResult,
    GetSandboxProcessEventsResult, NewAgentRequest, NewConversationRequest, PutSecretRequest,
    ReadArtifactRequest, SandboxId, SandboxProcessEventQuery, SandboxProcessRecord,
    SandboxProcessStatus, Secret, SecretId, SecretMetadata, SessionId, SnapshotId,
    StartSandboxProcessRequest, StartSandboxRequest, TurnId, TurnRecord, WaitSandboxProcessRequest,
    WriteArtifactRequest, WriteSandboxProcessInputRequest,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationHandleInfo {
    pub agent_id: AgentId,
    pub record: ConversationRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnHandleInfo {
    pub conversation: ConversationHandleInfo,
    pub record: TurnRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientMessage {
    Request { id: u64, request: Request },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    Response {
        id: u64,
        ok: bool,
        response: Option<Response>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    ListAgents,
    GetAgent {
        agent_id: AgentId,
    },
    NewAgent {
        request: NewAgentRequest,
    },
    DeleteAgent {
        agent_id: AgentId,
    },
    ListBindings,
    PutBinding {
        binding: Binding,
    },
    GetBinding {
        binding_id: BindingId,
    },
    ListSecrets,
    PutSecret {
        request: PutSecretRequest,
    },
    GetSecret {
        secret_id: SecretId,
    },
    ListConversations {
        agent_id: AgentId,
    },
    GetConversation {
        agent_id: AgentId,
        conversation_id: ConversationId,
    },
    NewConversation {
        agent_id: AgentId,
        request: NewConversationRequest,
    },
    DeleteConversation {
        agent_id: AgentId,
        conversation_id: ConversationId,
    },
    AgentListArtifacts {
        agent_id: AgentId,
    },
    AgentReadArtifact {
        agent_id: AgentId,
        request: ReadArtifactRequest,
    },
    AgentWriteArtifact {
        agent_id: AgentId,
        request: WriteArtifactRequest,
    },
    AgentListBindings {
        agent_id: AgentId,
    },
    AgentPutBinding {
        agent_id: AgentId,
        binding: Binding,
    },
    AgentGetBinding {
        agent_id: AgentId,
        binding_id: BindingId,
    },
    AgentListSecrets {
        agent_id: AgentId,
    },
    AgentPutSecret {
        agent_id: AgentId,
        request: PutSecretRequest,
    },
    AgentGetSecret {
        agent_id: AgentId,
        secret_id: SecretId,
    },
    ConversationStartSession {
        agent_id: AgentId,
        conversation_id: ConversationId,
    },
    ConversationEndSession {
        agent_id: AgentId,
        conversation_id: ConversationId,
        session_id: SessionId,
    },
    ConversationBeginTurn {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: BeginTurnRequest,
    },
    ConversationGetEvents {
        agent_id: AgentId,
        conversation_id: ConversationId,
        query: Option<EventQuery>,
    },
    ConversationGetEvent {
        agent_id: AgentId,
        conversation_id: ConversationId,
        event_id: EventId,
    },
    ConversationAddEvents {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: AddEventsRequest,
    },
    ConversationFork {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: ForkConversationRequest,
    },
    ConversationListArtifacts {
        agent_id: AgentId,
        conversation_id: ConversationId,
    },
    ConversationReadArtifact {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: ReadArtifactRequest,
    },
    ConversationWriteArtifact {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: WriteArtifactRequest,
    },
    ConversationCreateSandbox {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: CreateSandboxRequest,
    },
    ConversationSnapshotSandbox {
        agent_id: AgentId,
        conversation_id: ConversationId,
        sandbox_id: SandboxId,
    },
    ConversationStartSandbox {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: StartSandboxRequest,
    },
    ConversationStopSandbox {
        agent_id: AgentId,
        conversation_id: ConversationId,
        sandbox_id: SandboxId,
    },
    ConversationStartSandboxProcess {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: StartSandboxProcessRequest,
    },
    ConversationWriteSandboxProcessInput {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: WriteSandboxProcessInputRequest,
    },
    ConversationCloseSandboxProcessInput {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: CloseSandboxProcessInputRequest,
    },
    ConversationGetSandboxProcessEvents {
        agent_id: AgentId,
        conversation_id: ConversationId,
        query: SandboxProcessEventQuery,
    },
    ConversationWaitSandboxProcess {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: WaitSandboxProcessRequest,
    },
    ConversationCancelSandboxProcess {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: CancelSandboxProcessRequest,
    },
    ConversationListBindings {
        agent_id: AgentId,
        conversation_id: ConversationId,
    },
    ConversationPutBinding {
        agent_id: AgentId,
        conversation_id: ConversationId,
        binding: Binding,
    },
    ConversationGetBinding {
        agent_id: AgentId,
        conversation_id: ConversationId,
        binding_id: BindingId,
    },
    ConversationListSecrets {
        agent_id: AgentId,
        conversation_id: ConversationId,
    },
    ConversationPutSecret {
        agent_id: AgentId,
        conversation_id: ConversationId,
        request: PutSecretRequest,
    },
    ConversationGetSecret {
        agent_id: AgentId,
        conversation_id: ConversationId,
        secret_id: SecretId,
    },
    TurnAddEvents {
        agent_id: AgentId,
        conversation_id: ConversationId,
        session_id: SessionId,
        turn_id: TurnId,
        data: Vec<EventData>,
    },
    TurnWriteArtifact {
        agent_id: AgentId,
        conversation_id: ConversationId,
        session_id: SessionId,
        turn_id: TurnId,
        request: WriteArtifactRequest,
    },
    TurnFinish {
        agent_id: AgentId,
        conversation_id: ConversationId,
        session_id: SessionId,
        turn_id: TurnId,
    },
}

impl Request {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ListAgents => "list_agents",
            Self::GetAgent { .. } => "get_agent",
            Self::NewAgent { .. } => "new_agent",
            Self::DeleteAgent { .. } => "delete_agent",
            Self::ListBindings => "list_bindings",
            Self::PutBinding { .. } => "put_binding",
            Self::GetBinding { .. } => "get_binding",
            Self::ListSecrets => "list_secrets",
            Self::PutSecret { .. } => "put_secret",
            Self::GetSecret { .. } => "get_secret",
            Self::ListConversations { .. } => "list_conversations",
            Self::GetConversation { .. } => "get_conversation",
            Self::NewConversation { .. } => "new_conversation",
            Self::DeleteConversation { .. } => "delete_conversation",
            Self::AgentListArtifacts { .. } => "agent_list_artifacts",
            Self::AgentReadArtifact { .. } => "agent_read_artifact",
            Self::AgentWriteArtifact { .. } => "agent_write_artifact",
            Self::AgentListBindings { .. } => "agent_list_bindings",
            Self::AgentPutBinding { .. } => "agent_put_binding",
            Self::AgentGetBinding { .. } => "agent_get_binding",
            Self::AgentListSecrets { .. } => "agent_list_secrets",
            Self::AgentPutSecret { .. } => "agent_put_secret",
            Self::AgentGetSecret { .. } => "agent_get_secret",
            Self::ConversationStartSession { .. } => "conversation_start_session",
            Self::ConversationEndSession { .. } => "conversation_end_session",
            Self::ConversationBeginTurn { .. } => "conversation_begin_turn",
            Self::ConversationGetEvents { .. } => "conversation_get_events",
            Self::ConversationGetEvent { .. } => "conversation_get_event",
            Self::ConversationAddEvents { .. } => "conversation_add_events",
            Self::ConversationFork { .. } => "conversation_fork",
            Self::ConversationListArtifacts { .. } => "conversation_list_artifacts",
            Self::ConversationReadArtifact { .. } => "conversation_read_artifact",
            Self::ConversationWriteArtifact { .. } => "conversation_write_artifact",
            Self::ConversationCreateSandbox { .. } => "conversation_create_sandbox",
            Self::ConversationSnapshotSandbox { .. } => "conversation_snapshot_sandbox",
            Self::ConversationStartSandbox { .. } => "conversation_start_sandbox",
            Self::ConversationStopSandbox { .. } => "conversation_stop_sandbox",
            Self::ConversationStartSandboxProcess { .. } => "conversation_start_sandbox_process",
            Self::ConversationWriteSandboxProcessInput { .. } => {
                "conversation_write_sandbox_process_input"
            }
            Self::ConversationCloseSandboxProcessInput { .. } => {
                "conversation_close_sandbox_process_input"
            }
            Self::ConversationGetSandboxProcessEvents { .. } => {
                "conversation_get_sandbox_process_events"
            }
            Self::ConversationWaitSandboxProcess { .. } => "conversation_wait_sandbox_process",
            Self::ConversationCancelSandboxProcess { .. } => "conversation_cancel_sandbox_process",
            Self::ConversationListBindings { .. } => "conversation_list_bindings",
            Self::ConversationPutBinding { .. } => "conversation_put_binding",
            Self::ConversationGetBinding { .. } => "conversation_get_binding",
            Self::ConversationListSecrets { .. } => "conversation_list_secrets",
            Self::ConversationPutSecret { .. } => "conversation_put_secret",
            Self::ConversationGetSecret { .. } => "conversation_get_secret",
            Self::TurnAddEvents { .. } => "turn_add_events",
            Self::TurnWriteArtifact { .. } => "turn_write_artifact",
            Self::TurnFinish { .. } => "turn_finish",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Agents {
        agents: Vec<AgentRecord>,
    },
    Agent {
        agent: Option<AgentRecord>,
    },
    Bool {
        value: bool,
    },
    Conversations {
        conversations: Vec<ConversationHandleInfo>,
    },
    Conversation {
        conversation: Option<ConversationHandleInfo>,
    },
    Events {
        result: GetEventsResult,
    },
    Event {
        event: Option<Event>,
    },
    AddEvents {
        result: AddEventsResult,
    },
    SessionId {
        session_id: SessionId,
    },
    ArtifactVersions {
        artifacts: Vec<ArtifactVersion>,
    },
    Artifact {
        artifact: Option<Artifact>,
    },
    ArtifactVersion {
        artifact: ArtifactVersion,
    },
    SandboxId {
        sandbox_id: SandboxId,
    },
    SnapshotId {
        snapshot_id: SnapshotId,
    },
    SandboxProcess {
        process: SandboxProcessRecord,
    },
    SandboxProcessEvents {
        result: GetSandboxProcessEventsResult,
    },
    SandboxProcessStatus {
        status: SandboxProcessStatus,
    },
    Bindings {
        bindings: Vec<BindingRecord>,
    },
    Binding {
        binding: Option<Binding>,
    },
    Secrets {
        secrets: Vec<SecretMetadata>,
    },
    Secret {
        secret: Option<Secret>,
    },
    BindingId {
        binding_id: BindingId,
    },
    SecretId {
        secret_id: SecretId,
    },
    Turn {
        turn: TurnHandleInfo,
    },
    EventId {
        event_id: EventId,
    },
    Unit,
}

impl Response {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Agents { .. } => "agents",
            Self::Agent { .. } => "agent",
            Self::Bool { .. } => "bool",
            Self::Conversations { .. } => "conversations",
            Self::Conversation { .. } => "conversation",
            Self::Events { .. } => "events",
            Self::Event { .. } => "event",
            Self::AddEvents { .. } => "add_events",
            Self::SessionId { .. } => "session_id",
            Self::ArtifactVersions { .. } => "artifact_versions",
            Self::Artifact { .. } => "artifact",
            Self::ArtifactVersion { .. } => "artifact_version",
            Self::SandboxId { .. } => "sandbox_id",
            Self::SnapshotId { .. } => "snapshot_id",
            Self::SandboxProcess { .. } => "sandbox_process",
            Self::SandboxProcessEvents { .. } => "sandbox_process_events",
            Self::SandboxProcessStatus { .. } => "sandbox_process_status",
            Self::Bindings { .. } => "bindings",
            Self::Binding { .. } => "binding",
            Self::Secrets { .. } => "secrets",
            Self::Secret { .. } => "secret",
            Self::BindingId { .. } => "binding_id",
            Self::SecretId { .. } => "secret_id",
            Self::Turn { .. } => "turn",
            Self::EventId { .. } => "event_id",
            Self::Unit => "unit",
        }
    }
}
