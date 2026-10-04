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
    fn permission_policies_reject_unknown_options() {
        assert!(
            serde_json::from_str::<PermissionPolicy>(r#"{"type":"always_allow","ask":true}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<PermissionPolicy>(r#"{"type":"auto"}"#).is_err());
    }
}
