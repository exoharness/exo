use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Result, Secret};

const MASTER_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
#[cfg(feature = "apple-keychain")]
const KEYCHAIN_SERVICE: &str = "exo-exoharness-master-key";
const MASTER_KEY_FILE_PERMS: u32 = 0o600;
const MASTER_KEY_DIR_PERMS: u32 = 0o700;

#[derive(Clone)]
pub(crate) struct SecretCipher {
    key_provider: Arc<dyn SecretKeyProvider>,
}

impl SecretCipher {
    pub(crate) fn new(key_provider: Arc<dyn SecretKeyProvider>) -> Self {
        Self { key_provider }
    }

    #[cfg(test)]
    pub(crate) fn encrypt_secret(&self, secret: &Secret) -> Result<EncryptedSecret> {
        self.encrypt_bound(secret, &[])
    }

    pub(crate) fn encrypt_bound(&self, secret: &Secret, aad: &[u8]) -> Result<EncryptedSecret> {
        let key = self.key_provider.get_or_create_key()?;
        let payload = serde_json::to_vec(secret)?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| anyhow!("invalid secret encryption key length"))?;
        let nonce = random_nonce();
        let nonce = Nonce::from(nonce);
        let ciphertext = cipher
            .encrypt(&nonce, Payload { msg: &payload, aad })
            .context("failed to encrypt secret payload")?;
        Ok(EncryptedSecret {
            algorithm: SecretEncryptionAlgorithm::Aes256Gcm,
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }

    pub(crate) fn decrypt_secret(&self, encrypted: &EncryptedSecret) -> Result<Secret> {
        self.decrypt_bound(encrypted, &[])
    }

    pub(crate) fn decrypt_bound(&self, encrypted: &EncryptedSecret, aad: &[u8]) -> Result<Secret> {
        match encrypted.algorithm {
            SecretEncryptionAlgorithm::Aes256Gcm => {}
        }
        if encrypted.nonce.len() != NONCE_LEN {
            bail!("invalid secret nonce length");
        }
        let nonce: [u8; NONCE_LEN] = encrypted
            .nonce
            .clone()
            .try_into()
            .map_err(|_| anyhow!("invalid secret nonce length"))?;
        let key = self.key_provider.get_or_create_key()?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| anyhow!("invalid secret encryption key length"))?;
        let plaintext = cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &encrypted.ciphertext,
                    aad,
                },
            )
            .context("failed to decrypt secret payload")?;
        serde_json::from_slice(&plaintext).map_err(Into::into)
    }
}

pub(crate) trait SecretKeyProvider: Send + Sync {
    fn get_or_create_key(&self) -> Result<[u8; MASTER_KEY_LEN]>;
}

pub(crate) struct StaticSecretKeyProvider {
    key: [u8; MASTER_KEY_LEN],
}

impl StaticSecretKeyProvider {
    pub(crate) fn new(key: [u8; MASTER_KEY_LEN]) -> Self {
        Self { key }
    }
}

impl SecretKeyProvider for StaticSecretKeyProvider {
    fn get_or_create_key(&self) -> Result<[u8; MASTER_KEY_LEN]> {
        Ok(self.key)
    }
}

#[cfg(feature = "apple-keychain")]
pub(crate) struct AppleKeychainSecretKeyProvider {
    account: String,
    key: OnceLock<[u8; MASTER_KEY_LEN]>,
}

#[cfg(feature = "apple-keychain")]
impl AppleKeychainSecretKeyProvider {
    pub(crate) fn new(account: String) -> Self {
        Self {
            account,
            key: OnceLock::new(),
        }
    }
}

#[cfg(feature = "apple-keychain")]
impl SecretKeyProvider for AppleKeychainSecretKeyProvider {
    fn get_or_create_key(&self) -> Result<[u8; MASTER_KEY_LEN]> {
        use keyring_core::{Entry, Error as KeyringError};

        if let Some(key) = self.key.get() {
            return Ok(*key);
        }
        let _lock = lock_secret_file(&default_master_key_path()?.with_extension("keychain.lock"))?;
        ensure_apple_keychain_store()?;
        let entry = Entry::new(KEYCHAIN_SERVICE, &self.account)?;
        let key = match entry.get_password() {
            Ok(serialized) => deserialize_key(&serialized)?,
            Err(KeyringError::NoEntry) => {
                let key = random_master_key();
                entry
                    .set_password(&serde_json::to_string(&key.to_vec())?)
                    .context("failed to persist exoharness master key in keychain")?;
                key
            }
            Err(error) => return Err(error.into()),
        };
        Ok(*self.key.get_or_init(|| key))
    }
}

