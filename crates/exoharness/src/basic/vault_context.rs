use super::*;

pub(super) struct ScopedVaultContext<'a> {
    pub harness: &'a BasicExoHarness,
    pub scope: ResourceScope,
}

pub(super) struct ModelCredential {
    pub base_url: Option<String>,
    pub secret: Option<SecretReference>,
}

impl ScopedVaultContext<'_> {
    pub(super) async fn model_binding(&self, id: &BindingId) -> Result<ModelCredential> {
        let binding = match self.scope {
            ResourceScope::Global => self.harness.get_binding(id).await?,
            ResourceScope::Agent { agent_id } => {
                self.harness
                    .get_agent(&agent_id)
                    .await?
                    .context("agent is unavailable")?
                    .get_binding(id)
                    .await?
            }
            ResourceScope::Thread {
                agent_id,
                thread_id,
            } => {
                self.harness
                    .get_agent(&agent_id)
                    .await?
                    .context("agent is unavailable")?
                    .get_thread(&thread_id)
                    .await?
                    .context("thread is unavailable")?
                    .get_binding(id)
                    .await?
            }
        }
        .context("sandbox model binding is unavailable")?;
        let Binding::Llm {
            base_url, secret, ..
        } = binding
        else {
            bail!("sandbox model credential requires an LLM binding");
        };
        Ok(ModelCredential { base_url, secret })
    }

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
            None => Ok(vec![global_vault(self.harness).await?]),
        }
    }

    async fn get_vault(&self, id: &VaultId) -> Result<Option<Arc<dyn VaultHandle>>> {
        match self.resource().await? {
            Some(context) => context.get_vault(id).await,
            None => {
                let vault = global_vault(self.harness).await?;
                Ok((vault.record().id == *id).then_some(vault))
            }
        }
    }
}
