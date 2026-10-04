use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use exoharness::Uuid7;
use serde::Serialize;
use tokio::fs;

use crate::scheduler_backend::{SchedulerStoreBackend, SlotClaimer};

use crate::scheduler_types::{
    NewScheduledTask, ScheduledFireRecord, ScheduledTaskRecord, ScheduledTaskRunRecord,
    migrate_scheduled_task, now_ms,
};

#[derive(Debug, Clone)]
pub struct SchedulerStore {
    root: PathBuf,
}

impl SchedulerStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn create_task(&self, request: NewScheduledTask) -> Result<ScheduledTaskRecord> {
        let task = ScheduledTaskRecord::new(request, now_ms())?;
        self.put_task(&task).await?;
        Ok(task)
    }

    pub async fn list_tasks(&self) -> Result<Vec<ScheduledTaskRecord>> {
        let task_dir = self.tasks_dir();
        match fs::metadata(&task_dir).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        }
        let mut entries = fs::read_dir(&task_dir)
            .await
            .with_context(|| format!("failed to read scheduled task directory {task_dir:?}"))?;
        let mut tasks = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path)
                .await
                .with_context(|| format!("failed to read scheduled task {}", path.display()))?;
            tasks.push(decode_task(&bytes)?);
        }
        tasks.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        Ok(tasks)
    }

    pub async fn list_tasks_for_conversation(
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

    pub async fn due_tasks(&self, now_ms: u64) -> Result<Vec<ScheduledTaskRecord>> {
        Ok(self
            .list_tasks()
            .await?
            .into_iter()
            .filter(|task| task.is_due(now_ms))
            .collect())
    }

    /// Atomically claims one due task, or reports loss to a concurrent claimant.
    ///
    /// The claim marker is a file created with `create_new` semantics, keyed by
    /// `(task_id, next_run_at_ms)`: exactly one of two racing runners can
    /// create it, so the read-then-lease race in [`Self::claim_due_tasks`]
    /// becomes an atomic test-and-set. The written task record carries the
    /// lease for observability (listings show who holds it), but correctness
    /// rests on the marker file, which is claimed-or-lost in one syscall.
    ///
    /// Markers key by `next_run_at_ms` rather than a lease count so a released
    /// claim (grid advanced to the next slot) can never collide with a stale
    /// marker for the previous slot, and sweep to delete decided markers.
    pub async fn claim_task_atomically(
        &self,
        now_ms: u64,
        lease_ms: u64,
        task: &mut ScheduledTaskRecord,
    ) -> Result<bool> {
        if !task.is_due(now_ms) {
            return Ok(false);
        }
        let claimed_path = self.claim_path(&task.id, task.next_run_at_ms);
        // A marker only blocks while its lease is live; once the lease has
        // expired the marker names a crashed/stalled runner and the slot is
        // reclaimable. Expired markers are swept so the create_new below can
        // be the single atomic decision point.
        if self.claim_marker_exists(&claimed_path).await? {
            if let Some(expires_at_ms) = self.claim_marker_expiry(&claimed_path).await? {
                if expires_at_ms > now_ms {
                    return Ok(false); // live lease held by another runner
                }
                self.release_claim(&task.id, task.next_run_at_ms)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to sweep expired claim marker {}",
                            claimed_path.display()
                        )
                    })?;
            } else {
                // Unparseable marker (truncated write, foreign content):
                // treat as live — sweeping it here would let two runners
                // sweep-and-win the same slot.
                return Ok(false);
            }
        }
        fs::create_dir_all(self.claims_dir())
            .await
            .with_context(|| format!("failed to create claim directory {:?}", self.claims_dir()))?;
        let payload = serde_json::to_vec(&serde_json::json!({
            "task_id": task.id,
            "slot_ms": task.next_run_at_ms,
            "claimed_at_ms": now_ms,
            "expires_at_ms": now_ms.saturating_add(lease_ms),
            "lease_ms": lease_ms,
            "runner": std::process::id(),
        }))?;
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&claimed_path)
            .await
        {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt as _;
                file.write_all(&payload).await.with_context(|| {
                    format!("failed to write claim marker {}", claimed_path.display())
                })?;
                file.sync_all().await.with_context(|| {
                    format!("failed to flush claim marker {}", claimed_path.display())
                })?;
            }
            // Another runner won the create_new race: we lose the claim.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(false);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create claim marker {}", claimed_path.display())
                });
            }
        }
        // Claim won. Stamp the lease onto the record for observability; this
        // write loses races safely because the marker already settled ownership.
        task.claim(now_ms, lease_ms);
        self.put_task(task).await?;
        Ok(true)
    }

    pub async fn claim_marker_exists(&self, path: &Path) -> Result<bool> {
        match fs::metadata(path).await {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error)
                .with_context(|| format!("failed to stat claim marker {}", path.display())),
        }
    }

    /// Lease expiry stamped inside a claim marker; `None` when the payload is
    /// absent or unparseable. The marker's `expires_at_ms` is the authoritative
    /// claim lifetime — the record's lease copy is observability only.
    async fn claim_marker_expiry(&self, path: &Path) -> Result<Option<u64>> {
        let bytes = match fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read claim marker {}", path.display()));
            }
        };
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => Ok(value.get("expires_at_ms").and_then(|v| v.as_u64())),
            Err(_) => Ok(None),
        }
    }

    /// Deletes the claim marker for a slot once a run is recorded, so a
    /// multi-slot catch-up burst can claim consecutive slots of the same task.
    pub async fn release_claim(&self, task_id: &str, slot_ms: u64) -> Result<()> {
        match fs::remove_file(self.claim_path(task_id, slot_ms)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| {
                format!("failed to release claim for task {task_id} slot {slot_ms}")
            }),
        }
    }

    fn claims_dir(&self) -> PathBuf {
        self.root.join("claims")
    }

    fn claim_path(&self, task_id: &str, slot_ms: u64) -> PathBuf {
        self.claims_dir().join(format!("{task_id}-{slot_ms}.json"))
    }

    /// Reads, leases, and writes back without a conditional put, so two
    /// runners racing the same due task can both win. The PID lockfile in the
    /// runner is the real guard today. The fix is a claim keyed by
    /// `(task, slot)` written conditionally, which is deferred pending the
    /// conditional puts in upstream PR #113 rather than raced against it.
    pub async fn claim_due_tasks(
        &self,
        now_ms: u64,
        limit: usize,
        lease_ms: u64,
    ) -> Result<Vec<ScheduledTaskRecord>> {
        let mut due = self.due_tasks(now_ms).await?;
        due.sort_by_key(|task| task.next_run_at_ms);
        due.truncate(limit);
        let mut claimed = Vec::new();
        for task in due {
            let mut candidate = task;
            if self
                .claim_task_atomically(now_ms, lease_ms, &mut candidate)
                .await?
            {
                claimed.push(candidate);
            }
        }
        Ok(claimed)
    }

    pub async fn get_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>> {
        let path = self.task_path(task_id);
        match fs::read(&path).await {
            Ok(bytes) => Ok(Some(decode_task(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("failed to read scheduled task {}", path.display())),
        }
    }

    pub async fn put_task(&self, task: &ScheduledTaskRecord) -> Result<()> {
        fs::create_dir_all(self.tasks_dir()).await?;
        let path = self.task_path(&task.id);
        write_json_file(&path, task)
            .await
            .with_context(|| format!("failed to write scheduled task {}", path.display()))
    }

    pub async fn disable_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>> {
        let Some(mut task) = self.get_task(task_id).await? else {
            return Ok(None);
        };
        task.enabled = false;
        task.updated_at_ms = now_ms();
        self.put_task(&task).await?;
        Ok(Some(task))
    }

    pub async fn delete_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>> {
        let Some(task) = self.get_task(task_id).await? else {
            return Ok(None);
        };
        remove_file_if_exists(self.task_path(task_id)).await?;
        remove_dir_if_exists(self.runs_dir(task_id)).await?;
        Ok(Some(task))
    }

    /// Records a fire whose wakeup has not been delivered yet. A no-op if this
    /// `(task, slot)` was already delivered, so a retry cannot resurrect a
    /// wakeup the conversation has already had.
    pub async fn put_pending_fire(&self, fire: &ScheduledFireRecord) -> Result<()> {
        if self.fire_was_delivered(&fire.task_id, fire.slot_ms).await? {
            return Ok(());
        }
        fs::create_dir_all(self.pending_fires_dir()).await?;
        let path = self.pending_fire_path(&fire.task_id, fire.slot_ms);
        write_json_file(&path, fire)
            .await
            .with_context(|| format!("failed to write scheduled task fire {}", path.display()))
    }

    /// Fires written but never confirmed delivered, oldest first.
    pub async fn pending_fires(&self) -> Result<Vec<ScheduledFireRecord>> {
        let dir = self.pending_fires_dir();
        match fs::metadata(&dir).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        }
        let mut entries = fs::read_dir(&dir)
            .await
            .with_context(|| format!("failed to read scheduled fire directory {dir:?}"))?;
        let mut fires = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path).await.with_context(|| {
                format!("failed to read scheduled task fire {}", path.display())
            })?;
            fires.push(serde_json::from_slice::<ScheduledFireRecord>(&bytes)?);
        }
        fires.sort_by_key(|fire| (fire.fired_at_ms, fire.slot_ms));
        Ok(fires)
    }

    /// Moves a fire from pending to delivered. The rename is the commit point,
    /// mirroring how the adapter outbox claims a message.
    pub async fn mark_fire_delivered(&self, task_id: &str, slot_ms: u64) -> Result<()> {
        let pending_path = self.pending_fire_path(task_id, slot_ms);
        match fs::metadata(&pending_path).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        fs::create_dir_all(self.delivered_fires_dir()).await?;
        let delivered_path = self.delivered_fire_path(task_id, slot_ms);
        fs::rename(&pending_path, &delivered_path)
            .await
            .with_context(|| {
                format!(
                    "failed to mark scheduled task fire {} delivered as {}",
                    pending_path.display(),
                    delivered_path.display()
                )
            })
    }

    pub async fn fire_was_delivered(&self, task_id: &str, slot_ms: u64) -> Result<bool> {
        match fs::metadata(self.delivered_fire_path(task_id, slot_ms)).await {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn put_run(&self, run: &ScheduledTaskRunRecord) -> Result<()> {
        fs::create_dir_all(self.runs_dir(&run.task_id)).await?;
        let path = self.run_path(&run.task_id, &run.id);
        write_json_file(&path, run)
            .await
            .with_context(|| format!("failed to write scheduled task run {}", path.display()))
    }

    fn tasks_dir(&self) -> PathBuf {
        self.root.join("tasks")
    }

    fn task_path(&self, task_id: &str) -> PathBuf {
        self.tasks_dir().join(format!("{task_id}.json"))
    }

    fn runs_dir(&self, task_id: &str) -> PathBuf {
        self.root.join("runs").join(task_id)
    }

    fn run_path(&self, task_id: &str, run_id: &str) -> PathBuf {
        self.runs_dir(task_id).join(format!("{run_id}.json"))
    }

    fn pending_fires_dir(&self) -> PathBuf {
        self.root.join("fires").join("pending")
    }

    fn delivered_fires_dir(&self) -> PathBuf {
        self.root.join("fires").join("delivered")
    }

    fn pending_fire_path(&self, task_id: &str, slot_ms: u64) -> PathBuf {
        self.pending_fires_dir()
            .join(format!("{task_id}-{slot_ms}.json"))
    }

    fn delivered_fire_path(&self, task_id: &str, slot_ms: u64) -> PathBuf {
        self.delivered_fires_dir()
            .join(format!("{task_id}-{slot_ms}.json"))
    }
}

// The filesystem backend is the reference implementation of the persistence
// contract; the trait impl is deliberately thin over the inherent methods so
// existing call sites keep compiling unchanged.
#[async_trait::async_trait]
impl SlotClaimer for SchedulerStore {
    async fn try_claim_slot(
        &self,
        now_ms: u64,
        lease_ms: u64,
        task: &mut ScheduledTaskRecord,
    ) -> Result<bool> {
        self.claim_task_atomically(now_ms, lease_ms, task).await
    }

    async fn release_claim(&self, task_id: &str, slot_ms: u64) -> Result<()> {
        self.release_claim(task_id, slot_ms).await
    }
}

#[async_trait::async_trait]
impl SchedulerStoreBackend for SchedulerStore {
    async fn create_task(&self, request: NewScheduledTask) -> Result<ScheduledTaskRecord> {
        SchedulerStore::create_task(self, request).await
    }
    async fn list_tasks(&self) -> Result<Vec<ScheduledTaskRecord>> {
        SchedulerStore::list_tasks(self).await
    }
    async fn get_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>> {
        SchedulerStore::get_task(self, task_id).await
    }
    async fn put_task(&self, task: &ScheduledTaskRecord) -> Result<()> {
        SchedulerStore::put_task(self, task).await
    }
    async fn disable_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>> {
        SchedulerStore::disable_task(self, task_id).await
    }
    async fn delete_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>> {
        SchedulerStore::delete_task(self, task_id).await
    }
    async fn put_pending_fire(&self, fire: &ScheduledFireRecord) -> Result<()> {
        SchedulerStore::put_pending_fire(self, fire).await
    }
    async fn pending_fires(&self) -> Result<Vec<ScheduledFireRecord>> {
        SchedulerStore::pending_fires(self).await
    }
    async fn mark_fire_delivered(&self, task_id: &str, slot_ms: u64) -> Result<()> {
        SchedulerStore::mark_fire_delivered(self, task_id, slot_ms).await
    }
    async fn fire_was_delivered(&self, task_id: &str, slot_ms: u64) -> Result<bool> {
        SchedulerStore::fire_was_delivered(self, task_id, slot_ms).await
    }
    async fn put_run(&self, run: &ScheduledTaskRunRecord) -> Result<()> {
        SchedulerStore::put_run(self, run).await
    }
    async fn list_tasks_for_conversation(
        &self,
        agent_id: &str,
        conversation_id: &str,
        include_disabled: bool,
    ) -> Result<Vec<ScheduledTaskRecord>> {
        SchedulerStore::list_tasks_for_conversation(
            self,
            agent_id,
            conversation_id,
            include_disabled,
        )
        .await
    }
    async fn due_tasks(&self, now_ms: u64) -> Result<Vec<ScheduledTaskRecord>> {
        SchedulerStore::due_tasks(self, now_ms).await
    }
    async fn claim_due_tasks(
        &self,
        now_ms: u64,
        limit: usize,
        lease_ms: u64,
    ) -> Result<Vec<ScheduledTaskRecord>> {
        SchedulerStore::claim_due_tasks(self, now_ms, limit, lease_ms).await
    }
}

fn decode_task(bytes: &[u8]) -> Result<ScheduledTaskRecord> {
    migrate_scheduled_task(serde_json::from_slice::<ScheduledTaskRecord>(bytes)?)
}

/// Writes JSON through a temp file so a crash mid-write leaves the previous
/// record intact instead of a half-written one. Same shape as the adapter
/// store's writer.
async fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temp_path = path.with_extension(format!("json.{}.tmp", Uuid7::now()));
    fs::write(&temp_path, serde_json::to_vec_pretty(value)?)
        .await
        .with_context(|| format!("failed to write temp file {}", temp_path.display()))?;
    fs::rename(&temp_path, path).await.with_context(|| {
        format!(
            "failed to replace {} with temp file {}",
            path.display(),
            temp_path.display()
        )
    })
}

