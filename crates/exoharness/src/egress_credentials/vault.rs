use super::EgressDestination;
use crate::Result;
use crate::vault::{CredentialDestination, SecretReference, VaultContext, require_vault};

pub async fn resolve_credential(
    context: &dyn VaultContext,
    reference: &SecretReference,
    destination: &EgressDestination,
    rejected: Option<&str>,
) -> Result<String> {
    let vault = require_vault(context, &reference.vault_id).await?;
    let origin = url::Url::parse(&format!(
        "https://{}:{}",
        destination.host, destination.port
    ))?;
    let target = CredentialDestination::url(&format!(
        "{}{}",
        origin.origin().ascii_serialization(),
        destination.path
    ))?;
    let mut resolved = vault.resolve_secret(&reference.secret_id, &target).await?;
    if resolved.secret.is_refreshable() && rejected == Some(resolved.secret.bearer_value()) {
        resolved = vault
            .refresh_secret(&reference.secret_id, &target, resolved.revision)
            .await?;
    }
    Ok(resolved.secret.bearer_value().to_owned())
}
