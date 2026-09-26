mod temporary;

use std::{net::TcpListener, ops::Bound, sync::Arc};

use actix_web::{
    App, Error, HttpResponse, HttpServer,
    body::MessageBody,
    dev::{Server, ServiceRequest, ServiceResponse},
    error::{
        ErrorBadRequest, ErrorInternalServerError, ErrorNotFound, ErrorNotImplemented,
        ErrorUnauthorized,
    },
    http::header::{AUTHORIZATION, HeaderValue},
    middleware::{Next, from_fn},
    web,
};
use anyhow::{Result, bail};
use exo_managed_agents::http::{RUNTIME_PATH, protocol::*, sse};
use exoharness::{
    AgentHandle, AgentId, Event, EventData, EventQuery, EventQueryDirection, EventStream,
    ThreadHandle, ThreadId, TurnId, Uuid7,
};
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::{broadcast, oneshot};

use crate::{
    AgentConfig, AgentHarnessKind, ConversationModelConfig, ExecutionStreamEvent, Runtime,
    SendRequest, harness::HarnessTurnKey,
};

pub struct RuntimeHttpService {
    runtime: Arc<Runtime>,
    config: exoharness::BasicExoHarnessConfig,
    temporary: temporary::TemporaryAgents,
    authorization: HeaderValue,
    progress: broadcast::Sender<Event>,
    definition_updates: tokio::sync::Mutex<()>,
}

impl RuntimeHttpService {
    pub fn new(
        runtime: Arc<Runtime>,
        token: &str,
        config: exoharness::BasicExoHarnessConfig,
    ) -> Result<Self> {
        if token.trim().is_empty() {
            bail!("runtime HTTP service requires a bearer token");
        }
        Ok(Self {
            runtime,
            config,
            temporary: temporary::TemporaryAgents::new(),
            authorization: HeaderValue::from_str(&format!("Bearer {token}"))?,
            progress: broadcast::channel(1024).0,
            definition_updates: Default::default(),
        })
    }

    async fn agent(&self, id: AgentId) -> Result<Arc<dyn AgentHandle>, Error> {
        self.runtime_for(id)
            .exoharness_handle()
            .get_agent(&id)
            .await
            .map_err(ErrorInternalServerError)?
            .ok_or_else(|| ErrorNotFound("agent not found"))
    }

    fn runtime_for(&self, id: AgentId) -> Arc<Runtime> {
        self.temporary
            .get(id)
            .unwrap_or_else(|| self.runtime.clone())
    }

    async fn thread(
        &self,
        agent: &dyn AgentHandle,
        id: ThreadId,
    ) -> Result<Arc<dyn ThreadHandle>, Error> {
        agent
            .get_thread(&id)
            .await
            .map_err(ErrorInternalServerError)?
            .ok_or_else(|| ErrorNotFound("thread not found"))
    }
}

pub fn server(listener: TcpListener, service: Arc<RuntimeHttpService>) -> std::io::Result<Server> {
    Ok(HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(Arc::clone(&service)))
            .configure(configure)
    })
    .listen(listener)?
    .run())
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope(RUNTIME_PATH)
            .wrap(from_fn(authorize))
            .route("/identity", web::get().to(identity))
            .route("/vault", web::get().to(list_vaults))
            .route("/vault/{vault_id}/secret", web::get().to(list_secrets))
            .route("/agent/{agent_id}/vault", web::get().to(list_vaults))
            .route(
                "/agent/{agent_id}/vault/{vault_id}/secret",
                web::get().to(list_secrets),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/vault",
                web::get().to(list_vaults),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/vault/{vault_id}/secret",
                web::get().to(list_secrets),
            )
            .route("/agent", web::get().to(list_agents))
            .route("/agent", web::post().to(create_agent))
            .route("/agent/temporary", web::post().to(temporary::create_agent))
            .route("/agent/{agent_id}", web::get().to(get_agent))
            .route("/agent/{agent_id}", web::delete().to(delete_agent))
            .route("/agent/{agent_id}/artifact", web::get().to(list_artifacts))
            .route("/agent/{agent_id}/artifact", web::post().to(write_artifact))
            .route(
                "/agent/{agent_id}/artifact/read",
                web::get().to(read_artifact),
            )
            .route("/agent/{agent_id}/thread", web::get().to(list_threads))
            .route("/agent/{agent_id}/thread", web::post().to(create_thread))
            .route(
                "/agent/{agent_id}/thread/{thread_id}",
                web::get().to(get_thread),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}",
                web::delete().to(delete_thread),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/artifact",
                web::get().to(list_thread_artifacts),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/artifact/read",
                web::get().to(read_thread_artifact),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/fork",
                web::post().to(fork_thread),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/turn",
                web::post().to(submit_turn),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/turn/{turn_id}/cancel",
                web::post().to(cancel_turn),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/turn/{turn_id}/approval-response",
                web::post().to(unsupported_interaction),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/turn/{turn_id}/frontend-tool-result",
                web::post().to(unsupported_interaction),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/event",
                web::get().to(events),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/event/watch",
                web::get().to(watch),
            ),
    );
}

