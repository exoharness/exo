use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Result, Secret};

const MASTER_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
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

    #[cfg(feature = "basic-backend")]
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

#[cfg(feature = "basic-backend")]
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

#[cfg(feature = "basic-backend")]
mod native;
#[cfg(feature = "basic-backend")]
pub(crate) use native::*;
