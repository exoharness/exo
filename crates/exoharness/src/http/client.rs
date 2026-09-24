use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use tokio::sync::oneshot;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use url::Url;

use super::HTTP_EXOHARNESS_REQUEST_PATH;
use super::process::{
    LiveHttpSandboxProcess, spawn_http_sandbox_process_event_poller,
    spawn_http_sandbox_process_stdin_forwarder,
};
use crate::protocol::{
    ClientMessage, ConversationHandleInfo, Request, Response, ServerMessage, SnapshotScope,
};
use crate::vault::{ResolvedSecret, SecretTarget, VaultContext, VaultHandle, VaultId, VaultRecord};
use crate::{
    AddEventsRequest, AddEventsResult, AgentHandle, AgentId, AgentRecord, Artifact,
    ArtifactVersion, AttachSandboxRequest, BeginTurnRequest, Binding, BindingId, BindingRecord,
    CancelSandboxProcessRequest, CloseSandboxProcessInputRequest, ConversationHandle,
    ConversationId, ConversationRecord, CreateSandboxRequest, Event, EventData, EventId,
    EventQuery, EventStream, ExoHarness, ForkConversationRequest, ForkSandboxRequest,
    GetEventsResult, GetSandboxProcessEventsResult, ListConversationsRequest,
    ListConversationsResult, NewAgentRequest, NewConversationRequest, PutSecretRequest,
    ReadArtifactRequest, RestoreSandboxRequest, Result, RunInSandboxRequest, SandboxAttachment,
    SandboxHandle, SandboxId, SandboxProcess, SandboxProcessEventQuery, SandboxProcessParts,
    SandboxProcessRecord, SandboxProcessStatus, SandboxRecord, Secret, SecretId, SecretMetadata,
    SessionId, SnapshotHandle, SnapshotId, StartSandboxProcessRequest, StartSandboxRequest,
    TurnHandle, TurnRecord, WaitSandboxProcessRequest, WriteArtifactRequest,
    WriteSandboxProcessInputRequest,
};
use crate::{HttpClient, ResourceScope};

#[derive(Clone)]
pub struct HttpExoHarness {
    transport: Arc<dyn ExoHttpTransport>,
}

#[async_trait]
pub trait ExoHttpTransport: Send + Sync {
    fn endpoint(&self) -> &Url;
    async fn request(&self, request: Request) -> Result<Response>;
    async fn watch_events(
        &self,
        _agent_id: AgentId,
        _thread_id: crate::ThreadId,
        _after_exclusive: Bound<EventId>,
    ) -> Result<EventStream> {
        unsupported("watch_events")
    }
}

#[derive(Clone)]
struct RpcTransport {
    http: HttpClient,
    next_request_id: Arc<AtomicU64>,
}

impl HttpExoHarness {
    pub fn new(base_url: impl AsRef<str>, bearer_token: Option<String>) -> Result<Self> {
        let mut http = HttpClient::new(request_endpoint(base_url.as_ref())?)?;
        if let Some(token) = bearer_token {
            http = http.with_bearer_token(token);
        }
        Ok(Self::from_transport(Arc::new(RpcTransport {
            http,
            next_request_id: Arc::new(AtomicU64::new(1)),
        })))
    }

    pub fn from_transport(transport: Arc<dyn ExoHttpTransport>) -> Self {
        Self { transport }
    }

    pub fn endpoint(&self) -> &Url {
        self.transport.endpoint()
    }

    pub(super) async fn request(&self, request: Request) -> Result<Response> {
        self.transport.request(request).await
    }
}

#[async_trait]
impl ExoHttpTransport for RpcTransport {
    fn endpoint(&self) -> &Url {
        self.http.endpoint()
    }

    async fn request(&self, request: Request) -> Result<Response> {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let message = ClientMessage::Request { id, request };
        let message: ServerMessage = self
            .http
            .json(self.http.request(reqwest::Method::POST, "")?.json(&message))
            .await?;
        let ServerMessage::Response {
            id: response_id,
            ok,
            response,
            error,
        } = message;
        if response_id != id {
            bail!("HTTP exoharness response id {response_id} did not match request id {id}");
        }
        if ok {
            return response.ok_or_else(|| anyhow!("missing HTTP exoharness response payload"));
        }
        bail!(
            "{}",
            error.unwrap_or_else(|| "HTTP exoharness request failed".to_string())
        )
    }
}

