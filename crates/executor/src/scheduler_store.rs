use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::fs;

use crate::scheduler_types::{
    NewScheduledTask, ScheduledTaskRecord, ScheduledTaskRunRecord, now_ms,
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
            tasks.push(serde_json::from_slice::<ScheduledTaskRecord>(&bytes)?);
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

    pub async fn get_task(&self, task_id: &str) -> Result<Option<ScheduledTaskRecord>> {
        let path = self.task_path(task_id);
        match fs::read(&path).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice::<ScheduledTaskRecord>(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("failed to read scheduled task {}", path.display())),
        }
    }

    pub async fn put_task(&self, task: &ScheduledTaskRecord) -> Result<()> {
        fs::create_dir_all(self.tasks_dir()).await?;
        let path = self.task_path(&task.id);
        fs::write(&path, serde_json::to_vec_pretty(task)?)
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

    pub async fn put_run(&self, run: &ScheduledTaskRunRecord) -> Result<()> {
        fs::create_dir_all(self.runs_dir(&run.task_id)).await?;
        let path = self.run_path(&run.task_id, &run.id);
        fs::write(&path, serde_json::to_vec_pretty(run)?)
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
}
