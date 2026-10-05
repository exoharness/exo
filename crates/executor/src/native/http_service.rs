use std::{net::TcpListener, ops::Bound, sync::Arc};

use crate::managed_agents::service::{AgentPath, ThreadPath, TurnPath, VaultPath};
use actix_web::{
    App, Error, HttpMessage, HttpRequest, HttpResponse, HttpServer,
    body::MessageBody,
    dev::{Server, ServiceRequest, ServiceResponse},
    error::{
        ErrorBadRequest, ErrorInternalServerError, ErrorNotFound, ErrorNotImplemented,
        ErrorUnauthorized,
    },
    http::header::{AUTHORIZATION, HeaderValue},
    middleware::{Next, from_fn},
    mime, web,
};
use anyhow::{Context, Result, bail};
use exo_managed_agents::http::{RUNTIME_PATH, protocol::*, sse};
use exoharness::{
    AgentHandle, AgentId, Event, EventData, EventStream, ThreadHandle, ThreadId, Uuid7,
};
use futures::StreamExt;
use tokio::sync::{broadcast, oneshot};

use crate::{ExecutionStreamEvent, Runtime, harness::HarnessTurnKey};

// Thread creation can include a large environment definition. Bytes buffers the request.
const OPTIONAL_JSON_BODY_LIMIT: usize = 256 * 1024 * 1024;

fn optional_json_body<T: Default + serde::de::DeserializeOwned>(
    request: &HttpRequest,
    body: &web::Bytes,
) -> Result<T, Error> {
    if body.is_empty() {
        return Ok(T::default());
    }
    let json_content_type = request
        .mime_type()
        .ok()
        .flatten()
        .is_some_and(|content_type| {
            content_type.subtype() == mime::JSON || content_type.suffix() == Some(mime::JSON)
        });
    if !json_content_type {
        return Err(ErrorBadRequest("Content-Type must be JSON"));
    }
    serde_json::from_slice(body).map_err(ErrorBadRequest)
}

#[derive(Clone)]
pub struct RuntimeHttpService {
    runtime: Arc<Runtime>,
    agent_id: Option<AgentId>,
    bearer_token: Option<String>,
    progress: broadcast::Sender<Event>,
    definition_updates: Arc<tokio::sync::Mutex<()>>,
    auth: Option<Arc<crate::remote::AuthServer>>,
    multiplayer: bool,
    callers: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<Runtime>>>>,
    account_id: String,
    session: Option<String>,
}

