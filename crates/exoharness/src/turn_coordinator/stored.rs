use super::*;
use anyhow::{Context, ensure};
use std::collections::HashMap;
use std::sync::{Mutex, Weak};
use tokio::sync::Mutex as AsyncMutex;

#[cfg(test)]
mod tests;

#[derive(Default)]
pub struct TurnQueueLocks {
    mutations: Mutex<HashMap<TurnThread, Weak<AsyncMutex<()>>>>,
    owners: Mutex<HashMap<TurnThread, Weak<Uuid7>>>,
}

impl TurnQueueLocks {
    fn mutation(&self, thread: TurnThread) -> Arc<AsyncMutex<()>> {
        let mut locks = self.mutations.lock().expect("turn queue locks poisoned");
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&thread).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(thread, Arc::downgrade(&lock));
        lock
    }

    fn check(&self, lease: &TurnLease) -> Result<()> {
        ensure!(
            self.owners
                .lock()
                .expect("turn queue owners poisoned")
                .get(&lease.thread)
                .and_then(Weak::upgrade)
                .is_some_and(|owner| Arc::ptr_eq(&owner, &lease.identity)),
            "turn ownership lost"
        );
        Ok(())
    }
}

pub struct StoredTurnCoordinator<Work> {
    store: Arc<dyn TurnQueueStore<Work>>,
    locks: Arc<TurnQueueLocks>,
}

impl<Work: Clone + Send + Sync + 'static> StoredTurnCoordinator<Work> {
    pub fn new(store: Arc<dyn TurnQueueStore<Work>>, locks: Arc<TurnQueueLocks>) -> Self {
        Self { store, locks }
    }
    /// Volatile queues for embedded runtimes. CLI runtimes use Basic's durable store.
    pub fn in_memory() -> Self {
        Self::new(
            Arc::new(MemoryStore(Mutex::new(HashMap::new()))),
            Arc::default(),
        )
    }
}

