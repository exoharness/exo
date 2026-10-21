use super::{
    definitions::DefinitionCache,
    store::{AdapterStore, Authority, digest},
};
use crate::{Runtime, remote::IdentityRegistry};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use exoharness::{
    ResourceScope,
    access::{AccessPolicy, Caller},
    vault::{VaultId, VaultRecord},
};
use std::sync::Arc;

pub(super) struct AdapterPolicy {
    pub authority: Authority,
    pub identities: IdentityRegistry,
    pub definitions: Arc<DefinitionCache>,
    pub global: VaultId,
    pub principal: String,
    pub root: Arc<dyn exoharness::ExoHarness>,
}

#[derive(Debug)]
pub(super) struct AdapterAccessDenied;
impl std::fmt::Display for AdapterAccessDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Slack sender or channel is no longer allowed")
    }
}
impl std::error::Error for AdapterAccessDenied {}

#[async_trait]
impl AccessPolicy for AdapterPolicy {
    async fn check(&self, principal: &str, scope: ResourceScope) -> Result<()> {
        ensure!(principal == self.principal, "adapter caller mismatch");
        // Authority is immutable and included in the caller's principal. Grants
        // and adapter permissions are checked against their current state below.
        let agent = self
            .root
            .get_agent(&self.authority.agent)
            .await?
            .ok_or_else(|| anyhow::anyhow!("adapter agent was removed"))?;
        let definition = self
            .definitions
            .load(agent.as_ref())
            .await?
            .ok_or_else(|| anyhow::anyhow!("adapter agent definition is missing"))?;
        let config = definition
            .frontmatter
            .adapters
            .iter()
            .find_map(|definition| match definition {
                exo_managed_agents::adapters::AdapterDefinition::Slack(config)
                    if definition.name() == self.authority.adapter =>
                {
                    Some(config)
                }
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("Slack adapter was removed"))?;
        let message = &self.authority.source;
        if !(config
            .allowed_users
            .as_ref()
            .is_none_or(|users| users.contains(&message.sender))
            && (!message.external
                || (config.allow_slack_connect
                    && config
                        .allowed_channels
                        .as_ref()
                        .is_some_and(|channels| channels.contains(&message.channel))))
            && if message.direct {
                config.direct_messages
            } else {
                config
                    .allowed_channels
                    .as_ref()
                    .is_none_or(|channels| channels.contains(&message.channel))
            })
        {
            return Err(AdapterAccessDenied.into());
        }
        if let Some(grant) = &self.authority.grant {
            let sender_vault =
                exoharness::vault::require_vault(self.root.as_ref(), &grant.vault).await?;
            if !config.use_sender_vault
                || config.vault == grant.vault.to_string()
                || config.vault == sender_vault.record().name
            {
                return Err(AdapterAccessDenied.into());
            }
            self.identities.validate(grant).await?;
        }
        ensure!(
            match scope {
                ResourceScope::Global => true,
                ResourceScope::Agent { agent_id } => agent_id == self.authority.agent,
                ResourceScope::Thread {
                    agent_id,
                    thread_id,
                } => agent_id == self.authority.agent && thread_id == self.authority.thread,
            },
            "adapter caller cannot access this resource"
        );
        Ok(())
    }
    async fn check_operator(&self, _: &str) -> Result<()> {
        anyhow::bail!("adapter callers cannot perform operator operations")
    }
    async fn created(&self, principal: &str, scope: ResourceScope) -> Result<()> {
        self.check(principal, scope).await
    }
    async fn vault(
        &self,
        principal: &str,
        record: &VaultRecord,
        write: bool,
    ) -> Result<Option<VaultRecord>> {
        self.check(principal, ResourceScope::Global).await?;
        if write {
            return Ok(None);
        }
        if record.id == self.global {
            return Ok(Some(record.clone()));
        }
        if self
            .authority
            .grant
            .as_ref()
            .is_some_and(|g| g.vault == record.id)
        {
            let mut record = record.clone();
            record.name = "personal".into();
            return Ok(Some(record));
        }
        let agent = self
            .root
            .get_agent(&self.authority.agent)
            .await?
            .ok_or_else(|| anyhow::anyhow!("adapter agent was removed"))?;
        let definition = self
            .definitions
            .load(agent.as_ref())
            .await?
            .ok_or_else(|| anyhow::anyhow!("adapter agent definition is missing"))?;
        let allowed = definition.frontmatter.adapters.iter().any(|adapter| {
            let exo_managed_agents::adapters::AdapterDefinition::Slack(config) = adapter;
            adapter.name() == self.authority.adapter
                && config.vault != record.name
                && config.vault != record.id.to_string()
                && config
                    .turn_vaults
                    .iter()
                    .any(|vault| vault == &record.name || vault == &record.id.to_string())
        });
        Ok(allowed.then(|| record.clone()))
    }
    async fn vault_created(&self, _: &str, _: VaultId, _: &str) -> Result<()> {
        anyhow::bail!("adapter callers cannot create vaults")
    }
    async fn default_vault(&self, principal: &str) -> Result<VaultId> {
        self.check(principal, ResourceScope::Global).await?;
        Ok(self
            .authority
            .grant
            .as_ref()
            .map(|g| g.vault)
            .unwrap_or(self.global))
    }
}

pub(super) fn principal(authority: &Authority) -> Result<String> {
    Ok(format!(
        "adapter-{}",
        digest(&serde_json::to_string(authority)?)
    ))
}

pub(super) async fn caller(
    root: &Runtime,
    store: &AdapterStore,
    identities: IdentityRegistry,
    definitions: Arc<DefinitionCache>,
    authority: Authority,
) -> Result<Caller> {
    let principal = principal(&authority)?;
    store
        .put(&AdapterStore::authority(&principal), &authority)
        .await?;
    let global = exoharness::vault::global_vault(root.exoharness_handle().as_ref())
        .await?
        .record()
        .id;
    Ok(Caller {
        principal: principal.clone(),
        policy: Arc::new(AdapterPolicy {
            authority,
            identities,
            definitions,
            global,
            principal: principal.clone(),
            root: root.exoharness_handle(),
        }),
    })
}
