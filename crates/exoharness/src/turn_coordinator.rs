//! Durable turn queues, independent of worker discovery and sandbox lifetime.
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::{AgentId, Result, ThreadId, TurnId, TurnRecord};

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

/// Caller-supplied work. Execution and control state belong to the coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSubmission<Work> {
    pub turn: TurnRecord,
    pub work: Work,
    pub principal: Option<String>,
    pub idempotency_key: Option<String>,
    pub attention: TurnAttention,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnControl {
    #[default]
    Run,
    Cancel,
    Suspend,
}

/// A coordinator-owned snapshot of pending work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedTurn<Work> {
    pub turn: TurnRecord,
    pub work: Work,
    pub principal: Option<String>,
    pub started: bool,
    pub control: TurnControl,
}

pub struct TurnStart<Work> {
    pub head: QueuedTurn<Work>,
    pub control: BoxStream<'static, Result<TurnControl>>,
}

#[derive(Debug, Clone)]
pub struct AcceptedTurn {
    pub turn: TurnRecord,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnControlOutcome {
    NotFound,
    NotAccessible,
    Queued,
    Running,
}

pub enum TurnAuthority {
    ThreadOwner,
    Submitter(String),
}

/// Ownership token scoped to one thread. The guard holds implementation-owned
/// resources until the last clone is dropped. Single-owner hosts may use ().
#[derive(Clone)]
pub struct TurnLease {
    pub thread: TurnThread,
    pub(crate) identity: Arc<()>,
    _guard: Arc<dyn Send + Sync>,
}

impl TurnLease {
    pub fn new(thread: TurnThread, guard: impl Send + Sync + 'static) -> Self {
        Self {
            thread,
            identity: Arc::new(()),
            _guard: Arc::new(guard),
        }
    }
}

#[async_trait]
pub trait TurnCoordinator<Work: Send + Sync>: Send + Sync {
    /// Startup discovery for hosts serving multiple threads. Hosts waking a
    /// known thread can use the default and drain that thread directly.
    async fn pending_threads(&self) -> Result<Vec<TurnThread>> {
        Ok(Vec::new())
    }
    /// Preserve queue order and atomically apply interruption and deduplication.
    /// The host owns durable wakeups (for example, setting a DO alarm) and must
    /// arrange one before reporting acceptance to its caller.
    async fn enqueue(&self, thread: TurnThread, turn: TurnSubmission<Work>)
    -> Result<AcceptedTurn>;
    async fn claim(&self, thread: TurnThread) -> Result<Option<TurnLease>>;
    /// Read a pending turn's accepted work and control state.
    async fn get(&self, thread: TurnThread, turn: TurnId) -> Result<Option<QueuedTurn<Work>>>;
    async fn peek(&self, lease: &TurnLease) -> Result<Option<QueuedTurn<Work>>>;
    /// Persist admission and subscribe atomically to control changes. The watch
    /// emits the current state first, then changes without a gap. Distributed
    /// backends must observe remote writes and report lost ownership or errors.
    /// Suspended heads remain unstarted until explicitly resumed.
    async fn start(&self, lease: &TurnLease, turn: TurnId) -> Result<TurnStart<Work>>;
    /// Persist a control request. Cancellation also makes a suspended head ready
    /// for terminalization. Resume cannot undo cancellation.
    async fn control(
        &self,
        thread: TurnThread,
        turn: TurnId,
        authority: TurnAuthority,
        control: TurnControl,
    ) -> Result<TurnControlOutcome>;
    /// Call after terminal events are durable. Repeating the last acknowledgment
    /// must succeed without removing a different head. Return the next head.
    async fn acknowledge(
        &self,
        lease: &TurnLease,
        turn: TurnId,
    ) -> Result<Option<QueuedTurn<Work>>>;
    /// Atomically release an empty queue or a suspended head. Resume and enqueue
    /// must race with this operation without losing a wakeup. Suspension retains
    /// queue order, the unfinished journal, and the original turn identity.
    async fn release_if_idle(&self, lease: &TurnLease) -> Result<bool>;
}

#[cfg(feature = "store")]
pub mod stored;
#[cfg(feature = "store")]
pub use stored::{StoredTurnCoordinator, TurnQueueLocks};
