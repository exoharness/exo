use super::*;
use std::{collections::HashMap, sync::Mutex, time::Duration};
use tokio::time::Instant;

const LEASE: Duration = Duration::from_secs(90);

type Entries = HashMap<AgentId, (Arc<Runtime>, Instant)>;

pub(super) struct TemporaryAgents(Arc<Mutex<Entries>>);

impl TemporaryAgents {
    pub(super) fn new() -> Self {
        let entries = Arc::new(Mutex::new(Entries::new()));
        let weak = Arc::downgrade(&entries);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let Some(entries) = weak.upgrade() else {
                    return;
                };
                reap(&entries).await;
            }
        });
        Self(entries)
    }

    pub(super) fn get(&self, id: AgentId) -> Option<Arc<Runtime>> {
        let mut entries = self.0.lock().expect("temporary agents poisoned");
        let (runtime, expires) = entries.get_mut(&id)?;
        if *expires <= Instant::now() {
            return None;
        }
        *expires = Instant::now() + LEASE;
        Some(runtime.clone())
    }

    pub(super) fn remove(&self, id: AgentId) -> Option<Arc<Runtime>> {
        self.0
            .lock()
            .expect("temporary agents poisoned")
            .remove(&id)
            .map(|(runtime, _)| runtime)
    }
}

impl Drop for TemporaryAgents {
    fn drop(&mut self) {
        for (_, (runtime, _)) in self.0.lock().expect("temporary agents poisoned").drain() {
            drop(shutdown(runtime));
        }
    }
}

pub(super) fn shutdown(runtime: Arc<Runtime>) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        let result = runtime.shutdown().await;
        if let Err(error) = &result {
            tracing::error!(%error, "temporary agent cleanup failed");
        }
        result
    })
}

async fn reap(entries: &Mutex<Entries>) {
    let expired = {
        let mut entries = entries.lock().expect("temporary agents poisoned");
        let ids: Vec<_> = entries
            .iter()
            .filter(|(_, (_, expires))| *expires <= Instant::now())
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| entries.remove(&id))
            .map(|(runtime, _)| runtime)
            .collect::<Vec<_>>()
    };
    for runtime in expired {
        if let Err(error) = shutdown(runtime).await {
            tracing::error!(%error, "temporary agent cleanup task failed");
        }
    }
}

pub(super) async fn create_agent(
    service: web::Data<Arc<RuntimeHttpService>>,
    body: web::Json<exoharness::NewAgentRequest>,
) -> Result<web::Json<exoharness::AgentRecord>, Error> {
    let runtime = Arc::new(
        service
            .runtime
            .temporary(service.config.clone())
            .await
            .map_err(ErrorBadRequest)?,
    );
    let agent = runtime
        .exoharness_handle()
        .new_agent(body.into_inner())
        .await
        .map_err(ErrorBadRequest)?;
    service
        .temporary
        .0
        .lock()
        .expect("temporary agents poisoned")
        .insert(agent.record().id, (runtime, Instant::now() + LEASE));
    Ok(web::Json(agent.record().clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abandoned_agents_expire_and_service_drop_cleans_remaining_agents() -> anyhow::Result<()>
    {
        let config = crate::test_support::local_test_config("unused-temporary-http-test");
        let state = Arc::new(exoharness::BasicExoHarness::in_memory(config.clone(), None).await?);
        let root = Runtime::new(
            crate::LocalProvider::basic(
                state,
                Arc::new(crate::RouterModelClient::new(HashMap::new())),
                Arc::new(crate::BasicToolRuntime),
                Arc::new(cost::PricingTable::empty()),
            ),
            None,
        );
        let temporary = TemporaryAgents::new();
        let mut runtimes = Vec::new();
        for slug in ["expired", "alive"] {
            let runtime = Arc::new(root.temporary(config.clone()).await?);
            let agent = runtime
                .exoharness_handle()
                .new_agent(exoharness::NewAgentRequest {
                    slug: slug.into(),
                    name: slug.into(),
                    vaults: vec![],
                })
                .await?;
            temporary
                .0
                .lock()
                .unwrap()
                .insert(agent.record().id, (runtime.clone(), Instant::now() + LEASE));
            runtimes.push((agent.record().id, runtime));
        }
        let (expired, expired_runtime) = &runtimes[0];
        let (alive, alive_runtime) = &runtimes[1];
        temporary.0.lock().unwrap().get_mut(expired).unwrap().1 = Instant::now();
        assert!(temporary.get(*expired).is_none());
        assert!(temporary.get(*alive).is_some());
        reap(&temporary.0).await;
        assert!(expired_runtime.list_agents().await?.is_empty());
        assert_eq!(alive_runtime.list_agents().await?.len(), 1);
        drop(temporary);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !alive_runtime.list_agents().await?.is_empty() {
                tokio::task::yield_now().await;
            }
            anyhow::Result::<()>::Ok(())
        })
        .await??;
        assert!(root.list_agents().await?.is_empty());
        root.shutdown().await
    }
}