async fn authorize(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let service = req
        .app_data::<web::Data<Arc<RuntimeHttpService>>>()
        .ok_or_else(|| ErrorInternalServerError("runtime service not configured"))?;
    if req.headers().get(AUTHORIZATION) != Some(&service.authorization) {
        return Err(ErrorUnauthorized("runtime bearer token required"));
    }
    next.call(req).await
}

#[derive(Deserialize)]
struct AgentPath {
    agent_id: AgentId,
}
#[derive(Deserialize)]
struct ThreadPath {
    agent_id: AgentId,
    thread_id: ThreadId,
}
#[derive(Deserialize)]
struct TurnPath {
    agent_id: AgentId,
    thread_id: ThreadId,
    turn_id: TurnId,
}

async fn list_agents(
    service: web::Data<Arc<RuntimeHttpService>>,
    query: web::Query<AgentsQuery>,
) -> Result<web::Json<ListAgentsResult>, Error> {
    let mut agents = service
        .runtime
        .list_agents()
        .await
        .map_err(ErrorInternalServerError)?;
    if let Some(slug) = &query.slug {
        agents.retain(|agent| &agent.slug == slug);
    }
    Ok(web::Json(ListAgentsResult { agents }))
}

async fn identity() -> web::Json<ProviderIdentity> {
    web::Json(ProviderIdentity {
        account_id: "local".to_owned(),
    })
}

#[derive(Deserialize)]
struct VaultPath {
    agent_id: Option<AgentId>,
    thread_id: Option<ThreadId>,
    vault_id: Option<exoharness::vault::VaultId>,
}

async fn scoped_vaults(
    service: &RuntimeHttpService,
    path: &VaultPath,
) -> Result<Vec<Arc<dyn exoharness::vault::VaultHandle>>, Error> {
    if let Some(id) = path.agent_id {
        let agent = service.agent(id).await?;
        if let Some(thread_id) = path.thread_id {
            return service
                .thread(agent.as_ref(), thread_id)
                .await?
                .list_vaults()
                .await
                .map_err(ErrorBadRequest);
        }
        return agent.list_vaults().await.map_err(ErrorBadRequest);
    }
    service
        .runtime
        .exoharness_handle()
        .list_vaults()
        .await
        .map_err(ErrorBadRequest)
}

async fn list_vaults(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<VaultPath>,
) -> Result<web::Json<Vec<exoharness::vault::VaultRecord>>, Error> {
    Ok(web::Json(
        scoped_vaults(&service, &path)
            .await?
            .iter()
            .map(|vault| vault.record().clone())
            .collect(),
    ))
}

async fn list_secrets(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<VaultPath>,
) -> Result<web::Json<Vec<exoharness::SecretMetadata>>, Error> {
    let vault = scoped_vaults(&service, &path)
        .await?
        .into_iter()
        .find(|vault| Some(vault.record().id) == path.vault_id)
        .ok_or_else(|| ErrorNotFound("vault not available in this context"))?;
    Ok(web::Json(
        vault.list_secrets().await.map_err(ErrorBadRequest)?,
    ))
}

async fn create_agent(
    service: web::Data<Arc<RuntimeHttpService>>,
    body: web::Json<exoharness::NewAgentRequest>,
) -> Result<web::Json<exoharness::AgentRecord>, Error> {
    let agent = service
        .runtime
        .exoharness_handle()
        .new_agent(body.into_inner())
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(agent.record().clone()))
}

async fn get_agent(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<AgentPath>,
) -> Result<web::Json<Option<exoharness::AgentRecord>>, Error> {
    Ok(web::Json(
        service
            .runtime_for(path.agent_id)
            .exoharness_handle()
            .get_agent(&path.agent_id)
            .await
            .map_err(ErrorBadRequest)?
            .map(|agent| agent.record().clone()),
    ))
}

