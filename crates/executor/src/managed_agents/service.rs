//! Transport-independent managed-agent request handling.
use crate::{AgentConfig, AgentHarnessKind, ConversationModelConfig, Runtime, SendRequest};
use anyhow::Result;
use exo_managed_agents::http::protocol::*;
use exoharness::{AgentHandle, ThreadHandle};
use std::sync::Arc;

#[derive(Debug)]
pub struct UnsupportedRequest(pub &'static str);
impl std::fmt::Display for UnsupportedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for UnsupportedRequest {}

fn harness_name(config: &AgentConfig) -> &str {
    match config.harness {
        AgentHarnessKind::Basic => "basic",
        AgentHarnessKind::Rlm => "rlm",
        AgentHarnessKind::Exo => "exo",
        AgentHarnessKind::TypeScript => config
            .typescript
            .as_ref()
            .and_then(|config| std::path::Path::new(&config.module_path).file_stem())
            .and_then(|name| name.to_str())
            .unwrap_or("typescript"),
    }
}

async fn check_harness(
    agent: &dyn AgentHandle,
    config: &AgentConfig,
    requested: Option<&str>,
) -> Result<()> {
    let Some(requested) = requested else {
        return Ok(());
    };
    let definition = exo_managed_agents::load_definition(agent).await?;
    let actual = definition
        .as_ref()
        .map(|d| d.frontmatter.harness.as_str())
        .unwrap_or_else(|| harness_name(config));
    if requested != actual && !(requested == "native" && config.harness == AgentHarnessKind::Basic)
    {
        return Err(UnsupportedRequest("choose the harness in the saved agent definition").into());
    }
    Ok(())
}

pub async fn create_thread(
    runtime: &Runtime,
    agent: &Arc<dyn AgentHandle>,
    body: CreateThreadBody,
) -> Result<CreateThreadResult> {
    if body.thread_id.is_some()
        || body.endpoint_name.is_some()
        || body.reasoning_effort.is_some()
        || body.visibility != ThreadVisibility::Owner
        || body.source.is_some()
    {
        return Err(UnsupportedRequest(
            "local runtime does not support supplied thread IDs, endpoint/reasoning overrides, or shared/automation threads",
        ).into());
    }
    let config = runtime.get_agent_config(agent.as_ref()).await?;
    check_harness(agent.as_ref(), &config, body.harness.as_deref()).await?;
    let thread = runtime
        .open_managed_thread(
            agent,
            None,
            exoharness::NewThreadRequest {
                environment: body.environment,
                vaults: body.vaults,
                slug: body.thread_slug,
                name: body.thread_name,
            },
        )
        .await?
        .thread;
    if let Some(model) = body.model {
        crate::put_conversation_model_override(
            thread.as_ref(),
            Some(ConversationModelConfig {
                model,
                max_output_tokens: None,
            }),
        )
        .await?;
    }
    Ok(CreateThreadResult {
        agent: agent.record().clone(),
        thread: thread.record().clone(),
        harness: harness_name(&config).to_owned(),
    })
}

pub struct PreparedTurn {
    pub request: SendRequest,
    pub config: AgentConfig,
    pub harness: String,
}

pub async fn prepare_turn(
    runtime: &Runtime,
    agent: &dyn AgentHandle,
    thread: &dyn ThreadHandle,
    body: SubmitTurnBody,
) -> Result<PreparedTurn> {
    if body.idempotency_key.is_some()
        || body.attention != TurnAttention::Wake
        || body.parent.is_some()
        || body.endpoint_name.is_some()
        || body.reasoning_effort.is_some()
        || body.frontend_tools.is_some()
        || body.auto_approve_tools.is_some()
        || body.page_scope.is_some()
        || body.reset_history
        || body.delivery_callback.is_some()
    {
        return Err(UnsupportedRequest(
            "local runtime does not support the requested turn options",
        )
        .into());
    }
    let mut config = runtime.get_agent_config(agent).await?;
    check_harness(agent, &config, body.harness.as_deref()).await?;
    if let Some(model) = crate::get_conversation_model_override(thread).await? {
        config.model = model.model;
        config.max_output_tokens = model.max_output_tokens;
    }
    if let Some(model) = body.model {
        config.model = model;
    }
    if let Some(prompt) = body.system_prompt {
        config.instructions = vec![crate::harness_helpers::system_message(&prompt)];
    }
    let harness = harness_name(&config).to_owned();
    Ok(PreparedTurn {
        config,
        harness,
        request: SendRequest {
            input: body.input.map(OneOrMany::into_vec).unwrap_or_default(),
            session_id: body.session_id,
        },
    })
}

use exoharness::{AgentId, EventQuery, EventQueryDirection, ThreadId, TurnId};
use serde::Deserialize;

#[derive(Debug)]
pub struct RequestError {
    pub status: u16,
    pub message: String,
}
impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RequestError {}
fn failure(status: u16, error: impl std::fmt::Display) -> anyhow::Error {
    RequestError {
        status,
        message: error.to_string(),
    }
    .into()
}
fn bad_request(error: impl std::fmt::Display) -> anyhow::Error {
    failure(400, error)
}
fn not_found(error: impl std::fmt::Display) -> anyhow::Error {
    failure(404, error)
}
fn not_implemented(error: impl std::fmt::Display) -> anyhow::Error {
    failure(501, error)
}
fn internal_error(error: impl std::fmt::Display) -> anyhow::Error {
    failure(500, error)
}
pub fn error_status(error: &anyhow::Error) -> u16 {
    if let Some(error) = error.downcast_ref::<RequestError>() {
        error.status
    } else if error.is::<UnsupportedRequest>() {
        501
    } else {
        400
    }
}

pub struct Service<'a> {
    pub runtime: &'a Runtime,
    pub agent_id: Option<AgentId>,
    pub definition_updates: &'a tokio::sync::Mutex<()>,
}
impl Service<'_> {
    fn require_full_provider(&self) -> Result<()> {
        if self.agent_id.is_some() {
            return Err(failure(403, "this service serves one saved agent"));
        }
        Ok(())
    }
    pub async fn agent(&self, id: AgentId) -> Result<Arc<dyn AgentHandle>> {
        if self.agent_id.is_some_and(|agent_id| agent_id != id) {
            return Err(not_found("agent not found"));
        }
        self.runtime
            .exoharness_handle()
            .get_agent(&id)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| not_found("agent not found"))
    }
    pub async fn thread(
        &self,
        agent: &dyn AgentHandle,
        id: ThreadId,
    ) -> Result<Arc<dyn ThreadHandle>> {
        agent
            .get_thread(&id)
            .await
            .map_err(internal_error)?
            .ok_or_else(|| not_found("thread not found"))
    }
}

