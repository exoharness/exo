use anyhow::Result;
use exo_managed_agents as managed;
use exo_mcp::McpToolSet;
use exoharness::{AgentHandle, ThreadHandle};
use std::sync::Arc;

pub struct PreparedMcp {
    pub tools: Arc<McpToolSet>,
    pub selection: Option<managed::vaults::VaultSelection>,
    selection_is_new: bool,
    pub servers: Vec<exo_mcp::McpServerConfig>,
}

pub async fn connect_mcp(
    agent: &dyn AgentHandle,
    thread: &dyn ThreadHandle,
    connector: &dyn exo_mcp::McpConnector,
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
            Arc::new(
                if thread
                    .caller()
                    .is_some_and(|caller| caller.principal.starts_with("adapter-"))
                {
                    McpToolSet::connect_available_with_connector(
                        &servers,
                        Arc::new(credentials),
                        connector,
                    )
                    .await?
                } else {
                    McpToolSet::connect_with_connector(&servers, Arc::new(credentials), connector)
                        .await?
                },
            )
        }
        None => Arc::default(),
    };
    Ok(PreparedMcp {
        tools,
        selection,
        selection_is_new,
        servers,
    })
}

impl PreparedMcp {
    pub async fn validate_configuration(&self, agent: &dyn AgentHandle) -> Result<()> {
        let servers = match managed::load_definition(agent).await? {
            Some(definition) => definition.resolve_mcp_servers(&()).await?,
            None => vec![],
        };
        anyhow::ensure!(
            self.servers == servers,
            "MCP configuration changed; start a new thread"
        );
        Ok(())
    }

    pub async fn configure_thread(&self, conversation: &dyn ThreadHandle) -> Result<()> {
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
