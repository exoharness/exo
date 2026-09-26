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
use anyhow::{Context, Result, bail};
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

#[derive(Clone)]
pub struct RuntimeHttpService {
    runtime: Arc<Runtime>,
    agent_id: Option<AgentId>,
    authorization: Option<HeaderValue>,
    progress: broadcast::Sender<Event>,
    definition_updates: Arc<tokio::sync::Mutex<()>>,
    auth: Option<Arc<crate::remote::AuthServer>>,
    multiplayer: bool,
    callers: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<Runtime>>>>,
    account_id: String,
    session: Option<String>,
}

impl RuntimeHttpService {
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
            authorization: token
                .map(|token| HeaderValue::from_str(&format!("Bearer {token}")))
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

    fn require_full_provider(&self) -> Result<(), Error> {
        if self.agent_id.is_some() {
            return Err(actix_web::error::ErrorForbidden(
                "this service serves one saved agent",
            ));
        }
        Ok(())
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
    Ok(HttpServer::new(move || {
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
    .listen(listener)?
    .run())
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
    if let Some(authorization) = &service.authorization
        && req.headers().get(AUTHORIZATION) != Some(authorization)
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
    service: Service,
    query: web::Query<AgentsQuery>,
) -> Result<web::Json<ListAgentsResult>, Error> {
    let mut agents = service
        .runtime
        .list_agents()
        .await
        .map_err(ErrorInternalServerError)?;
    agents.retain(|agent| service.agent_id.is_none_or(|id| agent.id == id));
    if let Some(slug) = &query.slug {
        agents.retain(|agent| &agent.slug == slug);
    }
    Ok(web::Json(ListAgentsResult { agents }))
}

async fn identity(service: Service) -> web::Json<ProviderIdentity> {
    web::Json(ProviderIdentity {
        account_id: service.account_id.clone(),
    })
}

#[derive(Deserialize)]
struct VaultPath {
    agent_id: Option<AgentId>,
    thread_id: Option<ThreadId>,
    vault_id: Option<exoharness::vault::VaultId>,
    secret_id: Option<exoharness::SecretId>,
}

async fn scoped_vaults(
    service: &RuntimeHttpService,
    path: &VaultPath,
) -> Result<Vec<Arc<dyn exoharness::vault::VaultHandle>>, Error> {
    if let Some(id) = path.agent_id.or(service.agent_id) {
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

async fn list_environments(
    service: Service,
) -> Result<web::Json<Vec<exoharness::EnvironmentDefinition>>, Error> {
    Ok(web::Json(
        service
            .runtime
            .exoharness_handle()
            .list_environments()
            .await
            .map_err(ErrorBadRequest)?,
    ))
}
async fn put_environment(
    service: Service,
    body: web::Json<exoharness::EnvironmentDefinition>,
) -> Result<web::Json<bool>, Error> {
    service.require_full_provider()?;
    service
        .runtime
        .exoharness_handle()
        .put_environment(body.into_inner())
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(true))
}
async fn delete_environment(
    service: Service,
    name: web::Path<String>,
) -> Result<web::Json<bool>, Error> {
    service.require_full_provider()?;
    Ok(web::Json(
        service
            .runtime
            .exoharness_handle()
            .delete_environment(&name)
            .await
            .map_err(ErrorBadRequest)?,
    ))
}

async fn list_vaults(
    service: Service,
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

async fn create_vault(
    service: Service,
    body: web::Json<CreateVaultBody>,
) -> Result<web::Json<exoharness::vault::VaultRecord>, Error> {
    service.require_full_provider()?;
    let vault = service
        .runtime
        .exoharness_handle()
        .create_vault(&body.name)
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(vault.record().clone()))
}
async fn delete_vault(
    service: Service,
    path: web::Path<VaultPath>,
) -> Result<web::Json<bool>, Error> {
    service.require_full_provider()?;
    service
        .runtime
        .exoharness_handle()
        .delete_vault(
            &path
                .vault_id
                .ok_or_else(|| ErrorBadRequest("vault ID missing"))?,
        )
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(true))
}
async fn writable_vault(
    service: &RuntimeHttpService,
    path: &VaultPath,
) -> Result<Arc<dyn exoharness::vault::VaultHandle>, Error> {
    service.require_full_provider()?;
    scoped_vaults(service, path)
        .await?
        .into_iter()
        .find(|v| Some(v.record().id) == path.vault_id)
        .ok_or_else(|| ErrorNotFound("vault is unavailable"))
}
async fn put_secret(
    service: Service,
    path: web::Path<VaultPath>,
    body: web::Json<exoharness::PutSecretRequest>,
) -> Result<web::Json<exoharness::SecretId>, Error> {
    let vault = writable_vault(&service, &path).await?;
    Ok(web::Json(
        vault
            .put_secret(body.into_inner())
            .await
            .map_err(ErrorBadRequest)?,
    ))
}
async fn update_secret(
    service: Service,
    path: web::Path<VaultPath>,
    body: web::Json<exoharness::UpdateSecretRequest>,
) -> Result<web::Json<exoharness::SecretMetadata>, Error> {
    let vault = writable_vault(&service, &path).await?;
    Ok(web::Json(
        vault
            .update_secret(
                &path
                    .secret_id
                    .ok_or_else(|| ErrorBadRequest("secret ID missing"))?,
                body.into_inner(),
            )
            .await
            .map_err(ErrorBadRequest)?,
    ))
}
async fn delete_secret(
    service: Service,
    path: web::Path<VaultPath>,
) -> Result<web::Json<bool>, Error> {
    let vault = writable_vault(&service, &path).await?;
    vault
        .delete_secret(
            &path
                .secret_id
                .ok_or_else(|| ErrorBadRequest("secret ID missing"))?,
        )
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(true))
}

async fn list_secrets(
    service: Service,
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
    service: Service,
    body: web::Json<exoharness::NewAgentRequest>,
) -> Result<web::Json<exoharness::AgentRecord>, Error> {
    service.require_full_provider()?;
    let agent = service
        .runtime
        .exoharness_handle()
        .new_agent(body.into_inner())
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(agent.record().clone()))
}

async fn get_agent(
    service: Service,
    path: web::Path<AgentPath>,
) -> Result<web::Json<Option<exoharness::AgentRecord>>, Error> {
    if service
        .agent_id
        .is_some_and(|agent_id| agent_id != path.agent_id)
    {
        return Err(ErrorNotFound("agent not found"));
    }
    Ok(web::Json(
        service
            .runtime
            .exoharness_handle()
            .get_agent(&path.agent_id)
            .await
            .map_err(ErrorBadRequest)?
            .map(|agent| agent.record().clone()),
    ))
}

async fn delete_agent(
    service: Service,
    path: web::Path<AgentPath>,
) -> Result<web::Json<bool>, Error> {
    service.require_full_provider()?;
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
    service: Service,
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
    service: Service,
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
    service: Service,
    path: web::Path<ThreadPath>,
) -> Result<web::Json<Vec<exoharness::ArtifactVersion>>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    Ok(web::Json(
        thread.list_artifacts().await.map_err(ErrorBadRequest)?,
    ))
}

