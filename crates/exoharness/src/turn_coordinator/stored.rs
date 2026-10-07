use super::*;
use anyhow::{Context, ensure};
use futures::StreamExt;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, Weak};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio_stream::wrappers::WatchStream;

#[cfg(test)]
mod tests;

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnQueueState<Work> {
    pub pending: VecDeque<QueuedTurn<Work>>,
    pub receipts: Vec<TurnReceipt>,
    pub acknowledged: Option<TurnId>,
}

impl<Work> Default for TurnQueueState<Work> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            receipts: Vec::new(),
            acknowledged: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnReceipt {
    pub principal: Option<String>,
    pub key: String,
    pub turn: TurnRecord,
    pub expires_at: i64,
}

/// Persistence for a single process/object owner. Queue mutations are serialized
/// per thread. Competing workers need their own transactional coordinator.
#[async_trait]
pub trait TurnQueueStore<Work: Send + Sync>: Send + Sync {
    async fn load(&self, thread: TurnThread) -> Result<TurnQueueState<Work>>;
    async fn save(&self, thread: TurnThread, state: &TurnQueueState<Work>) -> Result<()>;
    async fn pending_threads(&self) -> Result<Vec<TurnThread>>;
}

#[derive(Default)]
pub struct TurnQueueLocks {
    threads: Mutex<HashMap<TurnThread, Weak<ThreadState>>>,
}

struct ThreadState {
    thread: TurnThread,
    locks: Weak<TurnQueueLocks>,
    mutation: AsyncMutex<ThreadMutation>,
}

#[derive(Default)]
struct ThreadMutation {
    owner: Weak<()>,
    control: Option<(TurnId, watch::Sender<TurnControl>)>,
}

impl Drop for ThreadState {
    fn drop(&mut self) {
        if let Some(locks) = self.locks.upgrade() {
            let mut threads = locks.threads.lock().expect("turn queue locks poisoned");
            if threads
                .get(&self.thread)
                .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
            {
                threads.remove(&self.thread);
            }
        }
    }
}

impl TurnQueueLocks {
    fn thread(self: &Arc<Self>, thread: TurnThread) -> Arc<ThreadState> {
        let mut threads = self.threads.lock().expect("turn queue locks poisoned");
        if let Some(state) = threads.get(&thread).and_then(Weak::upgrade) {
            return state;
        }
        let state = Arc::new(ThreadState {
            thread,
            locks: Arc::downgrade(self),
            mutation: AsyncMutex::default(),
        });
        threads.insert(thread, Arc::downgrade(&state));
        state
    }
}

impl ThreadMutation {
    fn check(&self, lease: &TurnLease) -> Result<()> {
        ensure!(
            self.owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &lease.identity)),
            "turn ownership lost"
        );
        Ok(())
    }
    fn notify(&self, turn: TurnId, control: TurnControl) {
        if let Some((id, sender)) = self.control.as_ref()
            && *id == turn
        {
            sender.send_replace(control);
        }
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
    /// Volatile queues for embedded runtimes. Hosts requiring durability must
    /// explicitly install a coordinator backed by their persistent state.
    pub fn in_memory() -> Self {
        Self::new(
            Arc::new(MemoryStore(Mutex::new(HashMap::new()))),
            Arc::default(),
        )
    }
}

