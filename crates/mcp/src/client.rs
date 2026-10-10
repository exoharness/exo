use crate::auth;

use crate::{McpServerConfig, validate_servers};

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    service::{RoleClient, RunningService},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

#[derive(Default)]
pub struct McpCredentials(HashMap<String, String>);

impl From<HashMap<String, String>> for McpCredentials {
    fn from(tokens: HashMap<String, String>) -> Self {
        Self(tokens)
    }
}

pub struct McpCredential {
    pub version: String,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct McpAuthenticationError {
    pub server_name: String,
    pub credential_supplied: bool,
    pub(crate) details: String,
}

impl std::fmt::Display for McpAuthenticationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.details)
    }
}

impl std::error::Error for McpAuthenticationError {}

#[async_trait::async_trait]
pub trait McpCredentialProvider: Send + Sync {
    async fn resolve(&self, server: &McpServerConfig) -> Result<Option<McpCredential>>;

    async fn refresh(
        &self,
        _server: &McpServerConfig,
        _rejected: &McpCredential,
    ) -> Result<Option<McpCredential>> {
        Ok(None)
    }

    fn authentication_failure_context(&self, server: &McpServerConfig, supplied: bool) -> String {
        let reason = if supplied {
            "the server rejected the supplied credential; check its validity and permissions"
        } else {
            "the server requires authentication, but no credential was selected"
        };
        format!(
            "MCP authentication failed for {} ({}): {reason}",
            server.name, server.url
        )
    }
}

#[async_trait::async_trait]
impl McpCredentialProvider for McpCredentials {
    async fn resolve(&self, server: &McpServerConfig) -> Result<Option<McpCredential>> {
        Ok(self.0.get(&server.name).map(|token| McpCredential {
            version: String::new(),
            token: token.clone(),
        }))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<rmcp::model::ToolAnnotations>,
}

struct Connection {
    server: McpServerConfig,
    disabled_tools: Vec<String>,
    service: tokio::sync::RwLock<Option<RunningService<RoleClient, ()>>>,
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

pub fn tool_result_is_error(result: &Value) -> bool {
    #[derive(Deserialize)]
    struct ToolResult {
        #[serde(default, rename = "isError")]
        is_error: bool,
    }
    ToolResult::deserialize(result).is_ok_and(|result| result.is_error)
}

#[derive(Default)]
pub struct McpToolSet {
    connections: Vec<Connection>,
    tools: Vec<McpTool>,
}

impl McpToolSet {
    pub async fn connect(servers: &[McpServerConfig], credentials: McpCredentials) -> Result<Self> {
        Self::connect_with_provider(servers, Arc::new(credentials)).await
    }

    pub async fn connect_with_provider(
        servers: &[McpServerConfig],
        credentials: Arc<dyn McpCredentialProvider>,
    ) -> Result<Self> {
        Self::connect_with_http_clients(servers, credentials, &vec![http_client()?; servers.len()])
            .await
    }

    /// Connect using a caller-supplied HTTP client for each server, in the same order.
    /// Hosts can enforce destination restrictions, DNS pinning and redirect policies.
    pub async fn connect_with_http_clients(
        servers: &[McpServerConfig],
        credentials: Arc<dyn McpCredentialProvider>,
        clients: &[reqwest::Client],
    ) -> Result<Self> {
        validate_servers(servers)?;
        ensure!(
            servers.len() == clients.len(),
            "each MCP server requires an HTTP client"
        );
        let connections = servers.iter().zip(clients);
        let connected = futures::future::try_join_all(connections.map(|(server, client)| async {
            let connected = async {
                let service = connect_service(server, credentials.clone(), client.clone()).await?;
                let tools = tokio::time::timeout(Duration::from_secs(60), service.list_all_tools())
                    .await
                    .context("MCP tool discovery timed out")?
                    .map_err(auth::authentication_context)?;
                server.validate_tool_names(tools.iter().map(|tool| tool.name.as_ref()))?;
                let disabled_tools = tools
                    .iter()
                    .filter(|tool| !server.allows_tool(&tool.name))
                    .map(|tool| tool.name.to_string())
                    .collect();
                let tools = tools
                    .into_iter()
                    .filter(|tool| server.allows_tool(&tool.name))
                    .map(|tool| McpTool {
                        name: exposed_name(&server.name, &tool.name),
                        server_name: server.name.clone(),
                        tool_name: tool.name.into_owned(),
                        description: tool.description.map(|d| d.into_owned()).unwrap_or_default(),
                        parameters: Value::Object((*tool.input_schema).clone()),
                        output_schema: tool
                            .output_schema
                            .map(|schema| Value::Object((*schema).clone())),
                        annotations: tool.annotations,
                    })
                    .collect::<Vec<_>>();
                Ok::<_, anyhow::Error>((
                    Connection {
                        server: server.clone(),
                        disabled_tools,
                        service: tokio::sync::RwLock::new(Some(service)),
                    },
                    tools,
                ))
            }
            .await;
            connected.with_context(|| format!("connecting MCP server {}", server.name))
        }))
        .await?;
        let mut set = Self::default();
        let mut names = HashSet::new();
        for (connection, tools) in connected {
            for tool in &tools {
                if !names.insert(tool.name.clone()) {
                    bail!("duplicate exposed MCP tool: {}", tool.name);
                }
            }
            set.connections.push(connection);
            set.tools.extend(tools);
        }
        Ok(set)
    }

    pub async fn close(&self) -> Result<()> {
        let results = futures::future::join_all(self.connections.iter().map(|connection| async {
            if let Some(mut service) = connection.service.write().await.take() {
                service
                    .close_with_timeout(Duration::from_secs(5))
                    .await
                    .with_context(|| format!("closing MCP server {}", connection.server.name))?;
            }
            Ok::<_, anyhow::Error>(())
        }))
        .await;
        for result in results {
            result?;
        }
        Ok(())
    }

    pub fn servers(&self) -> impl Iterator<Item = (&McpServerConfig, &[String])> {
        self.connections
            .iter()
            .map(|connection| (&connection.server, connection.disabled_tools.as_slice()))
    }

    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool.name == name)
    }

    pub async fn call(
        &self,
        name: &str,
        arguments: Map<String, Value>,
    ) -> Result<rmcp::model::CallToolResult> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .with_context(|| format!("MCP tool is not available: {name}"))?;
        let connection = self
            .connections
            .iter()
            .find(|connection| connection.server.name == tool.server_name)
            .context("MCP connection is missing")?;
        let current = connection.service.read().await;
        let service = current.as_ref().context("MCP connection is closed")?;
        tokio::time::timeout(
            Duration::from_secs(300),
            service.call_tool(
                CallToolRequestParams::new(tool.tool_name.clone()).with_arguments(arguments),
            ),
        )
        .await
        .context("MCP tool call timed out")?
        .map_err(auth::authentication_context)
        .with_context(|| format!("MCP tool {}.{} failed", tool.server_name, tool.tool_name))
    }
}

fn exposed_name(server: &str, tool: &str) -> String {
    let full = format!("exo_mcp__{server}__{tool}");
    if full.len() <= 64
        && full
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        && !server.contains("__")
    {
        return full;
    }
    let readable: String = full
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(39)
        .collect();
    let hash = Sha256::digest(format!("{server}\0{tool}").as_bytes());
    format!("{readable}__{:x}", hash)[..64].to_string()
}
