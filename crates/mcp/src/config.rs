use std::collections::HashSet;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub url: String,
    pub allowed_tools: Option<Vec<String>>,
    pub blocked_tools: Vec<String>,
}

impl McpServerConfig {
    pub fn validate_tool_names<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let known: HashSet<_> = names.into_iter().collect();
        for name in self
            .allowed_tools
            .iter()
            .flatten()
            .chain(&self.blocked_tools)
        {
            if !known.contains(name.as_str()) {
                bail!("MCP server {} has no tool named {name}", self.name);
            }
        }
        Ok(())
    }

    pub fn allows_tool(&self, name: &str) -> bool {
        self.allowed_tools
            .as_ref()
            .is_none_or(|names| names.iter().any(|allowed| allowed == name))
            && !self.blocked_tools.iter().any(|blocked| blocked == name)
    }
}

pub fn validate_server_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut seen = HashSet::new();
    for name in names {
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            bail!("MCP server names must contain only letters, numbers, underscores, or hyphens");
        }
        if !seen.insert(name) {
            bail!("duplicate MCP server: {name}");
        }
    }
    Ok(())
}

pub fn validate_servers(servers: &[McpServerConfig]) -> Result<()> {
    validate_server_names(servers.iter().map(|server| server.name.as_str()))?;
    for server in servers {
        let url = url::Url::parse(&server.url)
            .with_context(|| format!("invalid URL for MCP server {}", server.name))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            bail!(
                "MCP server {} requires an HTTP(S) URL without embedded credentials or a fragment",
                server.name
            );
        }
    }
    Ok(())
}