impl RuntimeHttpService {
    fn shared(&self) -> crate::managed_agents::service::Service<'_> {
        crate::managed_agents::service::Service {
            runtime: &self.runtime,
            agent_id: self.agent_id,
            definition_updates: &self.definition_updates,
        }
    }
    pub fn new(runtime: Arc<Runtime>, token: Option<&str>) -> Result<Self> {
        if token.is_some_and(|token| token.trim().is_empty()) {
            bail!("runtime HTTP service bearer token must not be empty");
        }
        Ok(Self {
            runtime,
            auth: None,
            multiplayer: false,
            callers: Arc::default(),
            account_id: "local".into(),
            session: None,
            agent_id: None,
            bearer_token: token
                .map(|token| {
                    HeaderValue::from_str(&format!("Bearer {token}")).map(|_| token.to_owned())
                })
                .transpose()?,
            progress: broadcast::channel(1024).0,
            definition_updates: Default::default(),
        })
    }

    pub fn with_auth(mut self, auth: Arc<crate::remote::AuthServer>, multiplayer: bool) -> Self {
        self.auth = Some(auth);
        self.multiplayer = multiplayer;
        self
    }

    pub fn spawn_recovery(&self) {
        self.runtime.begin_recovery_scan();
        let service = self.clone();
        tokio::spawn(async move {
            let resolver = service.auth.as_ref().map(|_| {
                let service = service.clone();
                Arc::new(move |principal: String| service.caller_runtime(principal))
                    as crate::harness_executor::RecoveryRuntimeResolver
            });
            if let Err(error) = service
                .runtime
                .recover_unfinished_turns_with_resolver(resolver)
                .await
            {
                tracing::error!(%error, "failed to recover unfinished turns");
            }
        });
    }

    pub fn caller_runtime(&self, principal: String) -> Result<Arc<Runtime>> {
        let mut callers = self.callers.lock().expect("caller runtimes poisoned");
        if let Some(runtime) = callers.get(&principal) {
            return Ok(runtime.clone());
        }
        let auth = self
            .auth
            .as_ref()
            .context("authentication is not configured")?;
        let runtime = Arc::new(
            self.runtime
                .with_caller(auth.caller(principal.clone(), self.multiplayer))?,
        );
        callers.insert(principal, runtime.clone());
        Ok(runtime)
    }

    pub async fn shutdown_callers(&self) -> Result<()> {
        let runtimes: Vec<_> = self
            .callers
            .lock()
            .expect("caller runtimes poisoned")
            .drain()
            .map(|(_, r)| r)
            .collect();
        futures::future::join_all(runtimes.iter().map(|runtime| runtime.shutdown()))
            .await
            .into_iter()
            .collect()
    }

    pub fn for_agent(mut self, agent_id: AgentId) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    async fn agent(&self, id: AgentId) -> Result<Arc<dyn AgentHandle>, Error> {
        if self.agent_id.is_some_and(|agent_id| agent_id != id) {
            return Err(ErrorNotFound("agent not found"));
        }
        self.runtime
            .exoharness_handle()
            .get_agent(&id)
            .await
            .map_err(ErrorInternalServerError)?
            .ok_or_else(|| ErrorNotFound("agent not found"))
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
    let recovery_service = Arc::clone(&service);
    let server = HttpServer::new(move || {
        let app = App::new().app_data(web::Data::new(Arc::clone(&service)));
        let auth = service.auth.clone();
        app.configure(move |cfg| {
            if let Some(auth) = auth {
                cfg.app_data(web::Data::new(auth));
                crate::remote::configure(cfg);
            }
            configure(cfg);
        })
    })
    .listen(listener)?;
    recovery_service.spawn_recovery();
    Ok(server.run())
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope(RUNTIME_PATH)
            .wrap(from_fn(authorize))
            .route("/identity", web::get().to(identity))
            .route(
                "/agent/{agent_id}/thread/{thread_id}/environment",
                web::put().to(update_thread_environment),
            )
            .route("/environment", web::get().to(list_environments))
            .route("/environment", web::put().to(put_environment))
            .route("/environment/{name}", web::delete().to(delete_environment))
            .configure(configure_vaults)
            .route("/agent", web::get().to(list_agents))
            .route("/agent", web::post().to(create_agent))
            .route("/agent/{agent_id}", web::get().to(get_agent))
            .route("/agent/{agent_id}", web::delete().to(delete_agent))
            .route("/agent/{agent_id}/artifact", web::get().to(list_artifacts))
            .route("/agent/{agent_id}/artifact", web::post().to(write_artifact))
            .route(
                "/agent/{agent_id}/artifact/read",
                web::get().to(read_artifact),
            )
            .service(
                web::resource("/agent/{agent_id}/thread")
                    .app_data(web::PayloadConfig::new(OPTIONAL_JSON_BODY_LIMIT))
                    .route(web::get().to(list_threads))
                    .route(web::post().to(create_thread)),
            )
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
            .service(
                web::resource("/agent/{agent_id}/thread/{thread_id}/fork")
                    .app_data(web::PayloadConfig::new(OPTIONAL_JSON_BODY_LIMIT))
                    .route(web::post().to(fork_thread)),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/turn",
                web::post().to(submit_turn),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/turn/{turn_id}",
                web::get().to(turn_status),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/turn/{turn_id}/cancel",
                web::post().to(cancel_turn),
            )
            .route(
                "/agent/{agent_id}/thread/{thread_id}/turn/{turn_id}/approval-response",
                web::post().to(approval_response),
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

fn configure_vaults(cfg: &mut web::ServiceConfig) {
    for prefix in [
        "/vault",
        "/agent/{agent_id}/vault",
        "/agent/{agent_id}/thread/{thread_id}/vault",
    ] {
        let secrets = format!("{prefix}/{{vault_id}}/secret");
        let secret = format!("{secrets}/{{secret_id}}");
        cfg.route(prefix, web::get().to(list_vaults))
            .route(&secrets, web::get().to(list_secrets))
            .route(&secrets, web::post().to(put_secret))
            .route(&secret, web::put().to(update_secret))
            .route(&secret, web::delete().to(delete_secret));
    }
    cfg.route("/vault", web::post().to(create_vault))
        .route("/vault/{vault_id}", web::delete().to(delete_vault))
        .route(
            "/agent/{agent_id}/thread/{thread_id}/vault",
            web::post().to(attach_thread_vaults),
        );
}

async fn authorize(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let service = req
        .app_data::<web::Data<Arc<RuntimeHttpService>>>()
        .ok_or_else(|| ErrorInternalServerError("runtime service not configured"))?;
    if let Some(token) = &service.bearer_token
        && !crate::http_auth::bearer_token_matches(
            token,
            req.headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
        )
    {
        return Err(ErrorUnauthorized("runtime bearer token required"));
    }
    next.call(req).await
}

struct Service(Arc<RuntimeHttpService>);
impl std::ops::Deref for Service {
    type Target = RuntimeHttpService;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl actix_web::FromRequest for Service {
    type Error = Error;
    type Future = futures::future::LocalBoxFuture<'static, Result<Self, Error>>;
    fn from_request(req: &actix_web::HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move {
            let service = req
                .app_data::<web::Data<Arc<RuntimeHttpService>>>()
                .ok_or_else(|| ErrorInternalServerError("runtime service missing"))?
                .get_ref()
                .clone();
            let Some(auth) = &service.auth else {
                return Ok(Self(service));
            };
            let (principal, session) = auth.authenticate(&req).await.map_err(|_| {
                #[derive(Debug)]
                struct Required(String);
                impl std::fmt::Display for Required {
                    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str("Exo login required")
                    }
                }
                impl actix_web::ResponseError for Required {
                    fn status_code(&self) -> actix_web::http::StatusCode {
                        actix_web::http::StatusCode::UNAUTHORIZED
                    }
                    fn error_response(&self) -> HttpResponse {
                        HttpResponse::Unauthorized()
                            .insert_header((
                                "WWW-Authenticate",
                                format!("Bearer resource_metadata=\"{}\"", self.0),
                            ))
                            .body("Exo login required")
                    }
                }
                Error::from(Required(auth.metadata_url()))
            })?;
            let mut scoped = (*service).clone();
            scoped.runtime = service
                .caller_runtime(principal.clone())
                .map_err(ErrorInternalServerError)?;
            scoped.account_id = principal;
            scoped.session = Some(session);
            Ok(Self(Arc::new(scoped)))
        })
    }
}

async fn list_agents(
    service: Service,
    query: web::Query<AgentsQuery>,
) -> Result<web::Json<ListAgentsResult>, Error> {
    crate::managed_agents::service::list_agents(&service.shared(), query.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn identity(service: Service) -> web::Json<ProviderIdentity> {
    web::Json(ProviderIdentity {
        account_id: service.account_id.clone(),
    })
}

async fn list_environments(
    service: Service,
) -> Result<web::Json<Vec<exoharness::EnvironmentDefinition>>, Error> {
    crate::managed_agents::service::list_environments(&service.shared())
        .await
        .map(web::Json)
        .map_err(request_error)
}
async fn put_environment(
    service: Service,
    body: web::Json<exoharness::EnvironmentDefinition>,
) -> Result<web::Json<bool>, Error> {
    crate::managed_agents::service::put_environment(&service.shared(), body.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}
async fn delete_environment(
    service: Service,
    name: web::Path<String>,
) -> Result<web::Json<bool>, Error> {
    crate::managed_agents::service::delete_environment(&service.shared(), &name)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn list_vaults(
    service: Service,
    path: web::Path<VaultPath>,
) -> Result<web::Json<Vec<exoharness::vault::VaultRecord>>, Error> {
    crate::managed_agents::service::list_vaults(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn create_vault(
    service: Service,
    body: web::Json<CreateVaultBody>,
) -> Result<web::Json<exoharness::vault::VaultRecord>, Error> {
    crate::managed_agents::service::create_vault(&service.shared(), body.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}
async fn delete_vault(
    service: Service,
    path: web::Path<VaultPath>,
) -> Result<web::Json<bool>, Error> {
    crate::managed_agents::service::delete_vault(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn put_secret(
    service: Service,
    path: web::Path<VaultPath>,
    body: web::Json<exoharness::PutSecretRequest>,
) -> Result<web::Json<exoharness::SecretId>, Error> {
    crate::managed_agents::service::put_secret(&service.shared(), &path, body.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}
async fn update_secret(
    service: Service,
    path: web::Path<VaultPath>,
    body: web::Json<exoharness::UpdateSecretRequest>,
) -> Result<web::Json<exoharness::SecretMetadata>, Error> {
    crate::managed_agents::service::update_secret(&service.shared(), &path, body.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}
async fn delete_secret(
    service: Service,
    path: web::Path<VaultPath>,
) -> Result<web::Json<bool>, Error> {
    crate::managed_agents::service::delete_secret(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn list_secrets(
    service: Service,
    path: web::Path<VaultPath>,
) -> Result<web::Json<Vec<exoharness::SecretMetadata>>, Error> {
    crate::managed_agents::service::list_secrets(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn create_agent(
    service: Service,
    body: web::Json<exoharness::NewAgentRequest>,
) -> Result<web::Json<exoharness::AgentRecord>, Error> {
    crate::managed_agents::service::create_agent(&service.shared(), body.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn get_agent(
    service: Service,
    path: web::Path<AgentPath>,
) -> Result<web::Json<Option<exoharness::AgentRecord>>, Error> {
    crate::managed_agents::service::get_agent(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn delete_agent(
    service: Service,
    path: web::Path<AgentPath>,
) -> Result<web::Json<bool>, Error> {
    crate::managed_agents::service::delete_agent(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn list_artifacts(
    service: Service,
    path: web::Path<AgentPath>,
) -> Result<web::Json<Vec<exoharness::ArtifactVersion>>, Error> {
    crate::managed_agents::service::list_artifacts(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn read_artifact(
    service: Service,
    path: web::Path<AgentPath>,
    query: web::Query<exoharness::ReadArtifactRequest>,
) -> Result<web::Json<Option<exoharness::Artifact>>, Error> {
    crate::managed_agents::service::read_artifact(&service.shared(), &path, query.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn list_thread_artifacts(
    service: Service,
    path: web::Path<ThreadPath>,
) -> Result<web::Json<Vec<exoharness::ArtifactVersion>>, Error> {
    crate::managed_agents::service::list_thread_artifacts(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn read_thread_artifact(
    service: Service,
    path: web::Path<ThreadPath>,
    query: web::Query<exoharness::ReadArtifactRequest>,
) -> Result<web::Json<Option<exoharness::Artifact>>, Error> {
    crate::managed_agents::service::read_thread_artifact(
        &service.shared(),
        &path,
        query.into_inner(),
    )
    .await
    .map(web::Json)
    .map_err(request_error)
}

async fn write_artifact(
    service: Service,
    path: web::Path<AgentPath>,
    body: web::Json<exoharness::WriteArtifactRequest>,
) -> Result<web::Json<exoharness::ArtifactVersion>, Error> {
    crate::managed_agents::service::write_artifact(&service.shared(), &path, body.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn get_thread(
    service: Service,
    path: web::Path<ThreadPath>,
) -> Result<web::Json<ThreadResult>, Error> {
    crate::managed_agents::service::get_thread(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn list_threads(
    service: Service,
    path: web::Path<AgentPath>,
    query: web::Query<ThreadsQuery>,
) -> Result<web::Json<ListThreadsResult>, Error> {
    crate::managed_agents::service::list_threads(&service.shared(), &path, query.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn create_thread(
    service: Service,
    path: web::Path<AgentPath>,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<web::Json<CreateThreadResult>, Error> {
    let body: CreateThreadBody = optional_json_body(&request, &body)?;
    let agent = service.agent(path.agent_id).await?;
    crate::managed_agents::service::create_thread(&service.runtime, &agent, body)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn update_thread_environment(
    service: Service,
    path: web::Path<ThreadPath>,
    body: web::Json<exoharness::EnvironmentDefinition>,
) -> Result<web::Json<ThreadResult>, Error> {
    crate::managed_agents::service::update_thread_environment(
        &service.shared(),
        &path,
        body.into_inner(),
    )
    .await
    .map(web::Json)
    .map_err(request_error)
}

async fn attach_thread_vaults(
    service: Service,
    path: web::Path<ThreadPath>,
    body: web::Json<AttachThreadVaultsBody>,
) -> Result<web::Json<ThreadResult>, Error> {
    crate::managed_agents::service::attach_thread_vaults(
        &service.shared(),
        &path,
        body.into_inner(),
    )
    .await
    .map(web::Json)
    .map_err(request_error)
}

async fn delete_thread(
    service: Service,
    path: web::Path<ThreadPath>,
) -> Result<web::Json<DeleteThreadResult>, Error> {
    crate::managed_agents::service::delete_thread(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn fork_thread(
    service: Service,
    path: web::Path<ThreadPath>,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<web::Json<ThreadResult>, Error> {
    crate::managed_agents::service::fork_thread(
        &service.shared(),
        &path,
        optional_json_body::<ForkThreadBody>(&request, &body)?,
    )
    .await
    .map(web::Json)
    .map_err(request_error)
}

async fn submit_turn(
    service: Service,
    path: web::Path<ThreadPath>,
    body: web::Json<SubmitTurnBody>,
) -> Result<HttpResponse, Error> {
    let body = body.into_inner();
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let crate::managed_agents::service::PreparedTurn {
        request,
        config,
        harness,
    } = crate::managed_agents::service::prepare_turn(
        &service.runtime,
        agent.as_ref(),
        thread.as_ref(),
        body,
    )
    .await
    .map_err(request_error)?;
    let runtime = service.runtime.clone();
    let progress = service.progress.clone();
    let (receipt, received) = oneshot::channel();
    tokio::spawn(async move {
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

async fn turn_status(
    service: Service,
    path: web::Path<TurnPath>,
) -> Result<web::Json<TurnStatusResult>, Error> {
    crate::managed_agents::service::turn_status(&service.shared(), &path)
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn cancel_turn(
    service: Service,
    path: web::Path<TurnPath>,
) -> Result<web::Json<CancelTurnResult>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let runtime = if service.auth.is_some() {
        if let Some(principal) = crate::permissions::turn_caller(thread.as_ref(), path.turn_id)
            .await
            .map_err(ErrorInternalServerError)?
        {
            service
                .caller_runtime(principal)
                .map_err(ErrorInternalServerError)?
        } else {
            service.runtime.clone()
        }
    } else {
        service.runtime.clone()
    };
    let canceled_active_turn = runtime
        .cancel_turn(HarnessTurnKey::new(path.thread_id, path.turn_id))
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(CancelTurnResult {
        canceled_active_turn,
        finished_event_id: None,
    }))
}

async fn approval_response(
    service: Service,
    path: web::Path<TurnPath>,
    body: web::Json<ApprovalResponseBody>,
) -> Result<web::Json<EventResult>, Error> {
    crate::managed_agents::service::approval_response(&service.shared(), &path, body.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn unsupported_interaction(_service: Service) -> Result<HttpResponse, Error> {
    Err(ErrorNotImplemented(
        "this runtime does not execute frontend tools",
    ))
}

async fn events(
    service: Service,
    path: web::Path<ThreadPath>,
    query: web::Query<EventsQuery>,
) -> Result<web::Json<exoharness::GetEventsResult>, Error> {
    crate::managed_agents::service::events(&service.shared(), &path, query.into_inner())
        .await
        .map(web::Json)
        .map_err(request_error)
}

async fn watch(
    service: Service,
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
    let mut stream: EventStream = Box::pin(futures::stream::select(
        durable,
        progress_stream(receiver, thread_id),
    ));
    if let Some(auth) = service.auth.clone() {
        let session = service
            .session
            .clone()
            .ok_or_else(|| ErrorUnauthorized("session missing"))?;
        stream = Box::pin(futures::stream::unfold(
            (
                stream,
                auth,
                session,
                tokio::time::interval(std::time::Duration::from_secs(1)),
            ),
            |(mut events, auth, session, mut timer)| async move {
                loop {
                    tokio::select! {
                        event = events.next() => return event.map(|event| (event, (events, auth, session, timer))),
                        _ = timer.tick() => if auth.session_principal(&session).await.is_err() { return None; },
                    }
                }
            },
        ));
    }
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

fn request_error(error: anyhow::Error) -> Error {
    let status =
        actix_web::http::StatusCode::from_u16(crate::managed_agents::service::error_status(&error))
            .expect("valid request status");
    actix_web::error::InternalError::new(error, status).into()
}
