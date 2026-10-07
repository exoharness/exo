use super::*;
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::{
    ObjectStore, local::LocalFileSystem, memory::InMemory, path::Path as ObjectPath,
};
use tokio::fs;

struct NativeStorage(Arc<dyn ObjectStore>);
impl BasicObjectStore {
    pub(crate) fn in_memory() -> Self {
        Self::new(Arc::new(NativeStorage(Arc::new(InMemory::new()))))
    }
    pub(crate) async fn local_filesystem(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).await?;
        Ok(Self::new(Arc::new(NativeStorage(Arc::new(
            LocalFileSystem::new_with_prefix(root)?,
        )))))
    }
}
#[async_trait::async_trait]
impl Storage for NativeStorage {
    async fn put(&self, key: &str, bytes: Vec<u8>, _blob: bool) -> Result<()> {
        self.0
            .put(&ObjectPath::parse(key)?, Bytes::from(bytes).into())
            .await?;
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.0.get(&ObjectPath::parse(key)?).await {
            Ok(value) => Ok(Some(value.bytes().await?.to_vec())),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let prefix = (!prefix.is_empty())
            .then(|| ObjectPath::parse(prefix))
            .transpose()?;
        Ok(self
            .0
            .list(prefix.as_ref())
            .map_ok(|meta| meta.location.to_string())
            .try_collect()
            .await?)
    }
    async fn list_directories(&self, prefix: &str) -> Result<Vec<String>> {
        let prefix = (!prefix.is_empty())
            .then(|| ObjectPath::parse(prefix))
            .transpose()?;
        Ok(self
            .0
            .list_with_delimiter(prefix.as_ref())
            .await?
            .common_prefixes
            .into_iter()
            .map(|path| path.to_string())
            .collect())
    }
    async fn delete(&self, key: &str) -> Result<()> {
        match self.0.delete(&ObjectPath::parse(key)?).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
    async fn copy(&self, source: &str, destination: &str) -> Result<()> {
        self.0
            .copy(
                &ObjectPath::parse(source)?,
                &ObjectPath::parse(destination)?,
            )
            .await?;
        Ok(())
    }
}
