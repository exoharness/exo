use super::*;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(feature = "apple-keychain")]
const KEYCHAIN_SERVICE: &str = "exo-exoharness-master-key";
const MASTER_KEY_FILE_PERMS: u32 = 0o600;
const MASTER_KEY_DIR_PERMS: u32 = 0o700;

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
