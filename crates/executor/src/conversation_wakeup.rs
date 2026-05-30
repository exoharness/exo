use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

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
