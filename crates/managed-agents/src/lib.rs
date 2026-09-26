pub mod http;
pub mod mcp;
pub mod permissions;
pub mod vaults;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, ExoHarness, ListThreadsRequest, NewAgentRequest, NewThreadRequest,
    ReadArtifactRequest, SessionId, ThreadHandle, Uuid7, WriteArtifactRequest,
};
use mcp::McpServerDefinition;
use serde::{Deserialize, Serialize};

/// An empty latest artifact means no saved definition, allowing a rejected first
/// upload to be rolled back without deleting artifact history.
pub const AGENT_DEFINITION_PATH: &str = "managed-agents/agent.md";

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentFrontmatter {
    #[serde(default)]
    pub resources: Vec<exoharness::resources::ResourceDefinition>,
    pub name: String,
    pub harness: String,
    pub config: AgentModelConfig,
    pub sandbox: Option<AgentSandboxConfig>,
    #[serde(default)]
    pub permission_policy: permissions::PermissionPolicy,
    #[serde(default)]
    pub tool_policies: std::collections::BTreeMap<String, permissions::PermissionPolicy>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerDefinition>,
    #[serde(default)]
    pub tools: Vec<PathBuf>,
    #[serde(default)]
    pub tool_creation: bool,
    #[serde(default)]
    pub adapters: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModelConfig {
    pub braintrust: Option<BraintrustTracingConfig>,
    pub module: Option<PathBuf>,
    pub model: String,
    pub max_output_tokens: Option<i64>,
    pub max_tool_round_trips: Option<u32>,
}

#[derive(Debug)]
pub struct AgentDefinition {
    pub frontmatter: AgentFrontmatter,
    pub instructions: String,
    source: String,
    path: Option<PathBuf>,
}

impl AgentDefinition {
    pub fn parse(source: String) -> Result<Self> {
        let normalized = source.trim_start_matches('\u{feff}').replace("\r\n", "\n");
        let contents = normalized
            .strip_prefix("---\n")
            .ok_or_else(|| anyhow!("agent file must start with YAML frontmatter (---)"))?;
        let mut offset = 0;
        let mut delimiter = None;
        for line in contents.split_inclusive('\n') {
            if line.trim_end() == "---" {
                delimiter = Some((offset, offset + line.len()));
                break;
            }
            offset += line.len();
        }
        let (yaml_end, body_start) = delimiter.ok_or_else(|| {
            anyhow!("agent file is missing the closing frontmatter delimiter (---)")
        })?;
        let frontmatter: AgentFrontmatter =
            serde_yaml_ng::from_str(&contents[..yaml_end]).context("invalid agent frontmatter")?;
        let instructions = contents[body_start..].trim().to_string();
        for (field, value) in [
            ("name", frontmatter.name.as_str()),
            ("harness", frontmatter.harness.as_str()),
            ("config.model", frontmatter.config.model.as_str()),
            ("instructions", instructions.as_str()),
        ] {
            if value.trim().is_empty() {
                bail!("agent {field} must not be empty");
            }
        }
        mcp::validate_servers(&frontmatter.mcp_servers)?;
        exoharness::resources::validate_resources(&frontmatter.resources)?;
        for (index, name) in frontmatter.adapters.iter().enumerate() {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("adapter names may only contain letters, numbers, '-' and '_'");
            }
            if frontmatter.adapters[..index].contains(name) {
                bail!("duplicate adapter: {name}");
            }
        }
        Ok(Self {
            frontmatter,
            instructions,
            source,
            path: None,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("reading agent file {}", path.display()))?;
        let mut definition =
            Self::parse(source).with_context(|| format!("agent file {}", path.display()))?;
        definition.path = Some(path.to_path_buf());
        Ok(definition)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn system_prompt(&self) -> String {
        format!(
            "You are {}.\n\n{}",
            self.frontmatter.name, self.instructions
        )
    }
}

#[async_trait]
pub trait AgentBackend: Send + Sync {
    fn exoharness(&self) -> Arc<dyn ExoHarness>;

    async fn configure_agent(
        &self,
        _agent: &Arc<dyn AgentHandle>,
        _definition: &AgentDefinition,
    ) -> Result<()> {
        Ok(())
    }

    async fn configure_thread(
        &self,
        _agent: &dyn AgentHandle,
        _thread: &dyn ThreadHandle,
        _created: bool,
    ) -> Result<ThreadInfo> {
        Ok(ThreadInfo::default())
    }

    async fn end_session(&self, _thread: &dyn ThreadHandle, _session_id: SessionId) -> Result<()> {
        Ok(())
    }
}

pub async fn find_agent(harness: &dyn ExoHarness, reference: &str) -> Result<Arc<dyn AgentHandle>> {
    if let Ok(id) = reference.parse::<Uuid7>()
        && let Some(agent) = harness.get_agent(&id).await?
    {
        return Ok(agent);
    }
    harness
        .list_agents()
        .await?
        .into_iter()
        .find(|agent| agent.record().slug == reference)
        .ok_or_else(|| anyhow!("agent not found: {reference}"))
}

pub async fn create_agent(
    backend: &dyn AgentBackend,
    definition: &AgentDefinition,
    slug: &str,
) -> Result<Arc<dyn AgentHandle>> {
    if slug.trim().is_empty() {
        bail!("saved agent name must not be empty");
    }
    let agent = backend
        .exoharness()
        .new_agent(NewAgentRequest {
            vaults: vec![],
            slug: slug.to_string(),
            name: definition.frontmatter.name.clone(),
        })
        .await?;
    let saved: Result<()> = async {
        backend.configure_agent(&agent, definition).await?;
        agent
            .write_artifact(WriteArtifactRequest {
                path: AGENT_DEFINITION_PATH.to_string(),
                contents: definition.source.as_bytes().to_vec(),
            })
            .await?;
        Ok(())
    }
    .await;
    if let Err(error) = &saved {
        backend
            .exoharness()
            .delete_agent(&agent.record().id)
            .await
            .with_context(|| format!("saving agent failed ({error:#}); cleanup also failed"))?;
    }
    saved?;
    Ok(agent)
}

pub async fn load_definition(agent: &dyn AgentHandle) -> Result<Option<AgentDefinition>> {
    let Some(artifact) = agent
        .list_artifacts()
        .await?
        .into_iter()
        .filter(|artifact| artifact.path == AGENT_DEFINITION_PATH)
        .max_by_key(|artifact| artifact.version)
    else {
        return Ok(None);
    };
    let artifact = agent
        .read_artifact(ReadArtifactRequest {
            artifact_id: artifact.artifact_id,
            version: Some(artifact.version),
        })
        .await?
        .context("saved agent definition is missing")?;
    if artifact.contents.is_empty() {
        return Ok(None);
    }
    AgentDefinition::parse(String::from_utf8(artifact.contents)?)
        .map(Some)
        .with_context(|| format!("invalid saved definition for agent {}", agent.record().slug))
}

pub async fn list_threads(agent: &dyn AgentHandle) -> Result<Vec<Arc<dyn ThreadHandle>>> {
    let mut threads = Vec::new();
    let mut cursor = None;
    loop {
        let page = agent
            .list_threads(ListThreadsRequest {
                cursor,
                limit: None,
            })
            .await?;
        threads.extend(page.threads);
        match page.next_cursor {
            Some(next) => {
                anyhow::ensure!(
                    cursor.is_none_or(|cursor| next < cursor),
                    "thread listing cursor did not advance"
                );
                cursor = Some(next);
            }
            None => return Ok(threads),
        }
    }
}

pub async fn find_thread(
    agent: &dyn AgentHandle,
    reference: &str,
) -> Result<Arc<dyn ThreadHandle>> {
    if let Ok(id) = reference.parse::<Uuid7>()
        && let Some(thread) = agent.get_thread(&id).await?
    {
        return Ok(thread);
    }
    list_threads(agent)
        .await?
        .into_iter()
        .find(|thread| thread.record().slug == reference)
        .ok_or_else(|| anyhow!("thread not found: {reference}"))
}

pub struct OpenedThread {
    pub thread: Arc<dyn ThreadHandle>,
    pub created: bool,
    pub info: ThreadInfo,
}

#[derive(Default)]
pub struct ThreadInfo {
    pub model: Option<String>,
    pub mcp_tools: Option<usize>,
}

pub async fn open_thread(
    backend: &dyn AgentBackend,
    agent: &Arc<dyn AgentHandle>,
    reference: Option<&str>,
    new_thread: NewThreadRequest,
) -> Result<OpenedThread> {
    let environment = new_thread.environment.clone();
    let vaults = new_thread.vaults.clone();
    let created = reference.is_none();
    let thread = match reference {
        Some(reference) => find_thread(agent.as_ref(), reference).await?,
        None => agent.new_thread(new_thread).await?,
    };
    if !created && environment.is_some() && thread.record().environment != environment {
        bail!("cannot change the environment of a saved thread; start a new thread");
    }
    if !vaults.is_empty() && thread.record().vaults != vaults {
        bail!("cannot switch vaults on an existing thread; start a new thread");
    }
    let configured = backend
        .configure_thread(agent.as_ref(), thread.as_ref(), created)
        .await;
    if let Err(error) = &configured
        && created
    {
        agent
            .delete_thread(&thread.record().id)
            .await
            .with_context(|| {
                format!("configuring thread failed ({error:#}); cleanup also failed")
            })?;
    }
    Ok(OpenedThread {
        thread,
        created,
        info: configured?,
    })
}

/// Agent-level defaults for conversation sandboxes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSandboxConfig {
    /// Default scope for conversation that don't specify a `sandbox_scope`.
    #[serde(default)]
    pub scope: SandboxScope,
    #[serde(default)]
    pub image: Option<String>,
    pub provider: exoharness::SandboxProvider,
    /// Mounts for the agent-scoped sandbox. These apply to every conversation
    /// that uses the shared agent sandbox.
    #[serde(default)]
    pub mounts: Vec<exoharness::FileSystemMount>,
    #[serde(default = "default_true")]
    pub enable_networking: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxScope {
    Agent,
    #[default]
    Conversation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BraintrustTracingConfig {
    pub org_name: Option<String>,
    pub project: BraintrustProject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum BraintrustProject {
    Name(String),
    Id(String),
}

#[cfg(test)]
mod tests;
