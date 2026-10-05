use super::*;
impl BasicVaultHandle {
    pub(super) async fn resolve_credentials(
        &self,
        id: &SecretId,
        target: &CredentialDestination,
    ) -> Result<ResolvedSecret> {
        let resolved = self.read_for_destination(id, target).await?;
        anyhow::ensure!(
            matches!(resolved.secret, Secret::Key { .. }),
            "this host does not support credential refresh"
        );
        Ok(resolved)
    }
    pub(super) async fn refresh_task(
        &self,
        _id: &SecretId,
        _target: &CredentialDestination,
        _revision: u64,
        _force: bool,
    ) -> Result<ResolvedSecret> {
        bail!("this host does not support credential refresh")
    }
}
