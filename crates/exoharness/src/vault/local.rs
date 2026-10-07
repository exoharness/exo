#[cfg(not(feature = "basic-backend"))]
mod hosted;
#[cfg(feature = "basic-backend")]
mod legacy;
#[cfg(feature = "basic-backend")]
mod native;
#[cfg(feature = "basic-backend")]
mod oauth;
#[cfg(all(test, feature = "basic-backend"))]
mod tests;
use super::*;
use crate::SecretType;
use crate::secrets::{EncryptedSecret, SecretCipher};
use anyhow::{Context, bail};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[cfg(feature = "basic-backend")]
use std::sync::Mutex;

#[derive(Clone)]
pub(crate) struct BasicVaultStore {
    inner: Arc<Inner>,
}

struct Inner {
    storage: Storage,
    cipher: SecretCipher,
    #[cfg(feature = "basic-backend")]
    native: native::RefreshState,
}

enum Storage {
    #[cfg(feature = "basic-backend")]
    File(std::path::PathBuf),
    #[cfg(not(feature = "basic-backend"))]
    Host {
        store: crate::storage::BasicObjectStore,
        lock: tokio::sync::Mutex<()>,
    },
    #[cfg(feature = "basic-backend")]
    Memory(Mutex<Catalog>),
}

#[derive(Default, Serialize, Deserialize)]
struct Catalog {
    vaults: Vec<StoredVault>,
}

#[derive(Serialize, Deserialize)]
struct StoredVault<M = SecretMetadata> {
    record: VaultRecord,
    secrets: Vec<StoredSecret<M>>,
}

#[derive(Serialize, Deserialize)]
struct StoredSecret<M = SecretMetadata> {
    metadata: M,
    secret: EncryptedSecret,
}

impl BasicVaultStore {
    #[cfg(not(feature = "basic-backend"))]
    pub(crate) fn hosted(store: crate::storage::BasicObjectStore, cipher: SecretCipher) -> Self {
        Self {
            inner: Arc::new(Inner {
                storage: Storage::Host {
                    store,
                    lock: tokio::sync::Mutex::new(()),
                },
                cipher,
            }),
        }
    }

    async fn access<T: Send + 'static>(
        &self,
        write: bool,
        operation: impl FnOnce(&mut Catalog, &SecretCipher) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        match &self.inner.storage {
            #[cfg(not(feature = "basic-backend"))]
            Storage::Host { store, lock } => {
                let _guard = lock.lock().await;
                let mut catalog = store
                    .get_json_if_exists::<Catalog>("vaults/vaults.json")
                    .await?
                    .unwrap_or_default();
                let result = operation(&mut catalog, &self.inner.cipher)?;
                if write {
                    store.put_json("vaults/vaults.json", &catalog).await?;
                }
                Ok(result)
            }
            #[cfg(feature = "basic-backend")]
            Storage::Memory(catalog) => {
                let mut catalog = catalog
                    .lock()
                    .map_err(|_| anyhow::anyhow!("vault store lock is poisoned"))?;
                operation(&mut catalog, &self.inner.cipher)
            }
            #[cfg(feature = "basic-backend")]
            Storage::File(root) => {
                let root = root.clone();
                let cipher = self.inner.cipher.clone();
                tokio::task::spawn_blocking(move || {
                    native::access_file(root, cipher, write, operation)
                })
                .await?
            }
        }
    }

    pub(crate) async fn global_vault(&self) -> Result<Arc<dyn VaultHandle>> {
        if let Some(record) = self
            .access(false, |catalog, _| {
                Ok(catalog
                    .vaults
                    .iter()
                    .find(|v| v.record.name == "global")
                    .map(|v| v.record.clone()))
            })
            .await?
        {
            return Ok(self.handle(record));
        }
        let record = self
            .access(true, |catalog, _| {
                if let Some(vault) = catalog.vaults.iter().find(|v| v.record.name == "global") {
                    return Ok(vault.record.clone());
                }
                Ok(create(catalog, "global".into()))
            })
            .await?;
        Ok(self.handle(record))
    }

    pub(crate) async fn create_vault(&self, name: &str) -> Result<Arc<dyn VaultHandle>> {
        let name = name.to_owned();
        let record = self
            .access(true, move |catalog, _| {
                if name.trim().is_empty() {
                    bail!("vault name must not be empty");
                }
                if catalog.vaults.iter().any(|v| v.record.name == name) {
                    bail!("vault already exists: {name}");
                }
                Ok(create(catalog, name))
            })
            .await?;
        Ok(self.handle(record))
    }

    pub(crate) async fn list_vaults(&self) -> Result<Vec<Arc<dyn VaultHandle>>> {
        let records = self
            .access(false, |catalog, _| {
                Ok(catalog
                    .vaults
                    .iter()
                    .map(|v| v.record.clone())
                    .collect::<Vec<_>>())
            })
            .await?;
        Ok(records
            .into_iter()
            .map(|record| self.handle(record))
            .collect())
    }

    pub(crate) async fn get_vault(&self, id: &VaultId) -> Result<Option<Arc<dyn VaultHandle>>> {
        let id = *id;
        let record = self
            .access(false, move |catalog, _| {
                Ok(catalog
                    .vaults
                    .iter()
                    .find(|v| v.record.id == id)
                    .map(|v| v.record.clone()))
            })
            .await?;
        Ok(record.map(|record| self.handle(record)))
    }

    pub(crate) async fn delete_vault(&self, id: &VaultId) -> Result<()> {
        let id = *id;
        self.access(true, move |catalog, _| {
            if vault(catalog, id)?.record.name == "global" {
                bail!("cannot delete the global vault");
            }
            catalog.vaults.retain(|v| v.record.id != id);
            Ok(())
        })
        .await
    }

    #[cfg(feature = "basic-backend")]
    pub(crate) async fn import_secret(
        &self,
        vault_id: VaultId,
        metadata: SecretMetadata,
        secret: Secret,
    ) -> Result<()> {
        self.access(true, move |catalog, cipher| {
            let vault = vault(catalog, vault_id)?;
            if let Some(stored) = vault.secrets.iter().find(|s| s.metadata.id == metadata.id) {
                let existing = cipher.decrypt_bound(
                    &stored.secret,
                    &serde_json::to_vec(&(vault_id, &stored.metadata))?,
                )?;
                if stored.metadata != metadata || existing != secret {
                    bail!("migrated secret conflicts with an existing vault entry");
                }
                return Ok(());
            }
            let encrypted = encrypt(cipher, vault_id, &metadata, &secret)?;
            vault.secrets.push(StoredSecret {
                metadata,
                secret: encrypted,
            });
            Ok(())
        })
        .await
    }

    fn handle(&self, record: VaultRecord) -> Arc<dyn VaultHandle> {
        Arc::new(BasicVaultHandle {
            store: self.clone(),
            record,
        })
    }
}

