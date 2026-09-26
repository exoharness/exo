use crate::{
    ResourceScope, Result,
    vault::{VaultId, VaultRecord},
};
use async_trait::async_trait;
use std::sync::Arc;

#[derive(Clone)]
pub struct Caller {
    pub principal: String,
    pub policy: Arc<dyn AccessPolicy>,
}

#[async_trait]
pub trait AccessPolicy: Send + Sync {
    async fn check(&self, principal: &str, scope: ResourceScope) -> Result<()>;
    async fn check_operator(&self, principal: &str) -> Result<()>;
    async fn created(&self, principal: &str, scope: ResourceScope) -> Result<()>;
    async fn vault(
        &self,
        principal: &str,
        record: &VaultRecord,
        write: bool,
    ) -> Result<Option<VaultRecord>>;
    async fn vault_created(&self, principal: &str, id: VaultId, name: &str) -> Result<()>;
    async fn default_vault(&self, principal: &str) -> Result<VaultId>;
}

impl Caller {
    pub async fn check(&self, scope: ResourceScope) -> Result<()> {
        self.policy.check(&self.principal, scope).await
    }
}
