use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::RwLock;

use exoharness::{AgentId, ConversationId, EventId, Result, TurnHandle};
use tokio::sync::mpsc;

use crate::{AgentConfig, ExecutionStreamEvent};

pub(crate) fn cache_insert<K, V>(cache: &RwLock<HashMap<K, V>>, key: K, value: V, name: &str)
where
    K: Eq + Hash,
{
    cache.write().expect(name).insert(key, value);
}

pub(crate) async fn get_or_load_cached<K, V, Load, LoadFuture>(
    cache: &RwLock<HashMap<K, V>>,
    key: K,
    name: &str,
    load: Load,
) -> Result<V>
where
    K: Eq + Hash + Clone,
    V: Clone,
    Load: FnOnce() -> LoadFuture,
    LoadFuture: Future<Output = Result<V>>,
{
    {
        let cache = cache.read().expect(name);
        if let Some(value) = cache.get(&key) {
            return Ok(value.clone());
        }
    }

    let value = load().await?;
    cache_insert(cache, key, value.clone(), name);
    Ok(value)
}

pub(crate) async fn finalize_turn(turn: &dyn TurnHandle, result: Result<()>) -> Result<EventId> {
    match result {
        Ok(()) => turn.finish().await,
        Err(error) => match turn.finish().await {
            Ok(_) => Err(error),
            Err(finish_error) => {
                Err(error.context(format!("also failed to finish turn: {finish_error}")))
            }
        },
    }
}

pub(crate) fn try_send_stream_event(
    event_tx: &mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
    event: ExecutionStreamEvent,
) {
    if event_tx.send(Ok(event)).is_err() {}
}

pub(crate) const AGENT_CONFIG_CACHE_NAME: &str = "agent config cache poisoned";
pub(crate) const CONVERSATION_CONFIG_CACHE_NAME: &str = "conversation config cache poisoned";
pub(crate) const HISTORY_CACHE_NAME: &str = "history cache poisoned";

pub(crate) fn cache_agent_config(
    cache: &RwLock<HashMap<AgentId, AgentConfig>>,
    agent_id: AgentId,
    config: AgentConfig,
) {
    cache_insert(cache, agent_id, config, AGENT_CONFIG_CACHE_NAME);
}

pub(crate) fn cache_conversation_config(
    cache: &RwLock<HashMap<ConversationId, crate::ConversationConfig>>,
    conversation_id: ConversationId,
    config: crate::ConversationConfig,
) {
    cache_insert(
        cache,
        conversation_id,
        config,
        CONVERSATION_CONFIG_CACHE_NAME,
    );
}
