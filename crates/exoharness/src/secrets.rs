use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, anyhow, bail};
use keyring_core::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Result, Secret};

const MASTER_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const KEYCHAIN_SERVICE: &str = "exo-exoharness-master-key";

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

pub(crate) struct AppleKeychainSecretKeyProvider {
    account: String,
    key: OnceLock<[u8; MASTER_KEY_LEN]>,
}

impl AppleKeychainSecretKeyProvider {
    pub(crate) fn new(account: String) -> Self {
        Self {
            account,
            key: OnceLock::new(),
        }
    }
}

impl SecretKeyProvider for AppleKeychainSecretKeyProvider {
    fn get_or_create_key(&self) -> Result<[u8; MASTER_KEY_LEN]> {
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

fn ensure_apple_keychain_store() -> Result<()> {
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