async fn read_thread_artifact(
    service: Service,
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
    service: Service,
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
    Ok(web::Json(
        service
            .runtime
            .update_managed_agent(&agent, &definition)
            .await
            .map_err(ErrorBadRequest)?,
    ))
}

async fn get_thread(
    service: Service,
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
    service: Service,
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
    service: Service,
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
        .runtime
        .get_agent_config(agent.as_ref())
        .await
        .map_err(ErrorBadRequest)?;
    check_harness(agent.as_ref(), &config, body.harness.as_deref()).await?;
    let thread = service
        .runtime
        .open_managed_thread(
            &agent,
            None,
            exoharness::NewThreadRequest {
                environment: body.environment,
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

async fn update_thread_environment(
    service: Service,
    path: web::Path<ThreadPath>,
    body: web::Json<exoharness::EnvironmentDefinition>,
) -> Result<web::Json<ThreadResult>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let opened = service
        .runtime
        .open_managed_thread(
            &agent,
            Some(&path.thread_id.to_string()),
            exoharness::NewThreadRequest {
                environment: Some(body.into_inner()),
                ..Default::default()
            },
        )
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(ThreadResult {
        agent: agent.record().clone(),
        thread: opened.thread.record().clone(),
    }))
}

async fn attach_thread_vaults(
    service: Service,
    path: web::Path<ThreadPath>,
    body: web::Json<AttachThreadVaultsBody>,
) -> Result<web::Json<ThreadResult>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service
        .thread(agent.as_ref(), path.thread_id)
        .await?
        .attach_vaults(body.into_inner().vaults)
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(ThreadResult {
        agent: agent.record().clone(),
        thread: thread.record().clone(),
    }))
}

async fn delete_thread(
    service: Service,
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
    service: Service,
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
    service: Service,
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
        .runtime
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
    let runtime = service.runtime.clone();
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

async fn turn_status(
    service: Service,
    path: web::Path<TurnPath>,
) -> Result<web::Json<TurnStatusResult>, Error> {
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let active = service
        .runtime
        .is_turn_active(thread.as_ref(), path.turn_id)
        .await
        .map_err(ErrorInternalServerError)?;
    Ok(web::Json(TurnStatusResult { active }))
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
    let agent = service.agent(path.agent_id).await?;
    service.thread(agent.as_ref(), path.thread_id).await?;
    let event_id = service
        .runtime
        .approval_response(path.agent_id, path.thread_id, path.turn_id, &body)
        .await
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(EventResult { event_id }))
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
    let agent = service.agent(path.agent_id).await?;
    let thread = service.thread(agent.as_ref(), path.thread_id).await?;
    let query = query.into_inner();
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
        .map_err(ErrorBadRequest)?;
    Ok(web::Json(result))
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
