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

/// Options shared by local submissions and remote runtime requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub attention: TurnAttention,
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

/// Admission semantics shared by hosts with different execution machinery.
#[async_trait]
pub trait TurnAdmission<Work: Send + Sync>: Send + Sync {
    /// Append in FIFO order. Interrupt atomically cancels the same principal's
    /// claimed or started head before appending. Unclaimed work is undisturbed.
    /// Duplicate principal/key pairs return the original turn and session for
    /// at least 24 hours, including after completion, without interrupting.
    /// The host must arrange a durable wakeup before reporting acceptance.
    async fn enqueue(&self, thread: TurnThread, turn: TurnSubmission<Work>)
    -> Result<AcceptedTurn>;
    /// Authorize the submitter or thread owner and prevent queued execution or
    /// request cancellation of the active head. Owners must observe cancellation
    /// within a bounded interval, including requests from other processes.
    async fn cancel(
        &self,
        thread: TurnThread,
        turn: TurnId,
        authority: TurnAuthority,
    ) -> Result<TurnControlOutcome>;
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

/// Queue admission and execution control consumed by Exo's executor.
#[async_trait]
pub trait TurnQueue<Work: Send + Sync>: TurnAdmission<Work> {
    /// Startup discovery for hosts serving multiple threads. Hosts waking a
    /// known thread can use the default and drain that thread directly.
    async fn pending_threads(&self) -> Result<Vec<TurnThread>> {
        Ok(Vec::new())
    }
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