#[async_trait]
impl<Work: Clone + Send + Sync + 'static> TurnCoordinator<QueuedTurn<Work>, TurnSubmission<Work>>
    for StoredTurnCoordinator<Work>
{
    type Scope = TurnThread;
    type Lease = TurnLease;
    type EnqueueOptions = ();
    type EnqueueOutcome = AcceptedTurn;
    type CancelOutcome = TurnControlOutcome;
    type Completion = Option<QueuedTurn<Work>>;

    async fn enqueue_turn(
        &self,
        thread: TurnThread,
        turn: TurnSubmission<Work>,
        (): (),
    ) -> Result<AcceptedTurn> {
        let state_lock = self.locks.thread(thread);
        let mutation = state_lock.mutation.lock().await;
        let mut state = self.store.load(thread).await?;
        let now = chrono::Utc::now().timestamp();
        state.receipts.retain(|receipt| receipt.expires_at > now);
        let duplicate = state
            .pending
            .iter()
            .find(|entry| entry.turn.id == turn.turn.id)
            .map(|entry| entry.turn.clone())
            .or_else(|| {
                turn.idempotency_key.as_ref().and_then(|key| {
                    state
                        .receipts
                        .iter()
                        .find(|receipt| &receipt.key == key && receipt.principal == turn.principal)
                        .map(|receipt| receipt.turn.clone())
                })
            });
        if let Some(record) = duplicate {
            return Ok(AcceptedTurn {
                turn: record,
                duplicate: true,
            });
        }
        let interrupted = if turn.attention == TurnAttention::Interrupt {
            state
                .pending
                .front_mut()
                .filter(|head| head.started && head.principal == turn.principal)
                .map(|head| {
                    head.control = TurnControl::Cancel;
                    head.turn.id
                })
        } else {
            None
        };
        let record = turn.turn.clone();
        if let Some(key) = turn.idempotency_key {
            state.receipts.push(TurnReceipt {
                principal: turn.principal.clone(),
                key,
                turn: record.clone(),
                expires_at: now + 24 * 60 * 60,
            });
        }
        let entry = QueuedTurn {
            turn: turn.turn,
            work: turn.work,
            principal: turn.principal,
            started: false,
            control: TurnControl::Run,
        };
        state.pending.push_back(entry);
        self.store.save(thread, &state).await?;
        if let Some(id) = interrupted {
            mutation.notify(id, TurnControl::Cancel);
        }
        Ok(AcceptedTurn {
            turn: record,
            duplicate: false,
        })
    }
    async fn peek_turn(&self, lease: &TurnLease) -> Result<Option<QueuedTurn<Work>>> {
        let state = self.locks.thread(lease.thread);
        let mutation = state.mutation.lock().await;
        mutation.check(lease)?;
        Ok(self
            .store
            .load(lease.thread)
            .await?
            .pending
            .front()
            .cloned())
    }
    async fn complete_turn(
        &self,
        lease: &TurnLease,
        turn: TurnId,
    ) -> Result<Option<QueuedTurn<Work>>> {
        let state_lock = self.locks.thread(lease.thread);
        let mut mutation = state_lock.mutation.lock().await;
        mutation.check(lease)?;
        let mut state = self.store.load(lease.thread).await?;
        if state.acknowledged == Some(turn) {
            return Ok(state.pending.front().cloned());
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
        self.store.save(lease.thread, &state).await?;
        mutation.control.take();
        Ok(state.pending.front().cloned())
    }
    async fn cancel_turn(
        &self,
        thread: TurnThread,
        turn: TurnId,
        user_id: &str,
        is_thread_owner: bool,
    ) -> Result<TurnControlOutcome> {
        let authority = if is_thread_owner {
            TurnAuthority::ThreadOwner
        } else {
            TurnAuthority::Submitter(user_id.to_owned())
        };
        self.control(thread, turn, authority, TurnControl::Cancel)
            .await
    }
    async fn turn_cancelled(&self, lease: &TurnLease, turn: TurnId) -> Result<bool> {
        let head = self
            .peek_turn(lease)
            .await?
            .context("turn queue is empty")?;
        ensure!(head.turn.id == turn, "turn is not the queue head");
        Ok(head.control == TurnControl::Cancel)
    }
}

#[async_trait]
impl<Work: Clone + Send + Sync + 'static> TurnQueue<Work> for StoredTurnCoordinator<Work> {
    async fn pending_threads(&self) -> Result<Vec<TurnThread>> {
        self.store.pending_threads().await
    }
    async fn claim(&self, thread: TurnThread) -> Result<Option<TurnLease>> {
        let state = self.locks.thread(thread);
        let mut mutation = state.mutation.lock().await;
        if mutation.owner.upgrade().is_some() {
            return Ok(None);
        }
        let lease = TurnLease::new(thread, state.clone());
        mutation.owner = Arc::downgrade(&lease.identity);
        Ok(Some(lease))
    }
    async fn get(&self, thread: TurnThread, turn: TurnId) -> Result<Option<QueuedTurn<Work>>> {
        let state = self.locks.thread(thread);
        let _mutation = state.mutation.lock().await;
        Ok(self
            .store
            .load(thread)
            .await?
            .pending
            .into_iter()
            .find(|entry| entry.turn.id == turn))
    }
    async fn start(&self, lease: &TurnLease, turn: TurnId) -> Result<TurnStart<Work>> {
        let state_lock = self.locks.thread(lease.thread);
        let mut mutation = state_lock.mutation.lock().await;
        mutation.check(lease)?;
        let mut state = self.store.load(lease.thread).await?;
        let head = state.pending.front_mut().context("turn queue is empty")?;
        ensure!(head.turn.id == turn, "turn is not the queue head");
        let changed = !head.started && head.control != TurnControl::Suspend;
        if changed {
            head.started = true;
        }
        let head = head.clone();
        if changed {
            self.store.save(lease.thread, &state).await?;
        }
        let (sender, receiver) = watch::channel(head.control);
        mutation.control = Some((turn, sender));
        Ok(TurnStart {
            head,
            control: Box::pin(WatchStream::new(receiver).map(Ok)),
        })
    }
    async fn control(
        &self,
        thread: TurnThread,
        turn: TurnId,
        authority: TurnAuthority,
        control: TurnControl,
    ) -> Result<TurnControlOutcome> {
        let state_lock = self.locks.thread(thread);
        let mutation = state_lock.mutation.lock().await;
        let mut state = self.store.load(thread).await?;
        let Some(entry) = state.pending.iter_mut().find(|entry| entry.turn.id == turn) else {
            return Ok(TurnControlOutcome::NotFound);
        };
        if let TurnAuthority::Submitter(principal) = authority
            && entry.principal.as_deref() != Some(&principal)
        {
            return Ok(TurnControlOutcome::NotAccessible);
        }
        ensure!(
            entry.control != TurnControl::Cancel || control == TurnControl::Cancel,
            "cancelled turns cannot be resumed or suspended"
        );
        let outcome = if entry.started {
            TurnControlOutcome::Running
        } else {
            TurnControlOutcome::Queued
        };
        entry.control = control;
        self.store.save(thread, &state).await?;
        mutation.notify(turn, control);
        Ok(outcome)
    }
    async fn release_if_idle(&self, lease: &TurnLease) -> Result<bool> {
        let state = self.locks.thread(lease.thread);
        let mut mutation = state.mutation.lock().await;
        mutation.check(lease)?;
        if self
            .store
            .load(lease.thread)
            .await?
            .pending
            .front()
            .is_some_and(|head| head.control != TurnControl::Suspend)
        {
            return Ok(false);
        }
        mutation.owner = Weak::new();
        mutation.control.take();
        Ok(true)
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
            .filter(|(_, state)| {
                state
                    .pending
                    .front()
                    .is_some_and(|head| head.control != TurnControl::Suspend)
            })
            .map(|(thread, _)| *thread)
            .collect())
    }
}