async fn remove_file_if_exists(path: PathBuf) -> Result<()> {
    match fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to delete file {}", path.display()))
        }
    }
}

async fn remove_dir_if_exists(path: PathBuf) -> Result<()> {
    match fs::remove_dir_all(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to delete directory {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::scheduler_types::SCHEDULED_TASK_SCHEMA_VERSION;

    #[tokio::test]
    async fn creates_and_lists_tasks() {
        let tempdir = TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());
        let task = store
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
                missed: None,
            })
            .await
            .unwrap();

        assert_eq!(store.list_tasks().await.unwrap(), vec![task]);
    }

    #[tokio::test]
    async fn disables_and_deletes_tasks() {
        let tempdir = TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());
        let task = store
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
                missed: None,
            })
            .await
            .unwrap();

        store.disable_task(&task.id).await.unwrap();
        assert!(
            store
                .list_tasks_for_conversation("agent", "conversation", false)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .list_tasks_for_conversation("agent", "conversation", true)
                .await
                .unwrap()
                .len(),
            1
        );

        let deleted = store.delete_task(&task.id).await.unwrap().unwrap();
        assert_eq!(deleted.id, task.id);
        assert!(store.get_task(&task.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reads_unversioned_task_records_as_version_one() {
        let tempdir = TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());
        let task = store
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
                missed: None,
            })
            .await
            .unwrap();

        // Rewrite the record the way a pre-versioning build left it on disk.
        let path = store.task_path(&task.id);
        let mut stored =
            serde_json::from_slice::<serde_json::Value>(&tokio::fs::read(&path).await.unwrap())
                .unwrap();
        stored
            .as_object_mut()
            .expect("task record is a json object")
            .remove("schema_version");
        tokio::fs::write(&path, serde_json::to_vec_pretty(&stored).unwrap())
            .await
            .unwrap();

        let migrated = store.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(migrated.schema_version, SCHEDULED_TASK_SCHEMA_VERSION);
        assert_eq!(migrated, task);
    }

    #[tokio::test]
    async fn rejects_task_records_from_a_newer_schema() {
        let tempdir = TempDir::new().unwrap();
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
                missed: None,
            })
            .await
            .unwrap();
        task.schema_version = SCHEDULED_TASK_SCHEMA_VERSION + 1;
        store.put_task(&task).await.unwrap();

        let error = store.get_task(&task.id).await.unwrap_err();
        assert!(
            error.to_string().contains("does not understand"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn task_writes_leave_no_temp_files_behind() {
        let tempdir = TempDir::new().unwrap();
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
                missed: None,
            })
            .await
            .unwrap();
        task.next_run_at_ms = 42;
        store.put_task(&task).await.unwrap();

        let mut entries = tokio::fs::read_dir(store.tasks_dir()).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, vec![format!("{}.json", task.id)]);
        assert_eq!(store.get_task(&task.id).await.unwrap().unwrap(), task);
    }

    #[tokio::test]
    async fn completed_one_shot_stays_listed_but_is_never_due() {
        let tempdir = TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());
        let mut task = store
            .create_task(NewScheduledTask {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "remind".to_string(),
                schedule: "@at 1970-01-01T00:00:10Z".to_string(),
                sandbox_mode: None,
                setup_command: None,
                command: vec!["true".to_string()],
                report_prompt: "Report.".to_string(),
                max_output_bytes: None,
                missed: None,
            })
            .await
            .unwrap();
        assert_eq!(store.due_tasks(10_000).await.unwrap().len(), 1);

        let plan = task.plan_missed_fires(10_000).unwrap();
        task.resume_after_fires(&plan, 10_000);
        store.put_task(&task).await.unwrap();

        assert!(store.due_tasks(u64::MAX).await.unwrap().is_empty());
        assert_eq!(
            store
                .list_tasks_for_conversation("agent", "conversation", false)
                .await
                .unwrap()
                .len(),
            1,
            "a fired one-shot is history, not a hidden task"
        );
    }

    #[tokio::test]
    async fn claim_due_tasks_leases_until_expiry() {
        let tempdir = TempDir::new().unwrap();
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
                missed: None,
            })
            .await
            .unwrap();
        task.next_run_at_ms = 1;
        store.put_task(&task).await.unwrap();

        let claimed = store.claim_due_tasks(2, 10, 100).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(store.claim_due_tasks(3, 10, 100).await.unwrap().is_empty());
        assert_eq!(store.claim_due_tasks(103, 10, 100).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn atomic_claim_is_winner_take_all_between_racing_runners() {
        let tempdir = TempDir::new().unwrap();
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
                missed: None,
            })
            .await
            .unwrap();
        task.next_run_at_ms = 1;
        store.put_task(&task).await.unwrap();

        // Two runners read the same due record; only one claim survives.
        let mut runner_a = store.get_task(&task.id).await.unwrap().unwrap();
        let mut runner_b = store.get_task(&task.id).await.unwrap().unwrap();
        assert!(
            store
                .claim_task_atomically(2, 100, &mut runner_a)
                .await
                .unwrap(),
            "first claimant wins an uncontested claim"
        );
        assert!(
            !store
                .claim_task_atomically(2, 100, &mut runner_b)
                .await
                .unwrap(),
            "second claimant must lose the race, not double-fire"
        );

        // The stored record carries exactly the winner's lease.
        let stored = store.get_task(&task.id).await.unwrap().unwrap();
        assert_eq!(
            stored.lease.as_ref().map(|lease| lease.leased_at_ms),
            Some(2)
        );
    }

    #[tokio::test]
    async fn released_claim_allows_the_next_slot_to_be_claimed() {
        let tempdir = TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());
        let task = store
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
                missed: None,
            })
            .await
            .unwrap();

        // Simulate a catch-up burst: two consecutive grid slots to claim.
        let mut slot_one = store.get_task(&task.id).await.unwrap().unwrap();
        slot_one.next_run_at_ms = 1_000;
        slot_one.lease = None;
        store.put_task(&slot_one).await.unwrap();
        assert!(
            store
                .claim_task_atomically(1_100, 100, &mut slot_one)
                .await
                .unwrap()
        );
        assert!(
            !store
                .claim_task_atomically(1_100, 100, &mut slot_one)
                .await
                .unwrap(),
            "re-claiming the same slot must lose even with a stale local copy"
        );

        // Run recorded: the (task, slot) claim is released so the next slot,
        // a different (task, slot) key, can be claimed without a lease wait.
        store.release_claim(&task.id, 1_000).await.unwrap();
        assert!(
            !store
                .claim_marker_exists(&store.claim_path(&task.id, 1_000))
                .await
                .unwrap()
        );

        let mut slot_two = store.get_task(&task.id).await.unwrap().unwrap();
        slot_two.next_run_at_ms = 2_000;
        slot_two.lease = None;
        store.put_task(&slot_two).await.unwrap();
        assert!(
            store
                .claim_task_atomically(2_100, 100, &mut slot_two)
                .await
                .unwrap(),
            "a released slot claim must not block the next slot"
        );
    }

    #[tokio::test]
    async fn atomic_claim_respects_lease_expiry_by_marker_not_record() {
        let tempdir = TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());
        let task = store
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
                missed: None,
            })
            .await
            .unwrap();

        // A crashed runner left a claim marker but the record write never
        // happened (crash between marker create and put_task). Even though
        // the record shows no lease, the marker says the slot is taken.
        let mut ghost = store.get_task(&task.id).await.unwrap().unwrap();
        ghost.next_run_at_ms = 1;
        store.put_task(&ghost).await.unwrap();
        let claimed_path = store.claim_path(&ghost.id, 1);
        tokio::fs::create_dir_all(store.claims_dir()).await.unwrap();
        tokio::fs::write(&claimed_path, b"ghost claim")
            .await
            .unwrap();

        let mut contender = store.get_task(&ghost.id).await.unwrap().unwrap();
        assert!(
            !store
                .claim_task_atomically(2, 100, &mut contender)
                .await
                .unwrap(),
            "a marker with no matching record still blocks re-claim; sweep reclaims it"
        );
        store.release_claim(&ghost.id, 1).await.unwrap();
        let mut contender = store.get_task(&ghost.id).await.unwrap().unwrap();
        assert!(
            store
                .claim_task_atomically(2, 100, &mut contender)
                .await
                .unwrap(),
            "after the stale marker is swept, the slot is claimable"
        );
    }

    fn fire(task_id: &str, slot_ms: u64) -> ScheduledFireRecord {
        ScheduledFireRecord {
            task_id: task_id.to_string(),
            task_name: "check".to_string(),
            slot_ms,
            run_id: "run".to_string(),
            agent_id: "agent".to_string(),
            conversation_id: "conversation".to_string(),
            prompt: "Scheduled task `check` completed.".to_string(),
            fired_at_ms: slot_ms,
        }
    }

    #[tokio::test]
    async fn pending_fires_survive_until_delivery_is_marked() {
        let tempdir = TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());

        store.put_pending_fire(&fire("task", 1_000)).await.unwrap();
        store.put_pending_fire(&fire("task", 2_000)).await.unwrap();
        assert_eq!(
            store
                .pending_fires()
                .await
                .unwrap()
                .iter()
                .map(|fire| fire.slot_ms)
                .collect::<Vec<_>>(),
            vec![1_000, 2_000],
            "a restart should see both undelivered wakeups, oldest first"
        );

        store.mark_fire_delivered("task", 1_000).await.unwrap();
        assert_eq!(
            store
                .pending_fires()
                .await
                .unwrap()
                .iter()
                .map(|fire| fire.slot_ms)
                .collect::<Vec<_>>(),
            vec![2_000]
        );
        assert!(store.fire_was_delivered("task", 1_000).await.unwrap());
        assert!(!store.fire_was_delivered("task", 2_000).await.unwrap());
    }

    #[tokio::test]
    async fn a_delivered_slot_cannot_be_woken_again() {
        let tempdir = TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());
        store.put_pending_fire(&fire("task", 1_000)).await.unwrap();
        store.mark_fire_delivered("task", 1_000).await.unwrap();

        // A retry of the same (task, slot) must not re-queue the wakeup.
        store.put_pending_fire(&fire("task", 1_000)).await.unwrap();
        assert!(store.pending_fires().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn marking_an_unknown_fire_delivered_is_a_no_op() {
        let tempdir = TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());

        store.mark_fire_delivered("task", 1_000).await.unwrap();
        assert!(store.pending_fires().await.unwrap().is_empty());
        assert!(!store.fire_was_delivered("task", 1_000).await.unwrap());
    }

    #[tokio::test]
    async fn fires_for_different_slots_are_independent() {
        let tempdir = TempDir::new().unwrap();
        let store = SchedulerStore::new(tempdir.path());
        store
            .put_pending_fire(&fire("task-a", 1_000))
            .await
            .unwrap();
        store
            .put_pending_fire(&fire("task-b", 1_000))
            .await
            .unwrap();

        store.mark_fire_delivered("task-a", 1_000).await.unwrap();

        let pending = store.pending_fires().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].task_id, "task-b");
    }
}
