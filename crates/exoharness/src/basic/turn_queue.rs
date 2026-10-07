use super::*;
use crate::turn_coordinator::stored::{TurnQueueState, TurnQueueStore};
use crate::turn_coordinator::{StoredTurnCoordinator, TurnControl, TurnThread};
use anyhow::ensure;
use serde::de::DeserializeOwned;
use std::marker::PhantomData;

#[derive(Serialize, Deserialize)]
struct StoredQueue<Work> {
    thread: TurnThread,
    state: TurnQueueState<Work>,
}

impl BasicExoHarness {
    /// Durable queues sharing this harness's storage, process ownership, and
    /// per-thread mutation locks. Caller-scoped providers share the coordinator.
    pub fn turn_coordinator<Work>(&self) -> Arc<StoredTurnCoordinator<Work>>
    where
        Work: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        Arc::new(StoredTurnCoordinator::new(
            Arc::new(BasicQueueStore {
                harness: self.clone(),
                work: PhantomData,
            }),
            self.inner.turn_queue_locks.clone(),
        ))
    }
}

struct BasicQueueStore<Work> {
    harness: BasicExoHarness,
    work: PhantomData<Work>,
}

fn scope(thread: TurnThread) -> ResourceScope {
    ResourceScope::Thread {
        agent_id: thread.agent_id,
        thread_id: thread.thread_id,
    }
}

#[async_trait]
impl<Work: Clone + Serialize + DeserializeOwned + Send + Sync + 'static> TurnQueueStore<Work>
    for BasicQueueStore<Work>
{
    async fn load(&self, thread: TurnThread) -> Result<TurnQueueState<Work>> {
        #[cfg(feature = "basic-backend")]
        let _lease = self.harness.claim_local_scope(scope(thread)).await?;
        #[cfg(not(feature = "basic-backend"))]
        self.harness.check(scope(thread)).await?;
        Ok(self
            .harness
            .inner
            .storage
            .get_json_if_exists::<StoredQueue<Work>>(
                self.harness
                    .owner_dir(scope(thread))
                    .join("turn_queue.json"),
            )
            .await?
            .map(|queue| queue.state)
            .unwrap_or_default())
    }

    async fn save(&self, thread: TurnThread, state: &TurnQueueState<Work>) -> Result<()> {
        let _resources = self
            .harness
            .inner
            .lock_thread_resources(thread.agent_id, thread.thread_id)
            .await;
        let directory = self.harness.owner_dir(scope(thread));
        ensure!(
            self.harness
                .inner
                .storage
                .get_bytes_if_exists(directory.join("record.json"))
                .await?
                .is_some(),
            "queue thread no longer exists"
        );
        self.harness
            .inner
            .storage
            .put_json(
                directory.join("turn_queue.json"),
                &StoredQueue {
                    thread,
                    state: state.clone(),
                },
            )
            .await
    }

    async fn pending_threads(&self) -> Result<Vec<TurnThread>> {
        Ok(self
            .harness
            .inner
            .storage
            .list_json_matching_suffix::<StoredQueue<Work>>(
                self.harness.agents_dir(),
                "/turn_queue.json",
            )
            .await?
            .into_iter()
            .filter(|queue| {
                queue
                    .state
                    .pending
                    .front()
                    .is_some_and(|head| head.control != TurnControl::Suspend)
            })
            .map(|queue| queue.thread)
            .collect())
    }
}