#[async_trait]
impl ExoHarness for HttpExoHarness {
    async fn list_environments(&self) -> Result<Vec<crate::EnvironmentDefinition>> {
        match self.request(Request::ListEnvironments).await? {
            Response::Environments { environments } => Ok(environments),
            response => unexpected_response(response, "environments"),
        }
    }
    async fn put_environment(&self, environment: crate::EnvironmentDefinition) -> Result<()> {
        match self
            .request(Request::PutEnvironment { environment })
            .await?
        {
            Response::Bool { value: true } => Ok(()),
            response => unexpected_response(response, "bool"),
        }
    }
    async fn delete_environment(&self, name: &str) -> Result<bool> {
        match self
            .request(Request::DeleteEnvironment {
                name: name.to_owned(),
            })
            .await?
        {
            Response::Bool { value } => Ok(value),
            response => unexpected_response(response, "bool"),
        }
    }

    async fn list_agents(&self) -> Result<Vec<Arc<dyn AgentHandle>>> {
        match self.request(Request::ListAgents).await? {
            Response::Agents { agents } => Ok(agents
                .into_iter()
                .map(|record| Arc::new(HttpAgentHandle::new(self.clone(), record)) as _)
                .collect()),
            response => unexpected_response(response, "agents"),
        }
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<Arc<dyn AgentHandle>>> {
        match self.request(Request::GetAgent { agent_id: *id }).await? {
            Response::Agent { agent } => {
                Ok(agent.map(|record| Arc::new(HttpAgentHandle::new(self.clone(), record)) as _))
            }
            response => unexpected_response(response, "agent"),
        }
    }

    async fn new_agent(&self, request: NewAgentRequest) -> Result<Arc<dyn AgentHandle>> {
        match self.request(Request::NewAgent { request }).await? {
            Response::Agent { agent: Some(agent) } => {
                Ok(Arc::new(HttpAgentHandle::new(self.clone(), agent)))
            }
            Response::Agent { agent: None } => bail!("HTTP exoharness did not return a new agent"),
            response => unexpected_response(response, "agent"),
        }
    }

    async fn delete_agent(&self, id: &AgentId) -> Result<bool> {
        match self.request(Request::DeleteAgent { agent_id: *id }).await? {
            Response::Bool { value } => Ok(value),
            response => unexpected_response(response, "bool"),
        }
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        match self.request(Request::ListBindings).await? {
            Response::Bindings { bindings } => Ok(bindings),
            response => unexpected_response(response, "bindings"),
        }
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        match self.request(Request::PutBinding { binding }).await? {
            Response::BindingId { binding_id } => Ok(binding_id),
            response => unexpected_response(response, "binding_id"),
        }
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        match self
            .request(Request::GetBinding { binding_id: *id })
            .await?
        {
            Response::Binding { binding } => Ok(binding),
            response => unexpected_response(response, "binding"),
        }
    }

    async fn create_vault(&self, name: &str) -> Result<Arc<dyn VaultHandle>> {
        match self
            .request(Request::CreateVault { name: name.into() })
            .await?
        {
            Response::Vault {
                vault: Some(record),
            } => Ok(self.vault_handle(ResourceScope::Global, record)),
            response => unexpected_response(response, "vault"),
        }
    }
    async fn delete_vault(&self, id: &VaultId) -> Result<()> {
        match self.request(Request::DeleteVault { vault_id: *id }).await? {
            Response::Bool { value: true } => Ok(()),
            response => unexpected_response(response, "true"),
        }
    }
}

struct HttpAgentHandle {
    harness: HttpExoHarness,
    record: AgentRecord,
}

impl HttpAgentHandle {
    fn new(harness: HttpExoHarness, record: AgentRecord) -> Self {
        Self { harness, record }
    }

