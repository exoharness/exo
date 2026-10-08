//! One StoredTurnCoordinator per thread object; only persistence is host-specific.
use crate::host::{Host, HostRequest, HostStorage, StorageOperation};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use executor::TurnWork;
use exoharness::Storage;
use exoharness::turn_coordinator::{
    TurnControl, TurnThread,
    stored::{TurnQueueState, TurnQueueStore},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub(crate) struct QueueStore(pub Arc<Host>);
#[derive(Serialize, Deserialize)]
struct Queue {
    thread: TurnThread,
    state: TurnQueueState<TurnWork>,
}
const KEY: &str = "turn_queue";
impl QueueStore {
    async fn get(&self) -> Result<Option<Queue>> {
        HostStorage(self.0.clone())
            .get(KEY)
            .await?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(Into::into))
            .transpose()
    }
}
#[async_trait]
impl TurnQueueStore<TurnWork> for QueueStore {
    async fn load(&self, thread: TurnThread) -> Result<TurnQueueState<TurnWork>> {
        let Some(queue) = self.get().await? else {
            return Ok(TurnQueueState::default());
        };
        ensure!(
            queue.thread == thread,
            "thread object cannot own another thread's queue"
        );
        Ok(queue.state)
    }
    async fn save(&self, thread: TurnThread, state: &TurnQueueState<TurnWork>) -> Result<()> {
        let alarm = state
            .pending
            .front()
            .is_some_and(|head| head.control != TurnControl::Suspend);
        let bytes = serde_json::to_vec(&Queue {
            thread,
            state: state.clone(),
        })?;
        self.0
            .call::<()>(HostRequest::Storage {
                operation: StorageOperation::Put {
                    key: KEY.into(),
                    blob: bytes.len() > 64 * 1024,
                    bytes,
                    alarm: Some(alarm),
                },
            })
            .await
    }
    async fn pending_threads(&self) -> Result<Vec<TurnThread>> {
        Ok(self
            .get()
            .await?
            .filter(|queue| {
                queue
                    .state
                    .pending
                    .front()
                    .is_some_and(|head| head.control != TurnControl::Suspend)
            })
            .map(|queue| vec![queue.thread])
            .unwrap_or_default())
    }
}
