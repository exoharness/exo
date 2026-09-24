use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use exo_mcp::{McpCredential, McpCredentialProvider, McpServerConfig};
use exoharness::vault::{
    SecretReference, SecretTarget, VaultContext, VaultHandle, VaultId, VaultRecord,
};
use exoharness::{ReadArtifactRequest, Secret, ThreadHandle, WriteArtifactRequest};
use serde::{Deserialize, Serialize};

const VAULT_SELECTION_PATH: &str = "managed-agents/vault.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpCredentialBinding {
    pub server_name: String,
    pub target: SecretTarget,
    pub secret: Option<SecretReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VaultSelection {
    pub vaults: Vec<VaultRecord>,
    pub bindings: Vec<McpCredentialBinding>,
}

pub async fn find_vault(store: &dyn VaultContext, reference: &str) -> Result<Arc<dyn VaultHandle>> {
    let id = reference.parse::<VaultId>().ok();
    let matches: Vec<_> = store
        .list_vaults()
        .await?
        .into_iter()
        .filter(|vault| Some(vault.record().id) == id || vault.record().name == reference)
        .collect();
    match matches.as_slice() {
        [vault] => Ok(vault.clone()),
        [] => bail!("vault not found: {reference}"),
        _ => bail!("ambiguous vault reference: {reference}; use its id"),
    }
}