async fn delete_agent(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<AgentPath>,
) -> Result<web::Json<bool>, Error> {
    if let Some(runtime) = service.temporary.remove(path.agent_id) {
        temporary::shutdown(runtime)
            .await
            .map_err(ErrorInternalServerError)?
            .map_err(ErrorBadRequest)?;
        return Ok(web::Json(true));
    }
    Ok(web::Json(
        service
            .runtime
            .exoharness_handle()
            .delete_agent(&path.agent_id)
            .await
            .map_err(ErrorBadRequest)?,
    ))
}

async fn list_artifacts(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<AgentPath>,
) -> Result<web::Json<Vec<exoharness::ArtifactVersion>>, Error> {
    Ok(web::Json(
        service
            .agent(path.agent_id)
            .await?
            .list_artifacts()
            .await
            .map_err(ErrorBadRequest)?,
    ))
}

async fn read_artifact(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<AgentPath>,
    query: web::Query<exoharness::ReadArtifactRequest>,
) -> Result<web::Json<Option<exoharness::Artifact>>, Error> {
    Ok(web::Json(
        service
            .agent(path.agent_id)
            .await?
            .read_artifact(query.into_inner())
            .await
            .map_err(ErrorBadRequest)?,
    ))
}

async fn list_thread_artifacts(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<ThreadPath>,
) -> Result<web::Json<Vec<exoharness::ArtifactVersion>>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    Ok(web::Json(
        thread.list_artifacts().await.map_err(ErrorBadRequest)?,
    ))
}

async fn read_thread_artifact(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<ThreadPath>,
    query: web::Query<exoharness::ReadArtifactRequest>,
) -> Result<web::Json<Option<exoharness::Artifact>>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    Ok(web::Json(
        thread
            .read_artifact(query.into_inner())
            .await
            .map_err(ErrorBadRequest)?,
    ))
}

async fn write_artifact(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<AgentPath>,
    body: web::Json<exoharness::WriteArtifactRequest>,
) -> Result<web::Json<exoharness::ArtifactVersion>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let request = body.into_inner();
    if request.path != exo_managed_agents::AGENT_DEFINITION_PATH {
        return Err(ErrorBadRequest(
            "only the managed-agent definition can be written through this provider",
        ));
    }
    let definition = exo_managed_agents::AgentDefinition::parse(
        String::from_utf8(request.contents.clone()).map_err(ErrorBadRequest)?,
    )
    .map_err(ErrorBadRequest)?;
    let _guard = service.definition_updates.lock().await;
    let previous = exo_managed_agents::load_definition(agent.as_ref())
        .await
        .map_err(ErrorBadRequest)?;
    let version = agent
        .write_artifact(request)
        .await
        .map_err(ErrorBadRequest)?;
    if let Err(error) = service
        .runtime_for(path.agent_id)
        .configure_managed_agent(&agent, &definition)
        .await
    {
        agent
            .write_artifact(exoharness::WriteArtifactRequest {
                path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
                contents: previous.map(|definition| definition.source().as_bytes().to_vec()).unwrap_or_default(),
            })
            .await
            .map_err(|rollback| ErrorInternalServerError(format!(
                "updating agent configuration failed: {error:#}; restoring previous definition failed: {rollback:#}"
            )))?;
        return Err(ErrorBadRequest(format!("{error:#}")));
    }
    Ok(web::Json(version))
}

async fn get_thread(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<ThreadPath>,
) -> Result<web::Json<ThreadResult>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    Ok(web::Json(ThreadResult {
        agent: agent.record().clone(),
        thread: thread.record().clone(),
    }))
}

async fn list_threads(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<AgentPath>,
    query: web::Query<ThreadsQuery>,
) -> Result<web::Json<ListThreadsResult>, Error> {
    if query.automation_id.is_some() {
        return Err(ErrorNotImplemented(
            "local runtime does not have automation-owned threads",
        ));
    }
    let agent = service.agent(path.agent_id).await?;
    let result = agent
        .list_threads(exoharness::ListThreadsRequest {
            cursor: query.cursor,
            limit: Some(query.limit.unwrap_or(100)),
        })
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(ListThreadsResult {
        agent: agent.record().clone(),
        threads: result
            .threads
            .into_iter()
            .map(|thread| thread.record().clone())
            .collect(),
        next_cursor: result.next_cursor,
    }))
}

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
) -> Result<(), Error> {
    let Some(requested) = requested else {
        return Ok(());
    };
    let definition = exo_managed_agents::load_definition(agent)
        .await
        .map_err(ErrorBadRequest)?;
    let actual = definition
        .as_ref()
        .map(|d| d.frontmatter.harness.as_str())
        .unwrap_or_else(|| harness_name(config));
    if requested != actual && !(requested == "native" && config.harness == AgentHarnessKind::Basic)
    {
        return Err(ErrorNotImplemented(
            "choose the harness in the saved agent definition",
        ));
    }
    Ok(())
}

