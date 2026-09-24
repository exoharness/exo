use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use exoharness::{ConversationHandle, EventData, TurnHandle};
use tokio::sync::{Mutex as AsyncMutex, oneshot};

use crate::harness::{
    HarnessEvent, HarnessEventAck, HarnessEventHandler, HarnessTurnKey, HarnessTurnOutcome,
};

struct ActiveEventTurn {
    thread: Arc<dyn ConversationHandle>,
    turn: Arc<dyn TurnHandle>,
    outcome: Option<HarnessTurnOutcome>,
    completion: Option<oneshot::Sender<HarnessTurnOutcome>>,
}

#[derive(Default)]
pub(crate) struct HarnessEvents {
    turns: Mutex<HashMap<HarnessTurnKey, Arc<AsyncMutex<ActiveEventTurn>>>>,
}

impl HarnessEvents {
    pub(crate) fn register(
        &self,
        thread: Arc<dyn ConversationHandle>,
        turn: Arc<dyn TurnHandle>,
    ) -> Result<oneshot::Receiver<HarnessTurnOutcome>> {
        let key = HarnessTurnKey {
            thread_id: thread.record().id,
            turn_id: turn.record().id,
        };
        let (completion, receiver) = oneshot::channel();
        let mut turns = self.turns.lock().expect("harness event routes poisoned");
        if turns.contains_key(&key) {
            bail!("harness turn is already registered: {key:?}");
        }
        turns.insert(
            key,
            Arc::new(AsyncMutex::new(ActiveEventTurn {
                thread,
                turn,
                outcome: None,
                completion: Some(completion),
            })),
        );
        Ok(receiver)
    }

    pub(crate) fn remove(&self, key: HarnessTurnKey) {
        self.turns
            .lock()
            .expect("harness event routes poisoned")
            .remove(&key);
    }
}

impl ActiveEventTurn {
    async fn append(&self, events: Vec<EventData>) -> Result<HarnessEventAck> {
        if events.is_empty() {
            return Ok(HarnessEventAck::default());
        }
        let result = self.turn.add_events(events).await?;
        let events =
            futures::future::try_join_all(result.event_ids.into_iter().map(|id| async move {
                self.thread
                    .get_event(id)
                    .await?
                    .ok_or_else(|| anyhow!("appended harness event is missing: {id}"))
            }))
            .await?;
        Ok(HarnessEventAck { events })
    }
}

#[async_trait]
impl HarnessEventHandler for HarnessEvents {
    async fn emit(&self, event: HarnessEvent) -> Result<HarnessEventAck> {
        let key = match &event {
            HarnessEvent::TurnEvents { key, .. }
            | HarnessEvent::TurnFinished { key, .. }
            | HarnessEvent::ExecutionStopped { key } => *key,
        };
        let active = self
            .turns
            .lock()
            .expect("harness event routes poisoned")
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow!("harness event references an inactive turn: {key:?}"))?;
        let mut active = active.lock().await;
        if active.completion.is_none() {
            bail!("harness event references a stopped turn: {key:?}");
        }
        match event {
            HarnessEvent::TurnEvents { events, .. } => {
                if active.outcome.is_some() {
                    bail!("harness emitted events after completion");
                }
                active.append(events).await
            }
            HarnessEvent::TurnFinished {
                outcome, events, ..
            } => {
                if active.outcome.is_some() {
                    bail!("harness completed the same turn twice");
                }
                match active.append(events).await {
                    Ok(ack) => {
                        active.outcome = Some(outcome);
                        Ok(ack)
                    }
                    Err(error) => {
                        active.outcome = Some(HarnessTurnOutcome::Failed(anyhow!("{error:#}")));
                        Err(error)
                    }
                }
            }
            HarnessEvent::ExecutionStopped { .. } => {
                let outcome = active.outcome.take().unwrap_or_else(|| {
                    HarnessTurnOutcome::Failed(anyhow!("harness stopped without a terminal event"))
                });
                if let Some(completion) = active.completion.take()
                    && completion.send(outcome).is_err()
                {
                    tracing::debug!(?key, "harness completion receiver was dropped");
                }
                self.remove(key);
                Ok(HarnessEventAck::default())
            }
        }
    }
}