    fn sandbox_scope(&self) -> ResourceScope {
        ResourceScope::Agent {
            agent_id: self.record.id,
        }
    }
}

async fn http_create_sandbox(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    request: CreateSandboxRequest,
) -> Result<SandboxId> {
    match harness
        .request(Request::CreateSandbox { scope, request })
        .await?
    {
        Response::SandboxId { sandbox_id } => Ok(sandbox_id),
        response => unexpected_response(response, "sandbox_id"),
    }
}

async fn http_fork_sandbox(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    request: ForkSandboxRequest,
) -> Result<SandboxId> {
    match harness
        .request(Request::ForkSandbox { scope, request })
        .await?
    {
        Response::SandboxId { sandbox_id } => Ok(sandbox_id),
        response => unexpected_response(response, "sandbox_id"),
    }
}

async fn http_restore_sandbox(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    request: RestoreSandboxRequest,
) -> Result<SandboxId> {
    match harness
        .request(Request::RestoreSandbox { scope, request })
        .await?
    {
        Response::SandboxId { sandbox_id } => Ok(sandbox_id),
        response => unexpected_response(response, "sandbox_id"),
    }
}

async fn http_list_sandboxes(
    harness: &HttpExoHarness,
    scope: ResourceScope,
) -> Result<Vec<SandboxRecord>> {
    match harness.request(Request::ListSandboxes { scope }).await? {
        Response::Sandboxes { sandboxes } => Ok(sandboxes),
        response => unexpected_response(response, "sandboxes"),
    }
}

async fn http_terminate_sandbox(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    sandbox_id: SandboxId,
) -> Result<()> {
    match harness
        .request(Request::TerminateSandbox { scope, sandbox_id })
        .await?
    {
        Response::Unit => Ok(()),
        response => unexpected_response(response, "unit"),
    }
}

async fn http_attach_sandbox(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    request: AttachSandboxRequest,
) -> Result<SandboxId> {
    match harness
        .request(Request::AttachSandbox { scope, request })
        .await?
    {
        Response::SandboxId { sandbox_id } => Ok(sandbox_id),
        response => unexpected_response(response, "sandbox_id"),
    }
}

async fn http_detach_sandbox(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    sandbox_id: SandboxId,
) -> Result<SandboxAttachment> {
    match harness
        .request(Request::DetachSandbox { scope, sandbox_id })
        .await?
    {
        Response::SandboxAttachment { attachment } => Ok(attachment),
        response => unexpected_response(response, "sandbox_attachment"),
    }
}

async fn http_snapshot_sandbox(
    harness: &HttpExoHarness,
    scope: SnapshotScope,
    id: SandboxId,
) -> Result<SnapshotId> {
    match harness
        .request(Request::SnapshotSandbox {
            scope,
            sandbox_id: id,
        })
        .await?
    {
        Response::SnapshotId { snapshot_id } => Ok(snapshot_id),
        response => unexpected_response(response, "snapshot_id"),
    }
}

async fn http_start_sandbox(
    harness: &HttpExoHarness,
    scope: SnapshotScope,
    request: StartSandboxRequest,
) -> Result<()> {
    match harness
        .request(Request::StartSandbox { scope, request })
        .await?
    {
        Response::Unit => Ok(()),
        response => unexpected_response(response, "unit"),
    }
}

async fn http_stop_sandbox(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    id: SandboxId,
) -> Result<()> {
    match harness
        .request(Request::StopSandbox {
            scope,
            sandbox_id: id,
        })
        .await?
    {
        Response::Unit => Ok(()),
        response => unexpected_response(response, "unit"),
    }
}

async fn http_start_sandbox_process(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    request: StartSandboxProcessRequest,
) -> Result<SandboxProcessRecord> {
    match harness
        .request(Request::StartSandboxProcess { scope, request })
        .await?
    {
        Response::SandboxProcess { process } => Ok(process),
        response => unexpected_response(response, "sandbox_process"),
    }
}

async fn http_write_sandbox_process_input(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    request: WriteSandboxProcessInputRequest,
) -> Result<()> {
    match harness
        .request(Request::WriteSandboxProcessInput { scope, request })
        .await?
    {
        Response::Unit => Ok(()),
        response => unexpected_response(response, "unit"),
    }
}

async fn http_close_sandbox_process_input(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    request: CloseSandboxProcessInputRequest,
) -> Result<()> {
    match harness
        .request(Request::CloseSandboxProcessInput { scope, request })
        .await?
    {
        Response::Unit => Ok(()),
        response => unexpected_response(response, "unit"),
    }
}

async fn http_get_sandbox_process_events(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    query: SandboxProcessEventQuery,
) -> Result<GetSandboxProcessEventsResult> {
    match harness
        .request(Request::GetSandboxProcessEvents { scope, query })
        .await?
    {
        Response::SandboxProcessEvents { result } => Ok(result),
        response => unexpected_response(response, "sandbox_process_events"),
    }
}

async fn http_wait_sandbox_process(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    request: WaitSandboxProcessRequest,
) -> Result<SandboxProcessStatus> {
    match harness
        .request(Request::WaitSandboxProcess { scope, request })
        .await?
    {
        Response::SandboxProcessStatus { status } => Ok(status),
        response => unexpected_response(response, "sandbox_process_status"),
    }
}

async fn http_cancel_sandbox_process(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    request: CancelSandboxProcessRequest,
) -> Result<SandboxProcessStatus> {
    match harness
        .request(Request::CancelSandboxProcess { scope, request })
        .await?
    {
        Response::SandboxProcessStatus { status } => Ok(status),
        response => unexpected_response(response, "sandbox_process_status"),
    }
}

async fn http_run_in_sandbox(
    harness: &HttpExoHarness,
    scope: ResourceScope,
    request: RunInSandboxRequest,
) -> Result<Box<dyn SandboxProcess>> {
    let sandbox_id = request.id;
    let process = http_start_sandbox_process(
        harness,
        scope,
        StartSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            name: None,
            command: request.command,
            env: request.env,
            cwd: None,
            mode: Default::default(),
            stdin: Default::default(),
            output: Default::default(),
            lifecycle: Default::default(),
        },
    )
    .await?;
    let (stdout_reader, stdout_writer) = tokio::io::duplex(64 * 1024);
    let (stderr_reader, stderr_writer) = tokio::io::duplex(64 * 1024);
    let (stdin_reader, stdin_writer) = tokio::io::duplex(64 * 1024);
    let (wait_tx, wait_rx) = oneshot::channel();
    spawn_http_sandbox_process_event_poller(
        harness.clone(),
        scope,
        sandbox_id.clone(),
        process.id.clone(),
        stdout_writer,
        stderr_writer,
        wait_tx,
    );
    spawn_http_sandbox_process_stdin_forwarder(
        harness.clone(),
        scope,
        sandbox_id,
        process.id,
        stdin_reader,
    );
    Ok(Box::new(LiveHttpSandboxProcess {
        parts: Some(SandboxProcessParts {
            stdout: Box::pin(stdout_reader.compat()),
            stderr: Box::pin(stderr_reader.compat()),
            stdin: Box::pin(stdin_writer.compat_write()),
            wait: Box::pin(async move {
                wait_rx
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("HTTP sandbox process poller stopped")))
            }),
        }),
    }))
}