fn create(catalog: &mut Catalog, name: String) -> VaultRecord {
    let record = VaultRecord {
        id: Uuid7::now(),
        name,
        created_at: Utc::now(),
    };
    catalog.vaults.push(StoredVault {
        record: record.clone(),
        secrets: vec![],
    });
    record
}

fn vault(catalog: &mut Catalog, id: VaultId) -> Result<&mut StoredVault> {
    catalog
        .vaults
        .iter_mut()
        .find(|v| v.record.id == id)
        .with_context(|| format!("vault {id} is unavailable"))
}

fn encrypt(
    cipher: &SecretCipher,
    vault_id: VaultId,
    metadata: &SecretMetadata,
    secret: &Secret,
) -> Result<EncryptedSecret> {
    validate_secret(secret, metadata.policy.as_ref())?;
    cipher.encrypt_bound(secret, &serde_json::to_vec(&(vault_id, metadata))?)
}

fn secret_type(secret: &Secret) -> SecretType {
    match secret {
        Secret::Key { .. } => SecretType::Key,
        Secret::Oauth { .. } => SecretType::Oauth,
        Secret::GithubCli { .. } => SecretType::GithubCli,
    }
}

#[derive(Clone)]
struct BasicVaultHandle {
    store: BasicVaultStore,
    record: VaultRecord,
}

#[async_trait]
impl VaultHandle for BasicVaultHandle {
    fn record(&self) -> &VaultRecord {
        &self.record
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        let id = self.record.id;
        self.store
            .access(false, move |catalog, _| {
                Ok(vault(catalog, id)?
                    .secrets
                    .iter()
                    .map(|s| s.metadata.clone())
                    .collect())
            })
            .await
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        let vault_id = self.record.id;
        self.store
            .access(true, move |catalog, cipher| {
                let vault = vault(catalog, vault_id)?;
                if request.name.trim().is_empty() {
                    bail!("secret name must not be empty");
                }
                let target = request
                    .policy
                    .map(|target| target.normalized())
                    .transpose()?;
                if vault.secrets.iter().any(|s| {
                    s.metadata.name == request.name
                        || target.as_ref().is_some_and(CredentialPolicy::is_resource)
                            && s.metadata.policy == target
                }) {
                    bail!(
                        "a secret with this name or destination already exists in vault {vault_id}"
                    );
                }
                let metadata = SecretMetadata {
                    id: Uuid7::now(),
                    name: request.name,
                    r#type: secret_type(&request.secret),
                    policy: target,
                    revision: 1,
                    created_at: Utc::now(),
                };
                let secret = encrypt(cipher, vault_id, &metadata, &request.secret)?;
                let id = metadata.id;
                vault.secrets.push(StoredSecret { metadata, secret });
                Ok(id)
            })
            .await
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        let vault_id = self.record.id;
        let id = *id;
        self.store
            .access(false, move |catalog, cipher| {
                vault(catalog, vault_id)?
                    .secrets
                    .iter()
                    .find(|s| s.metadata.id == id)
                    .map(|s| {
                        cipher.decrypt_bound(
                            &s.secret,
                            &serde_json::to_vec(&(vault_id, &s.metadata))?,
                        )
                    })
                    .transpose()
            })
            .await
    }

