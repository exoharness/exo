use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Instant;

use actix_web::{App, HttpResponse, HttpServer, Responder, web};

use super::{HTTP_EXOHARNESS_REQUEST_PATH, HTTP_EXOHARNESS_TRACING_TARGET};
use crate::protocol::{ClientMessage, Response, ServerMessage};
use crate::server::ExoHarnessServer;
use crate::{ExoHarness, Result};

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
