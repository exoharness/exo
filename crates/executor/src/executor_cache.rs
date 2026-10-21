//! Thread cache shared by native and hosted executors.
use crate::{AgentConfig, AgentHarnessKind, TypeScriptHarnessConfig};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use exoharness::ThreadId;
use std::{
    collections::HashMap,
    ops::Deref,
    sync::{Arc, Mutex},
};
use tokio::sync::OnceCell;

#[async_trait]
pub trait CachedExecutor: Send + Sync {
    async fn shutdown(&self) -> Result<()>;
}

pub struct Entry<T> {
    value: T,
    harness: AgentHarnessKind,
    typescript: Option<TypeScriptHarnessConfig>,
}
impl<T> Deref for Entry<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

type Threads<T> = HashMap<ThreadId, Arc<OnceCell<Entry<T>>>>;
pub struct ExecutorCache<T> {
    threads: Mutex<Threads<T>>,
}

impl<T> Default for ExecutorCache<T> {
    fn default() -> Self {
        Self {
            threads: Mutex::default(),
        }
    }
}

impl<T: CachedExecutor> ExecutorCache<T> {
    pub async fn get_or_try_init<F, Fut>(
        &self,
        thread: ThreadId,
        config: &AgentConfig,
        build: F,
    ) -> Result<Arc<OnceCell<Entry<T>>>>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        loop {
            let cell = self
                .threads
                .lock()
                .expect("executor cache poisoned")
                .entry(thread)
                .or_default()
                .clone();
            let entry = cell
                .get_or_try_init(|| async {
                    Ok::<_, anyhow::Error>(Entry {
                        value: build().await?,
                        harness: config.harness,
                        typescript: config.typescript.clone(),
                    })
                })
                .await?;
            if entry.harness == config.harness && entry.typescript == config.typescript {
                return Ok(cell);
            }
            drop(cell);
            self.reset(thread).await?;
        }
    }

    pub fn get(&self, thread: ThreadId) -> Option<Arc<OnceCell<Entry<T>>>> {
        self.threads
            .lock()
            .expect("executor cache poisoned")
            .get(&thread)
            .cloned()
    }

    fn remove_idle(&self, thread: ThreadId) -> Result<Option<Arc<OnceCell<Entry<T>>>>> {
        let mut threads = self.threads.lock().expect("executor cache poisoned");
        if let Some(cell) = threads.get(&thread) {
            // Calls retain the cell across awaits. Acquisition cannot race removal.
            ensure!(
                Arc::strong_count(cell) == 1,
                "thread is busy; wait for its current turn or setup to finish before reconfiguring it"
            );
        }
        Ok(threads.remove(&thread))
    }

    pub async fn reset(&self, thread: ThreadId) -> Result<()> {
        if let Some(cell) = self.remove_idle(thread)?
            && let Some(entry) = cell.get()
        {
            entry.shutdown().await?;
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let threads = std::mem::take(&mut *self.threads.lock().expect("executor cache poisoned"));
        let results = futures::future::join_all(threads.into_values().map(|cell| async move {
            if let Some(entry) = cell.get() {
                entry.shutdown().await?;
            }
            Ok::<_, anyhow::Error>(())
        }))
        .await;
        for result in results {
            result?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Stub;
    #[async_trait]
    impl CachedExecutor for Stub {
        async fn shutdown(&self) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn resetting_a_busy_thread_preserves_its_executor() -> Result<()> {
        let thread = exoharness::Uuid7::now();
        let cache = ExecutorCache::<Stub> {
            threads: Mutex::default(),
        };
        cache
            .threads
            .lock()
            .unwrap()
            .insert(thread, Arc::new(OnceCell::new()));
        let active = cache.get(thread).unwrap();
        assert!(cache.remove_idle(thread).is_err());
        assert!(Arc::ptr_eq(&active, &cache.get(thread).unwrap()));
        drop(active);
        assert!(cache.remove_idle(thread)?.is_some());
        assert!(cache.get(thread).is_none());
        Ok(())
    }
}
