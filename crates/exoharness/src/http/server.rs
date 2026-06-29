use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Instant;

use actix_web::{App, HttpResponse, HttpServer, Responder, web};

use super::{HTTP_EXOHARNESS_REQUEST_PATH, HTTP_EXOHARNESS_TRACING_TARGET};
use crate::protocol::{ClientMessage, Response, ServerMessage};
use crate::server::ExoHarnessServer;
use crate::{ExoHarness, Result};

#[derive(Debug, Clone, Default)]
pub struct ExoHarnessHttpServeOptions {
    pub verbosity: u8,
    /// When set, every `/request` call must present `Authorization: Bearer
    /// <token>`. Required by `exo serve` whenever it binds a non-loopback
    /// address, so the kernel is never reachable unauthenticated off-host.
    pub bearer_token: Option<String>,
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
    req: actix_web::HttpRequest,
    state: web::Data<Arc<HttpServerState>>,
    body: web::Bytes,
) -> impl Responder {
    // Authenticate before doing any work, including body parsing, so an
    // unauthenticated caller cannot reach the deserializer.
    if let Some(expected) = &state.options.bearer_token {
        let provided = req
            .headers()
            .get(actix_web::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if provided != Some(format!("Bearer {expected}").as_str()) {
            return HttpResponse::Unauthorized().body("unauthorized\n");
        }
    }
    let message: ClientMessage = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(error) => {
            return HttpResponse::BadRequest().body(format!("invalid request: {error}\n"));
        }
    };
    let ClientMessage::Request { id, request } = message;
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
