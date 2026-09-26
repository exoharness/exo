mod oauth;
#[cfg(test)]
mod tests;
use super::*;
use crate::SecretType;
use crate::secrets::{EncryptedSecret, SecretCipher, lock_secret_file, private_directory};
use anyhow::{Context, bail};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub(crate) struct BasicVaultStore {
    inner: Arc<Inner>,
}

struct Inner {
    storage: Storage,
    cipher: SecretCipher,
    refresh: Arc<tokio::sync::Mutex<()>>,
}

enum Storage {
    File(PathBuf),
    Memory(Mutex<Catalog>),
}

#[derive(Default, Serialize, Deserialize)]
struct Catalog {
    vaults: Vec<StoredVault>,
}

#[derive(Serialize, Deserialize)]
struct StoredVault {
    record: VaultRecord,
    secrets: Vec<StoredSecret>,
}

#[derive(Serialize, Deserialize)]
struct StoredSecret {
    metadata: SecretMetadata,
    secret: EncryptedSecret,
}

impl BasicVaultStore {
    pub(crate) fn new(root: Option<PathBuf>, cipher: SecretCipher) -> Result<Self> {
        let storage = match root {
            Some(root) => {
                private_directory(&root)?;
                Storage::File(root.canonicalize()?)
            }
            None => Storage::Memory(Mutex::new(Catalog::default())),
        };
        Ok(Self {
            inner: Arc::new(Inner {
                storage,
                cipher,
                refresh: Arc::default(),
            }),
        })
    }

    async fn access<T: Send + 'static>(
        &self,
        write: bool,
        operation: impl FnOnce(&mut Catalog, &SecretCipher) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || match &inner.storage {
            Storage::Memory(catalog) => {
                let mut catalog = catalog
                    .lock()
                    .map_err(|_| anyhow::anyhow!("vault store lock is poisoned"))?;
                operation(&mut catalog, &inner.cipher)
            }
            Storage::File(root) => {
                let lock = lock_secret_file(&root.join("vaults.lock"))?;
                let path = root.join("vaults.json");
                let mut catalog = match std::fs::read(&path) {
                    Ok(bytes) => serde_json::from_slice(&bytes).context("reading vault catalog")?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        Catalog::default()
                    }
                    Err(error) => return Err(error.into()),
                };
                if !write {
                    drop(lock);
                    return operation(&mut catalog, &inner.cipher);
                }
                let result = operation(&mut catalog, &inner.cipher)?;
                let mut file = tempfile::NamedTempFile::new_in(root)?;
                file.write_all(&serde_json::to_vec_pretty(&catalog)?)?;
                file.as_file().sync_all()?;
                file.persist(path).map_err(|error| error.error)?;
                std::fs::File::open(root)?.sync_all()?;
                Ok(result)
            }
        })
        .await?
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
    validate_secret(secret, metadata.target.as_ref())?;
    cipher.encrypt_bound(secret, &serde_json::to_vec(&(vault_id, metadata))?)
}

