use crate::{
    McpConnection, McpConnector, McpCredentialProvider, McpCredentials, McpServerConfig, auth,
    validate_servers,
};
use anyhow::{Context, Result};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, Tool},
    service::{RoleClient, RunningService},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Map, Value};
use std::{sync::Arc, time::Duration};

pub struct NativeMcpConnector;
pub(crate) struct HttpMcpConnector<'a> {
    pub(crate) servers: &'a [McpServerConfig],
    pub(crate) clients: &'a [reqwest::Client],
}
struct NativeConnection(tokio::sync::RwLock<Option<RunningService<RoleClient, ()>>>);
#[async_trait::async_trait]
impl McpConnector for NativeMcpConnector {
    async fn connect(
        &self,
        server: &McpServerConfig,
        credentials: Arc<dyn McpCredentialProvider>,
    ) -> Result<Arc<dyn McpConnection>> {
        connect(server, credentials, http_client()?).await
    }
}
#[async_trait::async_trait]
impl McpConnector for HttpMcpConnector<'_> {
    async fn connect(
        &self,
        server: &McpServerConfig,
        credentials: Arc<dyn McpCredentialProvider>,
    ) -> Result<Arc<dyn McpConnection>> {
        let index = self
            .servers
            .iter()
            .position(|configured| configured.name == server.name)
            .context("MCP HTTP client is missing")?;
        connect(server, credentials, self.clients[index].clone()).await
    }
}
async fn connect(
    server: &McpServerConfig,
    credentials: Arc<dyn McpCredentialProvider>,
    client: reqwest::Client,
) -> Result<Arc<dyn McpConnection>> {
    Ok(Arc::new(NativeConnection(tokio::sync::RwLock::new(Some(
        connect_service(server, credentials, client).await?,
    )))))
}
#[async_trait::async_trait]
impl McpConnection for NativeConnection {
    async fn list_tools(&self) -> Result<Vec<Tool>> {
        let current = self.0.read().await;
        let service = current.as_ref().context("MCP connection is closed")?;
        tokio::time::timeout(Duration::from_secs(60), service.list_all_tools())
            .await
            .context("MCP tool discovery timed out")?
            .map_err(auth::authentication_context)
    }
    async fn call(&self, name: &str, arguments: Map<String, Value>) -> Result<CallToolResult> {
        let current = self.0.read().await;
        let service = current.as_ref().context("MCP connection is closed")?;
        tokio::time::timeout(
            Duration::from_secs(300),
            service
                .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments)),
        )
        .await
        .context("MCP tool call timed out")?
        .map_err(auth::authentication_context)
    }
    async fn close(&self) -> Result<()> {
        if let Some(mut service) = self.0.write().await.take() {
            service.close_with_timeout(Duration::from_secs(5)).await?;
        }
        Ok(())
    }
}

async fn connect_service(
    server: &McpServerConfig,
    credentials: Arc<dyn McpCredentialProvider>,
    client: reqwest::Client,
) -> Result<RunningService<RoleClient, ()>> {
    let config = StreamableHttpClientTransportConfig::with_uri(server.url.clone());
    let client = auth::CredentialClient::new(client, server.clone(), credentials);
    let transport = StreamableHttpClientTransport::with_client(client, config);
    tokio::time::timeout(Duration::from_secs(60), ().serve(transport))
        .await
        .context("MCP initialization timed out")?
        .map_err(auth::authentication_context)
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .build()?)
}

pub async fn probe_auth_challenge(url: &str) -> Result<Option<String>> {
    let server = McpServerConfig {
        name: "authorization".into(),
        url: url.into(),
        allowed_tools: None,
        blocked_tools: Vec::new(),
    };
    validate_servers(std::slice::from_ref(&server))?;
    match connect_service(&server, Arc::new(McpCredentials::default()), http_client()?).await {
        Ok(mut service) => {
            service.close_with_timeout(Duration::from_secs(5)).await?;
            Ok(None)
        }
        Err(error) => match auth::auth_challenge(&error) {
            Some(challenge) => Ok(Some(challenge.to_owned())),
            None => Err(error),
        },
    }
}
