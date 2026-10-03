//! In-memory [`SchedulerStoreBackend`] — the test double.
//!
//! Mirrors the filesystem backend's durability *logic* (atomic slot claims,
//! decided-slot release, delivered-fire dedupe) without touching disk, so
//! runtime tests exercise the same contracts the production path does.
//! Claim atomicity is a `HashMap` entry guarded by the async-fn serialization
//! a single-actor test needs; for cross-actor races the contract is validated
//! in `scheduler_store`'s fs tests, not here.

use std::sync::Mutex;

use anyhow::Result;

use crate::scheduler_backend::{SchedulerStoreBackend, SlotClaimer};
use crate::scheduler_types::{
    NewScheduledTask, ScheduledFireRecord, ScheduledTaskRecord, ScheduledTaskRunRecord,
};

#[derive(Default)]
pub struct InMemorySchedulerStore {
    tasks: Mutex<Vec<ScheduledTaskRecord>>,
    pending_fires: Mutex<Vec<ScheduledFireRecord>>,
    delivered_slots: Mutex<Vec<(String, u64)>>,
    claim_markers: Mutex<Vec<(String, u64, u64)>>, // (task, slot, expires_at_ms)
    runs: Mutex<Vec<ScheduledTaskRunRecord>>,
}

impl InMemorySchedulerStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl SlotClaimer for InMemorySchedulerStore {
    async fn try_claim_slot(
        &self,
        now_ms: u64,
        lease_ms: u64,
        task: &mut ScheduledTaskRecord,
    ) -> Result<bool> {
        if !task.is_due(now_ms) {
            return Ok(false);
        }
        let mut markers = self.claim_markers.lock().unwrap();
        let slot = task.next_run_at_ms;
        if let Some((_, _, expires_at_ms)) = markers
            .iter()
            .find(|(id, slot_ms, _)| *id == task.id && *slot_ms == slot)
        {
            if *expires_at_ms > now_ms {
                return Ok(false); // live claim
            }
            markers.retain(|(id, slot_ms, _)| !(*id == task.id && *slot_ms == slot));
        }
        markers.push((task.id.clone(), slot, now_ms.saturating_add(lease_ms)));
        task.claim(now_ms, lease_ms);
        Ok(true)
    }

    async fn release_claim(&self, task_id: &str, slot_ms: u64) -> Result<()> {
        self.claim_markers
            .lock()
            .unwrap()
            .retain(|(id, slot, _)| !(*id == task_id && *slot == slot_ms));
        Ok(())
    }
}

#[async_trait::async_trait]
impl SchedulerStoreBackend for InMemorySchedulerStore {
    async fn create_task(&self, request: NewScheduledTask) -> Result<ScheduledTaskRecord> {
        let task = ScheduledTaskRecord::new(request, crate::scheduler_types::now_ms())?;
        self.tasks.lock().unwrap().push(task.clone());
        Ok(task)
    }

    async fn list_tasks(&self) -> Result<Vec<ScheduledTaskRecord>> {
        let mut tasks = self.tasks.lock().unwrap().clone();
        tasks.sort_by(|l, r| l.name.cmp(&r.name).then(l.id.cmp(&r.id)));
        Ok(tasks)
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>> {
        Ok(self
            .tasks
            .lock()
            .unwrap()
            .iter()
            .find(|task| task.id == task_id)
            .cloned())
    }

    async fn put_task(&self, task: &ScheduledTaskRecord) -> Result<()> {
        let mut tasks = self.tasks.lock().unwrap();
        if let Some(stored) = tasks.iter_mut().find(|t| t.id == task.id) {
            *stored = task.clone();
        } else {
            tasks.push(task.clone());
        }
        Ok(())
    }

    async fn disable_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>> {
        let mut tasks = self.tasks.lock().unwrap();
        if let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) {
            task.enabled = false;
            task.updated_at_ms = crate::scheduler_types::now_ms();
            return Ok(Some(task.clone()));
        }
        Ok(None)
    }

