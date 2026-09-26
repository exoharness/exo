use serde::{Deserialize, Serialize};

use crate::vault::{CredentialDestination, VaultId, VaultRecord};
use crate::{
    AddEventsRequest, AddEventsResult, AgentId, AgentRecord, Artifact, ArtifactVersion,
    AttachSandboxRequest, BeginTurnRequest, Binding, BindingId, BindingRecord,
    CancelSandboxProcessRequest, CloseSandboxProcessInputRequest, ConversationId,
    CreateSandboxRequest, Event, EventData, EventId, EventQuery, ForkConversationRequest,
    ForkSandboxRequest, GetEventsResult, GetSandboxProcessEventsResult, ListConversationsRequest,
    ListConversationsResult, NewAgentRequest, NewConversationRequest, PutSecretRequest,
    ReadArtifactRequest, ResourceScope, RestoreSandboxRequest, SandboxAttachment, SandboxId,
    SandboxProcessEventQuery, SandboxProcessRecord, SandboxProcessStatus, SandboxRecord, Secret,
    SecretId, SecretMetadata, SessionId, SnapshotId, StartSandboxProcessRequest,
    StartSandboxRequest, ThreadId, ThreadRecord, TurnId, TurnRecord, WaitSandboxProcessRequest,
    WriteArtifactRequest, WriteSandboxProcessInputRequest,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadHandleInfo {
    pub agent_id: AgentId,
    pub record: ThreadRecord,
}

/// Compatibility name for [`ThreadHandleInfo`].
pub type ConversationHandleInfo = ThreadHandleInfo;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnHandleInfo {
    pub conversation: ConversationHandleInfo,
    pub record: TurnRecord,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SnapshotScope {
    Resource {
        scope: ResourceScope,
    },
    Turn {
        agent_id: AgentId,
        thread_id: ThreadId,
        session_id: SessionId,
        turn_id: TurnId,
    },
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
    ListEnvironments,
    PutEnvironment {
        environment: crate::EnvironmentDefinition,
    },
    DeleteEnvironment {
        name: String,
    },
    ListVaults {
        #[serde(default)]
        scope: ResourceScope,
    },
    GetVault {
        #[serde(default)]
        scope: ResourceScope,
        vault_id: VaultId,
    },
    CreateVault {
        name: String,
    },
    DeleteVault {
        vault_id: VaultId,
    },
    VaultListSecrets {
        #[serde(default)]
        scope: ResourceScope,
        vault_id: VaultId,
    },
    VaultPutSecret {
        #[serde(default)]
        scope: ResourceScope,
        vault_id: VaultId,
        request: PutSecretRequest,
    },
    VaultGetSecret {
        #[serde(default)]
        scope: ResourceScope,
        vault_id: VaultId,
        secret_id: SecretId,
    },
    VaultUpdateSecret {
        #[serde(default)]
        scope: ResourceScope,
        vault_id: VaultId,
        secret_id: SecretId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<Secret>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Option<crate::CredentialPolicy>,
    },
    VaultDeleteSecret {
        #[serde(default)]
        scope: ResourceScope,
        vault_id: VaultId,
        secret_id: SecretId,
    },
    VaultResolveSecret {
        #[serde(default)]
        scope: ResourceScope,
        vault_id: VaultId,
        secret_id: SecretId,
        target: CredentialDestination,
    },
    VaultRefreshSecret {
        #[serde(default)]
        scope: ResourceScope,
        vault_id: VaultId,
        secret_id: SecretId,
        target: CredentialDestination,
        rejected_revision: u64,
    },
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
    ListConversations {
        agent_id: AgentId,
        request: ListConversationsRequest,
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
    ListSandboxes {
        scope: ResourceScope,
    },
    CreateSandbox {
        scope: ResourceScope,
        request: CreateSandboxRequest,
    },
    ForkSandbox {
        scope: ResourceScope,
        request: ForkSandboxRequest,
    },
    RestoreSandbox {
        scope: ResourceScope,
        request: RestoreSandboxRequest,
    },
    TerminateSandbox {
        scope: ResourceScope,
        sandbox_id: SandboxId,
    },
    AttachSandbox {
        scope: ResourceScope,
        request: AttachSandboxRequest,
    },
    DetachSandbox {
        scope: ResourceScope,
        sandbox_id: SandboxId,
    },
    SnapshotSandbox {
        scope: SnapshotScope,
        sandbox_id: SandboxId,
    },
    StartSandbox {
        scope: SnapshotScope,
        request: StartSandboxRequest,
    },
    StopSandbox {
        scope: ResourceScope,
        sandbox_id: SandboxId,
    },
    StartSandboxProcess {
        scope: ResourceScope,
        request: StartSandboxProcessRequest,
    },
    WriteSandboxProcessInput {
        scope: ResourceScope,
        request: WriteSandboxProcessInputRequest,
    },
    CloseSandboxProcessInput {
        scope: ResourceScope,
        request: CloseSandboxProcessInputRequest,
    },
    GetSandboxProcessEvents {
        scope: ResourceScope,
        query: SandboxProcessEventQuery,
    },
    WaitSandboxProcess {
        scope: ResourceScope,
        request: WaitSandboxProcessRequest,
    },
    CancelSandboxProcess {
        scope: ResourceScope,
        request: CancelSandboxProcessRequest,
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
    ConversationUpdateEnvironment {
        agent_id: AgentId,
        conversation_id: ConversationId,
        environment: crate::EnvironmentDefinition,
    },
    ConversationAttachVaults {
        agent_id: AgentId,
        conversation_id: ConversationId,
        vaults: Vec<VaultId>,
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
            Self::ListVaults { .. } => "list_vaults",
            Self::GetVault { .. } => "get_vault",
            Self::CreateVault { .. } => "create_vault",
            Self::DeleteVault { .. } => "delete_vault",
            Self::VaultListSecrets { .. } => "vault_list_secrets",
            Self::VaultPutSecret { .. } => "vault_put_secret",
            Self::VaultGetSecret { .. } => "vault_get_secret",
            Self::VaultUpdateSecret { .. } => "vault_update_secret",
            Self::VaultDeleteSecret { .. } => "vault_delete_secret",
            Self::VaultResolveSecret { .. } => "vault_resolve_secret",
            Self::VaultRefreshSecret { .. } => "vault_refresh_secret",
            Self::ListAgents => "list_agents",
            Self::GetAgent { .. } => "get_agent",
            Self::NewAgent { .. } => "new_agent",
            Self::DeleteAgent { .. } => "delete_agent",
            Self::ListEnvironments => "list_environments",
            Self::PutEnvironment { .. } => "put_environment",
            Self::DeleteEnvironment { .. } => "delete_environment",
            Self::ListBindings => "list_bindings",
            Self::PutBinding { .. } => "put_binding",
            Self::GetBinding { .. } => "get_binding",
            Self::ListConversations { .. } => "list_conversations",
            Self::GetConversation { .. } => "get_conversation",
            Self::NewConversation { .. } => "new_conversation",
            Self::DeleteConversation { .. } => "delete_conversation",
            Self::AgentListArtifacts { .. } => "agent_list_artifacts",
            Self::AgentReadArtifact { .. } => "agent_read_artifact",
            Self::AgentWriteArtifact { .. } => "agent_write_artifact",
            Self::ListSandboxes { .. } => "list_sandboxes",
            Self::CreateSandbox { .. } => "create_sandbox",
            Self::ForkSandbox { .. } => "fork_sandbox",
            Self::RestoreSandbox { .. } => "restore_sandbox",
            Self::TerminateSandbox { .. } => "terminate_sandbox",
            Self::AttachSandbox { .. } => "attach_sandbox",
            Self::DetachSandbox { .. } => "detach_sandbox",
            Self::SnapshotSandbox { .. } => "snapshot_sandbox",
            Self::StartSandbox { .. } => "start_sandbox",
            Self::StopSandbox { .. } => "stop_sandbox",
            Self::StartSandboxProcess { .. } => "start_sandbox_process",
            Self::WriteSandboxProcessInput { .. } => "write_sandbox_process_input",
            Self::CloseSandboxProcessInput { .. } => "close_sandbox_process_input",
            Self::GetSandboxProcessEvents { .. } => "get_sandbox_process_events",
            Self::WaitSandboxProcess { .. } => "wait_sandbox_process",
            Self::CancelSandboxProcess { .. } => "cancel_sandbox_process",
            Self::AgentListBindings { .. } => "agent_list_bindings",
            Self::AgentPutBinding { .. } => "agent_put_binding",
            Self::AgentGetBinding { .. } => "agent_get_binding",
            Self::ConversationUpdateEnvironment { .. } => "conversation_update_environment",
            Self::ConversationAttachVaults { .. } => "conversation_attach_vaults",
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
            Self::ConversationListBindings { .. } => "conversation_list_bindings",
            Self::ConversationPutBinding { .. } => "conversation_put_binding",
            Self::ConversationGetBinding { .. } => "conversation_get_binding",
            Self::TurnAddEvents { .. } => "turn_add_events",
            Self::TurnWriteArtifact { .. } => "turn_write_artifact",
            Self::TurnFinish { .. } => "turn_finish",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Environments {
        environments: Vec<crate::EnvironmentDefinition>,
    },
    Vault {
        vault: Option<VaultRecord>,
    },
    Vaults {
        vaults: Vec<VaultRecord>,
    },
    SecretMetadata {
        metadata: SecretMetadata,
    },
    ResolvedSecret {
        revision: u64,
        secret: Secret,
    },
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
        result: ListConversationsResult<ConversationHandleInfo>,
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
    Sandboxes {
        sandboxes: Vec<SandboxRecord>,
    },
    SandboxAttachment {
        attachment: SandboxAttachment,
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
            Self::Environments { .. } => "environments",
            Self::Vault { .. } => "vault",
            Self::Vaults { .. } => "vaults",
            Self::SecretMetadata { .. } => "secret_metadata",
            Self::ResolvedSecret { .. } => "resolved_secret",
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
            Self::Sandboxes { .. } => "sandboxes",
            Self::SandboxAttachment { .. } => "sandbox_attachment",
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
