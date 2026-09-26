use super::*;
use crate::vault::{ResolvedSecret, SecretTarget, VaultRecord};

impl BasicExoHarness {
    pub(super) async fn check(&self, scope: ResourceScope) -> Result<()> {
        if let Some(caller) = &self.caller {
            caller.check(scope).await?;
        }
        Ok(())
    }

    pub(super) async fn claim(&self, scope: ResourceScope) -> Result<()> {
        if let Some(caller) = &self.caller {
            caller.policy.created(&caller.principal, scope).await?;
        }
        Ok(())
    }

    pub(super) fn caller_bindings_dir(&self, base: &Path) -> PathBuf {
        match &self.caller {
            Some(caller) => base
                .parent()
                .unwrap_or(Path::new(""))
                .join("principals")
                .join(&caller.principal)
                .join("bindings"),
            None => base.to_path_buf(),
        }
    }

    pub(super) async fn binding_records(&self, base: &Path) -> Result<Vec<BindingRecord>> {
        list_binding_records(&self.inner.storage, &self.caller_bindings_dir(base)).await
    }

    pub(super) async fn find_binding(
        &self,
        paths: &[PathBuf],
        id: &BindingId,
    ) -> Result<Option<Binding>> {
        for base in paths {
            if let Some(stored) = self
                .inner
                .storage
                .get_json_if_exists::<StoredBinding>(
                    self.caller_bindings_dir(base).join(format!("{id}.json")),
                )
                .await?
            {
                if !matches!(stored.record.binding, Binding::Llm { .. }) {
                    return Ok(Some(stored.record.binding));
                }
            }
        }
        Ok(None)
    }

    pub(super) async fn check_binding(&self, binding: &Binding) -> Result<()> {
        anyhow::ensure!(
            !matches!(binding, Binding::Llm { .. }),
            "model bindings are no longer supported; configure the model and vault credential in the agent spec"
        );
        anyhow::ensure!(
            self.caller.is_none(),
            "bindings must be configured on the runtime host"
        );
        Ok(())
    }

    pub(super) async fn check_environment(
        &self,
        environment: &crate::EnvironmentDefinition,
    ) -> Result<()> {
        if let Some(caller) = &self.caller
            && caller
                .policy
                .check_operator(&caller.principal)
                .await
                .is_err()
        {
            anyhow::ensure!(
                self.list_environments().await?.contains(environment),
                "use an environment published by the server owner"
            );
        }
        Ok(())
    }

    pub(super) fn check_sandbox(&self, sandbox: &StoredSandbox) -> Result<()> {
        if let Some(caller) = &self.caller {
            anyhow::ensure!(
                sandbox.principal.as_deref() == Some(&caller.principal),
                "sandbox belongs to another caller"
            );
        }
        Ok(())
    }

    pub(super) async fn default_vault(&self) -> Result<Arc<dyn VaultHandle>> {
        match &self.caller {
            Some(caller) => {
                crate::vault::require_vault(
                    self,
                    &caller.policy.default_vault(&caller.principal).await?,
                )
                .await
            }
            None => global_vault(self).await,
        }
    }

    pub(super) async fn scoped_vault(
        &self,
        vault: Arc<dyn VaultHandle>,
    ) -> Result<Option<Arc<dyn VaultHandle>>> {
        let Some(caller) = &self.caller else {
            return Ok(Some(vault));
        };
        let Some(record) = caller
            .policy
            .vault(&caller.principal, vault.record(), false)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(CallerVault {
            vault,
            record,
            caller: caller.clone(),
        })))
    }
}

