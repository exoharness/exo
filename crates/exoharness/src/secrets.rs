use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use aes_gcm::aead::{Aead, KeyInit};
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

    pub(crate) fn encrypt_secret(&self, secret: &Secret) -> Result<EncryptedSecret> {
        let key = self.key_provider.get_or_create_key()?;
        let payload = serde_json::to_vec(secret)?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| anyhow!("invalid secret encryption key length"))?;
        let nonce = random_nonce();
        let nonce = Nonce::from(nonce);
        let ciphertext = cipher
            .encrypt(&nonce, payload.as_slice())
            .context("failed to encrypt secret payload")?;
        Ok(EncryptedSecret {
            algorithm: SecretEncryptionAlgorithm::Aes256Gcm,
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }

    pub(crate) fn decrypt_secret(&self, encrypted: &EncryptedSecret) -> Result<Secret> {
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
            .decrypt(&Nonce::from(nonce), encrypted.ciphertext.as_slice())
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
        match self.key.set(key) {
            Ok(()) => Ok(key),
            Err(key) => Ok(self.key.get().copied().unwrap_or(key)),
        }
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
        let _ = self.key.set(key);
        Ok(key)
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

fn random_master_key() -> [u8; MASTER_KEY_LEN] {
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

fn write_master_key_file(path: &Path, key: &[u8; MASTER_KEY_LEN]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating master key directory {}", parent.display()))?;
        std::fs::set_permissions(
            parent,
            std::fs::Permissions::from_mode(MASTER_KEY_DIR_PERMS),
        )
        .with_context(|| format!("setting permissions on {}", parent.display()))?;
    }

    let tmp_path = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp_path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(MASTER_KEY_FILE_PERMS)
        .open(&tmp_path)
        .with_context(|| format!("creating master key file {}", tmp_path.display()))?;
    file.write_all(key)
        .with_context(|| format!("writing master key file {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing master key file {}", tmp_path.display()))?;
    drop(file);
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming master key into place at {}", path.display()))?;
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

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn static_cipher() -> SecretCipher {
        SecretCipher::new(Arc::new(StaticSecretKeyProvider::new(
            [9u8; MASTER_KEY_LEN],
        )))
    }

    fn test_secret() -> Secret {
        Secret::Key {
            value: "hunter2".to_string(),
        }
    }

    #[test]
    fn encrypt_decrypt_round_trips_plaintext() {
        let cipher = static_cipher();
        let encrypted = cipher.encrypt_secret(&test_secret()).expect("encrypt");
        let Secret::Key { value } = cipher.decrypt_secret(&encrypted).expect("decrypt") else {
            panic!("decrypted secret should be the original Key variant");
        };
        assert_eq!(value, "hunter2");
    }

    #[test]
    fn encrypting_same_plaintext_twice_uses_fresh_nonces() {
        let cipher = static_cipher();
        let first = cipher.encrypt_secret(&test_secret()).expect("encrypt");
        let second = cipher.encrypt_secret(&test_secret()).expect("encrypt");
        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.ciphertext, second.ciphertext);
    }

    #[test]
    fn decrypt_rejects_tampered_ciphertext() {
        let cipher = static_cipher();
        let mut encrypted = cipher.encrypt_secret(&test_secret()).expect("encrypt");
        encrypted.ciphertext[0] ^= 0xff;
        let error = cipher
            .decrypt_secret(&encrypted)
            .expect_err("tampered ciphertext must not decrypt");
        assert!(
            format!("{error:#}").contains("failed to decrypt secret payload"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn file_backed_provider_creates_then_reuses_master_key() {
        let tempdir = TempDir::new().expect("tempdir");
        let path = tempdir.path().join("keys").join("master.key");
        let first = FileBackedSecretKeyProvider::new(path.clone())
            .get_or_create_key()
            .expect("first call creates the key");
        assert!(path.exists(), "master key file should be created");
        // A fresh provider on the same path must load the persisted key, not
        // mint a new one.
        let second = FileBackedSecretKeyProvider::new(path)
            .get_or_create_key()
            .expect("second call reads the key back");
        assert_eq!(first, second);
    }

    #[test]
    fn file_backed_provider_rejects_wrong_length_master_key_file() {
        let tempdir = TempDir::new().expect("tempdir");
        let path = tempdir.path().join("master.key");
        std::fs::write(&path, b"too-short").expect("write garbage key");
        let error = FileBackedSecretKeyProvider::new(path.clone())
            .get_or_create_key()
            .expect_err("wrong-length key must be rejected");
        assert!(
            format!("{error:#}").contains("invalid master key length"),
            "unexpected error: {error:#}"
        );
        // The bad file must survive untouched, not be silently regenerated.
        assert_eq!(
            std::fs::read(&path).expect("garbage file still present"),
            b"too-short"
        );
    }
}
