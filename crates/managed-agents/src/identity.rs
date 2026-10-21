use exoharness::vault::VaultId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityRecord {
    pub id: String,
    pub email: String,
    pub vault: VaultId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackIdentityPin {
    pub workspace: String,
    pub user: String,
    pub account: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityConfiguration {
    pub accounts: Vec<IdentityRecord>,
    pub slack_pins: Vec<SlackIdentityPin>,
    pub trusted_slack_workspaces: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityCommand {
    List,
    BindVault {
        vault: String,
        email: String,
    },
    BindSlack {
        workspace: String,
        user: String,
        account: String,
    },
    UnbindSlack {
        workspace: String,
        user: String,
    },
    TrustSlack {
        workspace: String,
        trusted: bool,
    },
}
