//! The MCP SDK owns protocol I/O; Rust owns credentials and the tool inventory.
use crate::host::{Host, HostRequest};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use executor::runtime_host::RuntimeHost;
use exo_mcp::{McpConnection, McpConnector, McpCredentialProvider, McpServerConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use wasm_bindgen::JsValue;

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Client(#[serde(with = "serde_wasm_bindgen::preserve")] JsValue);
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Command {
    Connect {
        id: String,
        url: String,
    },
    List {
        client: Client,
    },
    Call {
        client: Client,
        name: String,
        arguments: Map<String, Value>,
    },
    Close {
        client: Client,
    },
}
#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct HttpRequest {
    pub id: String,
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}
#[derive(Deserialize, Serialize)]
pub(crate) struct HttpResponse {
    pub status: u16,
    #[serde(with = "serde_wasm_bindgen::preserve")]
    pub response: JsValue,
}
struct Credentials {
    server: McpServerConfig,
    provider: Arc<dyn McpCredentialProvider>,
}
pub(crate) struct WorkerMcp {
    host: Arc<Host>,
    credentials: Arc<Mutex<HashMap<String, Arc<Credentials>>>>,
}
impl WorkerMcp {
    pub fn new(host: Arc<Host>) -> Self {
        Self {
            host,
            credentials: Arc::default(),
        }
    }
    pub async fn fetch(&self, request: HttpRequest) -> Result<HttpResponse> {
        let credentials = self
            .credentials
            .lock()
            .expect("MCP credentials poisoned")
            .get(&request.id)
            .cloned()
            .context("MCP connection is closed")?;
        ensure!(
            url::Url::parse(&request.url)? == url::Url::parse(&credentials.server.url)?,
            "MCP transport cannot send credentials to another endpoint"
        );
        let (response, supplied) = exo_mcp::send_authenticated(
            credentials.provider.as_ref(),
            &credentials.server,
            |token| {
                self.host.call::<HttpResponse>(HostRequest::McpHttp {
                    request: request.clone(),
                    token,
                })
            },
            |response| matches!(response, Ok(response) if response.status == 401),
        )
        .await?;
        let response = response?;
        ensure!(
            response.status != 401,
            "{}",
            credentials
                .provider
                .authentication_failure_context(&credentials.server, supplied)
        );
        Ok(response)
    }
}
// Own the registration before SDK initialization: cancellation must remove it
// even if the SDK has not returned a connection yet.
struct Registration {
    id: String,
    credentials: Arc<Mutex<HashMap<String, Arc<Credentials>>>>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.credentials
            .lock()
            .expect("MCP credentials poisoned")
            .remove(&self.id);
    }
}
struct Connection {
    host: Arc<Host>,
    client: Client,
    registration: Arc<Registration>,
    closed: std::sync::atomic::AtomicBool,
}
impl Drop for Connection {
    fn drop(&mut self) {
        if !self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let host = self.host.clone();
            let client = self.client.clone();
            let registration = self.registration.clone();
            self.host.spawn(Box::pin(async move {
                if let Err(error) = host
                    .call::<()>(HostRequest::Mcp {
                        command: Command::Close { client },
                    })
                    .await
                {
                    tracing::warn!(%error, "failed to close MCP connection");
                }
                drop(registration);
            }));
        }
    }
}
#[async_trait]
impl McpConnector for WorkerMcp {
    async fn connect(
        &self,
        server: &McpServerConfig,
        provider: Arc<dyn McpCredentialProvider>,
    ) -> Result<Arc<dyn McpConnection>> {
        let id = exoharness::Uuid7::now().to_string();
        self.credentials
            .lock()
            .expect("MCP credentials poisoned")
            .insert(
                id.clone(),
                Arc::new(Credentials {
                    server: server.clone(),
                    provider,
                }),
            );
        let registration = Arc::new(Registration {
            id: id.clone(),
            credentials: self.credentials.clone(),
        });
        let client = self
            .host
            .call(HostRequest::Mcp {
                command: Command::Connect {
                    id: id.clone(),
                    url: server.url.clone(),
                },
            })
            .await;
        Ok(Arc::new(Connection {
            host: self.host.clone(),
            client: client?,
            registration,
            closed: false.into(),
        }))
    }
}
#[async_trait]
impl McpConnection for Connection {
    async fn list_tools(&self) -> Result<Vec<exo_mcp::Tool>> {
        self.host
            .call(HostRequest::Mcp {
                command: Command::List {
                    client: self.client.clone(),
                },
            })
            .await
    }
    async fn call(
        &self,
        name: &str,
        arguments: Map<String, Value>,
    ) -> Result<exo_mcp::CallToolResult> {
        self.host
            .call(HostRequest::Mcp {
                command: Command::Call {
                    client: self.client.clone(),
                    name: name.into(),
                    arguments,
                },
            })
            .await
    }
    async fn close(&self) -> Result<()> {
        if !self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let result = self
                .host
                .call::<()>(HostRequest::Mcp {
                    command: Command::Close {
                        client: self.client.clone(),
                    },
                })
                .await;
            self.registration
                .credentials
                .lock()
                .expect("MCP credentials poisoned")
                .remove(&self.registration.id);
            result?;
        }
        Ok(())
    }
}