    async fn update_secret(
        &self,
        id: &SecretId,
        request: crate::UpdateSecretRequest,
    ) -> Result<SecretMetadata> {
        anyhow::ensure!(
            request.secret.is_some() || request.policy.is_some(),
            "provide a secret value or destination to update"
        );
        let vault_id = self.record.id;
        let id = *id;
        self.store
            .access(true, move |catalog, cipher| {
                let vault = vault(catalog, vault_id)?;
                let target = request
                    .policy
                    .map(|target| target.normalized())
                    .transpose()?;
                if target.as_ref().is_some_and(CredentialPolicy::is_resource)
                    && vault
                        .secrets
                        .iter()
                        .any(|stored| stored.metadata.id != id && stored.metadata.policy == target)
                {
                    bail!("a secret with this resource policy already exists in the vault");
                }
                let stored = vault
                    .secrets
                    .iter_mut()
                    .find(|s| s.metadata.id == id)
                    .context("secret is unavailable")?;
                let secret = match request.secret {
                    Some(secret) => secret,
                    None => cipher.decrypt_bound(
                        &stored.secret,
                        &serde_json::to_vec(&(vault_id, &stored.metadata))?,
                    )?,
                };
                let mut metadata = stored.metadata.clone();
                metadata.revision = metadata
                    .revision
                    .checked_add(1)
                    .context("secret revision overflow")?;
                if let Some(target) = target {
                    metadata.policy = Some(target);
                }
                metadata.r#type = secret_type(&secret);
                let encrypted = encrypt(cipher, vault_id, &metadata, &secret)?;
                stored.metadata = metadata.clone();
                stored.secret = encrypted;
                Ok(metadata)
            })
            .await
    }

    async fn delete_secret(&self, id: &SecretId) -> Result<()> {
        let vault_id = self.record.id;
        let id = *id;
        self.store
            .access(true, move |catalog, _| {
                let vault = vault(catalog, vault_id)?;
                if !vault.secrets.iter().any(|s| s.metadata.id == id) {
                    bail!("secret is unavailable");
                }
                vault.secrets.retain(|s| s.metadata.id != id);
                Ok(())
            })
            .await
    }

    async fn resolve_secret(
        &self,
        id: &SecretId,
        target: &CredentialDestination,
    ) -> Result<ResolvedSecret> {
        self.resolve_credentials(id, target).await
    }

    async fn refresh_secret(
        &self,
        id: &SecretId,
        target: &CredentialDestination,
        rejected_revision: u64,
    ) -> Result<ResolvedSecret> {
        self.refresh_task(id, target, rejected_revision, true).await
    }
}

impl BasicVaultHandle {
    async fn read_for_destination(
        &self,
        id: &SecretId,
        target: &CredentialDestination,
    ) -> Result<ResolvedSecret> {
        let vault_id = self.record.id;
        let id = *id;
        let target = target.normalized()?;
        self.store
            .access(false, move |catalog, cipher| {
                let stored = vault(catalog, vault_id)?
                    .secrets
                    .iter()
                    .find(|s| s.metadata.id == id)
                    .context("secret is unavailable")?;
                anyhow::ensure!(stored.metadata.policy.as_ref().is_some_and(|policy| policy.permits(&target)),
                    "secret {:?} is not authorized for {}; update its policy with --allow-origin or --allow-url", stored.metadata.name, target.as_str());
                let secret = cipher.decrypt_bound(
                    &stored.secret,
                    &serde_json::to_vec(&(vault_id, &stored.metadata))?,
                )?;
                validate_secret(&secret, stored.metadata.policy.as_ref())?;
                Ok(ResolvedSecret {
                    revision: stored.metadata.revision,
                    secret,
                })
            })
            .await
    }
}