    async fn delete_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>> {
        let mut tasks = self.tasks.lock().unwrap();
        let index = tasks.iter().position(|task| task.id == task_id);
        match index {
            Some(index) => {
                let task = tasks.remove(index);
                self.runs
                    .lock()
                    .unwrap()
                    .retain(|run| run.task_id != task_id);
                Ok(Some(task))
            }
            None => Ok(None),
        }
    }

    async fn put_pending_fire(&self, fire: &ScheduledFireRecord) -> Result<()> {
        if self
            .delivered_slots
            .lock()
            .unwrap()
            .contains(&(fire.task_id.clone(), fire.slot_ms))
        {
            return Ok(()); // a retry cannot resurrect a delivered wakeup
        }
        self.pending_fires.lock().unwrap().push(fire.clone());
        Ok(())
    }

    async fn pending_fires(&self) -> Result<Vec<ScheduledFireRecord>> {
        let mut fires = self.pending_fires.lock().unwrap().clone();
        fires.sort_by_key(|f| (f.fired_at_ms, f.slot_ms));
        Ok(fires)
    }

    async fn mark_fire_delivered(&self, task_id: &str, slot_ms: u64) -> Result<()> {
        self.pending_fires
            .lock()
            .unwrap()
            .retain(|f| !(f.task_id == task_id && f.slot_ms == slot_ms));
        self.delivered_slots
            .lock()
            .unwrap()
            .push((task_id.to_string(), slot_ms));
        Ok(())
    }

    async fn fire_was_delivered(&self, task_id: &str, slot_ms: u64) -> Result<bool> {
        Ok(self
            .delivered_slots
            .lock()
            .unwrap()
            .contains(&(task_id.to_string(), slot_ms)))
    }

    async fn put_run(&self, run: &ScheduledTaskRunRecord) -> Result<()> {
        self.runs.lock().unwrap().push(run.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_task(name: &str) -> NewScheduledTask {
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
            missed: None,
        }
    }

    #[tokio::test]
    async fn claims_are_winner_take_all_like_the_fs_backend() {
        let store = InMemorySchedulerStore::new();
        let mut task = store.create_task(new_task("check")).await.unwrap();
        task.next_run_at_ms = 1;
        store.put_task(&task).await.unwrap();

        let mut a = store.get_task(&task.id).await.unwrap().unwrap();
        let mut b = store.get_task(&task.id).await.unwrap().unwrap();
        assert!(store.try_claim_slot(2, 100, &mut a).await.unwrap());
        assert!(!store.try_claim_slot(2, 100, &mut b).await.unwrap());
        assert!(
            store.try_claim_slot(103, 100, &mut b).await.unwrap(),
            "after lease expiry the claimed slot is reclaimable"
        );
    }

    #[tokio::test]
    async fn put_task_acts_as_upsert() {
        let store = InMemorySchedulerStore::new();
        let mut task = store.create_task(new_task("check")).await.unwrap();
        task.next_run_at_ms = 42;
        store.put_task(&task).await.unwrap();
        assert_eq!(store.due_tasks(100).await.unwrap().len(), 1);
        task.enabled = false;
        store.put_task(&task).await.unwrap();
        assert!(store.due_tasks(100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delivered_fires_are_not_revived() {
        let store = InMemorySchedulerStore::new();
        let fire = ScheduledFireRecord {
            task_id: "t".into(),
            task_name: "check".into(),
            slot_ms: 1_000,
            run_id: "r".into(),
            agent_id: "agent".into(),
            conversation_id: "conversation".into(),
            prompt: "p".into(),
            fired_at_ms: 1_000,
        };
        store.put_pending_fire(&fire).await.unwrap();
        store.mark_fire_delivered("t", 1_000).await.unwrap();
        store.put_pending_fire(&fire).await.unwrap();
        assert!(store.pending_fires().await.unwrap().is_empty());
        assert!(store.fire_was_delivered("t", 1_000).await.unwrap());
    }
}