async fn create_thread(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<AgentPath>,
    body: Option<web::Json<CreateThreadBody>>,
) -> Result<web::Json<CreateThreadResult>, Error> {
    let body = body.map(web::Json::into_inner).unwrap_or_default();
    if body.thread_id.is_some()
        || body.endpoint_name.is_some()
        || body.reasoning_effort.is_some()
        || body.visibility != ThreadVisibility::Owner
        || body.source.is_some()
    {
        return Err(ErrorNotImplemented(
            "local runtime does not support supplied thread IDs, endpoint/reasoning overrides, or shared/automation threads",
        ));
    }
    let agent = service.agent(path.agent_id).await?;
    let config = service
        .runtime_for(path.agent_id)
        .get_agent_config(agent.as_ref())
        .await
        .map_err(ErrorBadRequest)?;
    check_harness(agent.as_ref(), &config, body.harness.as_deref()).await?;
    let thread = service
        .runtime_for(path.agent_id)
        .open_managed_thread(
            &agent,
            None,
            exoharness::NewThreadRequest {
                vaults: body.vaults,
                slug: body.thread_slug,
                name: body.thread_name,
            },
        )
        .await
        .map_err(ErrorBadRequest)?
        .thread;
    if let Some(model) = body.model {
        crate::put_conversation_model_override(
            thread.as_ref(),
            Some(ConversationModelConfig {
                model,
                max_output_tokens: None,
            }),
        )
        .await
        .map_err(ErrorBadRequest)?;
    }
    Ok(web::Json(CreateThreadResult {
        agent: agent.record().clone(),
        thread: thread.record().clone(),
        harness: harness_name(&config).to_owned(),
    }))
}

async fn delete_thread(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<ThreadPath>,
) -> Result<web::Json<DeleteThreadResult>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let deleted = agent
        .delete_thread(&path.thread_id)
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(DeleteThreadResult {
        agent: agent.record().clone(),
        thread_id: path.thread_id,
        deleted,
    }))
}

async fn fork_thread(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<ThreadPath>,
    body: Option<web::Json<ForkThreadBody>>,
) -> Result<web::Json<ThreadResult>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let forked = thread
        .fork(exoharness::ForkThreadRequest {
            name: body.and_then(|body| body.into_inner().thread_name),
            ..Default::default()
        })
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(ThreadResult {
        agent: agent.record().clone(),
        thread: forked.record().clone(),
    }))
}

async fn submit_turn(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<ThreadPath>,
    body: web::Json<SubmitTurnBody>,
) -> Result<HttpResponse, Error> {
    let body = body.into_inner();
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
        return Err(ErrorNotImplemented(
            "local runtime does not support the requested turn options",
        ));
    }
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let mut config = service
        .runtime_for(path.agent_id)
        .get_agent_config(agent.as_ref())
        .await
        .map_err(ErrorBadRequest)?;
    check_harness(agent.as_ref(), &config, body.harness.as_deref()).await?;
    if let Some(model) = crate::get_conversation_model_override(thread.as_ref())
        .await
        .map_err(ErrorBadRequest)?
    {
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
    let runtime = service.runtime_for(path.agent_id);
    let progress = service.progress.clone();
    let (receipt, received) = oneshot::channel();
    tokio::spawn(async move {
        let request = SendRequest {
            input: body.input.map(OneOrMany::into_vec).unwrap_or_default(),
            session_id: body.session_id,
        };
        let admitted = runtime
            .start_turn(
                Arc::clone(&agent),
                Arc::clone(&thread),
                request,
                true,
                Some(config),
            )
            .await;
        let (turn, mut events) = match admitted {
            Ok(admitted) => admitted,
            Err(error) => {
                if receipt.send(Err(error)).is_err() {
                    tracing::debug!("turn requester disconnected before admission failed");
                }
                return;
            }
        };
        let result = SubmitTurnResult {
            agent: agent.record().clone(),
            thread: thread.record().clone(),
            turn: turn.clone(),
            harness,
        };
        if receipt.send(Ok(result)).is_err() {
            tracing::debug!(turn_id = %turn.id, "turn requester disconnected after admission");
        }
        while let Some(event) = events.next().await {
            match event {
                Ok(ExecutionStreamEvent::Chunk(chunk)) => {
                    let event = Event {
                        id: Uuid7::now(),
                        thread_id: thread.record().id,
                        session_id: Some(turn.session_id),
                        turn_id: Some(turn.id),
                        created_at: chrono::Utc::now(),
                        data: EventData::LinguaStreamChunk { chunk },
                    };
                    if progress.send(event).is_err() {
                        tracing::trace!("no runtime progress subscribers");
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, turn_id = %turn.id, "runtime turn failed"),
            }
        }
    });
    let result = received
        .await
        .map_err(ErrorInternalServerError)?
        .map_err(ErrorBadRequest)?;
    Ok(HttpResponse::Accepted().json(result))
}

async fn cancel_turn(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<TurnPath>,
) -> Result<web::Json<CancelTurnResult>, Error> {
    let agent = service.agent(path.agent_id).await?;
    service.thread(agent.as_ref(), path.thread_id).await?;
    let canceled_active_turn = service
        .runtime_for(path.agent_id)
        .cancel_turn(HarnessTurnKey::new(path.thread_id, path.turn_id))
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(CancelTurnResult {
        canceled_active_turn,
        finished_event_id: None,
    }))
}

