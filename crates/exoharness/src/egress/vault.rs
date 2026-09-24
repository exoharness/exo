use anyhow::{Context, bail};

use super::EgressDestination;
use crate::vault::{SecretReference, SecretTarget, VaultContext, require_vault};
use crate::{Result, Secret};

pub async fn resolve_credential(
    context: &dyn VaultContext,
    reference: &SecretReference,
    destination: &EgressDestination,
    rejected: Option<&str>,
) -> Result<String> {
    let vault = require_vault(context, &reference.vault_id).await?;
    let metadata = vault
        .list_secrets()
        .await?
        .into_iter()
        .find(|secret| secret.id == reference.secret_id)
        .context("sandbox credential is unavailable")?;
    let origin = format!("https://{}:{}", destination.host, destination.port);
    let target = match metadata.target {
        Some(SecretTarget::Http { .. }) => SecretTarget::http(&origin)?,
        Some(SecretTarget::Mcp { .. }) => {
            SecretTarget::mcp(&format!("{origin}{}", destination.path))?
        }
        None => bail!("sandbox credential has no permitted destination"),
    };
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
