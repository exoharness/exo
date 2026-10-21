use std::{net::TcpListener, sync::Arc};

use crate::managed_agents::{http as dispatch, service as api};
use actix_web::{
    App, Error, HttpMessage, HttpRequest, HttpResponse, HttpServer,
    body::MessageBody,
    dev::{Server, ServiceRequest, ServiceResponse},
    error::{
        ErrorBadRequest, ErrorInternalServerError, ErrorNotFound, ErrorPayloadTooLarge,
        ErrorUnauthorized,
    },
    http::header::{AUTHORIZATION, HeaderValue},
    middleware::{Next, from_fn},
    mime, web,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use exo_managed_agents::http::{RUNTIME_PATH, protocol::*, sse};
use exoharness::{AgentId, Event, ThreadHandle};
use futures::StreamExt;
use tokio::sync::broadcast;

use crate::Runtime;

// Thread creation can include a large environment definition. Bytes buffers the request.
const OPTIONAL_JSON_BODY_LIMIT: usize = 256 * 1024 * 1024;

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
    adapters: Option<Arc<crate::managed_agents::adapters::AdapterService>>,
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
            adapters: None,
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

    pub fn with_adapters(
        mut self,
        adapters: Arc<crate::managed_agents::adapters::AdapterService>,
    ) -> Self {
        self.adapters = Some(adapters);
        self
    }

    pub fn spawn_recovery(&self) -> Result<()> {
        // Incoming turns can wake older queue entries before discovery finishes.
        if self.auth.is_some() || self.adapters.is_some() {
            let service = self.clone();
            self.runtime
                .set_recovery_resolver(Arc::new(move |principal: String| {
                    let service = service.clone();
                    Box::pin(async move {
                        if principal.starts_with("adapter-") {
                            let adapters = service
                                .adapters
                                .clone()
                                .context("adapter recovery is unavailable")?;
                            adapters.recover_caller(&principal).await
                        } else {
                            service.caller_runtime(principal)
                        }
                    })
                }))?;
        }
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(error) = service.runtime.recover_unfinished_turns().await {
                tracing::error!(%error, "failed to recover unfinished turns");
            }
        });
        Ok(())
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
    // The host coordinates signals with adapter and runtime shutdown.
    .disable_signals()
    .shutdown_timeout(5)
    .listen(listener)?;
    recovery_service
        .spawn_recovery()
        .map_err(std::io::Error::other)?;
    Ok(server.run())
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    // These exact callback routes authenticate using Slack signatures. The
    // remaining API routes keep their normal Exo authentication.
    cfg.service(
        web::resource(format!("{RUNTIME_PATH}/adapter/{{name}}"))
            .app_data(web::PayloadConfig::new(16 * 1024 * 1024))
            .route(web::post().to(slack_webhook)),
    );
    cfg.service(
        web::resource(format!("{RUNTIME_PATH}/agent/{{agent}}/adapter/{{name}}"))
            .app_data(web::PayloadConfig::new(16 * 1024 * 1024))
            .route(web::post().to(slack_webhook)),
    );
    cfg.service(
        web::scope(RUNTIME_PATH)
            .wrap(from_fn(authorize))
            .route("/identity/manage", web::post().to(manage_identity))
            .default_service(web::route().to(handle)),
    );
}

