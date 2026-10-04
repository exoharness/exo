use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use url::Url;

use super::HTTP_EXOHARNESS_REQUEST_PATH;
use super::process::HttpProcessTransport;
use crate::protocol::{ClientMessage, Request, Response, ServerMessage};
use crate::{ExoHttpTransport, HttpClient, HttpExoHarness, Result};

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
        Self::from_transports(transport, Arc::new(HttpProcessTransport))
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