#[async_trait]
impl<Work: Clone + Send + Sync + 'static> TurnCoordinator<Work> for StoredTurnCoordinator<Work> {
    async fn enqueue(
        &self,
        thread: TurnThread,
        mut turn: QueuedTurn<Work>,
    ) -> Result<AcceptedTurn> {
        let lock = self.locks.mutation(thread);
        let _guard = lock.lock().await;
        let mut state = self.store.load(thread).await?;
        let now = chrono::Utc::now().timestamp();
        state.receipts.retain(|receipt| receipt.expires_at > now);
        let duplicate = state
            .pending
            .iter()
            .find(|entry| entry.turn.id == turn.turn.id)
            .map(|entry| &entry.turn)
            .or_else(|| {
                turn.idempotency_key.as_ref().and_then(|key| {
                    state
                        .receipts
                        .iter()
                        .find(|receipt| &receipt.key == key && receipt.principal == turn.principal)
                        .map(|receipt| &receipt.turn)
                })
            });
        if let Some(record) = duplicate {
            return Ok(AcceptedTurn {
                turn: record.clone(),
                duplicate: true,
                interrupted: None,
            });
        }
        let interrupted = if turn.attention == TurnAttention::Interrupt {
            state
                .pending
                .front_mut()
                .filter(|head| head.started && head.principal == turn.principal)
                .map(|head| {
                    head.cancelled = true;
                    head.turn.id
                })
        } else {
            None
        };
        turn.cancelled = false;
        let record = turn.turn.clone();
        if let Some(key) = &turn.idempotency_key {
            state.receipts.push(TurnReceipt {
                principal: turn.principal.clone(),
                key: key.clone(),
                turn: record.clone(),
                expires_at: now + 24 * 60 * 60,
            });
        }
        if turn.started {
            let offset = state
                .pending
                .iter()
                .position(|entry| entry.turn.id > turn.turn.id)
                .unwrap_or(state.pending.len());
            state.pending.insert(offset, turn);
        } else {
            state.pending.push_back(turn);
        }
        self.store.save(thread, &state).await?;
        Ok(AcceptedTurn {
            turn: record,
            duplicate: false,
            interrupted,
        })
    }

    async fn claim(&self, thread: TurnThread) -> Result<Option<TurnLease>> {
        let mut owners = self
            .locks
            .owners
            .lock()
            .expect("turn queue owners poisoned");
        owners.retain(|_, owner| owner.strong_count() > 0);
        if owners.contains_key(&thread) {
            return Ok(None);
        }
        let lease = TurnLease::new(thread);
        owners.insert(thread, Arc::downgrade(&lease.identity));
        Ok(Some(lease))
    }

    async fn peek(&self, lease: &TurnLease) -> Result<Option<QueuedTurn<Work>>> {
        let lock = self.locks.mutation(lease.thread);
        let _guard = lock.lock().await;
        self.locks.check(lease)?;
        Ok(self
            .store
            .load(lease.thread)
            .await?
            .pending
            .front()
            .cloned())
    }

    async fn start(&self, lease: &TurnLease, turn: TurnId) -> Result<()> {
        let lock = self.locks.mutation(lease.thread);
        let _guard = lock.lock().await;
        self.locks.check(lease)?;
        let mut state = self.store.load(lease.thread).await?;
        let head = state.pending.front_mut().context("turn queue is empty")?;
        ensure!(head.turn.id == turn, "turn is not the queue head");
        head.started = true;
        self.store.save(lease.thread, &state).await
    }

    async fn cancel(
        &self,
        thread: TurnThread,
        turn: TurnId,
        authority: CancelAuthority,
    ) -> Result<CancelTurnOutcome> {
        let lock = self.locks.mutation(thread);
        let _guard = lock.lock().await;
        let mut state = self.store.load(thread).await?;
        let Some(entry) = state.pending.iter_mut().find(|entry| entry.turn.id == turn) else {
            return Ok(CancelTurnOutcome::NotFound);
        };
        if let CancelAuthority::Submitter(principal) = authority
            && entry.principal.as_deref() != Some(&principal)
        {
            return Ok(CancelTurnOutcome::NotAccessible);
        }
        let outcome = if entry.started {
            CancelTurnOutcome::Running
        } else {
            CancelTurnOutcome::Queued
        };
        // Keep a durable tombstone until the drain persists cancellation events.
        entry.cancelled = true;
        self.store.save(thread, &state).await?;
        Ok(outcome)
    }

    async fn cancelled(&self, lease: &TurnLease, turn: TurnId) -> Result<bool> {
        Ok(self
            .peek(lease)
            .await?
            .filter(|head| head.turn.id == turn)
            .context("turn is not the queue head")?
            .cancelled)
    }

    async fn acknowledge(&self, lease: &TurnLease, turn: TurnId) -> Result<()> {
        let lock = self.locks.mutation(lease.thread);
        let _guard = lock.lock().await;
        self.locks.check(lease)?;
        let mut state = self.store.load(lease.thread).await?;
        if state.acknowledged == Some(turn) {
            return Ok(());
        }
        ensure!(
            state
                .pending
                .front()
                .is_some_and(|head| head.turn.id == turn),
            "turn is not the queue head"
        );
        state.pending.pop_front();
        state.acknowledged = Some(turn);
        self.store.save(lease.thread, &state).await
    }

    async fn release_if_idle(&self, lease: &TurnLease) -> Result<bool> {
        let lock = self.locks.mutation(lease.thread);
        let _guard = lock.lock().await;
        self.locks.check(lease)?;
        if !self.store.load(lease.thread).await?.pending.is_empty() {
            return Ok(false);
        }
        self.locks
            .owners
            .lock()
            .expect("turn queue owners poisoned")
            .remove(&lease.thread);
        Ok(true)
    }
}

#[async_trait]
impl<Work: Clone + Send + Sync + 'static> TurnQueueDiscovery for StoredTurnCoordinator<Work> {
    async fn pending_threads(&self) -> Result<Vec<TurnThread>> {
        self.store.pending_threads().await
    }
}

struct MemoryStore<Work>(Mutex<HashMap<TurnThread, TurnQueueState<Work>>>);
#[async_trait]
impl<Work: Clone + Send + Sync + 'static> TurnQueueStore<Work> for MemoryStore<Work> {
    async fn load(&self, thread: TurnThread) -> Result<TurnQueueState<Work>> {
        Ok(self
            .0
            .lock()
            .expect("memory queue poisoned")
            .get(&thread)
            .cloned()
            .unwrap_or_default())
    }
    async fn save(&self, thread: TurnThread, state: &TurnQueueState<Work>) -> Result<()> {
        self.0
            .lock()
            .expect("memory queue poisoned")
            .insert(thread, state.clone());
        Ok(())
    }
    async fn pending_threads(&self) -> Result<Vec<TurnThread>> {
        Ok(self
            .0
            .lock()
            .expect("memory queue poisoned")
            .iter()
            .filter(|(_, state)| !state.pending.is_empty())
            .map(|(thread, _)| *thread)
            .collect())
    }
}
