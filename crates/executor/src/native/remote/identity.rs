use super::{
    AuthServer,
    store::{Principal, Store},
};
use anyhow::{Context, Result, ensure};
use exo_managed_agents::identity::{
    IdentityCommand, IdentityConfiguration, IdentityRecord, SlackIdentityPin,
};
use exoharness::{ExoHarness, Uuid7, vault::VaultId};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
pub struct IdentityRegistry {
    store: Arc<Store>,
    harness: Arc<dyn ExoHarness>,
    auth: Option<Arc<AuthServer>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityGrant {
    pub account: String,
    pub vault: VaultId,
    pub revision: String,
    pub workspace: String,
    pub user: String,
    pub email: Option<String>,
}

#[derive(Debug)]
pub struct IdentityAccessChanged;
impl std::fmt::Display for IdentityAccessChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("sender's vault access changed after this turn was accepted")
    }
}
impl std::error::Error for IdentityAccessChanged {}

impl IdentityRegistry {
    pub async fn open(harness: Arc<dyn ExoHarness>) -> Result<Self> {
        Ok(Self {
            store: Arc::new(Store::standalone(harness.as_ref()).await?),
            harness,
            auth: None,
        })
    }
    pub fn authenticated(auth: Arc<AuthServer>, harness: Arc<dyn ExoHarness>) -> Self {
        Self {
            store: auth.store.clone(),
            harness,
            auth: Some(auth),
        }
    }
    pub async fn execute(&self, command: IdentityCommand) -> Result<IdentityConfiguration> {
        self.store.refresh().await?;
        let mut state = self.store.state.lock().await;
        let mut next = state.clone();
        let changed = !matches!(command, IdentityCommand::List);
        match command {
            IdentityCommand::List => {}
            IdentityCommand::BindVault { vault, email } => {
                let email = email.trim().to_ascii_lowercase();
                ensure!(
                    email.contains('@') && !email.contains(char::is_whitespace),
                    "provide a valid email address"
                );
                let vault =
                    exo_managed_agents::vaults::find_vault(self.harness.as_ref(), &vault).await?;
                ensure!(
                    vault.record().name != "global"
                        && vault.record().id != self.store.vault_id()
                        && !next.protected_vaults.contains(&vault.record().id)
                        && !next.shared_vaults.contains(&vault.record().id),
                    "global, shared, and server-state vaults cannot be assigned to a user"
                );
                for agent in self.harness.list_agents().await? {
                    if let Some(definition) =
                        exo_managed_agents::load_definition(agent.as_ref()).await?
                    {
                        ensure!(
                            !definition.frontmatter.adapters.iter().any(|adapter| adapter
                                .vault()
                                .is_some_and(|reference| reference == vault.record().name
                                    || reference == vault.record().id.to_string())),
                            "adapter transport vaults cannot be assigned to a user"
                        );
                    }
                }
                let id = match next
                    .principals
                    .values()
                    .find(|p| p.email.eq_ignore_ascii_case(&email))
                {
                    Some(principal) => principal.id.clone(),
                    None if self.auth.as_ref().is_some_and(|auth| {
                        auth.config.owner_email.eq_ignore_ascii_case(&email)
                    }) =>
                    {
                        next.owner.clone()
                    }
                    None => Uuid7::now().to_string(),
                };
                ensure!(
                    !next.principals.values().any(|principal|
                        principal.vault == vault.record().id && principal.id != id),
                    "vault is already assigned to another user"
                );
                ensure!(
                    next.vault_owners
                        .get(&vault.record().id)
                        .is_none_or(|owner| owner == &next.owner || owner == &id),
                    "vault is owned by another user; bind a vault owned by the server operator or this user"
                );
                if let Some(previous) = next.principals.get(&id)
                    && previous.vault != vault.record().id
                {
                    let previous_vault =
                        exoharness::vault::require_vault(self.harness.as_ref(), &previous.vault)
                            .await?;
                    let name = if previous_vault.record().name == "personal" {
                        format!("personal-{}", previous.vault)
                    } else {
                        previous_vault.record().name.clone()
                    };
                    next.vault_names.insert(previous.vault, name);
                }
                next.principals
                    .entry(id.clone())
                    .and_modify(|p| p.vault = vault.record().id)
                    .or_insert(Principal {
                        id: id.clone(),
                        issuer: String::new(),
                        subject: String::new(),
                        email,
                        vault: vault.record().id,
                    });
                next.vault_owners.insert(vault.record().id, id);
                next.vault_names
                    .insert(vault.record().id, "personal".into());
            }
            IdentityCommand::BindSlack {
                workspace,
                user,
                account,
            } => {
                validate_slack_id(&workspace, 'T')?;
                ensure!(
                    user.starts_with(['U', 'W']) && user.chars().all(|c| c.is_ascii_alphanumeric()),
                    "invalid Slack user ID"
                );
                let matches = next
                    .principals
                    .values()
                    .filter(|p| p.id == account || p.email.eq_ignore_ascii_case(&account))
                    .collect::<Vec<_>>();
                let [principal] = matches.as_slice() else {
                    anyhow::bail!("account must identify exactly one existing user");
                };
                let pin = SlackIdentityPin {
                    workspace: workspace.clone(),
                    user: user.clone(),
                    account: principal.id.clone(),
                };
                next.slack_pins
                    .retain(|p| p.workspace != workspace || p.user != user);
                next.slack_pins.push(pin);
            }
            IdentityCommand::UnbindSlack { workspace, user } => {
                next.slack_pins
                    .retain(|p| p.workspace != workspace || p.user != user);
            }
            IdentityCommand::TrustSlack { workspace, trusted } => {
                validate_slack_id(&workspace, 'T')?;
                next.trusted_slack_workspaces.retain(|id| id != &workspace);
                if trusted {
                    next.trusted_slack_workspaces.push(workspace);
                }
            }
        }
        if changed {
            next.identity_revision = next
                .identity_revision
                .checked_add(1)
                .context("identity revision overflow")?;
            self.store.save(&next).await?;
            *state = next;
            tracing::info!(
                revision = state.identity_revision,
                "provider identity configuration updated"
            );
        }
        Ok(IdentityConfiguration {
            accounts: state
                .principals
                .values()
                .map(|p| IdentityRecord {
                    id: p.id.clone(),
                    email: p.email.clone(),
                    vault: p.vault,
                })
                .collect(),
            slack_pins: state.slack_pins.clone(),
            trusted_slack_workspaces: state.trusted_slack_workspaces.clone(),
        })
    }
    pub async fn needs_slack_email(&self, workspace: &str, user: &str) -> Result<bool> {
        self.store.refresh().await?;
        let state = self.store.state.lock().await;
        Ok(state
            .trusted_slack_workspaces
            .iter()
            .any(|id| id == workspace)
            && !state
                .slack_pins
                .iter()
                .any(|p| p.workspace == workspace && p.user == user))
    }
    pub async fn resolve_slack(
        &self,
        workspace: &str,
        user: &str,
        email: Option<&str>,
    ) -> Result<Option<IdentityGrant>> {
        self.store.refresh().await?;
        let state = self.store.state.lock().await;
        let principal = if let Some(pin) = state
            .slack_pins
            .iter()
            .find(|p| p.workspace == workspace && p.user == user)
        {
            state.principals.get(&pin.account)
        } else if state
            .trusted_slack_workspaces
            .iter()
            .any(|id| id == workspace)
        {
            let matches = state
                .principals
                .values()
                .filter(|p| email.is_some_and(|email| p.email.eq_ignore_ascii_case(email)))
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [principal] => Some(*principal),
                _ => None,
            }
        } else {
            None
        };
        let pin = state
            .slack_pins
            .iter()
            .find(|pin| pin.workspace == workspace && pin.user == user);
        let trusted = state
            .trusted_slack_workspaces
            .iter()
            .any(|id| id == workspace);
        principal
            .filter(|p| {
                state.vault_owners.get(&p.vault) == Some(&p.id)
                    && !state.shared_vaults.contains(&p.vault)
                    && !state.protected_vaults.contains(&p.vault)
                    && p.vault != self.store.vault_id()
                    && self
                        .auth
                        .as_ref()
                        .is_none_or(|auth| auth.config.admitted(&p.email))
            })
            .map(|p| {
                use sha2::{Digest, Sha256};
                let revision = format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&(p, pin, trusted))?)
                );
                Ok(IdentityGrant {
                    account: p.id.clone(),
                    vault: p.vault,
                    revision,
                    workspace: workspace.into(),
                    user: user.into(),
                    email: if pin.is_some() {
                        None
                    } else {
                        email.map(str::to_owned)
                    },
                })
            })
            .transpose()
    }
    pub async fn validate(&self, grant: &IdentityGrant) -> Result<()> {
        let current = self
            .resolve_slack(&grant.workspace, &grant.user, grant.email.as_deref())
            .await?;
        if current.as_ref() != Some(grant) {
            return Err(IdentityAccessChanged.into());
        }
        Ok(())
    }
}

fn validate_slack_id(id: &str, prefix: char) -> Result<()> {
    ensure!(
        id.starts_with(prefix) && id.len() > 1 && id.chars().all(|c| c.is_ascii_alphanumeric()),
        "invalid Slack ID: {id}"
    );
    Ok(())
}