impl VaultSelection {
    pub async fn from_vaults(
        vaults: &[Arc<dyn VaultHandle>],
        servers: &[McpServerConfig],
    ) -> Result<Self> {
        let secrets =
            futures::future::try_join_all(vaults.iter().map(|vault| vault.list_secrets())).await?;
        let bindings = servers
            .iter()
            .map(|server| {
                let target = SecretTarget::mcp(&server.url)?;
                let secret = vaults
                    .iter()
                    .zip(&secrets)
                    .rev()
                    .find_map(|(vault, secrets)| {
                        secrets
                            .iter()
                            .find(|s| s.target.as_ref() == Some(&target))
                            .map(|s| SecretReference {
                                vault_id: vault.record().id,
                                secret_id: s.id,
                            })
                    });
                Ok(McpCredentialBinding {
                    server_name: server.name.clone(),
                    target,
                    secret,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            vaults: vaults.iter().map(|v| v.record().clone()).collect(),
            bindings,
        })
    }

    pub fn validate_servers(&self, servers: &[McpServerConfig]) -> Result<()> {
        let targets: Vec<_> = servers
            .iter()
            .map(|s| Ok((s.name.as_str(), SecretTarget::mcp(&s.url)?)))
            .collect::<Result<_>>()?;
        if targets.len() != self.bindings.len()
            || targets.iter().any(|(name, target)| {
                !self
                    .bindings
                    .iter()
                    .any(|b| b.server_name == *name && &b.target == target)
            })
        {
            bail!("MCP destinations have changed since this thread started; start a new thread");
        }
        if self.bindings.iter().any(|b| {
            b.secret
                .as_ref()
                .is_some_and(|r| !self.vaults.iter().any(|v| v.id == r.vault_id))
        }) {
            bail!("thread credential does not belong to its selected vault");
        }
        Ok(())
    }

    async fn validate_context(&self, context: &dyn VaultContext) -> Result<()> {
        futures::future::try_join_all(
            self.vaults
                .iter()
                .map(|v| exoharness::vault::require_vault(context, &v.id)),
        )
        .await?;
        Ok(())
    }

    pub async fn save(&self, thread: &dyn ThreadHandle) -> Result<()> {
        if let Some(saved) = load_selection(thread).await? {
            if saved != *self {
                bail!("cannot change an existing thread's vault selection");
            }
            return Ok(());
        }
        self.validate_context(thread).await?;
        thread
            .write_artifact(WriteArtifactRequest {
                path: VAULT_SELECTION_PATH.into(),
                contents: serde_json::to_vec(self)?,
            })
            .await?;
        Ok(())
    }
}

pub async fn load_selection(thread: &dyn ThreadHandle) -> Result<Option<VaultSelection>> {
    let Some(version) = thread
        .list_artifacts()
        .await?
        .into_iter()
        .filter(|v| v.path == VAULT_SELECTION_PATH)
        .max_by_key(|v| v.version)
    else {
        return Ok(None);
    };
    let artifact = thread
        .read_artifact(ReadArtifactRequest {
            artifact_id: version.artifact_id,
            version: Some(version.version),
        })
        .await?
        .context("saved thread vault selection is missing")?;
    let selected: VaultSelection =
        serde_json::from_slice(&artifact.contents).context("invalid saved vault selection")?;
    selected.validate_context(thread).await?;
    Ok(Some(selected))
}

pub struct VaultMcpCredentials {
    vaults: Vec<Arc<dyn VaultHandle>>,
    selection: VaultSelection,
}

impl VaultMcpCredentials {
    pub fn new(vaults: Vec<Arc<dyn VaultHandle>>, selection: VaultSelection) -> Self {
        Self { vaults, selection }
    }
}

#[async_trait]
impl McpCredentialProvider for VaultMcpCredentials {
    fn authentication_failure_context(&self, server: &McpServerConfig, supplied: bool) -> String {
        let vaults = self
            .selection
            .vaults
            .iter()
            .map(|v| v.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if !supplied {
            return format!(
                "MCP server {} requires authentication, but no matching vault secret was selected for {} (selected vaults: [{}]). Add a secret for this MCP server URL to a selected vault, or attach the vault containing it, then start a new thread",
                server.name, server.url, vaults
            );
        }
        let secrets = self
            .selection
            .bindings
            .iter()
            .filter(|binding| binding.server_name == server.name)
            .filter_map(|binding| binding.secret.as_ref())
            .map(|reference| format!("{} (vault {})", reference.secret_id, reference.vault_id))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "MCP server {} rejected the vault credential selected for {} (secret: [{}]; selected vaults: [{}]). Check the token's validity and permissions",
            server.name, server.url, secrets, vaults
        )
    }

    async fn resolve(&self, server: &McpServerConfig) -> Result<Option<McpCredential>> {
        self.credential(server, None).await
    }

    async fn refresh(
        &self,
        server: &McpServerConfig,
        rejected: &McpCredential,
    ) -> Result<Option<McpCredential>> {
        self.credential(server, Some(rejected)).await
    }
}

impl VaultMcpCredentials {
    async fn credential(
        &self,
        server: &McpServerConfig,
        rejected: Option<&McpCredential>,
    ) -> Result<Option<McpCredential>> {
        let target = SecretTarget::mcp(&server.url)?;
        let binding = self
            .selection
            .bindings
            .iter()
            .find(|binding| binding.server_name == server.name && binding.target == target)
            .context("MCP server has no saved credential selection")?;
        let Some(reference) = &binding.secret else {
            return Ok(None);
        };
        let vault = self
            .vaults
            .iter()
            .find(|v| v.record().id == reference.vault_id)
            .context("credential vault is unavailable in this context")?;
        let mut resolved = vault.resolve_secret(&reference.secret_id, &target).await?;
        if let Some(rejected) = rejected {
            let version = format!(
                "{}:{}:{}",
                reference.vault_id, reference.secret_id, resolved.revision
            );
            if version == rejected.version {
                if !matches!(
                    &resolved.secret,
                    Secret::Oauth {
                        refresh_token: Some(_),
                        refresh: Some(_),
                        ..
                    }
                ) {
                    return Ok(None);
                }
                resolved = vault
                    .refresh_secret(&reference.secret_id, &target, resolved.revision)
                    .await?;
            }
        }
        let token = match resolved.secret {
            Secret::Key { value } => value,
            Secret::Oauth { access_token, .. } => access_token,
        };
        Ok(Some(McpCredential {
            version: format!(
                "{}:{}:{}",
                reference.vault_id, reference.secret_id, resolved.revision
            ),
            token,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exoharness::ExoHarness;
    use exoharness::{
        BasicExoHarness, BasicExoHarnessConfig, PutSecretRequest, SandboxBackendRegistration,
        SandboxProvider, SecretBackendChoice,
    };

    #[tokio::test]
    async fn saved_selection_pins_each_servers_destination() -> Result<()> {
        let mut servers = vec![
            McpServerConfig {
                name: "tickets".into(),
                url: "https://tickets.example/mcp".into(),
                allowed_tools: None,
                blocked_tools: vec![],
            },
            McpServerConfig {
                name: "docs".into(),
                url: "https://docs.example/mcp".into(),
                allowed_tools: None,
                blocked_tools: vec![],
            },
        ];
        let selection = VaultSelection::from_vaults(&[], &servers).await?;
        selection.validate_servers(&servers)?;
        servers.reverse();
        selection.validate_servers(&servers)?;
        servers[0].name = "tickets".into();
        servers[1].name = "docs".into();
        assert!(selection.validate_servers(&servers).is_err());
        servers[0].name = "docs".into();
        servers[1].name = "tickets".into();
        servers[0].url = "http://127.0.0.1:8000/mcp".into();
        assert!(selection.validate_servers(&servers).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn mcp_composes_vaults_and_keeps_its_selection_across_rotation_and_revocation()
    -> Result<()> {
        let harness = BasicExoHarness::in_memory(
            BasicExoHarnessConfig {
                root: Default::default(),
                secret_backend: SecretBackendChoice::Static([1; 32]),
                sandbox_default: SandboxProvider::LocalProcess,
                sandbox_policy: None,
                sandbox_backends: vec![SandboxBackendRegistration::local_process()],
            },
            None,
        )
        .await?;
        let global = exoharness::vault::global_vault(&harness).await?;
        let user = harness.create_vault("alice").await?;
        let target = SecretTarget::mcp("https://example.com/mcp/")?;
        global
            .put_secret(PutSecretRequest {
                name: "mcp".into(),
                target: Some(target.clone()),
                secret: Secret::Key {
                    value: "global-token".into(),
                },
            })
            .await?;
        let servers = vec![McpServerConfig {
            name: "service".into(),
            url: "https://example.com/mcp/".into(),
            allowed_tools: None,
            blocked_tools: vec![],
        }];
        let public = VaultSelection::from_vaults(std::slice::from_ref(&user), &servers).await?;
        let public_provider = VaultMcpCredentials::new(vec![user.clone()], public);
        assert!(public_provider.resolve(&servers[0]).await?.is_none());
        let id = user
            .put_secret(PutSecretRequest {
                name: "mcp".into(),
                target: Some(target),
                secret: Secret::Key {
                    value: "user-token".into(),
                },
            })
            .await?;
        let agent = harness
            .new_agent(exoharness::NewAgentRequest {
                slug: "analyst".into(),
                name: "Analyst".into(),
                vaults: vec![],
            })
            .await?;
        let thread = agent
            .new_thread(exoharness::NewThreadRequest {
                vaults: vec![user.record().id],
                ..Default::default()
            })
            .await?;
        let selected = VaultSelection::from_vaults(&thread.list_vaults().await?, &servers).await?;
        selected.save(thread.as_ref()).await?;
        assert_eq!(
            load_selection(thread.as_ref()).await?,
            Some(selected.clone())
        );
        assert_eq!(selected.bindings[0].secret.as_ref().unwrap().secret_id, id);
        let provider = VaultMcpCredentials::new(thread.list_vaults().await?, selected);
        let first = provider.resolve(&servers[0]).await?.unwrap();
        assert_eq!(first.token, "user-token");
        user.update_secret(
            &id,
            Secret::Key {
                value: "rotated".into(),
            },
        )
        .await?;
        let second = provider.resolve(&servers[0]).await?.unwrap();
        assert_eq!(second.token, "rotated");
        assert_ne!(second.version, first.version);
        user.delete_secret(&id).await?;
        assert!(provider.resolve(&servers[0]).await.is_err());
        Ok(())
    }
}
