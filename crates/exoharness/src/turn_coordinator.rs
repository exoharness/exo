//! Durable turn queues, independent of worker discovery and sandbox lifetime.
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{AgentId, Result, ThreadId, TurnId, TurnRecord, Uuid7};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnThread {
    pub agent_id: AgentId,
    pub thread_id: ThreadId,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAttention {
    #[default]
    Wake,
    Interrupt,
}

impl TurnAttention {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wake => "wake",
            Self::Interrupt => "interrupt",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedTurn<Work> {
    pub turn: TurnRecord,
    pub work: Work,
    pub principal: Option<String>,
    pub idempotency_key: Option<String>,
    pub attention: TurnAttention,
    pub started: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone)]
pub struct AcceptedTurn {
    pub turn: TurnRecord,
    pub duplicate: bool,
    pub interrupted: Option<TurnId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelTurnOutcome {
    NotFound,
    NotAccessible,
    Queued,
    Running,
}

pub enum CancelAuthority {
    ThreadOwner,
    Submitter(String),
}

/// Ownership is interpreted by the implementation, without exposing worker IDs
/// or requiring timed leases. Expiring implementations must stop execution when
/// their ownership expires; queue ownership alone does not fence event writes.
#[derive(Clone)]
pub struct TurnLease {
    pub thread: TurnThread,
    identity: Arc<Uuid7>,
    revoked: Shared<BoxFuture<'static, ()>>,
}

impl TurnLease {
    pub fn new(thread: TurnThread) -> Self {
        Self::with_revocation(thread, futures::future::pending())
    }

    /// Expiring backends supply a future that renews ownership and completes
    /// when ownership is lost. Single-owner hosts use a non-expiring lease.
    pub fn with_revocation(
        thread: TurnThread,
        revoked: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self {
            thread,
            identity: Arc::new(Uuid7::now()),
            revoked: revoked.boxed().shared(),
        }
    }

    pub fn token(&self) -> Uuid7 {
        *self.identity
    }

    pub async fn revoked(&self) {
        self.revoked.clone().await;
    }

    pub fn same_owner(&self, other: &Self) -> bool {
        self.thread == other.thread && Arc::ptr_eq(&self.identity, &other.identity)
    }
}

#[async_trait]
pub trait TurnCoordinator<Work: Send + Sync>: Send + Sync {
    /// Preserve queue order and atomically apply interruption and deduplication.
    /// Accepted work must remain discoverable after a crash. Hosts without a
    /// polling scheduler must durably arrange a wakeup before reporting success.
    async fn enqueue(&self, thread: TurnThread, turn: QueuedTurn<Work>) -> Result<AcceptedTurn>;
    async fn claim(&self, thread: TurnThread) -> Result<Option<TurnLease>>;
    async fn peek(&self, lease: &TurnLease) -> Result<Option<QueuedTurn<Work>>>;
    /// Persist execution admission before writing the turn's recovery journal.
    async fn start(&self, lease: &TurnLease, turn: TurnId) -> Result<()>;
    async fn cancel(
        &self,
        thread: TurnThread,
        turn: TurnId,
        authority: CancelAuthority,
    ) -> Result<CancelTurnOutcome>;
    async fn cancelled(&self, lease: &TurnLease, turn: TurnId) -> Result<bool>;
    /// Call after terminal events are durable. Repeating the last acknowledgment
    /// must succeed without removing a different head.
    async fn acknowledge(&self, lease: &TurnLease, turn: TurnId) -> Result<()>;
    /// Release only while empty, atomically with enqueue, preventing lost wakes.
    async fn release_if_idle(&self, lease: &TurnLease) -> Result<bool>;
}

/// Startup discovery for local and worker-pool deployments. A thread Durable
/// Object can wake its own queue without implementing global discovery.
#[async_trait]
pub trait TurnQueueDiscovery: Send + Sync {
    async fn pending_threads(&self) -> Result<Vec<TurnThread>>;
}

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

/// Persistence for the single-owner implementation. All read/modify/write
/// operations are serialized by shared TurnQueueLocks. The host must enforce a
/// single process/object owner; competing workers need a transactional backend.
#[async_trait]
pub trait TurnQueueStore<Work: Send + Sync>: Send + Sync {
    async fn load(&self, thread: TurnThread) -> Result<TurnQueueState<Work>>;
    async fn save(&self, thread: TurnThread, state: &TurnQueueState<Work>) -> Result<()>;
    async fn pending_threads(&self) -> Result<Vec<TurnThread>>;
}

#[cfg(feature = "store")]
mod stored;
#[cfg(feature = "store")]
pub use stored::{StoredTurnCoordinator, TurnQueueLocks};
