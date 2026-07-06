use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::ObjectStore;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::fs;

use crate::Result;

#[derive(Clone)]
pub(crate) struct BasicObjectStore {
    store: Arc<dyn ObjectStore>,
}

impl BasicObjectStore {
    pub(crate) async fn local_filesystem(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).await?;
        let store = LocalFileSystem::new_with_prefix(&root)?;
        Ok(Self {
            store: Arc::new(store),
        })
    }

    pub(crate) async fn put_json<T: Serialize>(
        &self,
        key: impl AsRef<Path>,
        value: &T,
    ) -> Result<()> {
        let path = object_path(key.as_ref())?;
        let bytes = serde_json::to_vec_pretty(value)?;
        self.store.put(&path, Bytes::from(bytes).into()).await?;
        Ok(())
    }

    pub(crate) async fn put_bytes(&self, key: impl AsRef<Path>, value: Vec<u8>) -> Result<()> {
        let path = object_path(key.as_ref())?;
        self.store.put(&path, Bytes::from(value).into()).await?;
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
        let path = object_path(key)?;
        let bytes = self
            .store
            .get(&path)
            .await
            .with_context(|| format!("failed to get {}", key.display()))?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }

    pub(crate) async fn get_bytes_if_exists(
        &self,
        key: impl AsRef<Path>,
    ) -> Result<Option<Vec<u8>>> {
        let path = object_path(key.as_ref())?;
        let get_result = match self.store.get(&path).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(get_result.bytes().await?.to_vec()))
    }

    pub(crate) async fn list_keys(&self, prefix: impl AsRef<Path>) -> Result<Vec<String>> {
        let prefix = normalize_path(prefix.as_ref());
        let object_prefix = match prefix.is_empty() {
            true => None,
            false => Some(object_prefix(Path::new(&prefix))?),
        };
        let mut keys = self
            .store
            .list(object_prefix.as_ref())
            .map_ok(|meta| meta.location.to_string())
            .try_collect::<Vec<_>>()
            .await?;
        keys.sort();
        Ok(keys)
    }

    pub(crate) async fn delete_prefix(&self, prefix: impl AsRef<Path>) -> Result<()> {
        for key in self.list_keys(prefix).await? {
            self.store.delete(&object_path(Path::new(&key))?).await?;
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
            self.store
                .copy(
                    &object_path(Path::new(&key))?,
                    &object_path(Path::new(&destination))?,
                )
                .await?;
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

fn object_path(path: &Path) -> Result<ObjectPath> {
    ObjectPath::parse(normalize_path(path)).map_err(Into::into)
}

fn object_prefix(prefix: &Path) -> Result<ObjectPath> {
    ObjectPath::parse(normalize_path(prefix)).map_err(Into::into)
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    async fn test_store() -> (TempDir, BasicObjectStore) {
        let tempdir = TempDir::new().expect("tempdir");
        let store = BasicObjectStore::local_filesystem(tempdir.path())
            .await
            .expect("store");
        (tempdir, store)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bytes_round_trip_and_missing_key_semantics() {
        let (_tempdir, store) = test_store().await;
        store
            .put_bytes("dir/blob.bin", b"payload".to_vec())
            .await
            .expect("put");
        assert_eq!(
            store.get_bytes("dir/blob.bin").await.expect("get"),
            b"payload"
        );
        assert_eq!(
            store
                .get_bytes_if_exists("dir/missing.bin")
                .await
                .expect("miss is not an error"),
            None
        );
        store
            .get_bytes("dir/missing.bin")
            .await
            .expect_err("hard get on a missing key errors");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn json_round_trip_and_missing_key_returns_none() {
        let (_tempdir, store) = test_store().await;
        let value = serde_json::json!({"name": "exo", "count": 3});
        store
            .put_json("dir/record.json", &value)
            .await
            .expect("put");
        assert_eq!(
            store
                .get_json::<serde_json::Value>("dir/record.json")
                .await
                .expect("get"),
            value
        );
        assert_eq!(
            store
                .get_json_if_exists::<serde_json::Value>("dir/missing.json")
                .await
                .expect("miss is not an error"),
            None
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_keys_isolates_prefixes() {
        let (_tempdir, store) = test_store().await;
        store
            .put_bytes("alpha/one.bin", vec![1])
            .await
            .expect("put");
        store
            .put_bytes("alpha/nested/two.bin", vec![2])
            .await
            .expect("put");
        store
            .put_bytes("alphabet/three.bin", vec![3])
            .await
            .expect("put");
        store
            .put_bytes("beta/four.bin", vec![4])
            .await
            .expect("put");
        // Prefixes are path segments: "alphabet" must not leak into "alpha".
        assert_eq!(
            store.list_keys("alpha").await.expect("list"),
            vec!["alpha/nested/two.bin", "alpha/one.bin"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn copy_prefix_copies_objects_and_keeps_source_intact() {
        let (_tempdir, store) = test_store().await;
        store
            .put_bytes("src/a.bin", b"a".to_vec())
            .await
            .expect("put");
        store
            .put_bytes("src/nested/b.bin", b"b".to_vec())
            .await
            .expect("put");
        store.copy_prefix("src", "dst").await.expect("copy");
        assert_eq!(store.get_bytes("dst/a.bin").await.expect("copied"), b"a");
        assert_eq!(
            store
                .get_bytes("dst/nested/b.bin")
                .await
                .expect("copied nested"),
            b"b"
        );
        assert_eq!(
            store.list_keys("src").await.expect("source intact"),
            vec!["src/a.bin", "src/nested/b.bin"]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_prefix_removes_only_matching_objects() {
        let (_tempdir, store) = test_store().await;
        store.put_bytes("drop/a.bin", vec![1]).await.expect("put");
        store
            .put_bytes("drop/nested/b.bin", vec![2])
            .await
            .expect("put");
        store.put_bytes("keep/c.bin", vec![3]).await.expect("put");
        store.delete_prefix("drop").await.expect("delete");
        assert_eq!(store.list_keys("").await.expect("list"), vec!["keep/c.bin"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn redundant_path_components_normalize_to_the_same_key() {
        let (_tempdir, store) = test_store().await;
        store
            .put_bytes("dir//sub/./file.bin", b"x".to_vec())
            .await
            .expect("put");
        assert_eq!(
            store
                .get_bytes("dir/sub/file.bin")
                .await
                .expect("get via canonical key"),
            b"x"
        );
        assert_eq!(
            store.list_keys("dir").await.expect("list"),
            vec!["dir/sub/file.bin"]
        );
    }
}
