use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Result;

/// Byte storage for the shared harness. The host may place blobs separately;
/// keys and all metadata formats are owned by Rust. Calls are serialized by the
/// harness for mutations; hosts must provide read-after-write consistency.
#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    async fn put(&self, key: &str, bytes: Vec<u8>, blob: bool) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
    async fn list_directories(&self, prefix: &str) -> Result<Vec<String>> {
        let base = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        let mut directories = std::collections::BTreeSet::new();
        for key in self.list(prefix).await? {
            if let Some(relative) = key.strip_prefix(&base)
                && let Some((directory, _)) = relative.split_once('/')
            {
                directories.insert(format!("{base}{directory}"));
            }
        }
        Ok(directories.into_iter().collect())
    }
    async fn delete(&self, key: &str) -> Result<()>;
    async fn copy(&self, source: &str, destination: &str) -> Result<()>;
}

#[cfg(feature = "basic-backend")]
mod native;

#[derive(Clone)]
pub(crate) struct BasicObjectStore {
    store: Arc<dyn Storage>,
    #[cfg(test)]
    fail_json_put_after: Arc<std::sync::Mutex<Option<usize>>>,
}

impl BasicObjectStore {
    pub(crate) fn new(store: Arc<dyn Storage>) -> Self {
        Self {
            store,
            #[cfg(test)]
            fail_json_put_after: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_json_put_after(&self, successful_puts: usize) {
        *self
            .fail_json_put_after
            .lock()
            .expect("fault counter poisoned") = Some(successful_puts);
    }

    pub(crate) async fn put_json<T: Serialize>(
        &self,
        key: impl AsRef<Path>,
        value: &T,
    ) -> Result<()> {
        #[cfg(test)]
        {
            let mut remaining = self
                .fail_json_put_after
                .lock()
                .expect("fault counter poisoned");
            if let Some(count) = *remaining {
                if count == 0 {
                    *remaining = None;
                    anyhow::bail!("injected JSON object write failure");
                }
                *remaining = Some(count - 1);
            }
        }
        self.put_bytes(key, serde_json::to_vec_pretty(value)?).await
    }

    pub(crate) async fn put_bytes(&self, key: impl AsRef<Path>, value: Vec<u8>) -> Result<()> {
        let blob = value.len() > 64 * 1024;
        self.store
            .put(&normalize_path(key.as_ref()), value, blob)
            .await?;
        Ok(())
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(&self, key: impl AsRef<Path>) -> Result<T> {
        let key = key.as_ref();
        let bytes = self.get_bytes(key).await?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to decode JSON {}", key.display()))
    }

    pub(crate) async fn get_json_if_exists<T: DeserializeOwned>(
        &self,
        key: impl AsRef<Path>,
    ) -> Result<Option<T>> {
        let key = key.as_ref();
        let Some(bytes) = self.get_bytes_if_exists(key).await? else {
            return Ok(None);
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("failed to decode JSON {}", key.display()))
    }

    pub(crate) async fn get_bytes(&self, key: impl AsRef<Path>) -> Result<Vec<u8>> {
        let key = key.as_ref();
        self.store
            .get(&normalize_path(key))
            .await?
            .with_context(|| format!("failed to get {}", key.display()))
    }

    pub(crate) async fn get_bytes_if_exists(
        &self,
        key: impl AsRef<Path>,
    ) -> Result<Option<Vec<u8>>> {
        self.store.get(&normalize_path(key.as_ref())).await
    }

    pub(crate) async fn list_keys(&self, prefix: impl AsRef<Path>) -> Result<Vec<String>> {
        let mut keys = self.store.list(&normalize_path(prefix.as_ref())).await?;
        keys.sort();
        Ok(keys)
    }

    pub(crate) async fn list_directories(&self, prefix: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        Ok(self
            .store
            .list_directories(&normalize_path(prefix.as_ref()))
            .await?
            .into_iter()
            .map(PathBuf::from)
            .collect())
    }

    /// Delete the object at exactly `key`, tolerating absence. Unlike
    /// `delete_prefix`, this works for a single object: `list`-based prefix
    /// deletion never matches an object at exactly the prefix path.
    pub(crate) async fn delete_key_if_exists(&self, key: impl AsRef<Path>) -> Result<()> {
        self.store.delete(&normalize_path(key.as_ref())).await
    }

    pub(crate) async fn delete_prefix(&self, prefix: impl AsRef<Path>) -> Result<()> {
        for key in self.list_keys(prefix).await? {
            self.store.delete(&key).await?;
        }
        Ok(())
    }

    pub(crate) async fn copy_prefix(
        &self,
        src_prefix: impl AsRef<Path>,
        dst_prefix: impl AsRef<Path>,
    ) -> Result<()> {
        let src_prefix = normalize_path(src_prefix.as_ref());
        let dst_prefix = normalize_path(dst_prefix.as_ref());
        for key in self.list_keys(&src_prefix).await? {
            let relative = key
                .strip_prefix(&src_prefix)
                .expect("listed key should share prefix");
            let destination = format!("{dst_prefix}{relative}");
            self.store.copy(&key, &destination).await?;
        }
        Ok(())
    }

    pub(crate) async fn list_json_matching_suffix<T: DeserializeOwned>(
        &self,
        prefix: impl AsRef<Path>,
        suffix: &str,
    ) -> Result<Vec<T>> {
        let mut values = Vec::new();
        for key in self.list_keys(prefix).await? {
            if !key.ends_with(suffix) {
                continue;
            }
            if let Some(value) = self.get_json_if_exists::<T>(Path::new(&key)).await? {
                values.push(value);
            }
        }
        Ok(values)
    }
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}