#[derive(Deserialize)]
pub struct AgentPath {
    pub agent_id: AgentId,
}
#[derive(Deserialize)]
pub struct ThreadPath {
    pub agent_id: AgentId,
    pub thread_id: ThreadId,
}
#[derive(Deserialize)]
pub struct TurnPath {
    pub agent_id: AgentId,
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
}
#[derive(Deserialize)]
pub struct VaultPath {
    pub agent_id: Option<AgentId>,
    pub thread_id: Option<ThreadId>,
    pub vault_id: Option<exoharness::vault::VaultId>,
    pub secret_id: Option<exoharness::SecretId>,
}

pub async fn list_agents(service: &Service<'_>, query: AgentsQuery) -> Result<ListAgentsResult> {
    let mut agents = service
        .runtime
        .list_agents()
        .await
        .map_err(internal_error)?;
    agents.retain(|agent| service.agent_id.is_none_or(|id| agent.id == id));
    if let Some(slug) = &query.slug {
        agents.retain(|agent| &agent.slug == slug);
    }
    Ok(ListAgentsResult { agents })
}

pub async fn scoped_vaults(
    service: &Service<'_>,
    path: &VaultPath,
) -> Result<Vec<Arc<dyn exoharness::vault::VaultHandle>>> {
    if let Some(id) = path.agent_id.or(service.agent_id) {
        let agent = service.agent(id).await?;
        if let Some(thread_id) = path.thread_id {
            return service
                .thread(agent.as_ref(), thread_id)
                .await?
                .list_vaults()
                .await
                .map_err(bad_request);
        }
        return agent.list_vaults().await.map_err(bad_request);
    }
    service
        .runtime
        .exoharness_handle()
        .list_vaults()
        .await
        .map_err(bad_request)
}

pub async fn list_environments(
    service: &Service<'_>,
) -> Result<Vec<exoharness::EnvironmentDefinition>> {
    service
        .runtime
        .exoharness_handle()
        .list_environments()
        .await
        .map_err(bad_request)
}