#[async_trait]
impl AgentHandle for HttpAgentHandle {
    fn record(&self) -> &AgentRecord {
        &self.record
    }

    async fn list_conversations(
        &self,
        request: ListConversationsRequest,
    ) -> Result<ListConversationsResult<Arc<dyn ConversationHandle>>> {
        match self
            .harness
            .request(Request::ListConversations {
                agent_id: self.record.id,
                request,
            })
            .await?
        {
            Response::Conversations { result } => Ok(ListConversationsResult {
                conversations: result
                    .conversations
                    .into_iter()
                    .map(|conversation| {
                        Arc::new(HttpConversationHandle::new(
                            self.harness.clone(),
                            conversation,
                        )) as _
                    })
                    .collect(),
                next_cursor: result.next_cursor,
            }),
            response => unexpected_response(response, "conversations"),
        }
    }

    async fn get_conversation(
        &self,
        id: &ConversationId,
    ) -> Result<Option<Arc<dyn ConversationHandle>>> {
        match self
            .harness
            .request(Request::GetConversation {
                agent_id: self.record.id,
                conversation_id: *id,
            })
            .await?
        {
            Response::Conversation { conversation } => Ok(conversation.map(|conversation| {
                Arc::new(HttpConversationHandle::new(
                    self.harness.clone(),
                    conversation,
                )) as _
            })),
            response => unexpected_response(response, "conversation"),
        }
    }

    async fn new_conversation(
        &self,
        request: NewConversationRequest,
    ) -> Result<Arc<dyn ConversationHandle>> {
        match self
            .harness
            .request(Request::NewConversation {
                agent_id: self.record.id,
                request,
            })
            .await?
        {
            Response::Conversation {
                conversation: Some(conversation),
            } => Ok(Arc::new(HttpConversationHandle::new(
                self.harness.clone(),
                conversation,
            ))),
            Response::Conversation { conversation: None } => {
                bail!("HTTP exoharness did not return a new conversation")
            }
            response => unexpected_response(response, "conversation"),
        }
    }

    async fn delete_conversation(&self, id: &ConversationId) -> Result<bool> {
        match self
            .harness
            .request(Request::DeleteConversation {
                agent_id: self.record.id,
                conversation_id: *id,
            })
            .await?
        {
            Response::Bool { value } => Ok(value),
            response => unexpected_response(response, "bool"),
        }
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        match self
            .harness
            .request(Request::AgentListBindings {
                agent_id: self.record.id,
            })
            .await?
        {
            Response::Bindings { bindings } => Ok(bindings),
            response => unexpected_response(response, "bindings"),
        }
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        match self
            .harness
            .request(Request::AgentPutBinding {
                agent_id: self.record.id,
                binding,
            })
            .await?
        {
            Response::BindingId { binding_id } => Ok(binding_id),
            response => unexpected_response(response, "binding_id"),
        }
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        match self
            .harness
            .request(Request::AgentGetBinding {
                agent_id: self.record.id,
                binding_id: *id,
            })
            .await?
        {
            Response::Binding { binding } => Ok(binding),
            response => unexpected_response(response, "binding"),
        }
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        match self
            .harness
            .request(Request::AgentWriteArtifact {
                agent_id: self.record.id,
                request,
            })
            .await?
        {
            Response::ArtifactVersion { artifact } => Ok(artifact),
            response => unexpected_response(response, "artifact_version"),
        }
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        match self
            .harness
            .request(Request::AgentReadArtifact {
                agent_id: self.record.id,
                request,
            })
            .await?
        {
            Response::Artifact { artifact } => Ok(artifact),
            response => unexpected_response(response, "artifact"),
        }
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        match self
            .harness
            .request(Request::AgentListArtifacts {
                agent_id: self.record.id,
            })
            .await?
        {
            Response::ArtifactVersions { artifacts } => Ok(artifacts),
            response => unexpected_response(response, "artifact_versions"),
        }
    }
}

