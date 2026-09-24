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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretTarget {
    Mcp { server_url: String },
    Http { origin: String },
}

impl SecretTarget {
    pub fn http(origin: &str) -> Result<Self> {
        let url = url::Url::parse(origin).context("invalid HTTP credential origin")?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!(
                "HTTP credentials require an HTTPS origin without a path, query, embedded credentials, or fragment"
            );
        }
        Ok(Self::Http {
            origin: url.origin().ascii_serialization(),
        })
    }

    pub fn mcp(server_url: &str) -> Result<Self> {
        let url = url::Url::parse(server_url).context("invalid MCP credential URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            bail!(
                "MCP credentials require an HTTP(S) URL without embedded credentials or a fragment"
            );
        }
        Ok(Self::Mcp {
            server_url: url.to_string(),
        })
    }

    pub fn normalized(&self) -> Result<Self> {
        match self {
            Self::Mcp { server_url } => Self::mcp(server_url),
            Self::Http { origin } => Self::http(origin),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretReference {
    pub vault_id: VaultId,
    pub secret_id: SecretId,
}

#[async_trait]
pub trait VaultContext: Send + Sync {
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
    pub resource: Option<String>,
    pub scopes: Vec<String>,
}

pub fn validate_secret(secret: &Secret, target: Option<&SecretTarget>) -> Result<()> {
    if let Some(target) = target {
        let token = match secret {
            Secret::Key { value } => value,
            Secret::Oauth {
                access_token,
                refresh,
                ..
            } if matches!(target, SecretTarget::Mcp { .. }) => {
                if let Some(refresh) = refresh {
                    let url = url::Url::parse(&refresh.token_endpoint)
                        .context("invalid OAuth token endpoint")?;
                    let loopback = match url.host() {
                        Some(url::Host::Domain("localhost")) => true,
                        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                        _ => false,
                    };
                    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
                        || !url.username().is_empty()
                        || url.password().is_some()
                        || url.fragment().is_some()
                    {
                        bail!(
                            "OAuth token endpoint must use HTTPS or loopback HTTP without embedded credentials or a fragment"
                        );
                    }
                    if refresh.client_id.is_empty() {
                        bail!("OAuth client ID is missing");
                    }
                }
                access_token
            }
            Secret::Oauth { .. } => bail!("HTTP credentials currently require a static key"),
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
    async fn update_secret(&self, id: &SecretId, secret: Secret) -> Result<SecretMetadata>;
    async fn delete_secret(&self, id: &SecretId) -> Result<()>;
    async fn resolve_secret(&self, id: &SecretId, target: &SecretTarget) -> Result<ResolvedSecret>;
    async fn refresh_secret(
        &self,
        _id: &SecretId,
        _target: &SecretTarget,
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

pub(crate) fn model_endpoint(
    base_url: Option<&str>,
    environment_variable: &str,
) -> Result<url::Url> {
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

pub(crate) async fn model_credential_vault(
    context: &dyn VaultContext,
    reference: &SecretReference,
    endpoint: &url::Url,
) -> Result<Arc<dyn VaultHandle>> {
    let vault = require_vault(context, &reference.vault_id).await?;
    let metadata = vault
        .list_secrets()
        .await?
        .into_iter()
        .find(|secret| secret.id == reference.secret_id)
        .context("sandbox model credential is unavailable")?;
    anyhow::ensure!(
        metadata.r#type == crate::SecretType::Key,
        "sandbox model credentials currently require an API key"
    );
    if let Some(target) = metadata.target {
        anyhow::ensure!(
            target == SecretTarget::http(&endpoint.origin().ascii_serialization())?,
            "model credential is not authorized for this endpoint"
        );
    }
    Ok(vault)
}
