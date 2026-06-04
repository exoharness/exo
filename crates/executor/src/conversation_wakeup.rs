use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use exoharness::Result;
use lingua::Message;
use lingua::universal::UserContent;
use tokio::sync::Mutex as AsyncMutex;

use crate::{HarnessConversation, SendRequest, SendResult};

pub async fn send_conversation_wakeup(
    conversation: &dyn HarnessConversation,
    prompt: String,
) -> Result<SendResult> {
    let wakeup_lock = conversation_wakeup_lock(&conversation.record().id.to_string());
    let _guard = wakeup_lock.lock().await;
    let _file_guard = WakeupFileLock::acquire(&conversation.record().id.to_string()).await?;
    let result = conversation
        .send(SendRequest {
            input: vec![Message::User {
                content: UserContent::String(prompt),
            }],
            session_id: None,
        })
        .await?;
    conversation.close_session(result.session_id).await?;
    Ok(result)
}

struct WakeupFileLock {
    path: PathBuf,
}

impl WakeupFileLock {
    async fn acquire(conversation_id: &str) -> Result<Self> {
        let dir = std::env::temp_dir().join("exo-wakeup-locks");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{conversation_id}.lock"));
        loop {
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
            {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    remove_stale_lock(&path).await?;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to acquire wakeup lock {}", path.display())
                    });
                }
            }
        }
    }
}

impl Drop for WakeupFileLock {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            tracing::error!(
                path = %self.path.display(),
                %error,
                "failed to remove wakeup lock"
            );
        }
    }
}

async fn remove_stale_lock(path: &PathBuf) -> Result<()> {
    const STALE_AFTER: Duration = Duration::from_secs(30 * 60);
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return Ok(());
    };
    let Ok(modified) = metadata.modified() else {
        return Ok(());
    };
    if SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > STALE_AFTER)
    {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn conversation_wakeup_lock(conversation_id: &str) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .expect("conversation wakeup lock registry poisoned");
    Arc::clone(
        locks
            .entry(conversation_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
    )
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use exoharness::Uuid7;

    use super::*;

    #[tokio::test]
    async fn wakeup_file_lock_serializes_conversation_ids() {
        let conversation_id = format!("test-{}", Uuid7::now());
        let first = WakeupFileLock::acquire(&conversation_id).await.unwrap();
        let mut second = pin!(WakeupFileLock::acquire(&conversation_id));

        tokio::select! {
            _ = &mut second => {
                panic!("second lock acquired while first lock was held");
            }
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap();
        drop(second);
    }
}