#[async_trait]
impl SnapshotHandle for HttpAgentHandle {
    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        http_snapshot_sandbox(
            &self.harness,
            SnapshotScope::Resource {
                scope: self.sandbox_scope(),
            },
            id,
        )
        .await
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        http_start_sandbox(
            &self.harness,
            SnapshotScope::Resource {
                scope: self.sandbox_scope(),
            },
            request,
        )
        .await
    }
}

#[async_trait]
impl SandboxHandle for HttpAgentHandle {
    async fn list_sandboxes(&self) -> Result<Vec<SandboxRecord>> {
        http_list_sandboxes(&self.harness, self.sandbox_scope()).await
    }

    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        http_create_sandbox(&self.harness, self.sandbox_scope(), request).await
    }

    async fn fork_sandbox(&self, request: ForkSandboxRequest) -> Result<SandboxId> {
        http_fork_sandbox(&self.harness, self.sandbox_scope(), request).await
    }

    async fn restore_sandbox(&self, request: RestoreSandboxRequest) -> Result<SandboxId> {
        http_restore_sandbox(&self.harness, self.sandbox_scope(), request).await
    }
    async fn terminate_sandbox(&self, id: SandboxId) -> Result<()> {
        http_terminate_sandbox(&self.harness, self.sandbox_scope(), id).await
    }

    async fn attach_sandbox(&self, request: AttachSandboxRequest) -> Result<SandboxId> {
        http_attach_sandbox(&self.harness, self.sandbox_scope(), request).await
    }

    async fn detach_sandbox(&self, id: SandboxId) -> Result<SandboxAttachment> {
        http_detach_sandbox(&self.harness, self.sandbox_scope(), id).await
    }

    async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        http_stop_sandbox(&self.harness, self.sandbox_scope(), id).await
    }

    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        http_start_sandbox_process(&self.harness, self.sandbox_scope(), request).await
    }

    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        http_write_sandbox_process_input(&self.harness, self.sandbox_scope(), request).await
    }

    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        http_close_sandbox_process_input(&self.harness, self.sandbox_scope(), request).await
    }

    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult> {
        http_get_sandbox_process_events(&self.harness, self.sandbox_scope(), query).await
    }

    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        http_wait_sandbox_process(&self.harness, self.sandbox_scope(), request).await
    }

    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        http_cancel_sandbox_process(&self.harness, self.sandbox_scope(), request).await
    }

    async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        http_run_in_sandbox(&self.harness, self.sandbox_scope(), request).await
    }
}

struct HttpConversationHandle {
    harness: HttpExoHarness,
    agent_id: AgentId,
    record: ConversationRecord,
}

impl HttpConversationHandle {
    fn new(harness: HttpExoHarness, info: ConversationHandleInfo) -> Self {
        Self {
            harness,
            agent_id: info.agent_id,
            record: info.record,
        }
    }

    fn info(&self) -> ConversationHandleInfo {
        ConversationHandleInfo {
            agent_id: self.agent_id,
            record: self.record.clone(),
        }
    }

    fn sandbox_scope(&self) -> ResourceScope {
        ResourceScope::Thread {
            agent_id: self.agent_id,
            thread_id: self.record.id,
        }
    }
}

#[async_trait]
impl ConversationHandle for HttpConversationHandle {
    fn record(&self) -> &ConversationRecord {
        &self.record
    }

    async fn start_session(&self) -> Result<SessionId> {
        match self
            .harness
            .request(Request::ConversationStartSession {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
            })
            .await?
        {
            Response::SessionId { session_id } => Ok(session_id),
            response => unexpected_response(response, "session_id"),
        }
    }

    async fn end_session(&self, id: SessionId) -> Result<()> {
        match self
            .harness
            .request(Request::ConversationEndSession {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                session_id: id,
            })
            .await?
        {
            Response::Unit => Ok(()),
            response => unexpected_response(response, "unit"),
        }
    }