pub async fn put_environment(
    service: &Service<'_>,
    body: exoharness::EnvironmentDefinition,
) -> Result<bool> {
    service.require_full_provider()?;
    service
        .runtime
        .exoharness_handle()
        .put_environment(body)
        .await
        .map_err(bad_request)?;
    Ok(true)
}

pub async fn delete_environment(service: &Service<'_>, name: &str) -> Result<bool> {
    service.require_full_provider()?;
    service
        .runtime
        .exoharness_handle()
        .delete_environment(name)
        .await
        .map_err(bad_request)
}

pub async fn list_vaults(
    service: &Service<'_>,
    path: &VaultPath,
) -> Result<Vec<exoharness::vault::VaultRecord>> {
    Ok(scoped_vaults(service, path)
        .await?
        .iter()
        .map(|vault| vault.record().clone())
        .collect())
}

pub async fn create_vault(
    service: &Service<'_>,
    body: CreateVaultBody,
) -> Result<exoharness::vault::VaultRecord> {
    service.require_full_provider()?;
    let vault = service
        .runtime
        .exoharness_handle()
        .create_vault(&body.name)
        .await
        .map_err(bad_request)?;
    Ok(vault.record().clone())
}

pub async fn delete_vault(service: &Service<'_>, path: &VaultPath) -> Result<bool> {
    service.require_full_provider()?;
    service
        .runtime
        .exoharness_handle()
        .delete_vault(
            &path
                .vault_id
                .ok_or_else(|| bad_request("vault ID missing"))?,
        )
        .await
        .map_err(bad_request)?;
    Ok(true)
}

pub async fn writable_vault(
    service: &Service<'_>,
    path: &VaultPath,
) -> Result<Arc<dyn exoharness::vault::VaultHandle>> {
    service.require_full_provider()?;
    scoped_vaults(service, path)
        .await?
        .into_iter()
        .find(|v| Some(v.record().id) == path.vault_id)
        .ok_or_else(|| not_found("vault is unavailable"))
}

pub async fn put_secret(
    service: &Service<'_>,
    path: &VaultPath,
    body: exoharness::PutSecretRequest,
) -> Result<exoharness::SecretId> {
    exoharness::vault::require_portable_secret(&body.secret).map_err(bad_request)?;
    let vault = writable_vault(service, path).await?;
    vault.put_secret(body).await.map_err(bad_request)
}

pub async fn update_secret(
    service: &Service<'_>,
    path: &VaultPath,
    body: exoharness::UpdateSecretRequest,
) -> Result<exoharness::SecretMetadata> {
    if let Some(secret) = &body.secret {
        exoharness::vault::require_portable_secret(secret).map_err(bad_request)?;
    }
    let vault = writable_vault(service, path).await?;
    vault
        .update_secret(
            &path
                .secret_id
                .ok_or_else(|| bad_request("secret ID missing"))?,
            body,
        )
        .await
        .map_err(bad_request)
}

pub async fn delete_secret(service: &Service<'_>, path: &VaultPath) -> Result<bool> {
    let vault = writable_vault(service, path).await?;
    vault
        .delete_secret(
            &path
                .secret_id
                .ok_or_else(|| bad_request("secret ID missing"))?,
        )
        .await
        .map_err(bad_request)?;
    Ok(true)
}

pub async fn list_secrets(
    service: &Service<'_>,
    path: &VaultPath,
) -> Result<Vec<exoharness::SecretMetadata>> {
    let vault = scoped_vaults(service, path)
        .await?
        .into_iter()
        .find(|vault| Some(vault.record().id) == path.vault_id)
        .ok_or_else(|| not_found("vault not available in this context"))?;
    vault.list_secrets().await.map_err(bad_request)
}

pub async fn create_agent(
    service: &Service<'_>,
    body: exoharness::NewAgentRequest,
) -> Result<exoharness::AgentRecord> {
    service.require_full_provider()?;
    let agent = service
        .runtime
        .exoharness_handle()
        .new_agent(body)
        .await
        .map_err(bad_request)?;
    Ok(agent.record().clone())
}

pub async fn get_agent(
    service: &Service<'_>,
    path: &AgentPath,
) -> Result<Option<exoharness::AgentRecord>> {
    if service
        .agent_id
        .is_some_and(|agent_id| agent_id != path.agent_id)
    {
        return Err(not_found("agent not found"));
    }
    Ok(service
        .runtime
        .exoharness_handle()
        .get_agent(&path.agent_id)
        .await
        .map_err(bad_request)?
        .map(|agent| agent.record().clone()))
}

