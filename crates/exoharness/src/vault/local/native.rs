use super::*;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct RefreshState {
    refresh: Arc<tokio::sync::Mutex<()>>,
    pub(super) github_checked: Mutex<HashMap<SecretId, (u64, Instant)>>,
}
use crate::secrets::{lock_secret_file, private_directory};
use std::io::Write;

impl BasicVaultStore {
    pub(crate) fn new(root: Option<std::path::PathBuf>, cipher: SecretCipher) -> Result<Self> {
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
                native: RefreshState::default(),
            }),
        })
    }
}

pub(super) fn access_file<T>(
    root: std::path::PathBuf,
    cipher: SecretCipher,
    write: bool,
    operation: impl FnOnce(&mut Catalog, &SecretCipher) -> Result<T>,
) -> Result<T> {
    let lock = lock_secret_file(&root.join("vaults.lock"))?;
    let path = root.join("vaults.json");
    let (mut catalog, migrated) = match std::fs::read(&path) {
        Ok(bytes) => legacy::read_catalog(&bytes, &cipher)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Catalog::default(), false),
        Err(error) => return Err(error.into()),
    };
    if !write && !migrated {
        drop(lock);
        return operation(&mut catalog, &cipher);
    }
    let result = operation(&mut catalog, &cipher)?;
    let mut file = tempfile::NamedTempFile::new_in(&root)?;
    file.write_all(&serde_json::to_vec_pretty(&catalog)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    std::fs::File::open(root)?.sync_all()?;
    Ok(result)
}
impl BasicVaultHandle {
    pub(super) async fn resolve_credentials(
        &self,
        id: &SecretId,
        target: &CredentialDestination,
    ) -> Result<ResolvedSecret> {
        let resolved = self.read_for_destination(id, target).await?;
        let needs_refresh = match &resolved.secret {
            Secret::GithubCli { .. } => !self.github_cache_fresh(id, resolved.revision)?,
            secret => oauth::needs_refresh(secret),
        };
        if !needs_refresh {
            return Ok(resolved);
        }
        self.refresh_task(id, target, resolved.revision, false)
            .await
    }
}
const GITHUB_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

impl BasicVaultHandle {
    fn github_cache_fresh(&self, id: &SecretId, revision: u64) -> Result<bool> {
        Ok(self
            .store
            .inner
            .native
            .github_checked
            .lock()
            .map_err(|_| anyhow::anyhow!("GitHub credential cache lock is poisoned"))?
            .get(id)
            .is_some_and(|(checked_revision, at)| {
                *checked_revision == revision && at.elapsed() < GITHUB_CACHE_TTL
            }))
    }

    pub(super) async fn refresh_task(
        &self,
        id: &SecretId,
        target: &CredentialDestination,
        rejected_revision: u64,
        force: bool,
    ) -> Result<ResolvedSecret> {
        let vault = self.clone();
        let id = *id;
        let target = target.clone();
        // Finish persisting rotated tokens even if the calling request is canceled.
        tokio::spawn(async move { vault.refresh(&id, &target, rejected_revision, force).await })
            .await?
    }

    async fn refresh(
        &self,
        id: &SecretId,
        target: &CredentialDestination,
        rejected_revision: u64,
        force: bool,
    ) -> Result<ResolvedSecret> {
        let _guard = self.refresh_guard().await?;
        let resolved = self.read_for_destination(id, target).await?;
        if resolved.revision != rejected_revision {
            return Ok(resolved);
        }
        let github = matches!(resolved.secret, Secret::GithubCli { .. });
        if github && !force && self.github_cache_fresh(id, resolved.revision)? {
            return Ok(resolved);
        }
        let secret = match &resolved.secret {
            Secret::GithubCli { account, .. } => {
                let value = super::github_cli_token(account).await?;
                Secret::GithubCli {
                    value,
                    account: account.clone(),
                }
            }
            _ => oauth::refresh(resolved.secret.clone()).await?,
        };
        let resolved = if secret == resolved.secret {
            let current = self.read_for_destination(id, target).await?;
            if current.revision != resolved.revision {
                return Ok(current);
            }
            current
        } else {
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
                        bail!("credential changed during refresh; retry the operation");
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
                .await?
        };
        if github {
            let mut checked = self
                .store
                .inner
                .native
                .github_checked
                .lock()
                .map_err(|_| anyhow::anyhow!("GitHub credential cache lock is poisoned"))?;
            checked.retain(|_, (_, at)| at.elapsed() < GITHUB_CACHE_TTL);
            checked.insert(*id, (resolved.revision, Instant::now()));
        }
        Ok(resolved)
    }
}

pub(super) enum RefreshGuard {
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
    pub(super) async fn refresh_guard(&self) -> Result<RefreshGuard> {
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
                    _lock: self.store.inner.native.refresh.clone().lock_owned().await,
                }),
            }
        })
        .await
        .context("timed out waiting for OAuth refresh lock")?
    }
}
