use super::*;
use std::{collections::HashMap, sync::Mutex};

pub(super) struct RefreshState {
    host: Arc<dyn crate::runtime_host::RuntimeHost>,
    http: Option<Arc<dyn OAuthHttpClient>>,
    locks: Mutex<HashMap<SecretId, std::sync::Weak<tokio::sync::Mutex<()>>>>,
}
impl RefreshState {
    pub(super) fn new(
        host: Arc<dyn crate::runtime_host::RuntimeHost>,
        http: Option<Arc<dyn OAuthHttpClient>>,
    ) -> Self {
        Self {
            host,
            http,
            locks: Mutex::default(),
        }
    }
    fn lock(&self, id: SecretId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().expect("OAuth refresh locks poisoned");
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&id).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(id, Arc::downgrade(&lock));
        lock
    }
}
impl BasicVaultHandle {
    pub(super) fn platform_needs_refresh(
        &self,
        _id: &SecretId,
        resolved: &ResolvedSecret,
    ) -> Result<bool> {
        anyhow::ensure!(
            !matches!(resolved.secret, Secret::GithubCli { .. }),
            "GitHub CLI credentials require a native host; use a token or OAuth grant"
        );
        Ok(oauth::needs_refresh(&resolved.secret))
    }
    pub(super) fn refresh_host(&self) -> Result<Arc<dyn crate::runtime_host::RuntimeHost>> {
        Ok(self.store.inner.hosted.host.clone())
    }
    pub(super) fn refresh_http(&self) -> Result<&dyn OAuthHttpClient> {
        self.store
            .inner
            .hosted
            .http
            .as_deref()
            .context("this host does not support credential refresh")
    }
    pub(super) async fn refresh_guard(
        &self,
        id: &SecretId,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        Ok(self.store.inner.hosted.lock(*id).lock_owned().await)
    }
}