struct CallerVault {
    vault: Arc<dyn VaultHandle>,
    record: VaultRecord,
    caller: crate::access::Caller,
}
impl CallerVault {
    async fn check(&self, write: bool) -> Result<()> {
        anyhow::ensure!(
            self.caller
                .policy
                .vault(&self.caller.principal, &self.record, write)
                .await?
                .is_some(),
            "vault is unavailable"
        );
        Ok(())
    }
}
#[async_trait]
impl VaultHandle for CallerVault {
    fn record(&self) -> &VaultRecord {
        &self.record
    }
    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        self.check(false).await?;
        self.vault.list_secrets().await
    }
    async fn put_secret(&self, request: crate::PutSecretRequest) -> Result<SecretId> {
        self.check(true).await?;
        self.vault.put_secret(request).await
    }
    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        self.check(true).await?;
        self.vault.get_secret(id).await
    }
    async fn update_secret(
        &self,
        id: &SecretId,
        request: crate::UpdateSecretRequest,
    ) -> Result<SecretMetadata> {
        self.check(true).await?;
        self.vault.update_secret(id, request).await
    }
    async fn delete_secret(&self, id: &SecretId) -> Result<()> {
        self.check(true).await?;
        self.vault.delete_secret(id).await
    }
    async fn resolve_secret(&self, id: &SecretId, target: &SecretTarget) -> Result<ResolvedSecret> {
        self.check(false).await?;
        self.vault.resolve_secret(id, target).await
    }
    async fn refresh_secret(
        &self,
        id: &SecretId,
        target: &SecretTarget,
        rejected_revision: u64,
    ) -> Result<ResolvedSecret> {
        self.check(false).await?;
        self.vault
            .refresh_secret(id, target, rejected_revision)
            .await
    }
}

#[async_trait]
impl VaultContext for BasicExoHarness {
    fn caller(&self) -> Option<&crate::access::Caller> {
        self.caller.as_ref()
    }
    async fn list_vaults(&self) -> Result<Vec<Arc<dyn VaultHandle>>> {
        self.check(ResourceScope::Global).await?;
        if self.caller.is_none() {
            self.inner.vaults.global_vault().await?;
        }
        let mut vaults = Vec::new();
        for vault in self.inner.vaults.list_vaults().await? {
            if let Some(vault) = self.scoped_vault(vault).await? {
                vaults.push(vault);
            }
        }
        Ok(vaults)
    }
    async fn get_vault(&self, id: &VaultId) -> Result<Option<Arc<dyn VaultHandle>>> {
        self.check(ResourceScope::Global).await?;
        match self.inner.vaults.get_vault(id).await? {
            Some(vault) => self.scoped_vault(vault).await,
            None => Ok(None),
        }
    }
}

#[async_trait]
impl VaultContext for BasicAgentHandle {
    fn caller(&self) -> Option<&crate::access::Caller> {
        self.harness.caller.as_ref()
    }
    async fn list_vaults(&self) -> Result<Vec<Arc<dyn VaultHandle>>> {
        self.harness
            .check(ResourceScope::Agent {
                agent_id: self.record.id,
            })
            .await?;
        if self.harness.caller.is_none() {
            return compose_vaults(
                &self.harness,
                vec![global_vault(&self.harness).await?],
                &self.record.vaults,
            )
            .await;
        }
        let mut vaults = vec![self.harness.default_vault().await?];
        for id in &self.record.vaults {
            if let Some(vault) = self.harness.get_vault(id).await?
                && !vaults.iter().any(|v| v.record().id == *id)
            {
                vaults.push(vault);
            }
        }
        Ok(vaults)
    }
}

#[async_trait]
impl VaultContext for BasicConversationHandle {
    fn caller(&self) -> Option<&crate::access::Caller> {
        self.harness.caller.as_ref()
    }
    async fn list_vaults(&self) -> Result<Vec<Arc<dyn VaultHandle>>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let agent = self
            .harness
            .get_agent(&self.agent_id)
            .await?
            .context("agent is unavailable")?;
        if self.harness.caller.is_none() {
            return compose_vaults(
                &self.harness,
                agent.list_vaults().await?,
                &self.record.vaults,
            )
            .await;
        }
        let mut vaults = agent.list_vaults().await?;
        for id in &self.record.vaults {
            if let Some(vault) = self.harness.get_vault(id).await?
                && !vaults.iter().any(|v| v.record().id == *id)
            {
                vaults.push(vault);
            }
        }
        Ok(vaults)
    }
}
