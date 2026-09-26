use std::sync::Arc;

use async_trait::async_trait;
use exo_mcp::McpToolSet;
use exoharness::{AgentHandle, ConversationHandle, Result, ToolRequest, ToolResult, TurnHandle};

use crate::{AgentConfig, ConversationConfig, ToolDefinition, ToolRuntime};

pub struct McpToolRuntime<T> {
    inner: T,
    mcp: Arc<McpToolSet>,
}

impl<T> McpToolRuntime<T> {
    pub fn new(inner: T, mcp: Arc<McpToolSet>) -> Self {
        Self { inner, mcp }
    }
}

fn definitions(mcp: &McpToolSet) -> Vec<ToolDefinition> {
    mcp.tools()
        .iter()
        .map(|tool| ToolDefinition {
            strict: Some(false),
            name: tool.name.clone(),
            description: format!(
                "{}\nMCP server: {}; tool: {}",
                tool.description, tool.server_name, tool.tool_name
            ),
            parameters: tool.parameters.clone(),
        })
        .collect()
}

#[async_trait]
impl<T: ToolRuntime> ToolRuntime for McpToolRuntime<T> {
    fn permission_policy(
        &self,
        policies: &exo_managed_agents::permissions::PermissionPolicies,
        name: &str,
    ) -> exo_managed_agents::permissions::PermissionPolicy {
        match self.mcp.tools().iter().find(|tool| tool.name == name) {
            Some(tool) => policies.for_mcp_tool(tool),
            None => self.inner.permission_policy(policies, name),
        }
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut tools = self.inner.definitions();
        tools.extend(definitions(&self.mcp));
        tools
    }

    async fn prepare_conversation(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
    ) -> Result<()> {
        for (name, policies) in &config.permissions.mcp_servers {
            let (_, disabled) = self
                .mcp
                .servers()
                .find(|(server, _)| &server.name == name)
                .ok_or_else(|| anyhow::anyhow!("unknown MCP server in tool policies: {name}"))?;
            for tool in policies.tool_policies.keys() {
                anyhow::ensure!(
                    disabled.contains(tool)
                        || self.mcp.tools().iter().any(|known| {
                            &known.server_name == name && &known.tool_name == tool
                        }),
                    "MCP server {name} has no tool named {tool} in tool_policies"
                );
            }
        }
        self.inner
            .prepare_conversation(agent, conversation, agent_config, config)
            .await
    }

    async fn execute(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        turn: Option<&dyn TurnHandle>,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
        request: &ToolRequest,
    ) -> Result<ToolResult> {
        if self.mcp.has_tool(&request.function_name) {
            let result = self
                .mcp
                .call(&request.function_name, request.arguments.clone())
                .await?;
            return Ok(serde_json::to_value(result)?);
        }
        self.inner
            .execute(agent, conversation, turn, agent_config, config, request)
            .await
    }
}