pub async fn delete_agent(service: &Service<'_>, path: &AgentPath) -> Result<bool> {
    service.require_full_provider()?;
    service
        .runtime
        .delete_agent(&path.agent_id.to_string())
        .await
        .map_err(bad_request)
}

pub async fn list_artifacts(
    service: &Service<'_>,
    path: &AgentPath,
) -> Result<Vec<exoharness::ArtifactVersion>> {
    service
        .agent(path.agent_id)
        .await?
        .list_artifacts()
        .await
        .map_err(bad_request)
}

pub async fn read_artifact(
    service: &Service<'_>,
    path: &AgentPath,
    query: exoharness::ReadArtifactRequest,
) -> Result<Option<exoharness::Artifact>> {
    service
        .agent(path.agent_id)
        .await?
        .read_artifact(query)
        .await
        .map_err(bad_request)
}

pub async fn list_thread_artifacts(
    service: &Service<'_>,
    path: &ThreadPath,
) -> Result<Vec<exoharness::ArtifactVersion>> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    thread.list_artifacts().await.map_err(bad_request)
}

pub async fn read_thread_artifact(
    service: &Service<'_>,
    path: &ThreadPath,
    query: exoharness::ReadArtifactRequest,
) -> Result<Option<exoharness::Artifact>> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    thread.read_artifact(query).await.map_err(bad_request)
}

pub async fn write_artifact(
    service: &Service<'_>,
    path: &AgentPath,
    body: exoharness::WriteArtifactRequest,
) -> Result<exoharness::ArtifactVersion> {
    let agent = service.agent(path.agent_id).await?;
    let request = body;
    if request.path != exo_managed_agents::AGENT_DEFINITION_PATH {
        return Err(bad_request(
            "only the managed-agent definition can be written through this provider",
        ));
    }
    let definition = exo_managed_agents::AgentDefinition::parse(
        String::from_utf8(request.contents.clone()).map_err(bad_request)?,
    )
    .map_err(bad_request)?;
    let _guard = service.definition_updates.lock().await;
    service
        .runtime
        .update_managed_agent(&agent, &definition)
        .await
        .map_err(bad_request)
}

pub async fn get_thread(service: &Service<'_>, path: &ThreadPath) -> Result<ThreadResult> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    Ok(ThreadResult {
        agent: agent.record().clone(),
        thread: thread.record().clone(),
    })
}

pub async fn list_threads(
    service: &Service<'_>,
    path: &AgentPath,
    query: ThreadsQuery,
) -> Result<ListThreadsResult> {
    if query.automation_id.is_some() {
        return Err(not_implemented(
            "local runtime does not have automation-owned threads",
        ));
    }
    let agent = service.agent(path.agent_id).await?;
    let result = agent
        .list_threads(exoharness::ListThreadsRequest {
            cursor: query.cursor,
            limit: Some(query.limit.unwrap_or(100)),
            ..Default::default()
        })
        .await
        .map_err(bad_request)?;
    Ok(ListThreadsResult {
        agent: agent.record().clone(),
        threads: result
            .threads
            .into_iter()
            .map(|thread| thread.record().clone())
            .collect(),
        next_cursor: result.next_cursor,
    })
}

pub async fn update_thread_environment(
    service: &Service<'_>,
    path: &ThreadPath,
    body: exoharness::EnvironmentDefinition,
) -> Result<ThreadResult> {
    let agent = service.agent(path.agent_id).await?;
    let opened = service
        .runtime
        .open_managed_thread(
            &agent,
            Some(&path.thread_id.to_string()),
            exoharness::NewThreadRequest {
                environment: Some(body),
                ..Default::default()
            },
        )
        .await
        .map_err(bad_request)?;
    Ok(ThreadResult {
        agent: agent.record().clone(),
        thread: opened.thread.record().clone(),
    })
}

pub async fn attach_thread_vaults(
    service: &Service<'_>,
    path: &ThreadPath,
    body: AttachThreadVaultsBody,
) -> Result<ThreadResult> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service
        .thread(agent.as_ref(), path.thread_id)
        .await?
        .attach_vaults(body.vaults)
        .await
        .map_err(bad_request)?;
    Ok(ThreadResult {
        agent: agent.record().clone(),
        thread: thread.record().clone(),
    })
}

