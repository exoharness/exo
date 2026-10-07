use super::{AuthServer, oidc::AuthConfig, store::Store};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use exoharness::{
    ExoHarness, ResourceScope,
    access::{AccessPolicy, Caller},
    vault::{VaultId, VaultRecord},
};
use std::sync::Arc;

struct Policy {
    store: Arc<Store>,
    config: AuthConfig,
    multiplayer: bool,
}

fn key(scope: ResourceScope) -> String {
    match scope {
        ResourceScope::Global => "global".into(),
        ResourceScope::Agent { agent_id } => format!("agent/{agent_id}"),
        ResourceScope::Thread {
            agent_id,
            thread_id,
        } => format!("agent/{agent_id}/thread/{thread_id}"),
    }
}

impl AuthServer {
    pub async fn adopt_local_state(&self, harness: &dyn ExoHarness) -> Result<()> {
        let mut stored = self.store.state.lock().await;
        let mut next = stored.clone();
        for agent in harness.list_agents().await? {
            let agent_id = agent.record().id;
            next.owners
                .entry(key(ResourceScope::Agent { agent_id }))
                .or_insert(next.owner.clone());
            for thread in exo_managed_agents::list_threads(agent.as_ref()).await? {
                next.owners
                    .entry(key(ResourceScope::Thread {
                        agent_id,
                        thread_id: thread.record().id,
                    }))
                    .or_insert(next.owner.clone());
            }
        }
        let vaults = harness.list_vaults().await?;
        next.shared_vaults.clear();
        for reference in &self.config.shared_vaults {
            let vault = vaults
                .iter()
                .find(|v| v.record().name == *reference || v.record().id.to_string() == *reference)
                .ok_or_else(|| anyhow::anyhow!("shared vault not found: {reference}"))?;
            ensure!(
                vault.record().id != self.auth_vault()
                    && !next
                        .principals
                        .values()
                        .any(|p| p.vault == vault.record().id),
                "auth and personal vaults cannot be shared"
            );
            next.shared_vaults.push(vault.record().id);
        }
        for vault in vaults {
            if vault.record().id != self.auth_vault() {
                next.vault_owners
                    .entry(vault.record().id)
                    .or_insert(next.owner.clone());
                next.vault_names
                    .entry(vault.record().id)
                    .or_insert(vault.record().name.clone());
            }
        }
        self.store.save(&next).await?;
        *stored = next;
        Ok(())
    }

    pub fn caller(&self, principal: String, multiplayer: bool) -> Caller {
        Caller {
            principal,
            policy: Arc::new(Policy {
                store: self.store.clone(),
                config: self.config.clone(),
                multiplayer,
            }),
        }
    }

    pub async fn wait_for_owner(&self) {
        loop {
            let notified = self.store.enrolled.notified();
            {
                let state = self.store.state.lock().await;
                if state.principals.contains_key(&state.owner) {
                    return;
                }
            }
            notified.await;
        }
    }

    pub async fn owner(&self) -> String {
        self.store.state.lock().await.owner.clone()
    }
}

#[async_trait]
impl AccessPolicy for Policy {
    async fn check(&self, principal: &str, scope: ResourceScope) -> Result<()> {
        let state = self.store.state.lock().await;
        ensure!(
            state.owner == principal
                || state
                    .principals
                    .get(principal)
                    .is_some_and(|p| self.config.admitted(&p.email)),
            "identity is not admitted"
        );
        ensure!(
            scope == ResourceScope::Global
                || state
                    .owners
                    .get(&key(scope))
                    .map_or(state.owner == principal, |owner| owner == principal
                        || self.multiplayer),
            "resource not available"
        );
        Ok(())
    }

    async fn check_operator(&self, principal: &str) -> Result<()> {
        self.check(principal, ResourceScope::Global).await?;
        ensure!(
            self.store.state.lock().await.owner == principal,
            "operation requires the server owner"
        );
        Ok(())
    }

    async fn created(&self, principal: &str, scope: ResourceScope) -> Result<()> {
        self.check(principal, ResourceScope::Global).await?;
        let mut stored = self.store.state.lock().await;
        let mut next = stored.clone();
        let previous = next.owners.insert(key(scope), principal.into());
        ensure!(
            previous.is_none_or(|p| p == principal),
            "resource already belongs to another identity"
        );
        self.store.save(&next).await?;
        *stored = next;
        Ok(())
    }

    async fn vault(
        &self,
        principal: &str,
        record: &VaultRecord,
        write: bool,
    ) -> Result<Option<VaultRecord>> {
        self.check(principal, ResourceScope::Global).await?;
        let state = self.store.state.lock().await;
        let owns = state
            .vault_owners
            .get(&record.id)
            .is_some_and(|owner| owner == principal);
        if record.id == self.store.vault_id()
            || (!owns && (write || !state.shared_vaults.contains(&record.id)))
        {
            return Ok(None);
        }
        let mut record = record.clone();
        if let Some(name) = state.vault_names.get(&record.id) {
            record.name = name.clone();
        }
        Ok(Some(record))
    }

    async fn vault_created(&self, principal: &str, id: VaultId, name: &str) -> Result<()> {
        self.check(principal, ResourceScope::Global).await?;
        let mut stored = self.store.state.lock().await;
        let mut next = stored.clone();
        next.vault_owners.insert(id, principal.into());
        next.vault_names.insert(id, name.into());
        self.store.save(&next).await?;
        *stored = next;
        Ok(())
    }

    async fn default_vault(&self, principal: &str) -> Result<VaultId> {
        self.check(principal, ResourceScope::Global).await?;
        self.store
            .state
            .lock()
            .await
            .principals
            .get(principal)
            .map(|p| p.vault)
            .ok_or_else(|| anyhow::anyhow!("the server owner must log in before running adapters"))
    }
}
