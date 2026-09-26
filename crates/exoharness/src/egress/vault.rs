use super::EgressDestination;
use crate::vault::{CredentialDestination, SecretReference, VaultContext, require_vault};
use crate::{Result, Secret};

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
    if matches!((&resolved.secret, rejected), (Secret::Oauth { access_token, refresh_token: Some(_), refresh: Some(_), .. }, Some(rejected)) if access_token == rejected)
    {
        resolved = vault
            .refresh_secret(&reference.secret_id, &target, resolved.revision)
            .await?;
    }
    Ok(match resolved.secret {
        Secret::Key { value } => value,
        Secret::Oauth { access_token, .. } => access_token,
    })
}