async fn slack_webhook(
    service: web::Data<Arc<RuntimeHttpService>>,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, Error> {
    let adapters = service
        .adapters
        .as_ref()
        .ok_or_else(|| ErrorNotFound("adapters are not configured"))?;
    let content = std::str::from_utf8(&body).map_err(ErrorBadRequest)?;
    let signature = request
        .headers()
        .get("x-slack-signature")
        .and_then(|value| value.to_str().ok());
    let timestamp = request
        .headers()
        .get("x-slack-request-timestamp")
        .and_then(|value| value.to_str().ok());
    let form = request
        .mime_type()?
        .is_some_and(|mime| mime == mime::APPLICATION_WWW_FORM_URLENCODED);
    let challenge = adapters
        .slack_webhook(
            request.match_info().get("agent"),
            request
                .match_info()
                .get("name")
                .ok_or_else(|| ErrorBadRequest("adapter name required"))?,
            content,
            form,
            signature,
            timestamp,
        )
        .await
        .map_err(|error| {
            if error.downcast_ref::<slack_morphism::signature_verifier::SlackEventSignatureVerifierError>().is_some() {
                ErrorUnauthorized("Slack signature verification failed")
            } else if error.downcast_ref::<serde_json::Error>().is_some() || error.downcast_ref::<serde_urlencoded::de::Error>().is_some() {
                ErrorBadRequest("invalid Slack payload")
            } else {
                tracing::warn!(%error, "Slack webhook retained for retry");
                actix_web::error::ErrorServiceUnavailable("Slack adapter is not ready; retry")
            }
        })?;
    #[derive(serde::Serialize)]
    struct Challenge {
        challenge: String,
    }
    Ok(match challenge {
        Some(challenge) => HttpResponse::Ok().json(Challenge { challenge }),
        None => HttpResponse::Ok().finish(),
    })
}

async fn manage_identity(
    service: Service,
    body: web::Json<exo_managed_agents::identity::IdentityCommand>,
) -> Result<HttpResponse, Error> {
    if let Some(caller) = service.runtime.exoharness_handle().caller() {
        caller
            .policy
            .check_operator(&caller.principal)
            .await
            .map_err(ErrorUnauthorized)?;
    }
    let adapters = service
        .adapters
        .as_ref()
        .ok_or_else(|| ErrorNotFound("identity service is unavailable"))?;
    Ok(HttpResponse::Ok().json(
        adapters
            .identities()
            .execute(body.into_inner())
            .await
            .map_err(ErrorBadRequest)?,
    ))
}

async fn authorize(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    let service = req
        .app_data::<web::Data<Arc<RuntimeHttpService>>>()
        .ok_or_else(|| ErrorInternalServerError("runtime service not configured"))?;
    if let Some(token) = &service.bearer_token
        && req
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            != Some(token.as_str())
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

#[async_trait]
impl dispatch::Platform for RuntimeHttpService {
    fn account_id(&self) -> &str {
        &self.account_id
    }
    fn host(&self) -> Arc<dyn crate::runtime_host::RuntimeHost> {
        Arc::new(crate::TokioRuntimeHost)
    }
    async fn previews(
        &self,
        agent: &dyn exoharness::AgentHandle,
        thread: Arc<dyn ThreadHandle>,
    ) -> Result<Option<PreviewEndpoint>> {
        let endpoint = self.runtime.active_preview_endpoint();
        if endpoint.is_some()
            && self
                .runtime
                .register_previews(agent, thread)
                .await?
                .is_none()
        {
            return Ok(None);
        }
        Ok(endpoint)
    }
    async fn cancel(
        &self,
        runtime: &Runtime,
        path: &api::TurnPath,
        thread: Arc<dyn ThreadHandle>,
    ) -> Result<CancelTurnResult> {
        let caller = if self.auth.is_some() {
            crate::permissions::turn_caller(thread.as_ref(), path.turn_id)
                .await?
                .map(|principal| self.caller_runtime(principal))
                .transpose()?
        } else {
            None
        };
        api::cancel_turn(
            caller.as_deref().unwrap_or(runtime),
            path.agent_id,
            path.thread_id,
            path.turn_id,
        )
        .await
    }
    async fn frontend_tool_result(
        &self,
        runtime: &Runtime,
        path: &api::TurnPath,
        body: FrontendToolResultBody,
    ) -> Result<EventResult> {
        Ok(EventResult {
            event_id: runtime
                .frontend_tool_result(path.agent_id, path.thread_id, path.turn_id, &body)
                .await?,
        })
    }
}

async fn handle(
    service: Service,
    request: HttpRequest,
    mut payload: web::Payload,
) -> Result<HttpResponse, Error> {
    // Only thread creation and forks accept large embedded environments.
    let large_body = request.method() == actix_web::http::Method::POST
        && (request.path().ends_with("/thread") || request.path().ends_with("/fork"));
    let limit = if large_body {
        OPTIONAL_JSON_BODY_LIMIT
    } else {
        2 * 1024 * 1024
    };
    let mut body = Vec::new();
    while let Some(chunk) = payload.next().await {
        let chunk = chunk?;
        if chunk.len() > limit - body.len() {
            return Err(ErrorPayloadTooLarge("request body exceeds limit"));
        }
        body.extend_from_slice(&chunk);
    }
    if !body.is_empty()
        && !request
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| {
                h.split(';').next().is_some_and(|h| {
                    h.trim().eq_ignore_ascii_case("application/json")
                        || h.trim().to_ascii_lowercase().ends_with("+json")
                })
            })
    {
        return Ok(HttpResponse::BadRequest().body("Content-Type must be JSON"));
    }
    let relative = request
        .path()
        .strip_prefix(RUNTIME_PATH)
        .ok_or_else(|| ErrorInternalServerError("request outside runtime scope"))?;
    let path = relative
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| {
            percent_encoding::percent_decode_str(part)
                .decode_utf8()
                .map(|s| s.into_owned())
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(actix_web::error::ErrorBadRequest)?;
    let input = dispatch::Request {
        method: request.method().to_string(),
        path,
        query: request.query_string().into(),
        body: String::from_utf8(body).map_err(actix_web::error::ErrorBadRequest)?,
    };
    match dispatch::handle(
        &service.shared(),
        service.0.as_ref(),
        &service.progress,
        &input,
    )
    .await
    {
        dispatch::Response::Error { status, message } => Ok(HttpResponse::build(
            actix_web::http::StatusCode::from_u16(status).expect("valid request status"),
        )
        .body(message)),
        dispatch::Response::Json { status, body } => Ok(HttpResponse::build(
            actix_web::http::StatusCode::from_u16(status).expect("valid request status"),
        )
        .content_type("application/json")
        .body(body)),
        dispatch::Response::Watch(mut stream) => {
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
                    sse::encode(Box::pin(stream))
                        .map(|result| result.map_err(ErrorInternalServerError)),
                ))
        }
    }
}
