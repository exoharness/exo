use std::net::{SocketAddr, TcpListener};
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use actix_web::{App, HttpResponse, HttpServer, Responder, web};
use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt as TokioAsyncReadExt, AsyncWriteExt as TokioAsyncWriteExt};
use tokio::sync::oneshot;
use tokio::time::{self, Duration};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use url::Url;

use crate::protocol::{ClientMessage, ConversationHandleInfo, Request, Response, ServerMessage};
use crate::server::ExoHarnessServer;
use crate::{
    AddEventsRequest, AddEventsResult, AgentHandle, AgentId, AgentRecord, Artifact,
    ArtifactVersion, BeginTurnRequest, Binding, BindingId, BindingMetadata,
    CancelSandboxProcessRequest, CloseSandboxProcessInputRequest, ConversationHandle,
    ConversationId, ConversationRecord, CreateSandboxRequest, Event, EventData, EventId,
    EventQuery, EventStream, ExoHarness, ForkConversationRequest, GetEventsResult,
    GetSandboxProcessEventsResult, NewAgentRequest, NewConversationRequest, PutSecretRequest,
    ReadArtifactRequest, Result, RunInSandboxRequest, SandboxId, SandboxProcess,
    SandboxProcessEvent, SandboxProcessEventQuery, SandboxProcessId, SandboxProcessParts,
    SandboxProcessRecord, SandboxProcessStatus, Secret, SecretId, SecretMetadata, SessionId,
    SnapshotId, StartSandboxProcessRequest, StartSandboxRequest, TurnHandle, TurnRecord,
    WaitSandboxProcessRequest, WriteArtifactRequest, WriteSandboxProcessInputRequest,
};

pub const HTTP_EXOHARNESS_REQUEST_PATH: &str = "/v1/requests";
pub const HTTP_EXOHARNESS_TRACING_TARGET: &str = "exo::exoharness_http";

#[derive(Debug, Clone, Copy, Default)]
pub struct ExoHarnessHttpServeOptions {
    pub verbosity: u8,
}

struct HttpServerState {
    server: Arc<ExoHarnessServer>,
    options: ExoHarnessHttpServeOptions,
}

pub async fn serve_exoharness_http(addr: SocketAddr, root: Arc<dyn ExoHarness>) -> Result<()> {
    serve_exoharness_http_with_options(addr, root, ExoHarnessHttpServeOptions::default()).await
}

pub async fn serve_exoharness_http_with_options(
    addr: SocketAddr,
    root: Arc<dyn ExoHarness>,
    options: ExoHarnessHttpServeOptions,
) -> Result<()> {
    let listener = TcpListener::bind(addr)?;
    serve_exoharness_http_listener_with_options(listener, root, options).await
}

pub async fn serve_exoharness_http_listener(
    listener: TcpListener,
    root: Arc<dyn ExoHarness>,
) -> Result<()> {
    serve_exoharness_http_listener_with_options(
        listener,
        root,
        ExoHarnessHttpServeOptions::default(),
    )
    .await
}

pub async fn serve_exoharness_http_listener_with_options(
    listener: TcpListener,
    root: Arc<dyn ExoHarness>,
    options: ExoHarnessHttpServeOptions,
) -> Result<()> {
    let state = Arc::new(HttpServerState {
        server: Arc::new(ExoHarnessServer::new(root)),
        options,
    });
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(Arc::clone(&state)))
            .route("/health", web::get().to(health))
            .route(
                HTTP_EXOHARNESS_REQUEST_PATH,
                web::post().to(handle_http_request),
            )
    })
    .listen(listener)?
    .run()
    .await?;
    Ok(())
}

async fn health() -> impl Responder {
    HttpResponse::Ok().body("ok\n")
}

async fn handle_http_request(
    state: web::Data<Arc<HttpServerState>>,
    message: web::Json<ClientMessage>,
) -> impl Responder {
    let ClientMessage::Request { id, request } = message.into_inner();
    let kind = request.kind();
    let start = Instant::now();
    if state.options.verbosity > 0 {
        tracing::info!(
            target: HTTP_EXOHARNESS_TRACING_TARGET,
            request_id = id,
            request_kind = %kind,
            "exoharness request"
        );
    }
    let response = match state.server.handle_request(request).await {
        Ok(response) => ServerMessage::Response {
            id,
            ok: true,
            response: Some(response),
            error: None,
        },
        Err(error) => ServerMessage::Response {
            id,
            ok: false,
            response: None,
            error: Some(error.to_string()),
        },
    };
    if state.options.verbosity > 0 {
        log_http_response(&response, start);
    }
    HttpResponse::Ok().json(response)
}

