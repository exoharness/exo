use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdapterDefinition {
    Slack(SlackAdapter),
}

impl AdapterDefinition {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Slack(_) => "slack",
        }
    }
    pub fn name(&self) -> &str {
        let name = match self {
            Self::Slack(c) => &c.name,
        };
        name.as_deref().unwrap_or(self.kind())
    }
    pub fn vault(&self) -> Option<&str> {
        match self {
            Self::Slack(c) => Some(&c.vault),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlackTransport {
    #[default]
    Socket,
    Http,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelTrigger {
    #[default]
    MentionsOnly,
    AllMessages,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackAdapter {
    pub name: Option<String>,
    pub vault: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_vaults: Vec<String>,
    #[serde(default)]
    pub transport: SlackTransport,
    #[serde(default)]
    pub trigger: ChannelTrigger,
    pub allowed_channels: Option<Vec<String>>,
    pub allowed_users: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub direct_messages: bool,
    #[serde(default)]
    pub allow_slack_connect: bool,
    #[serde(default)]
    pub use_sender_vault: bool,
}

pub fn validate(adapters: &[AdapterDefinition]) -> Result<()> {
    let mut names = std::collections::BTreeSet::new();
    for adapter in adapters {
        let name = adapter.name();
        ensure!(
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "adapter names may only contain letters, numbers, '-' and '_'"
        );
        ensure!(names.insert(name), "duplicate adapter: {name}");
        ensure!(
            adapter.vault().is_none_or(|vault| !vault.trim().is_empty()),
            "adapter {name} needs a vault name"
        );
        let AdapterDefinition::Slack(slack) = adapter;
        ensure!(
            slack.vault != "global",
            "Slack transport credentials need a separate vault"
        );
        ensure!(
            slack
                .turn_vaults
                .iter()
                .all(|vault| !vault.trim().is_empty() && vault != &slack.vault),
            "execution vaults must be named and separate from Slack transport credentials"
        );
        ensure!(
            !slack.allow_slack_connect
                || slack
                    .allowed_channels
                    .as_ref()
                    .is_some_and(|channels| !channels.is_empty()),
            "allow_slack_connect requires explicit allowed_channels"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_resolved_before_duplicate_validation() -> Result<()> {
        let adapters = serde_yaml_ng::from_str::<Vec<AdapterDefinition>>(
            "- type: slack\n  vault: first\n- type: slack\n  name: slack\n  vault: second\n",
        )?;
        assert!(
            validate(&adapters)
                .unwrap_err()
                .to_string()
                .contains("duplicate adapter: slack")
        );
        assert!(
            serde_yaml_ng::from_str::<AdapterDefinition>(
                "type: slack\nvault: bot\nbind: localhost:3939"
            )
            .is_err()
        );
        Ok(())
    }
}