    async fn begin_turn(&self, request: BeginTurnRequest) -> Result<Arc<dyn TurnHandle>> {
        match self
            .harness
            .request(Request::ConversationBeginTurn {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::Turn { turn } => Ok(Arc::new(HttpTurnHandle::new(
                self.harness.clone(),
                turn.conversation,
                turn.record,
            ))),
            response => unexpected_response(response, "turn"),
        }
    }

    async fn turn_handle(&self, record: TurnRecord) -> Result<Arc<dyn TurnHandle>> {
        Ok(Arc::new(HttpTurnHandle::new(
            self.harness.clone(),
            self.info(),
            record,
        )))
    }

    async fn get_events(&self, query: Option<EventQuery>) -> Result<GetEventsResult> {
        match self
            .harness
            .request(Request::ConversationGetEvents {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                query,
            })
            .await?
        {
            Response::Events { result } => Ok(result),
            response => unexpected_response(response, "events"),
        }
    }

    async fn watch_events(&self, after_exclusive: Bound<EventId>) -> Result<EventStream> {
        self.harness
            .transport
            .watch_events(self.agent_id, self.record.id, after_exclusive)
            .await
    }

    async fn get_event(&self, id: EventId) -> Result<Option<Event>> {
        match self
            .harness
            .request(Request::ConversationGetEvent {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                event_id: id,
            })
            .await?
        {
            Response::Event { event } => Ok(event),
            response => unexpected_response(response, "event"),
        }
    }

    async fn add_events(&self, request: AddEventsRequest) -> Result<AddEventsResult> {
        match self
            .harness
            .request(Request::ConversationAddEvents {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::AddEvents { result } => Ok(result),
            response => unexpected_response(response, "add_events"),
        }
    }

    async fn fork(&self, request: ForkConversationRequest) -> Result<Arc<dyn ConversationHandle>> {
        match self
            .harness
            .request(Request::ConversationFork {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::Conversation {
                conversation: Some(conversation),
            } => Ok(Arc::new(HttpConversationHandle::new(
                self.harness.clone(),
                conversation,
            ))),
            Response::Conversation { conversation: None } => {
                bail!("HTTP exoharness did not return a forked conversation")
            }
            response => unexpected_response(response, "conversation"),
        }
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        match self
            .harness
            .request(Request::ConversationWriteArtifact {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::ArtifactVersion { artifact } => Ok(artifact),
            response => unexpected_response(response, "artifact_version"),
        }
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        match self
            .harness
            .request(Request::ConversationReadArtifact {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::Artifact { artifact } => Ok(artifact),
            response => unexpected_response(response, "artifact"),
        }
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        match self
            .harness
            .request(Request::ConversationListArtifacts {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
            })
            .await?
        {
            Response::ArtifactVersions { artifacts } => Ok(artifacts),
            response => unexpected_response(response, "artifact_versions"),
        }
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        match self
            .harness
            .request(Request::ConversationListBindings {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
            })
            .await?
        {
            Response::Bindings { bindings } => Ok(bindings),
            response => unexpected_response(response, "bindings"),
        }
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        match self
            .harness
            .request(Request::ConversationPutBinding {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                binding,
            })
            .await?
        {
            Response::BindingId { binding_id } => Ok(binding_id),
            response => unexpected_response(response, "binding_id"),
        }
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        match self
            .harness
            .request(Request::ConversationGetBinding {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                binding_id: *id,
            })
            .await?
        {
            Response::Binding { binding } => Ok(binding),
            response => unexpected_response(response, "binding"),
        }
    }
}

#[async_trait]
impl SnapshotHandle for HttpConversationHandle {
    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        http_snapshot_sandbox(
            &self.harness,
            SnapshotScope::Resource {
                scope: self.sandbox_scope(),
            },
            id,
        )
        .await
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        http_start_sandbox(
            &self.harness,
            SnapshotScope::Resource {
                scope: self.sandbox_scope(),
            },
            request,
        )
        .await
    }
}

#[async_trait]
impl SandboxHandle for HttpConversationHandle {
    async fn list_sandboxes(&self) -> Result<Vec<SandboxRecord>> {
        http_list_sandboxes(&self.harness, self.sandbox_scope()).await
    }

    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        http_create_sandbox(&self.harness, self.sandbox_scope(), request).await
    }

    async fn fork_sandbox(&self, request: ForkSandboxRequest) -> Result<SandboxId> {
        http_fork_sandbox(&self.harness, self.sandbox_scope(), request).await
    }

    async fn restore_sandbox(&self, request: RestoreSandboxRequest) -> Result<SandboxId> {
        http_restore_sandbox(&self.harness, self.sandbox_scope(), request).await
    }
    async fn terminate_sandbox(&self, id: SandboxId) -> Result<()> {
        http_terminate_sandbox(&self.harness, self.sandbox_scope(), id).await
    }

    async fn attach_sandbox(&self, request: AttachSandboxRequest) -> Result<SandboxId> {
        http_attach_sandbox(&self.harness, self.sandbox_scope(), request).await
    }

    async fn detach_sandbox(&self, id: SandboxId) -> Result<SandboxAttachment> {
        http_detach_sandbox(&self.harness, self.sandbox_scope(), id).await
    }

    async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        http_stop_sandbox(&self.harness, self.sandbox_scope(), id).await
    }

    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        http_start_sandbox_process(&self.harness, self.sandbox_scope(), request).await
    }

    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        http_write_sandbox_process_input(&self.harness, self.sandbox_scope(), request).await
    }

    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        http_close_sandbox_process_input(&self.harness, self.sandbox_scope(), request).await
    }

    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult> {
        http_get_sandbox_process_events(&self.harness, self.sandbox_scope(), query).await
    }

    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        http_wait_sandbox_process(&self.harness, self.sandbox_scope(), request).await
    }

    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        http_cancel_sandbox_process(&self.harness, self.sandbox_scope(), request).await
    }

    async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        http_run_in_sandbox(&self.harness, self.sandbox_scope(), request).await
    }
}