fn log_http_response(response: &ServerMessage, start: Instant) {
    let elapsed_ms = start.elapsed().as_millis() as u64;
    let ServerMessage::Response {
        id,
        ok,
        response,
        error,
    } = response;
    if *ok {
        let kind = response
            .as_ref()
            .map(Response::kind)
            .unwrap_or("missing_response");
        tracing::info!(
            target: HTTP_EXOHARNESS_TRACING_TARGET,
            request_id = *id,
            response_kind = %kind,
            elapsed_ms,
            "exoharness response"
        );
        return;
    }
    let error = error.as_deref().unwrap_or("unknown error");
    tracing::warn!(
        target: HTTP_EXOHARNESS_TRACING_TARGET,
        request_id = *id,
        error = %error,
        elapsed_ms,
        "exoharness response"
    );
}

#[derive(Clone)]
pub struct HttpExoHarness {
    client: reqwest::Client,
    endpoint: Url,
    bearer_token: Option<String>,
    next_request_id: Arc<AtomicU64>,
}

impl HttpExoHarness {
    pub fn new(base_url: impl AsRef<str>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            endpoint: request_endpoint(base_url.as_ref())?,
            bearer_token: None,
            next_request_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn with_bearer_token(mut self, bearer_token: String) -> Self {
        self.bearer_token = Some(bearer_token);
        self
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    async fn request(&self, request: Request) -> Result<Response> {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let message = ClientMessage::Request { id, request };
        let mut request = self.client.post(self.endpoint.clone()).json(&message);
        if let Some(bearer_token) = &self.bearer_token {
            request = request.bearer_auth(bearer_token);
        }
        let response = request
            .send()
            .await
            .context("failed to send HTTP exoharness request")?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("failed to read response body: {error}"));
            bail!("HTTP exoharness request failed ({status}): {body}");
        }

        let message = response
            .json::<ServerMessage>()
            .await
            .context("failed to decode HTTP exoharness response")?;
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

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>> {
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

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        match self.request(Request::ListSecrets).await? {
            Response::Secrets { secrets } => Ok(secrets),
            response => unexpected_response(response, "secrets"),
        }
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        match self.request(Request::PutSecret { request }).await? {
            Response::SecretId { secret_id } => Ok(secret_id),
            response => unexpected_response(response, "secret_id"),
        }
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        match self.request(Request::GetSecret { secret_id: *id }).await? {
            Response::Secret { secret } => Ok(secret),
            response => unexpected_response(response, "secret"),
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
}

#[async_trait]
impl AgentHandle for HttpAgentHandle {
    fn record(&self) -> &AgentRecord {
        &self.record
    }

    async fn list_conversations(&self) -> Result<Vec<Arc<dyn ConversationHandle>>> {
        match self
            .harness
            .request(Request::ListConversations {
                agent_id: self.record.id,
            })
            .await?
        {
            Response::Conversations { conversations } => Ok(conversations
                .into_iter()
                .map(|conversation| {
                    Arc::new(HttpConversationHandle::new(
                        self.harness.clone(),
                        conversation,
                    )) as _
                })
                .collect()),
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

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>> {
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

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        match self
            .harness
            .request(Request::AgentListSecrets {
                agent_id: self.record.id,
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
            .request(Request::AgentPutSecret {
                agent_id: self.record.id,
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
            .request(Request::AgentGetSecret {
                agent_id: self.record.id,
                secret_id: *id,
            })
            .await?
        {
            Response::Secret { secret } => Ok(secret),
            response => unexpected_response(response, "secret"),
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

    async fn watch_events(&self, _after_exclusive: Bound<EventId>) -> Result<EventStream> {
        unsupported("watch_events")
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

    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        match self
            .harness
            .request(Request::ConversationCreateSandbox {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::SandboxId { sandbox_id } => Ok(sandbox_id),
            response => unexpected_response(response, "sandbox_id"),
        }
    }

    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        match self
            .harness
            .request(Request::ConversationSnapshotSandbox {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                sandbox_id: id,
            })
            .await?
        {
            Response::SnapshotId { snapshot_id } => Ok(snapshot_id),
            response => unexpected_response(response, "snapshot_id"),
        }
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        match self
            .harness
            .request(Request::ConversationStartSandbox {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::Unit => Ok(()),
            response => unexpected_response(response, "unit"),
        }
    }

    async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        match self
            .harness
            .request(Request::ConversationStopSandbox {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                sandbox_id: id,
            })
            .await?
        {
            Response::Unit => Ok(()),
            response => unexpected_response(response, "unit"),
        }
    }

    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        match self
            .harness
            .request(Request::ConversationStartSandboxProcess {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::SandboxProcess { process } => Ok(process),
            response => unexpected_response(response, "sandbox_process"),
        }
    }

    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        match self
            .harness
            .request(Request::ConversationWriteSandboxProcessInput {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::Unit => Ok(()),
            response => unexpected_response(response, "unit"),
        }
    }

    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        match self
            .harness
            .request(Request::ConversationCloseSandboxProcessInput {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::Unit => Ok(()),
            response => unexpected_response(response, "unit"),
        }
    }

    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult> {
        match self
            .harness
            .request(Request::ConversationGetSandboxProcessEvents {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                query,
            })
            .await?
        {
            Response::SandboxProcessEvents { result } => Ok(result),
            response => unexpected_response(response, "sandbox_process_events"),
        }
    }

    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        match self
            .harness
            .request(Request::ConversationWaitSandboxProcess {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::SandboxProcessStatus { status } => Ok(status),
            response => unexpected_response(response, "sandbox_process_status"),
        }
    }

    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        match self
            .harness
            .request(Request::ConversationCancelSandboxProcess {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                request,
            })
            .await?
        {
            Response::SandboxProcessStatus { status } => Ok(status),
            response => unexpected_response(response, "sandbox_process_status"),
        }
    }

    async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        let sandbox_id = request.id;
        let process = self
            .start_sandbox_process(StartSandboxProcessRequest {
                sandbox_id: sandbox_id.clone(),
                command: request.command,
                env: request.env,
                cwd: None,
                mode: Default::default(),
                stdin: Default::default(),
                output: Default::default(),
                lifecycle: Default::default(),
            })
            .await?;
        let (stdout_reader, stdout_writer) = tokio::io::duplex(64 * 1024);
        let (stderr_reader, stderr_writer) = tokio::io::duplex(64 * 1024);
        let (stdin_reader, stdin_writer) = tokio::io::duplex(64 * 1024);
        let (wait_tx, wait_rx) = oneshot::channel();
        spawn_http_sandbox_process_event_poller(
            self.harness.clone(),
            self.agent_id,
            self.record.id,
            sandbox_id.clone(),
            process.id.clone(),
            stdout_writer,
            stderr_writer,
            wait_tx,
        );
        spawn_http_sandbox_process_stdin_forwarder(
            self.harness.clone(),
            self.agent_id,
            self.record.id,
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

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>> {
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

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        match self
            .harness
            .request(Request::ConversationListSecrets {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
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
            .request(Request::ConversationPutSecret {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
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
            .request(Request::ConversationGetSecret {
                agent_id: self.agent_id,
                conversation_id: self.record.id,
                secret_id: *id,
            })
            .await?
        {
            Response::Secret { secret } => Ok(secret),
            response => unexpected_response(response, "secret"),
        }
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

struct LiveHttpSandboxProcess {
    parts: Option<SandboxProcessParts>,
}

impl SandboxProcess for LiveHttpSandboxProcess {
    fn into_parts(mut self: Box<Self>) -> SandboxProcessParts {
        self.parts
            .take()
            .expect("live HTTP sandbox process parts already consumed")
    }
}

fn spawn_http_sandbox_process_event_poller(
    harness: HttpExoHarness,
    agent_id: AgentId,
    conversation_id: ConversationId,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
    mut stdout: tokio::io::DuplexStream,
    mut stderr: tokio::io::DuplexStream,
    wait_tx: oneshot::Sender<Result<i32>>,
) {
    tokio::spawn(async move {
        let mut cursor = None;
        let mut wait_tx = Some(wait_tx);
        loop {
            let response = harness
                .request(Request::ConversationGetSandboxProcessEvents {
                    agent_id,
                    conversation_id,
                    query: SandboxProcessEventQuery {
                        sandbox_id: sandbox_id.clone(),
                        process_id: process_id.clone(),
                        after: cursor,
                        limit: None,
                        follow: None,
                    },
                })
                .await;
            let result = match response {
                Ok(Response::SandboxProcessEvents { result }) => result,
                Ok(response) => {
                    send_http_sandbox_process_wait_result(
                        &mut wait_tx,
                        Err(anyhow!(
                            "expected sandbox_process_events response, got {}",
                            response.kind()
                        )),
                    );
                    return;
                }
                Err(error) => {
                    send_http_sandbox_process_wait_result(&mut wait_tx, Err(error));
                    return;
                }
            };

            for event in result.events {
                cursor = Some(event.cursor());
                match event {
                    SandboxProcessEvent::Stdout { data, .. } => {
                        if let Err(error) = stdout.write_all(&data).await {
                            send_http_sandbox_process_wait_result(&mut wait_tx, Err(error.into()));
                            return;
                        }
                    }
                    SandboxProcessEvent::Stderr { data, .. } => {
                        if let Err(error) = stderr.write_all(&data).await {
                            send_http_sandbox_process_wait_result(&mut wait_tx, Err(error.into()));
                            return;
                        }
                    }
                    SandboxProcessEvent::Exit { exit_code, .. } => {
                        send_http_sandbox_process_wait_result(&mut wait_tx, Ok(exit_code));
                        return;
                    }
                    SandboxProcessEvent::Error { message, .. } => {
                        send_http_sandbox_process_wait_result(&mut wait_tx, Err(anyhow!(message)));
                        return;
                    }
                    SandboxProcessEvent::Cancelled { .. } => {
                        send_http_sandbox_process_wait_result(
                            &mut wait_tx,
                            Err(anyhow!("sandbox process was cancelled")),
                        );
                        return;
                    }
                }
            }

            match result.status {
                SandboxProcessStatus::Running => {
                    time::sleep(Duration::from_millis(50)).await;
                }
                SandboxProcessStatus::Exited { exit_code } => {
                    send_http_sandbox_process_wait_result(&mut wait_tx, Ok(exit_code));
                    return;
                }
                SandboxProcessStatus::Failed { message } => {
                    send_http_sandbox_process_wait_result(&mut wait_tx, Err(anyhow!(message)));
                    return;
                }
                SandboxProcessStatus::Cancelled => {
                    send_http_sandbox_process_wait_result(
                        &mut wait_tx,
                        Err(anyhow!("sandbox process was cancelled")),
                    );
                    return;
                }
            }
        }
    });
}

fn spawn_http_sandbox_process_stdin_forwarder(
    harness: HttpExoHarness,
    agent_id: AgentId,
    conversation_id: ConversationId,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
    mut stdin: tokio::io::DuplexStream,
) {
    tokio::spawn(async move {
        let mut buffer = vec![0; 8192];
        loop {
            match stdin.read(&mut buffer).await {
                Ok(0) => {
                    let response = harness
                        .request(Request::ConversationCloseSandboxProcessInput {
                            agent_id,
                            conversation_id,
                            request: CloseSandboxProcessInputRequest {
                                sandbox_id: sandbox_id.clone(),
                                process_id: process_id.clone(),
                            },
                        })
                        .await;
                    if let Err(error) = response {
                        tracing::warn!(
                            target: HTTP_EXOHARNESS_TRACING_TARGET,
                            error = %error,
                            "failed to close HTTP sandbox process stdin"
                        );
                    }
                    return;
                }
                Ok(length) => {
                    let response = harness
                        .request(Request::ConversationWriteSandboxProcessInput {
                            agent_id,
                            conversation_id,
                            request: WriteSandboxProcessInputRequest {
                                sandbox_id: sandbox_id.clone(),
                                process_id: process_id.clone(),
                                data: buffer[..length].to_vec(),
                            },
                        })
                        .await;
                    if let Err(error) = response {
                        tracing::warn!(
                            target: HTTP_EXOHARNESS_TRACING_TARGET,
                            error = %error,
                            "failed to write HTTP sandbox process stdin"
                        );
                        return;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: HTTP_EXOHARNESS_TRACING_TARGET,
                        error = %error,
                        "failed to read HTTP sandbox process stdin pipe"
                    );
                    return;
                }
            }
        }
    });
}

fn send_http_sandbox_process_wait_result(
    sender: &mut Option<oneshot::Sender<Result<i32>>>,
    result: Result<i32>,
) {
    if let Some(sender) = sender.take() {
        match sender.send(result) {
            Ok(()) => {}
            Err(_result) => {}
        }
    }
}

fn request_endpoint(base_url: &str) -> Result<Url> {
    let mut url = Url::parse(base_url).context("invalid HTTP exoharness URL")?;
    match url.scheme() {
        "http" | "https" => {}
        scheme => bail!("HTTP exoharness URL must use http or https, got {scheme}"),
    }
    url.set_query(None);
    url.set_fragment(None);
    if url.path().trim_end_matches('/') == HTTP_EXOHARNESS_REQUEST_PATH {
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
