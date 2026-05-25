use serde::{Deserialize, Serialize};

use crate::{
    AddEventsRequest, AddEventsResult, AgentId, AgentRecord, Artifact, ArtifactVersion, Binding,
    BindingId, BindingMetadata, ConversationId, ConversationRecord, Event, EventData, EventId,
    EventQuery, ForkConversationRequest, GetEventsResult, NewAgentRequest, NewConversationRequest,
    PutSecretRequest, ReadArtifactRequest, Secret, SecretId, SecretMetadata, SessionId, TurnRecord,
    WriteArtifactRequest,
};

pub type HandleId = u64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationHandleInfo {
    pub agent_id: AgentId,
    pub record: ConversationRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnHandleInfo {
    pub handle_id: HandleId,
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
        handle_id: HandleId,
        data: Vec<EventData>,
    },
    TurnWriteArtifact {
        handle_id: HandleId,
        request: WriteArtifactRequest,
    },
    TurnFinish {
        handle_id: HandleId,
    },
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
    Bindings {
        bindings: Vec<BindingMetadata>,
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
