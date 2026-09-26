use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use exo_mcp::McpServerConfig;
use serde::Deserialize;

use crate::AgentDefinition;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpServerDefinition {
    Url {
        name: String,
        url: String,
        #[serde(default)]
        allowed_tools: Option<Vec<String>>,
        #[serde(default)]
        blocked_tools: Vec<String>,
    },
    Provider {
        name: String,
        #[serde(default)]
        allowed_tools: Option<Vec<String>>,
        #[serde(default)]
        blocked_tools: Vec<String>,
    },
}

impl McpServerDefinition {
    fn name(&self) -> &str {
        match self {
            Self::Url { name, .. } | Self::Provider { name, .. } => name,
        }
    }

    fn with_url(&self, url: String) -> McpServerConfig {
        let (name, allowed_tools, blocked_tools) = match self {
            Self::Url {
                name,
                allowed_tools,
                blocked_tools,
                ..
            }
            | Self::Provider {
                name,
                allowed_tools,
                blocked_tools,
                ..
            } => (name, allowed_tools, blocked_tools),
        };
        McpServerConfig {
            name: name.clone(),
            url,
            allowed_tools: allowed_tools.clone(),
            blocked_tools: blocked_tools.clone(),
        }
    }
}

pub(crate) fn validate_servers(servers: &[McpServerDefinition]) -> Result<()> {
    exo_mcp::validate_server_names(servers.iter().map(McpServerDefinition::name))?;
    for server in servers {
        if let McpServerDefinition::Url { url, .. } = server {
            exo_mcp::validate_servers(&[server.with_url(url.clone())])?;
        }
    }
    Ok(())
}

#[async_trait]
pub trait McpServerResolver: Send + Sync {
    async fn resolve(&self, _name: &str) -> Result<String> {
        bail!("this provider does not supply a built-in MCP server")
    }
}

impl McpServerResolver for () {}

impl AgentDefinition {
    pub async fn resolve_mcp_servers(
        &self,
        resolver: &dyn McpServerResolver,
    ) -> Result<Vec<McpServerConfig>> {
        let servers = futures::future::try_join_all(self.frontmatter.mcp_servers.iter().map(
            |server| async move {
                let url =
                    match server {
                        McpServerDefinition::Url { url, .. } => url.clone(),
                        McpServerDefinition::Provider { .. } => resolver
                            .resolve(server.name())
                            .await
                            .with_context(|| format!("resolving MCP server {}", server.name()))?,
                    };
                Ok::<_, anyhow::Error>(server.with_url(url))
            },
        ))
        .await?;
        exo_mcp::validate_servers(&servers)?;
        Ok(servers)
    }
}