pub(crate) struct FileBackedSecretKeyProvider {
    path: PathBuf,
    key: OnceLock<[u8; MASTER_KEY_LEN]>,
}

impl FileBackedSecretKeyProvider {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            key: OnceLock::new(),
        }
    }
}

impl SecretKeyProvider for FileBackedSecretKeyProvider {
    fn get_or_create_key(&self) -> Result<[u8; MASTER_KEY_LEN]> {
        if let Some(key) = self.key.get() {
            return Ok(*key);
        }
        let _lock = lock_secret_file(&self.path.with_extension("lock"))?;
        let key = match std::fs::read(&self.path) {
            Ok(bytes) => parse_master_key_bytes(&bytes)
                .with_context(|| format!("reading master key at {}", self.path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key = random_master_key();
                write_master_key_file(&self.path, &key)?;
                key
            }
            Err(error) => {
                return Err(anyhow::Error::from(error)
                    .context(format!("reading master key at {}", self.path.display())));
            }
        };
        Ok(*self.key.get_or_init(|| key))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EncryptedSecret {
    pub(crate) algorithm: SecretEncryptionAlgorithm,
    pub(crate) nonce: Vec<u8>,
    pub(crate) ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SecretEncryptionAlgorithm {
    Aes256Gcm,
}

#[cfg(feature = "apple-keychain")]
fn deserialize_key(serialized: &str) -> Result<[u8; MASTER_KEY_LEN]> {
    let bytes: Vec<u8> = serde_json::from_str(serialized)?;
    if bytes.len() != MASTER_KEY_LEN {
        bail!("invalid secret master key length");
    }
    let mut key = [0u8; MASTER_KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

pub(crate) fn random_master_key() -> [u8; MASTER_KEY_LEN] {
    let mut key = [0u8; MASTER_KEY_LEN];
    key[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    key
}

fn random_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&Uuid::new_v4().as_bytes()[..NONCE_LEN]);
    nonce
}

#[cfg(feature = "apple-keychain")]
fn ensure_apple_keychain_store() -> Result<()> {
    use std::collections::HashMap;

    static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let result = INIT.get_or_init(|| {
        keyring::use_apple_keychain_store(&HashMap::new())
            .map_err(|error| format!("failed to initialize macOS keychain store: {error}"))
    });
    match result {
        Ok(()) => Ok(()),
        Err(message) => Err(anyhow!(message.clone())),
    }
}

fn parse_master_key_bytes(bytes: &[u8]) -> Result<[u8; MASTER_KEY_LEN]> {
    if bytes.len() != MASTER_KEY_LEN {
        bail!(
            "invalid master key length: expected {MASTER_KEY_LEN}, got {}",
            bytes.len()
        );
    }
    let mut key = [0u8; MASTER_KEY_LEN];
    key.copy_from_slice(bytes);
    Ok(key)
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(MASTER_KEY_DIR_PERMS)
        .create(path)
        .with_context(|| format!("creating private directory {}", path.display()))
}

pub(crate) fn lock_secret_file(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        private_directory(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(MASTER_KEY_FILE_PERMS)
        .open(path)?;
    file.lock().context("locking secret storage")?;
    Ok(file)
}

fn write_master_key_file(path: &Path, key: &[u8; MASTER_KEY_LEN]) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    private_directory(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(key)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("persisting master key at {}", path.display()))?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn default_master_key_path() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Ok(path.join("exo").join("master.key"));
        }
    }
    if let Some(value) = std::env::var_os("HOME") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Ok(path.join(".config").join("exo").join("master.key"));
        }
    }
    bail!("could not determine config directory: set XDG_CONFIG_HOME or HOME")
}