struct HttpTurnHandle {
    harness: HttpExoHarness,
    agent_id: AgentId,
    conversation_id: ConversationId,
    record: TurnRecord,
}

impl HttpTurnHandle {
    fn new(
        harness: HttpExoHarness,
        conversation: ConversationHandleInfo,
        record: TurnRecord,
    ) -> Self {
        Self {
            harness,
            agent_id: conversation.agent_id,
            conversation_id: conversation.record.id,
            record,
        }
    }

    fn sandbox_scope(&self) -> SnapshotScope {
        SnapshotScope::Turn {
            agent_id: self.agent_id,
            thread_id: self.conversation_id,
            session_id: self.record.session_id,
            turn_id: self.record.id,
        }
    }
}

#[async_trait]
impl SnapshotHandle for HttpTurnHandle {
    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        http_snapshot_sandbox(&self.harness, self.sandbox_scope(), id).await
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        http_start_sandbox(&self.harness, self.sandbox_scope(), request).await
    }
}

#[async_trait]
impl TurnHandle for HttpTurnHandle {
    fn record(&self) -> &TurnRecord {
        &self.record
    }

    async fn add_events(&self, data: Vec<EventData>) -> Result<AddEventsResult> {
        match self
            .harness
            .request(Request::TurnAddEvents {
                agent_id: self.agent_id,
                conversation_id: self.conversation_id,
                session_id: self.record.session_id,
                turn_id: self.record.id,
                data,
            })
            .await?
        {
            Response::AddEvents { result } => Ok(result),
            response => unexpected_response(response, "add_events"),
        }
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        match self
            .harness
            .request(Request::TurnWriteArtifact {
                agent_id: self.agent_id,
                conversation_id: self.conversation_id,
                session_id: self.record.session_id,
                turn_id: self.record.id,
                request,
            })
            .await?
        {
            Response::ArtifactVersion { artifact } => Ok(artifact),
            response => unexpected_response(response, "artifact_version"),
        }
    }

    async fn finish(&self) -> Result<EventId> {
        match self
            .harness
            .request(Request::TurnFinish {
                agent_id: self.agent_id,
                conversation_id: self.conversation_id,
                session_id: self.record.session_id,
                turn_id: self.record.id,
            })
            .await?
        {
            Response::EventId { event_id } => Ok(event_id),
            response => unexpected_response(response, "event_id"),
        }
    }
}

fn request_endpoint(base_url: &str) -> Result<Url> {
    let mut url = Url::parse(base_url).context("invalid HTTP exoharness URL")?;
    url.set_query(None);
    url.set_fragment(None);
    if url
        .path()
        .trim_end_matches('/')
        .ends_with(HTTP_EXOHARNESS_REQUEST_PATH)
    {
        return Ok(url);
    }
    let normalized_path = match url.path().trim_end_matches('/') {
        "" => "/".to_string(),
        path => format!("{path}/"),
    };
    url.set_path(&normalized_path);
    Ok(url.join(HTTP_EXOHARNESS_REQUEST_PATH.trim_start_matches('/'))?)
}

fn unexpected_response<T>(response: Response, expected: &str) -> Result<T> {
    bail!(
        "expected HTTP exoharness {expected} response, got {}",
        response.kind()
    )
}

fn unsupported<T>(operation: &str) -> Result<T> {
    bail!("HTTP exoharness does not support {operation} yet")
}

impl HttpExoHarness {
    fn vault_handle(&self, scope: ResourceScope, record: VaultRecord) -> Arc<dyn VaultHandle> {
        Arc::new(HttpVaultHandle {
            harness: self.clone(),
            record,
            scope,
        })
    }
    async fn get_scoped_vault(
        &self,
        scope: ResourceScope,
        vault_id: VaultId,
    ) -> Result<Option<Arc<dyn VaultHandle>>> {
        match self.request(Request::GetVault { scope, vault_id }).await? {
            Response::Vault { vault } => Ok(vault.map(|record| self.vault_handle(scope, record))),
            response => unexpected_response(response, "vault"),
        }
    }
}

struct HttpVaultHandle {
    scope: ResourceScope,
    harness: HttpExoHarness,
    record: VaultRecord,
}

