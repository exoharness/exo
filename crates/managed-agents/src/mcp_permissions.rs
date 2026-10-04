use crate::permissions::{PermissionPolicies, PermissionPolicy};

impl PermissionPolicies {
    pub fn for_mcp_tool(&self, tool: &exo_mcp::McpTool) -> PermissionPolicy {
        let server = self.mcp_servers.get(&tool.server_name);
        self.tool_policies
            .get(&tool.name)
            .copied()
            .or_else(|| server.and_then(|s| s.tool_policies.get(&tool.tool_name).copied()))
            .or_else(|| server.and_then(|s| s.permission_policy))
            .unwrap_or(PermissionPolicy::AlwaysAsk {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_use_specific_tool_then_server_defaults() {
        let definition = crate::AgentDefinition::parse("---\nname: Policies\nharness: basic\nconfig:\n  model: gpt-5-mini\npermission_policy: {type: always_ask}\ntool_policies:\n  shell: {type: always_allow}\nmcp_servers:\n  - type: url\n    name: notes\n    url: https://example.com/mcp\n    permission_policy: {type: always_allow}\n    tool_policies:\n      write: {type: always_ask}\n---\nUse tools.\n".into()).unwrap();
        let mut policies = definition.permissions();
        assert_eq!(policies.for_tool("other"), PermissionPolicy::AlwaysAsk {});
        assert_eq!(policies.for_tool("shell"), PermissionPolicy::AlwaysAllow {});
        let mut tool = exo_mcp::McpTool {
            name: "hashed_exposed_name".into(),
            server_name: "notes".into(),
            tool_name: "read".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
            output_schema: None,
            annotations: None,
        };
        assert_eq!(
            policies.for_mcp_tool(&tool),
            PermissionPolicy::AlwaysAllow {}
        );
        tool.tool_name = "write".into();
        assert_eq!(policies.for_mcp_tool(&tool), PermissionPolicy::AlwaysAsk {});
        policies
            .tool_policies
            .insert(tool.name.clone(), PermissionPolicy::AlwaysAllow {});
        assert_eq!(
            policies.for_mcp_tool(&tool),
            PermissionPolicy::AlwaysAllow {}
        );
    }

    #[test]
    fn builtin_tools_default_to_allow_and_mcp_tools_default_to_ask() {
        let definition = crate::AgentDefinition::parse("---\nname: Defaults\nharness: basic\nconfig:\n  model: gpt-5-mini\nmcp_servers:\n  - type: url\n    name: notes\n    url: https://example.com/mcp\n---\nUse tools.\n".into()).unwrap();
        let mut policies = definition.permissions();
        let tool = exo_mcp::McpTool {
            name: "exo_mcp__notes__read".into(),
            server_name: "notes".into(),
            tool_name: "read".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
            output_schema: None,
            annotations: None,
        };
        assert_eq!(policies.for_tool("shell"), PermissionPolicy::AlwaysAllow {});
        assert_eq!(policies.for_mcp_tool(&tool), PermissionPolicy::AlwaysAsk {});
        policies.mcp_servers.clear();
        assert_eq!(policies.for_mcp_tool(&tool), PermissionPolicy::AlwaysAsk {});
    }
}
