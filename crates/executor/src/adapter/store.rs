use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::fs;

use super::types::{
    AdapterEventRecord, AdapterEventType, AdapterOutboundMessageRecord, AdapterRecord, NewAdapter,
    now_ms,
};

#[derive(Debug, Clone)]
pub struct AdapterStore {
    root: PathBuf,
}

impl AdapterStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn create_adapter(&self, request: NewAdapter) -> Result<AdapterRecord> {
        let adapter = AdapterRecord::new(request, now_ms())?;
        self.put_adapter(&adapter).await?;
        Ok(adapter)
    }

    pub async fn list_adapters(&self) -> Result<Vec<AdapterRecord>> {
        let adapter_dir = self.adapters_dir();
        match fs::metadata(&adapter_dir).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        }
        let mut entries = fs::read_dir(&adapter_dir)
            .await
            .with_context(|| format!("failed to read adapter directory {adapter_dir:?}"))?;
        let mut adapters = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path)
                .await
                .with_context(|| format!("failed to read adapter {}", path.display()))?;
            adapters.push(serde_json::from_slice::<AdapterRecord>(&bytes)?);
        }
        adapters.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        Ok(adapters)
    }

    pub async fn list_adapters_for_conversation(
        &self,
        agent_id: &str,
        conversation_id: &str,
        include_disabled: bool,
    ) -> Result<Vec<AdapterRecord>> {
        Ok(self
            .list_adapters()
            .await?
            .into_iter()
            .filter(|adapter| {
                adapter.agent_id == agent_id && adapter.conversation_id == conversation_id
            })
            .filter(|adapter| include_disabled || adapter.enabled)
            .collect())
    }

    pub async fn enabled_adapters(&self) -> Result<Vec<AdapterRecord>> {
        Ok(self
            .list_adapters()
            .await?
            .into_iter()
            .filter(|adapter| adapter.enabled)
            .collect())
    }

    pub async fn get_adapter(&self, adapter_id: &str) -> Result<Option<AdapterRecord>> {
        let path = self.adapter_path(adapter_id);
        match fs::read(&path).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice::<AdapterRecord>(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("failed to read adapter {}", path.display()))
            }
        }
    }

    pub async fn put_adapter(&self, adapter: &AdapterRecord) -> Result<()> {
        fs::create_dir_all(self.adapters_dir()).await?;
        let path = self.adapter_path(&adapter.id);
        fs::write(&path, serde_json::to_vec_pretty(adapter)?)
            .await
            .with_context(|| format!("failed to write adapter {}", path.display()))
    }

    pub async fn disable_adapter(&self, adapter_id: &str) -> Result<Option<AdapterRecord>> {
        let Some(mut adapter) = self.get_adapter(adapter_id).await? else {
            return Ok(None);
        };
        adapter.enabled = false;
        adapter.updated_at_ms = now_ms();
        self.put_adapter(&adapter).await?;
        Ok(Some(adapter))
    }

    pub async fn delete_adapter(&self, adapter_id: &str) -> Result<Option<AdapterRecord>> {
        let Some(adapter) = self.get_adapter(adapter_id).await? else {
            return Ok(None);
        };
        remove_file_if_exists(self.adapter_path(adapter_id)).await?;
        remove_dir_if_exists(self.events_dir(adapter_id)).await?;
        remove_dir_if_exists(self.outbox_dir(adapter_id)).await?;
        Ok(Some(adapter))
    }

    pub async fn mark_connected(&self, adapter_id: &str) -> Result<Option<AdapterRecord>> {
        let Some(mut adapter) = self.get_adapter(adapter_id).await? else {
            return Ok(None);
        };
        adapter.last_connected_at_ms = Some(now_ms());
        adapter.last_error = None;
        adapter.updated_at_ms = now_ms();
        self.put_adapter(&adapter).await?;
        Ok(Some(adapter))
    }

    pub async fn mark_error(
        &self,
        adapter_id: &str,
        error: impl Into<String>,
    ) -> Result<Option<AdapterRecord>> {
        let Some(mut adapter) = self.get_adapter(adapter_id).await? else {
            return Ok(None);
        };
        adapter.last_error = Some(error.into());
        adapter.updated_at_ms = now_ms();
        self.put_adapter(&adapter).await?;
        Ok(Some(adapter))
    }

    pub async fn put_event(&self, event: &AdapterEventRecord) -> Result<()> {
        fs::create_dir_all(self.events_dir(&event.adapter_id)).await?;
        let path = self.event_path(&event.adapter_id, &event.id);
        fs::write(&path, serde_json::to_vec_pretty(event)?)
            .await
            .with_context(|| format!("failed to write adapter event {}", path.display()))
    }

    pub async fn record_event(
        &self,
        adapter_id: String,
        event_type: AdapterEventType,
        summary: String,
    ) -> Result<AdapterEventRecord> {
        let event = AdapterEventRecord::new(adapter_id.clone(), event_type, summary, now_ms())?;
        self.put_event(&event).await?;
        if let Some(mut adapter) = self.get_adapter(&adapter_id).await? {
            adapter.updated_at_ms = now_ms();
            self.put_adapter(&adapter).await?;
        }
        Ok(event)
    }

    pub async fn enqueue_outbound_message(
        &self,
        adapter_id: String,
        text: String,
        target: Option<String>,
    ) -> Result<AdapterOutboundMessageRecord> {
        let message = AdapterOutboundMessageRecord::new(adapter_id, text, target, now_ms())?;
        fs::create_dir_all(self.outbox_dir(&message.adapter_id)).await?;
        let path = self.outbox_path(&message.adapter_id, &message.id);
        fs::write(&path, serde_json::to_vec_pretty(&message)?)
            .await
            .with_context(|| {
                format!(
                    "failed to write adapter outbound message {}",
                    path.display()
                )
            })?;
        Ok(message)
    }

    pub async fn take_outbound_messages(
        &self,
        adapter_id: &str,
    ) -> Result<Vec<AdapterOutboundMessageRecord>> {
        let outbox_dir = self.outbox_dir(adapter_id);
        match fs::metadata(&outbox_dir).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.into()),
        }
        let mut entries = fs::read_dir(&outbox_dir)
            .await
            .with_context(|| format!("failed to read adapter outbox directory {outbox_dir:?}"))?;
        let mut messages = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path).await.with_context(|| {
                format!("failed to read adapter outbound message {}", path.display())
            })?;
            let message = serde_json::from_slice::<AdapterOutboundMessageRecord>(&bytes)?;
            remove_file_if_exists(path).await?;
            messages.push(message);
        }
        messages.sort_by_key(|message| message.created_at_ms);
        Ok(messages)
    }

    fn adapters_dir(&self) -> PathBuf {
        self.root.join("adapters")
    }

    fn adapter_path(&self, adapter_id: &str) -> PathBuf {
        self.adapters_dir().join(format!("{adapter_id}.json"))
    }

    fn events_dir(&self, adapter_id: &str) -> PathBuf {
        self.root.join("events").join(adapter_id)
    }

    fn event_path(&self, adapter_id: &str, event_id: &str) -> PathBuf {
        self.events_dir(adapter_id).join(format!("{event_id}.json"))
    }

    fn outbox_dir(&self, adapter_id: &str) -> PathBuf {
        self.root.join("outbox").join(adapter_id)
    }

    fn outbox_path(&self, adapter_id: &str, message_id: &str) -> PathBuf {
        self.outbox_dir(adapter_id)
            .join(format!("{message_id}.json"))
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

    use super::super::types::{AdapterConfig, AdapterEventType, AdapterSource};

    use super::*;

    #[tokio::test]
    async fn creates_lists_disables_and_deletes_adapters() {
        let tempdir = TempDir::new().unwrap();
        let store = AdapterStore::new(tempdir.path());
        let adapter = store
            .create_adapter(NewAdapter {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "irc".to_string(),
                source: AdapterSource::BuiltIn,
                config: AdapterConfig {
                    adapter_type: "irc".to_string(),
                    worker_command: vec!["node".to_string(), "irc.js".to_string()],
                    initialization: serde_json::json!({}),
                    state_dir: None,
                    secret_env: Vec::new(),
                },
            })
            .await
            .unwrap();

        assert_eq!(store.list_adapters().await.unwrap(), vec![adapter.clone()]);
        store
            .record_event(
                adapter.id.clone(),
                AdapterEventType::Connected,
                "connected".to_string(),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .list_adapters_for_conversation("agent", "conversation", false)
                .await
                .unwrap()
                .len(),
            1
        );
        store.disable_adapter(&adapter.id).await.unwrap();
        assert!(
            store
                .list_adapters_for_conversation("agent", "conversation", false)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.delete_adapter(&adapter.id).await.unwrap().is_some());
        assert!(store.get_adapter(&adapter.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn preserves_outbound_targets() {
        let tempdir = TempDir::new().unwrap();
        let store = AdapterStore::new(tempdir.path());
        let message = store
            .enqueue_outbound_message(
                "adapter".to_string(),
                "hello".to_string(),
                Some("123@s.whatsapp.net".to_string()),
            )
            .await
            .unwrap();

        assert_eq!(message.target.as_deref(), Some("123@s.whatsapp.net"));
        let messages = store.take_outbound_messages("adapter").await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].target.as_deref(), Some("123@s.whatsapp.net"));
        assert!(
            store
                .take_outbound_messages("adapter")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
