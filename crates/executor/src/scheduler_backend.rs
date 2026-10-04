//! The scheduler persistence contract.
//!
//! [`SchedulerStoreBackend`] is the seam between the scheduler's *correctness
//! logic* (lease/claim semantics, missed-fire planning, fire delivery) and the
//! *place the records live*. [`SchedulerStore`] (local filesystem) is the
//! production backend; tests embed an in-memory one.
//!
//! Design notes, kept beside the code because they constrain every future
//! backend:
//!
//! - **Object-safe, `#[async_trait]`** — the runtime fans out with
//!   `futures::future::try_join_all`, so backends are shared as
//!   `Arc<dyn SchedulerStoreBackend>` (house style, mirroring
//!   `exoharness::types::ExoHarness`).
//! - **Least-privilege method set** — only operations the runtime and tools
//!   actually call. `root()`/path helpers are fs-private and stay off the trait.
//! - **Claims live on the trait, not in an impl.** [`claim_due_tasks`]'s
//!   winner-take-all semantics come from
//!   [`Self::try_claim_slot`], which every backend must implement atomically;
//!   the default-provided [`Self::claim_due_tasks`] composes it, so a new
//!   backend inherits correct claim behavior with only `try_claim_slot` to
//!   get right.
//!
//! See exoharness/exo#184 for the overall durable-scheduler plan.

use anyhow::Result;

use crate::scheduler_types::{
    NewScheduledTask, ScheduledFireRecord, ScheduledTaskRecord, ScheduledTaskRunRecord,
};

/// Atomic test-and-set for one grid slot of one task.
///
/// Implementations must guarantee: if this returns `Ok(true)`, no other
/// claimant holds (or can later acquire) the same `(task_id, slot_ms)` while
/// the claim is live. Returning `Ok(false)` means "you lost; the task should
/// be skipped this round" — never both true.
#[async_trait::async_trait]
pub trait SlotClaimer: Send + Sync {
    async fn try_claim_slot(
        &self,
        now_ms: u64,
        lease_ms: u64,
        task: &mut ScheduledTaskRecord,
    ) -> Result<bool>;

    /// Frees a decided `(task, slot)` so the next slot of the same task is
    /// immediately claimable. No-op when no claim exists.
    async fn release_claim(&self, task_id: &str, slot_ms: u64) -> Result<()>;
}

#[async_trait::async_trait]
pub trait SchedulerStoreBackend: SlotClaimer {
    async fn create_task(&self, request: NewScheduledTask) -> Result<ScheduledTaskRecord>;

    async fn list_tasks(&self) -> Result<Vec<ScheduledTaskRecord>>;

    async fn get_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>>;

    async fn put_task(&self, task: &ScheduledTaskRecord) -> Result<()>;

    async fn disable_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>>;

    async fn delete_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>>;

    async fn put_pending_fire(&self, fire: &ScheduledFireRecord) -> Result<()>;

    async fn pending_fires(&self) -> Result<Vec<ScheduledFireRecord>>;

    async fn mark_fire_delivered(&self, task_id: &str, slot_ms: u64) -> Result<()>;

    async fn fire_was_delivered(&self, task_id: &str, slot_ms: u64) -> Result<bool>;

    async fn put_run(&self, run: &ScheduledTaskRunRecord) -> Result<()>;

    /// Filtered view; default composes `list_tasks` so minimal backends only
    /// implement enumeration.
    async fn list_tasks_for_conversation(
        &self,
        agent_id: &str,
        conversation_id: &str,
        include_disabled: bool,
    ) -> Result<Vec<ScheduledTaskRecord>> {
        Ok(self
            .list_tasks()
            .await?
            .into_iter()
            .filter(|task| task.agent_id == agent_id && task.conversation_id == conversation_id)
            .filter(|task| include_disabled || task.enabled)
            .collect())
    }

    /// Due for firing at `now_ms`; default composes `list_tasks`.
    async fn due_tasks(&self, now_ms: u64) -> Result<Vec<ScheduledTaskRecord>> {
        Ok(self
            .list_tasks()
            .await?
            .into_iter()
            .filter(|task| task.is_due(now_ms))
            .collect())
    }

    /// Claim up to `limit` due tasks, oldest slot first. Semantics (sort,
    /// truncate, per-task atomic claim) are fixed here so every backend gets
    /// identical behavior; only [`SlotClaimer::try_claim_slot`] varies.
    async fn claim_due_tasks(
        &self,
        now_ms: u64,
        limit: usize,
        lease_ms: u64,
    ) -> Result<Vec<ScheduledTaskRecord>> {
        let mut due = self.due_tasks(now_ms).await?;
        due.sort_by_key(|task| task.next_run_at_ms);
        due.truncate(limit);
        let mut claimed = Vec::new();
        for mut task in due {
            if self.try_claim_slot(now_ms, lease_ms, &mut task).await? {
                claimed.push(task);
            }
        }
        Ok(claimed)
    }
}
