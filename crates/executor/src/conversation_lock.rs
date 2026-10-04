use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Mutex as AsyncMutex;

pub(crate) fn conversation_send_lock(conversation_id: &str) -> Arc<AsyncMutex<()>> {
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