fn secret_type(secret: &Secret) -> SecretType {
    match secret {
        Secret::Key { .. } => SecretType::Key,
        Secret::Oauth { .. } => SecretType::Oauth,
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
                    .target
                    .map(|target| target.normalized())
                    .transpose()?;
                if vault.secrets.iter().any(|s| {
                    s.metadata.name == request.name
                        || matches!(target, Some(SecretTarget::Mcp { .. }))
                            && s.metadata.target == target
                }) {
                    bail!(
                        "a secret with this name or destination already exists in vault {vault_id}"
                    );
                }
                let metadata = SecretMetadata {
                    id: Uuid7::now(),
                    name: request.name,
                    r#type: secret_type(&request.secret),
                    target,
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
            request.secret.is_some() || request.target.is_some(),
            "provide a secret value or destination to update"
        );
        let vault_id = self.record.id;
        let id = *id;
        self.store
            .access(true, move |catalog, cipher| {
                let vault = vault(catalog, vault_id)?;
                let target = request
                    .target
                    .map(|target| target.normalized())
                    .transpose()?;
                if matches!(target, Some(SecretTarget::Mcp { .. }))
                    && vault
                        .secrets
                        .iter()
                        .any(|stored| stored.metadata.id != id && stored.metadata.target == target)
                {
                    bail!("a secret for this MCP destination already exists in the vault");
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
                    metadata.target = Some(target);
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

    async fn resolve_secret(&self, id: &SecretId, target: &SecretTarget) -> Result<ResolvedSecret> {
        let resolved = self.read_for_destination(id, target).await?;
        if !oauth::needs_refresh(&resolved.secret) {
            return Ok(resolved);
        }
        self.refresh_secret(id, target, resolved.revision).await
    }

    async fn refresh_secret(
        &self,
        id: &SecretId,
        target: &SecretTarget,
        rejected_revision: u64,
    ) -> Result<ResolvedSecret> {
        let vault = self.clone();
        let id = *id;
        let target = target.clone();
        // Finish persisting rotated tokens even if the calling request is canceled.
        tokio::spawn(async move { vault.refresh(&id, &target, rejected_revision).await }).await?
    }
}

impl BasicVaultHandle {
    async fn refresh(
        &self,
        id: &SecretId,
        target: &SecretTarget,
        rejected_revision: u64,
    ) -> Result<ResolvedSecret> {
        let _guard = self.refresh_guard().await?;
        let resolved = self.read_for_destination(id, target).await?;
        if resolved.revision != rejected_revision {
            return Ok(resolved);
        }
        let secret = oauth::refresh(resolved.secret).await?;
        let vault_id = self.record.id;
        let id = *id;
        self.store
            .access(true, move |catalog, cipher| {
                let stored = vault(catalog, vault_id)?
                    .secrets
                    .iter_mut()
                    .find(|s| s.metadata.id == id)
                    .context("secret is unavailable")?;
                if stored.metadata.revision != resolved.revision {
                    bail!("credential changed during OAuth refresh; retry the operation");
                }
                let mut metadata = stored.metadata.clone();
                metadata.revision = metadata
                    .revision
                    .checked_add(1)
                    .context("secret revision overflow")?;
                let encrypted = encrypt(cipher, vault_id, &metadata, &secret)?;
                stored.metadata = metadata;
                stored.secret = encrypted;
                Ok(ResolvedSecret {
                    revision: stored.metadata.revision,
                    secret,
                })
            })
            .await
    }
}

enum RefreshGuard {
    File {
        _lock: std::fs::File,
    },
    Memory {
        _lock: tokio::sync::OwnedMutexGuard<()>,
    },
}

impl BasicVaultHandle {
    // One stable lock serializes refreshes across this local store. Slow token
    // endpoints can exhaust the wait below; hosted stores should lock per secret.
    // Unlinking this file would let waiters lock different inodes concurrently.
    async fn refresh_guard(&self) -> Result<RefreshGuard> {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            match &self.store.inner.storage {
                Storage::File(root) => {
                    use std::os::unix::fs::OpenOptionsExt;
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .mode(0o600)
                        .open(root.join("oauth.lock"))?;
                    loop {
                        match file.try_lock() {
                            Ok(()) => return Ok(RefreshGuard::File { _lock: file }),
                            Err(std::fs::TryLockError::WouldBlock) => {
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            }
                            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
                        }
                    }
                }
                Storage::Memory(_) => Ok(RefreshGuard::Memory {
                    _lock: self.store.inner.refresh.clone().lock_owned().await,
                }),
            }
        })
        .await
        .context("timed out waiting for OAuth refresh lock")?
    }

    async fn read_for_destination(
        &self,
        id: &SecretId,
        target: &SecretTarget,
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
                if stored.metadata.target.as_ref() != Some(&target) {
                    if let SecretTarget::Http { origin } = &target {
                        bail!("secret {:?} is not authorized for {origin}; set its HTTP destination with --http-origin {origin}", stored.metadata.name);
                    }
                    bail!("secret {:?} is not authorized for this MCP destination", stored.metadata.name);
                }
                let secret = cipher.decrypt_bound(
                    &stored.secret,
                    &serde_json::to_vec(&(vault_id, &stored.metadata))?,
                )?;
                validate_secret(&secret, Some(&target))?;
                Ok(ResolvedSecret {
                    revision: stored.metadata.revision,
                    secret,
                })
            })
            .await
    }
}