#[async_trait]
impl VaultHandle for HttpVaultHandle {
    fn record(&self) -> &VaultRecord {
        &self.record
    }
    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        match self
            .harness
            .request(Request::VaultListSecrets {
                scope: self.scope,
                vault_id: self.record.id,
            })
            .await?
        {
            Response::Secrets { secrets } => Ok(secrets),
            response => unexpected_response(response, "secrets"),
        }
    }
    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        match self
            .harness
            .request(Request::VaultPutSecret {
                scope: self.scope,
                vault_id: self.record.id,
                request,
            })
            .await?
        {
            Response::SecretId { secret_id } => Ok(secret_id),
            response => unexpected_response(response, "secret_id"),
        }
    }
    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        match self
            .harness
            .request(Request::VaultGetSecret {
                scope: self.scope,
                vault_id: self.record.id,
                secret_id: *id,
            })
            .await?
        {
            Response::Secret { secret } => Ok(secret),
            response => unexpected_response(response, "secret"),
        }
    }
    async fn update_secret(&self, id: &SecretId, secret: Secret) -> Result<SecretMetadata> {
        match self
            .harness
            .request(Request::VaultUpdateSecret {
                scope: self.scope,
                vault_id: self.record.id,
                secret_id: *id,
                secret,
            })
            .await?
        {
            Response::SecretMetadata { metadata } => Ok(metadata),
            response => unexpected_response(response, "secret_metadata"),
        }
    }
    async fn delete_secret(&self, id: &SecretId) -> Result<()> {
        match self
            .harness
            .request(Request::VaultDeleteSecret {
                scope: self.scope,
                vault_id: self.record.id,
                secret_id: *id,
            })
            .await?
        {
            Response::Bool { value: true } => Ok(()),
            response => unexpected_response(response, "true"),
        }
    }
    async fn refresh_secret(
        &self,
        id: &SecretId,
        target: &SecretTarget,
        rejected_revision: u64,
    ) -> Result<ResolvedSecret> {
        match self
            .harness
            .request(Request::VaultRefreshSecret {
                scope: self.scope,
                vault_id: self.record.id,
                secret_id: *id,
                target: target.clone(),
                rejected_revision,
            })
            .await?
        {
            Response::ResolvedSecret { revision, secret } => {
                Ok(ResolvedSecret { revision, secret })
            }
            response => unexpected_response(response, "resolved_secret"),
        }
    }

    async fn resolve_secret(&self, id: &SecretId, target: &SecretTarget) -> Result<ResolvedSecret> {
        match self
            .harness
            .request(Request::VaultResolveSecret {
                scope: self.scope,
                vault_id: self.record.id,
                secret_id: *id,
                target: target.clone(),
            })
            .await?
        {
            Response::ResolvedSecret { revision, secret } => {
                Ok(ResolvedSecret { revision, secret })
            }
            response => unexpected_response(response, "resolved_secret"),
        }
    }
}

impl HttpExoHarness {
    async fn list_scoped_vaults(&self, scope: ResourceScope) -> Result<Vec<Arc<dyn VaultHandle>>> {
        match self.request(Request::ListVaults { scope }).await? {
            Response::Vaults { vaults } => Ok(vaults
                .into_iter()
                .map(|record| self.vault_handle(scope, record))
                .collect()),
            response => unexpected_response(response, "vaults"),
        }
    }
}

#[async_trait]
impl VaultContext for HttpExoHarness {
    async fn list_vaults(&self) -> Result<Vec<Arc<dyn VaultHandle>>> {
        self.list_scoped_vaults(ResourceScope::Global).await
    }
    async fn get_vault(&self, id: &VaultId) -> Result<Option<Arc<dyn VaultHandle>>> {
        self.get_scoped_vault(ResourceScope::Global, *id).await
    }
}

#[async_trait]
impl VaultContext for HttpAgentHandle {
    async fn list_vaults(&self) -> Result<Vec<Arc<dyn VaultHandle>>> {
        self.harness.list_scoped_vaults(self.sandbox_scope()).await
    }
    async fn get_vault(&self, id: &VaultId) -> Result<Option<Arc<dyn VaultHandle>>> {
        self.harness
            .get_scoped_vault(self.sandbox_scope(), *id)
            .await
    }
}

#[async_trait]
impl VaultContext for HttpConversationHandle {
    async fn list_vaults(&self) -> Result<Vec<Arc<dyn VaultHandle>>> {
        self.harness.list_scoped_vaults(self.sandbox_scope()).await
    }
    async fn get_vault(&self, id: &VaultId) -> Result<Option<Arc<dyn VaultHandle>>> {
        self.harness
            .get_scoped_vault(self.sandbox_scope(), *id)
            .await
    }
}
