//! Pluggable work sources for the scheduler claim loop.
//!
//! The scheduler's `run_due_tasks` loop historically claimed work from exactly
//! one place: the file/db-backed [`SchedulerStore`]. A [`WorkSource`] abstracts
//! "where due work comes from" so the loop can drain from a list of sources
//! while keeping exo's existing claim / lease / run / record contract intact.
//!
//! The contract is deliberately narrow so it can land upstream:
//!
//! * A source is asked to **atomically claim** up to `limit` units of due work,
//!   each leased for `lease_ms`, and returns them as [`ScheduledTaskRecord`]s
//!   (the unit exo already knows how to run).
//! * Recording the *result* of a run stays with the [`SchedulerStore`] exactly
//!   as before. A source may optionally observe completion via
//!   [`ClaimedWork::on_complete`] (e.g. to acknowledge an external queue), but
//!   the store remains the system of record for the run itself.
//!
//! The existing store is exposed as [`StoreWorkSource`]; [`MeshBoardSource`]
//! is one external impl (see [`crate::mesh_work_source`]).

use std::fmt;

use anyhow::Result;
use async_trait::async_trait;

use crate::scheduler_store::SchedulerStore;
use crate::scheduler_types::ScheduledTaskRecord;

/// Outcome of running a unit of claimed work, handed to
/// [`ClaimedWork::on_complete`] so an external source can close its own loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkOutcome {
    /// The command ran to completion (regardless of its exit code).
    Completed { exit_code: Option<i32> },
    /// The run errored before/while producing an exit code.
    Errored,
}

/// A boxed, fallible completion hook. Sources that need to acknowledge work
/// back to an external system (e.g. `meshi self complete`) attach one of these;
/// the store-backed source attaches none.
pub type CompletionHook =
    Box<dyn FnOnce(WorkOutcome) -> futures::future::BoxFuture<'static, Result<()>> + Send>;

/// One unit of work a [`WorkSource`] has atomically claimed and leased.
///
/// The [`ScheduledTaskRecord`] is the schedulable unit exo runs. `on_complete`,
/// if present, is invoked by the scheduler after the run finishes so the source
/// can acknowledge the external system that owns the underlying item.
pub struct ClaimedWork {
    /// Identifies the source that produced this work (for tracing/telemetry).
    pub source: String,
    /// The schedulable task exo will run.
    pub task: ScheduledTaskRecord,
    /// Optional hook invoked once the run finishes.
    pub on_complete: Option<CompletionHook>,
}

impl ClaimedWork {
    /// Wrap a record claimed from a source with no external completion hook
    /// (the store-backed case: the store records the run itself).
    pub fn from_store(source: impl Into<String>, task: ScheduledTaskRecord) -> Self {
        Self {
            source: source.into(),
            task,
            on_complete: None,
        }
    }
}

impl fmt::Debug for ClaimedWork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaimedWork")
            .field("source", &self.source)
            .field("task_id", &self.task.id)
            .field("has_completion_hook", &self.on_complete.is_some())
            .finish()
    }
}

/// Abstracts where the scheduler's due work comes from.
///
/// Implementations own the *claim*: returning a record means the source has
/// taken responsibility for it (leased it, removed it from its own "open" set,
/// etc.) so two concurrent drains never double-run the same unit. This mirrors
/// [`SchedulerStore::claim_due_tasks`], which exo already relied on.
#[async_trait]
pub trait WorkSource: Send + Sync {
    /// A stable identifier for the source (used in tracing).
    fn name(&self) -> &str;

    /// Atomically claim up to `limit` units of due work, each leased for
    /// `lease_ms`. Implementations must not return more than `limit` units and
    /// must not return the same unit to two concurrent callers.
    async fn claim_due(&self, now_ms: u64, limit: usize, lease_ms: u64)
    -> Result<Vec<ClaimedWork>>;
}

/// The default work source: the file/db-backed [`SchedulerStore`].
///
/// This is a thin adapter that preserves exo's existing semantics exactly —
/// it delegates to [`SchedulerStore::claim_due_tasks`] and attaches no
/// completion hook, because the store records runs through `put_run`/`put_task`
/// in the scheduler as it always has.
#[derive(Clone)]
pub struct StoreWorkSource {
    store: SchedulerStore,
    name: String,
}

impl StoreWorkSource {
    pub fn new(store: SchedulerStore) -> Self {
        Self {
            store,
            name: "store".to_string(),
        }
    }

    pub fn store(&self) -> &SchedulerStore {
        &self.store
    }
}

#[async_trait]
impl WorkSource for StoreWorkSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        limit: usize,
        lease_ms: u64,
    ) -> Result<Vec<ClaimedWork>> {
        let claimed = self.store.claim_due_tasks(now_ms, limit, lease_ms).await?;
        Ok(claimed
            .into_iter()
            .map(|task| ClaimedWork::from_store("store", task))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler_types::NewScheduledTask;

    fn sample_task(name: &str) -> ScheduledTaskRecord {
        ScheduledTaskRecord::new(
            NewScheduledTask {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: name.to_string(),
                schedule: "@every 1m".to_string(),
                sandbox_mode: None,
                setup_command: None,
                command: vec!["true".to_string()],
                report_prompt: "Report.".to_string(),
                max_output_bytes: None,
            },
            1,
        )
        .unwrap()
    }

    #[test]
    fn from_store_has_no_completion_hook() {
        let work = ClaimedWork::from_store("store", sample_task("check"));
        assert_eq!(work.source, "store");
        assert!(work.on_complete.is_none());
    }

    #[tokio::test]
    async fn store_work_source_claims_and_leases() {
        let tempdir = tempfile::TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());
        let mut task = store
            .create_task(NewScheduledTask {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "check".to_string(),
                schedule: "@every 1m".to_string(),
                sandbox_mode: None,
                setup_command: None,
                command: vec!["true".to_string()],
                report_prompt: "Report.".to_string(),
                max_output_bytes: None,
            })
            .await
            .unwrap();
        task.next_run_at_ms = 1;
        store.put_task(&task).await.unwrap();

        let source = StoreWorkSource::new(store);
        let claimed = source.claim_due(2, 10, 100).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].source, "store");
        assert!(claimed[0].on_complete.is_none());
        // Lease holds: a second claim before expiry yields nothing.
        assert!(source.claim_due(3, 10, 100).await.unwrap().is_empty());
    }
}