pub async fn delete_thread(service: &Service<'_>, path: &ThreadPath) -> Result<DeleteThreadResult> {
    let agent = service.agent(path.agent_id).await?;
    let deleted = service
        .runtime
        .delete_conversation(agent.as_ref(), &path.thread_id.to_string())
        .await
        .map_err(bad_request)?;
    Ok(DeleteThreadResult {
        agent: agent.record().clone(),
        thread_id: path.thread_id,
        deleted,
    })
}

pub async fn fork_thread(
    service: &Service<'_>,
    path: &ThreadPath,
    body: ForkThreadBody,
) -> Result<ThreadResult> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let forked = thread
        .fork(exoharness::ForkThreadRequest {
            name: body.thread_name,
            ..Default::default()
        })
        .await
        .map_err(bad_request)?;
    Ok(ThreadResult {
        agent: agent.record().clone(),
        thread: forked.record().clone(),
    })
}

pub async fn turn_status(service: &Service<'_>, path: &TurnPath) -> Result<TurnStatusResult> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let active = service
        .runtime
        .is_turn_active(thread.as_ref(), path.turn_id)
        .await
        .map_err(internal_error)?;
    Ok(TurnStatusResult { active })
}

pub async fn approval_response(
    service: &Service<'_>,
    path: &TurnPath,
    body: ApprovalResponseBody,
) -> Result<EventResult> {
    let agent = service.agent(path.agent_id).await?;
    service.thread(agent.as_ref(), path.thread_id).await?;
    let event_id = service
        .runtime
        .approval_response(path.agent_id, path.thread_id, path.turn_id, &body)
        .await
        .map_err(bad_request)?;
    Ok(EventResult { event_id })
}

pub async fn events(
    service: &Service<'_>,
    path: &ThreadPath,
    query: EventsQuery,
) -> Result<exoharness::GetEventsResult> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let result = thread
        .get_events(Some(EventQuery {
            cursor: query.after,
            direction: Some(query.direction.unwrap_or(EventQueryDirection::Asc)),
            limit: Some(query.limit.unwrap_or(100)),
            types: query.event_type.map(|names| {
                names
                    .split(',')
                    .filter(|name| !name.is_empty())
                    .map(|name| exoharness::EventKind::custom(name.to_owned()))
                    .collect()
            }),
            session_id: query.session_id,
            turn_id: query.turn_id,
        }))
        .await
        .map_err(bad_request)?;
    Ok(result)
}

/// Admit a turn; the transport owns draining or forwarding its progress stream.
pub async fn submit_turn(
    runtime: &Runtime,
    agent: Arc<dyn AgentHandle>,
    thread: Arc<dyn ThreadHandle>,
    prepared: PreparedTurn,
    streaming: bool,
) -> Result<(SubmitTurnResult, crate::ExecutionStreamHandle)> {
    let (turn, stream) = runtime
        .start_turn(
            agent.clone(),
            thread.clone(),
            prepared.request,
            streaming,
            Some(prepared.config),
        )
        .await?;
    Ok((
        SubmitTurnResult {
            agent: agent.record().clone(),
            thread: thread.record().clone(),
            turn,
            harness: prepared.harness,
        },
        stream,
    ))
}

pub async fn cancel_turn(
    runtime: &Runtime,
    thread_id: ThreadId,
    turn_id: TurnId,
) -> Result<CancelTurnResult> {
    Ok(CancelTurnResult {
        canceled_active_turn: runtime
            .cancel_turn(crate::harness::HarnessTurnKey::new(thread_id, turn_id))
            .await?,
        finished_event_id: None,
    })
}

/// Register the watcher before loading history so an append cannot fall between them.
pub async fn wait_events(
    service: &Service<'_>,
    path: &ThreadPath,
    query: EventsQuery,
) -> Result<exoharness::GetEventsResult> {
    use futures::StreamExt;
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let after = query.after;
    let mut watcher = thread
        .watch_events(std::ops::Bound::Excluded(
            after.unwrap_or_else(|| exoharness::Uuid7(Default::default())),
        ))
        .await?;
    let mut page = events(service, path, query).await?;
    if page.events.is_empty() {
        let event = watcher
            .next()
            .await
            .ok_or_else(|| bad_request("event stream closed"))??;
        page.cursor = Some(event.id);
        page.events.push(event);
    }
    Ok(page)
}
