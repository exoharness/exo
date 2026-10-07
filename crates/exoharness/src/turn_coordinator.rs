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

/// Ordered pending work. Hosts supply their own scope, ownership token, and
/// admission results; queue payloads need not expose executor or journal state.
#[async_trait]
pub trait TurnCoordinator<Work: Send + Sync, Submission: Send = Work>: Send + Sync {
    type Scope: Send + Sync;
    type Lease: Send + Sync;
    type EnqueueOptions: Send;
    type EnqueueOutcome: Send;
    type CancelOutcome: Send;
    type Completion: Send;

    /// Append durably, deduplicate, and apply interruption atomically. The host
    /// must arrange any durable wakeup before reporting acceptance.
    async fn enqueue_turn(
        &self,
        scope: Self::Scope,
        turn: Submission,
        options: Self::EnqueueOptions,
    ) -> Result<Self::EnqueueOutcome>;
    /// Read the head without removing it, checking ownership.
    async fn peek_turn(&self, lease: &Self::Lease) -> Result<Option<Work>>;
    /// Authorize and persist cancellation. A backend may remove queued work or
    /// retain it for its executor to terminalize; the outcome tells the host.
    async fn cancel_turn(
        &self,
        scope: Self::Scope,
        turn: TurnId,
        user_id: &str,
        is_thread_owner: bool,
    ) -> Result<Self::CancelOutcome>;
    /// Check cancellation and ownership during execution.
    async fn turn_cancelled(&self, lease: &Self::Lease, turn: TurnId) -> Result<bool>;
    /// Remove only the specified head after its terminal events are durable.
    async fn complete_turn(&self, lease: &Self::Lease, turn: TurnId) -> Result<Self::Completion>;
}

/// Discovery and renewable ownership for hosts with competing workers.
/// Journal writes are not fenced by this interface: a worker that loses its
/// lease must stop when renewal or a cancellation check reports ownership loss.
#[async_trait]
pub trait WorkerTurnCoordinator<Work: Send + Sync>: TurnCoordinator<Work> {
    type Owner: Send + Sync;

    fn runtime_id(&self) -> &Self::Owner;
    fn lease_expiration_timeout(&self) -> std::time::Duration;
    async fn pending_turn_count(&self) -> Result<u64>;
    async fn claim_ready_conversations(&self, max: usize) -> Result<Vec<Self::Lease>>;
    async fn renew_conversation(&self, lease: &Self::Lease, processing: bool) -> Result<bool>;
    async fn release_idle_conversation(&self, lease: &Self::Lease) -> Result<bool>;
}

/// Admission, recovery and suspension used by Exo's shared executor runtime.
/// These capabilities do not impose extra storage state on other hosts.
/// Completion must tolerate repeating the last acknowledgment without popping
/// another head. Cancellation retains queued work for durable terminalization.
#[async_trait]
pub trait TurnQueue<Work: Send + Sync>:
    TurnCoordinator<
        QueuedTurn<Work>,
        TurnSubmission<Work>,
        Scope = TurnThread,
        Lease = TurnLease,
        EnqueueOptions = (),
        EnqueueOutcome = AcceptedTurn,
        CancelOutcome = TurnControlOutcome,
        Completion = Option<QueuedTurn<Work>>,
    >
{
    /// Startup discovery for hosts serving multiple threads. Hosts waking a
    /// known thread can drain that thread directly.
    async fn pending_threads(&self) -> Result<Vec<TurnThread>> {
        Ok(Vec::new())
    }
    async fn claim(&self, thread: TurnThread) -> Result<Option<TurnLease>>;
    async fn get(&self, thread: TurnThread, turn: TurnId) -> Result<Option<QueuedTurn<Work>>>;
    /// Persist admission and subscribe atomically to control changes. The watch
    /// emits current state first, then changes without a gap, including ownership
    /// loss. Suspended heads remain unstarted until explicitly resumed.
    async fn start(&self, lease: &TurnLease, turn: TurnId) -> Result<TurnStart<Work>>;
    /// Resume cannot undo cancellation.
    async fn control(
        &self,
        thread: TurnThread,
        turn: TurnId,
        authority: TurnAuthority,
        control: TurnControl,
    ) -> Result<TurnControlOutcome>;
    /// Release only an empty queue or suspended head, without losing wakeups.
    async fn release_if_idle(&self, lease: &TurnLease) -> Result<bool>;
}

#[cfg(feature = "store")]
pub mod stored;
#[cfg(feature = "store")]
pub use stored::{StoredTurnCoordinator, TurnQueueLocks};