async fn unsupported_interaction() -> Result<HttpResponse, Error> {
    Err(ErrorNotImplemented(
        "this runtime does not execute frontend tools or approval requests",
    ))
}

async fn events(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<ThreadPath>,
    query: web::Query<EventsQuery>,
) -> Result<web::Json<exoharness::GetEventsResult>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let query = query.into_inner();
    let result = thread
        .get_events(Some(EventQuery {
            cursor: query.after,
            direction: Some(query.direction.unwrap_or(EventQueryDirection::Asc)),
            limit: Some(query.limit.unwrap_or(100)),
            types: query
                .event_type
                .map(|name| vec![exoharness::EventKind::custom(name)]),
            ..Default::default()
        }))
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(result))
}

async fn watch(
    service: web::Data<Arc<RuntimeHttpService>>,
    path: web::Path<ThreadPath>,
    query: web::Query<WatchQuery>,
) -> Result<HttpResponse, Error> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let receiver = service.progress.subscribe();
    let thread_id = path.thread_id;
    // Unbounded is live-only; the nil cursor includes all saved events.
    let durable = thread
        .watch_events(Bound::Excluded(
            query.after.unwrap_or_else(|| Uuid7(Default::default())),
        ))
        .await
        .map_err(ErrorBadRequest)?;
    let stream = futures::stream::select(durable, progress_stream(receiver, thread_id));
    Ok(HttpResponse::Ok()
        .insert_header(("content-type", "text/event-stream; charset=utf-8"))
        .insert_header(("cache-control", "no-cache, no-transform"))
        .insert_header(("connection", "keep-alive"))
        .insert_header(("x-accel-buffering", "no"))
        .streaming(
            sse::encode(Box::pin(stream)).map(|result| result.map_err(ErrorInternalServerError)),
        ))
}

fn progress_stream(receiver: broadcast::Receiver<Event>, thread_id: ThreadId) -> EventStream {
    Box::pin(futures::stream::unfold(
        receiver,
        move |mut receiver| async move {
            loop {
                match receiver.recv().await {
                    Ok(event) if event.thread_id == thread_id => {
                        return Some((Ok(event), receiver));
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[tokio::test]
    async fn lagged_progress_skips_lost_chunks_and_keeps_the_stream_open() -> Result<()> {
        let thread_id = Uuid7::now();
        let event = |thread_id| Event {
            id: Uuid7::now(),
            thread_id,
            session_id: None,
            turn_id: None,
            created_at: chrono::Utc::now(),
            data: EventData::LinguaStreamChunk {
                chunk: lingua::UniversalStreamChunk::new(
                    Some("chunk".into()),
                    None,
                    vec![],
                    None,
                    None,
                ),
            },
        };
        let (sender, receiver) = broadcast::channel(2);
        let mut progress = progress_stream(receiver, thread_id);
        sender.send(event(thread_id))?;
        sender.send(event(Uuid7::now()))?;
        let retained = event(thread_id);
        sender.send(retained.clone())?;
        let actual = tokio::time::timeout(std::time::Duration::from_secs(1), progress.next())
            .await?
            .context("progress stream ended after lag")??;
        assert_eq!(actual.id, retained.id);
        let next = event(thread_id);
        sender.send(next.clone())?;
        assert_eq!(
            progress.next().await.context("progress stream ended")??.id,
            next.id
        );
        drop(sender);
        assert!(progress.next().await.is_none());
        Ok(())
    }
}
