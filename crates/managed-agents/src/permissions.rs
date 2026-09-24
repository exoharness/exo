use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PermissionPolicy {
    AlwaysAllow {},
    AlwaysAsk {},
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self::AlwaysAllow {}
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPermissions {
    #[serde(default)]
    pub permission_policy: Option<PermissionPolicy>,
    #[serde(default)]
    pub tool_policies: BTreeMap<String, PermissionPolicy>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionPolicies {
    #[serde(default)]
    pub permission_policy: PermissionPolicy,
    #[serde(default)]
    pub tool_policies: BTreeMap<String, PermissionPolicy>,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, ToolPermissions>,
}

impl PermissionPolicies {
    pub fn validate_tool_names<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> anyhow::Result<()> {
        let known: std::collections::HashSet<_> = names.into_iter().collect();
        for name in self.tool_policies.keys() {
            anyhow::ensure!(
                known.contains(name.as_str()),
                "unknown tool in tool_policies: {name}"
            );
        }
        Ok(())
    }

    pub fn for_tool(&self, name: &str) -> PermissionPolicy {
        self.tool_policies
            .get(name)
            .copied()
            .unwrap_or(self.permission_policy)
    }

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

impl crate::AgentDefinition {
    pub fn permissions(&self) -> PermissionPolicies {
        PermissionPolicies {
            permission_policy: self.frontmatter.permission_policy,
            tool_policies: self.frontmatter.tool_policies.clone(),
            mcp_servers: self
                .frontmatter
                .mcp_servers
                .iter()
                .map(|server| (server.name().to_owned(), server.permissions().clone()))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_exact_names_against_the_active_inventory() {
        let mut policies = PermissionPolicies::default();
        policies
            .tool_policies
            .insert("Bash".into(), PermissionPolicy::AlwaysAsk {});
        assert!(policies.validate_tool_names(["claude.Bash"]).is_err());
        assert!(policies.validate_tool_names(["Bash"]).is_ok());
    }

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

    #[test]
    fn permission_policies_reject_unknown_options() {
        assert!(
            serde_json::from_str::<PermissionPolicy>(r#"{"type":"always_allow","ask":true}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<PermissionPolicy>(r#"{"type":"auto"}"#).is_err());
    }
}
