use crate::{AgentConfig, ConversationConfig};
use anyhow::{Result, bail};
use exo_managed_agents as managed;
use exo_mcp::McpToolSet;
use exoharness::{AgentHandle, ThreadHandle};
use std::{path::Path, sync::Arc};

pub(crate) struct PreparedMcp {
    pub(crate) tools: Arc<McpToolSet>,
    pub(crate) selection: Option<managed::vaults::VaultSelection>,
    pub(crate) selection_is_new: bool,
    pub(crate) config: exoharness::BasicExoHarnessConfig,
    pub(crate) servers: Vec<exo_mcp::McpServerConfig>,
}

pub(crate) async fn connect_mcp(
    agent: &dyn AgentHandle,
    thread: &dyn ThreadHandle,
    config: &exoharness::BasicExoHarnessConfig,
) -> Result<PreparedMcp> {
    let vaults = thread.list_vaults().await?;
    let saved = managed::vaults::load_selection(thread).await?;
    let selection_is_new = saved.is_none();
    let servers = match managed::load_definition(agent).await? {
        Some(definition) => definition.resolve_mcp_servers(&()).await?,
        None => Vec::new(),
    };
    let selection = match saved {
        Some(selection) => Some(selection),
        None if servers.is_empty() && vaults.len() == 1 => None,
        None => Some(managed::vaults::VaultSelection::from_vaults(&vaults, &servers).await?),
    };
    let tools = match &selection {
        Some(selection) => {
            selection.validate_servers(&servers)?;
            let credentials = managed::vaults::VaultMcpCredentials::new(vaults, selection.clone());
            Arc::new(McpToolSet::connect_with_provider(&servers, Arc::new(credentials)).await?)
        }
        None => Arc::default(),
    };
    Ok(PreparedMcp {
        tools,
        selection,
        selection_is_new,
        config: config.clone(),
        servers,
    })
}

impl PreparedMcp {
    pub(crate) fn validate_thread(
        &self,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
    ) -> Result<()> {
        let provider = config
            .sandbox_provider
            .as_ref()
            .unwrap_or(&agent_config.sandbox.provider);
        if *provider != exoharness::SandboxProvider::LocalProcess {
            for mount in agent_config
                .sandbox
                .mounts
                .iter()
                .chain(config.mounts.iter())
            {
                self.config
                    .validate_secret_mount(Path::new(&mount.host_path))?;
            }
        } else if let Some(selection) = &self.selection
            && (selection.vaults.len() > 1 || selection.bindings.iter().any(|b| b.secret.is_some()))
        {
            bail!(
                "vault-backed chat requires an isolated sandbox; local-process can read host credentials"
            );
        }
        Ok(())
    }

    pub(crate) async fn configure_thread(
        &self,
        agent_config: &AgentConfig,
        conversation: &dyn ThreadHandle,
        config: &ConversationConfig,
    ) -> Result<()> {
        self.validate_thread(agent_config, config)?;
        if self.selection_is_new
            && let Some(selection) = &self.selection
        {
            selection.save(conversation).await?;
        }

        let inventory = serde_json::to_value(self.tools.tools())?;
        let previous = conversation
            .get_events(Some(exoharness::EventQuery {
                direction: Some(exoharness::EventQueryDirection::Desc),
                limit: Some(1),
                types: Some(vec![exoharness::EventKind::custom("mcp_tools")]),
                ..Default::default()
            }))
            .await?;
        if (!self.tools.tools().is_empty() || !previous.events.is_empty())
            && !matches!(previous.events.first().map(|event| &event.data),
                Some(crate::EventData::Custom { payload, .. }) if payload == &inventory)
        {
            conversation
                .add_events(exoharness::AddEventsRequest {
                    session_id: None,
                    turn_id: None,
                    data: vec![crate::EventData::Custom {
                        event_type: "mcp_tools".to_string(),
                        payload: inventory,
                    }],
                })
                .await?;
        }
        Ok(())
    }
}
