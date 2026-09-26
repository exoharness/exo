use super::*;

pub(super) struct ScopedVaultContext<'a> {
    pub harness: &'a BasicExoHarness,
    pub scope: ResourceScope,
}

impl ScopedVaultContext<'_> {
    async fn resource(&self) -> Result<Option<Arc<dyn VaultContext>>> {
        let Some(agent_id) = self.scope.agent_id() else {
            return Ok(None);
        };
        let agent = self
            .harness
            .get_agent(&agent_id)
            .await?
            .context("agent is unavailable")?;
        if let ResourceScope::Thread { thread_id, .. } = self.scope {
            return Ok(Some(
                agent
                    .get_thread(&thread_id)
                    .await?
                    .context("thread is unavailable")?,
            ));
        }
        Ok(Some(agent))
    }
}

#[async_trait]
impl VaultContext for ScopedVaultContext<'_> {
    async fn list_vaults(&self) -> Result<Vec<Arc<dyn VaultHandle>>> {
        match self.resource().await? {
            Some(context) => context.list_vaults().await,
            None => Ok(vec![self.harness.default_vault().await?]),
        }
    }

    async fn get_vault(&self, id: &VaultId) -> Result<Option<Arc<dyn VaultHandle>>> {
        match self.resource().await? {
            Some(context) => context.get_vault(id).await,
            None => {
                let vault = self.harness.default_vault().await?;
                Ok((vault.record().id == *id).then_some(vault))
            }
        }
    }
}
