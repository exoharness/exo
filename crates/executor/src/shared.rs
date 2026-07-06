use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use anyhow::Error;
use exoharness::{
    AgentHandle, AgentId, ConversationHandle, ConversationId, ConversationLease, EventId, Result,
    TurnCoordinator, TurnHandle, TurnId,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::execution_tracing::{ExecutionTracer, TurnExecutionTrace};
use crate::{AgentConfig, ExecutionStreamEvent, ExecutionStreamHandle, SendResult};

pub(crate) type TurnFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// Coordinator bookkeeping for a queued turn executed by a spawned stream
/// task: stop renewing, pop the head, and free the lease once the turn
/// finishes.
pub(crate) struct TurnCompletion {
    pub(crate) coordinator: Arc<dyn TurnCoordinator>,
    pub(crate) lease: ConversationLease,
    pub(crate) turn_id: TurnId,
    pub(crate) renewal: tokio::task::JoinHandle<()>,
}

impl TurnCompletion {
    pub(crate) async fn finish(self) {
        self.renewal.abort();
        if let Err(error) = self
            .coordinator
            .complete_turn(&self.lease, self.turn_id)
            .await
        {
            tracing::error!(%error, turn_id = %self.turn_id, "failed to complete queued turn");
        }
        if let Err(error) = self.coordinator.release_idle(&self.lease).await {
            tracing::error!(
                %error,
                conversation_id = %self.lease.conversation_id,
                "failed to release conversation lease"
            );
        }
    }
}

/// Keeps the conversation lease alive while a turn executes. Aborted by
/// `TurnCompletion::finish()` (or the caller) once the turn is done.
pub(crate) fn spawn_lease_renewal(
    coordinator: Arc<dyn TurnCoordinator>,
    lease: ConversationLease,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = coordinator.lease_ttl() / 3;
        loop {
            tokio::time::sleep(interval).await;
            match coordinator.renew(&lease, true).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::error!(
                        conversation_id = %lease.conversation_id,
                        "lost conversation lease while executing turn"
                    );
                    return;
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to renew conversation lease");
                }
            }
        }
    })
}

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

pub(crate) async fn execute_prepared_turn<Run>(
    tracer: &dyn ExecutionTracer,
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    turn: &dyn TurnHandle,
    agent_config: &AgentConfig,
    run: Run,
) -> Result<SendResult>
where
    Run: for<'a> FnOnce(Option<&'a dyn TurnExecutionTrace>) -> TurnFuture<'a>,
{
    let session_id = turn.record().session_id;
    let turn_id = turn.record().id;
    let turn_trace = tracer
        .start_turn(
            agent_config.braintrust.as_ref(),
            agent.record(),
            conversation.record(),
            agent_config,
            session_id,
            turn_id,
            false,
        )
        .await;
    let latest_event_id = finalize_turn(turn, run(turn_trace.as_deref()).await).await;

    finish_turn_trace(turn_trace, &latest_event_id).await;

    Ok(SendResult {
        session_id,
        turn_id,
        latest_event_id: latest_event_id?,
    })
}

pub(crate) fn spawn_prepared_turn_stream<Run>(
    tracer: Arc<dyn ExecutionTracer>,
    agent: Arc<dyn AgentHandle>,
    conversation: Arc<dyn ConversationHandle>,
    turn: Arc<dyn TurnHandle>,
    agent_config: AgentConfig,
    completion: Option<TurnCompletion>,
    run: Run,
) -> ExecutionStreamHandle
where
    Run: for<'a> FnOnce(
            Option<&'a dyn TurnExecutionTrace>,
            &'a mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
        ) -> TurnFuture<'a>
        + Send
        + 'static,
{
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        let session_id = turn.record().session_id;
        let turn_id = turn.record().id;
        let turn_trace = tracer
            .start_turn(
                agent_config.braintrust.as_ref(),
                agent.record(),
                conversation.record(),
                &agent_config,
                session_id,
                turn_id,
                true,
            )
            .await;
        let send_result = finalize_turn(turn.as_ref(), run(turn_trace.as_deref(), &event_tx).await)
            .await
            .map(|latest_event_id| SendResult {
                session_id,
                turn_id,
                latest_event_id,
            });

        if let Some(turn_trace) = turn_trace {
            match &send_result {
                Ok(result) => {
                    turn_trace
                        .finish_success(Some(result.latest_event_id))
                        .await
                }
                Err(error) => turn_trace.finish_error(error).await,
            }
        }

        if let Some(completion) = completion {
            completion.finish().await;
        }

        if let Err(error) = &send_result {
            try_send_stream_error(&event_tx, error);
        } else if let Ok(result) = &send_result {
            try_send_stream_event(&event_tx, ExecutionStreamEvent::Completed(result.clone()));
        }
    });

    ExecutionStreamHandle::new(UnboundedReceiverStream::new(event_rx))
}

async fn finish_turn_trace(
    turn_trace: Option<Box<dyn TurnExecutionTrace>>,
    latest_event_id: &Result<EventId>,
) {
    if let Some(turn_trace) = turn_trace {
        match latest_event_id {
            Ok(event_id) => turn_trace.finish_success(Some(*event_id)).await,
            Err(error) => turn_trace.finish_error(error).await,
        }
    }
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

pub(crate) fn try_send_stream_error(
    event_tx: &mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
    error: &Error,
) {
    if event_tx.send(Err(Error::msg(error.to_string()))).is_err() {}
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
