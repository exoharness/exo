use crate::{DateTimeUtc, PutSecretRequest, Result, Secret, SecretId, SecretMetadata, Uuid7};
use anyhow::{Context, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod local;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub(crate) use local::BasicVaultStore;

pub type VaultId = Uuid7;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultRecord {
    pub id: VaultId,
    pub name: String,
    pub created_at: DateTimeUtc,
}

pub use crate::credential_policy::{CredentialDestination, CredentialPolicy};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretReference {
    pub vault_id: VaultId,
    pub secret_id: SecretId,
}

#[async_trait]
pub trait VaultContext: Send + Sync {
    fn caller(&self) -> Option<&crate::access::Caller> {
        None
    }

    async fn list_vaults(&self) -> Result<Vec<Arc<dyn VaultHandle>>>;
    async fn get_vault(&self, id: &VaultId) -> Result<Option<Arc<dyn VaultHandle>>> {
        Ok(self
            .list_vaults()
            .await?
            .into_iter()
            .find(|v| v.record().id == *id))
    }
}

#[derive(Serialize, Deserialize)]
pub struct ResolvedSecret {
    pub revision: u64,
    pub secret: Secret,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthRefresh {
    pub token_endpoint: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub client_secret_basic: bool,
    pub resource: Option<String>,
    pub scopes: Vec<String>,
}

pub fn validate_secret(secret: &Secret, policy: Option<&CredentialPolicy>) -> Result<()> {
    if policy.is_some() {
        let token = match secret {
            Secret::Key { value } => value,
            Secret::Oauth {
                access_token,
                refresh,
                ..
            } => {
                if let Some(refresh) = refresh {
                    validate_oauth_endpoint(&refresh.token_endpoint)?;
                    if refresh.client_id.is_empty() {
                        bail!("OAuth client ID is missing");
                    }
                }
                access_token
            }
        };
        if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            bail!("bearer token must be nonempty ASCII without whitespace or control characters");
        }
    }
    Ok(())
}

#[async_trait]
pub trait VaultHandle: Send + Sync {
    fn record(&self) -> &VaultRecord;
    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>>;
    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId>;
    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>>;
    async fn update_secret(
        &self,
        id: &SecretId,
        request: crate::UpdateSecretRequest,
    ) -> Result<SecretMetadata>;
    async fn delete_secret(&self, id: &SecretId) -> Result<()>;
    async fn resolve_secret(
        &self,
        id: &SecretId,
        target: &CredentialDestination,
    ) -> Result<ResolvedSecret>;
    async fn refresh_secret(
        &self,
        _id: &SecretId,
        _target: &CredentialDestination,
        _rejected_revision: u64,
    ) -> Result<ResolvedSecret> {
        bail!("OAuth refresh is not supported by this vault")
    }
}

pub async fn require_vault(
    harness: &dyn VaultContext,
    id: &VaultId,
) -> Result<Arc<dyn VaultHandle>> {
    harness
        .get_vault(id)
        .await?
        .with_context(|| format!("vault {id} is unavailable"))
}

pub(crate) async fn require_vaults(context: &dyn VaultContext, ids: &[VaultId]) -> Result<()> {
    for id in ids {
        require_vault(context, id).await?;
    }
    Ok(())
}

pub async fn global_vault(context: &dyn VaultContext) -> Result<Arc<dyn VaultHandle>> {
    context
        .list_vaults()
        .await?
        .into_iter()
        .find(|v| v.record().name == "global")
        .context("global vault is unavailable")
}

pub async fn compose_vaults(
    catalog: &dyn VaultContext,
    mut inherited: Vec<Arc<dyn VaultHandle>>,
    attached: &[VaultId],
) -> Result<Vec<Arc<dyn VaultHandle>>> {
    let vaults =
        futures::future::try_join_all(attached.iter().map(|id| require_vault(catalog, id))).await?;
    for vault in vaults {
        inherited.retain(|v| v.record().id != vault.record().id);
        inherited.push(vault);
    }
    Ok(inherited)
}

pub async fn find_secret(
    context: &dyn VaultContext,
    reference: &str,
) -> Result<Option<SecretReference>> {
    let vaults = context.list_vaults().await?;
    let secrets = futures::future::try_join_all(vaults.iter().map(|v| v.list_secrets())).await?;
    let id = reference.parse::<SecretId>().ok();
    Ok(vaults
        .iter()
        .zip(secrets)
        .rev()
        .find_map(|(vault, secrets)| {
            secrets
                .into_iter()
                .find(|s| id.map_or_else(|| s.name == reference, |id| s.id == id))
                .map(|s| SecretReference {
                    vault_id: vault.record().id,
                    secret_id: s.id,
                })
        }))
}

pub fn model_endpoint(base_url: Option<&str>, environment_variable: &str) -> Result<url::Url> {
    let default_url = match environment_variable {
        "OPENAI_API_KEY" => "https://api.openai.com/v1",
        "ANTHROPIC_API_KEY" => "https://api.anthropic.com",
        "GEMINI_API_KEY" => "https://generativelanguage.googleapis.com",
        _ => bail!("unsupported sandbox model credential variable: {environment_variable}"),
    };
    let url = url::Url::parse(base_url.unwrap_or(default_url))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("sandbox model credentials require an HTTPS model endpoint on port 443");
    }
    Ok(url)
}

pub async fn resolve_model_key(
    context: &dyn VaultContext,
    reference: &SecretReference,
    endpoint: &url::Url,
) -> Result<String> {
    let vault = require_vault(context, &reference.vault_id).await?;
    let destination = CredentialDestination::origin(&endpoint.origin().ascii_serialization())?;
    let secret = vault
        .resolve_secret(&reference.secret_id, &destination)
        .await?
        .secret;
    match secret {
        Secret::Key { value } => Ok(value),
        Secret::Oauth { access_token, .. } => Ok(access_token),
    }
}

pub fn validate_oauth_endpoint(endpoint: &str) -> Result<()> {
    let url = url::Url::parse(endpoint).context("invalid OAuth token endpoint")?;
    let loopback = crate::credential_policy::is_loopback(&url);
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "OAuth token endpoint must use HTTPS or loopback HTTP without embedded credentials or a fragment"
        );
    }

    Ok(())
}

pub async fn credential_policy(
    context: &dyn VaultContext,
    reference: &SecretReference,
) -> Result<CredentialPolicy> {
    require_vault(context, &reference.vault_id).await?.list_secrets().await?.into_iter()
        .find(|secret| secret.id == reference.secret_id).context("credential is unavailable")?
        .policy.context("credential has no permitted destinations; set its policy with --allow-origin, --allow-url, or --policy")
}
