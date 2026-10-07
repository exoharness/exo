use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use exoharness::{
    AddEventsResult, ArtifactVersion, ConversationHandle, Event, EventData, EventId, SandboxId,
    SnapshotHandle, SnapshotId, StartSandboxRequest, TurnHandle, TurnRecord, WriteArtifactRequest,
};
use futures::StreamExt;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

use crate::ExecutionStreamEvent;
use crate::harness::{
    HarnessEvent, HarnessEventAck, HarnessEventHandler, HarnessEventSink, HarnessTurnKey,
    HarnessTurnOutcome,
};

pub(crate) struct HarnessTurn {
    inner: Arc<dyn TurnHandle>,
    events: HarnessEventSink,
    key: HarnessTurnKey,
}

impl HarnessTurn {
    pub(crate) fn new(
        inner: Arc<dyn TurnHandle>,
        events: HarnessEventSink,
        key: HarnessTurnKey,
    ) -> Self {
        Self { inner, events, key }
    }
}

#[async_trait]
impl SnapshotHandle for HarnessTurn {
    async fn snapshot_sandbox(
        &self,
        id: SandboxId,
        kind: exoharness::SnapshotKind,
    ) -> Result<SnapshotId> {
        self.inner.snapshot_sandbox(id, kind).await
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        self.inner.start_sandbox(request).await
    }
}

#[async_trait]
impl TurnHandle for HarnessTurn {
    fn record(&self) -> &TurnRecord {
        self.inner.record()
    }

    async fn add_events(&self, data: Vec<EventData>) -> Result<AddEventsResult> {
        if data.is_empty() {
            return self.inner.add_events(data).await;
        }
        let acknowledgement = self
            .events
            .emit(HarnessEvent::TurnEvents {
                key: self.key,
                events: data,
            })
            .await?;
        Ok(AddEventsResult {
            latest_event_id: acknowledgement
                .events
                .last()
                .context("harness event append returned no events")?
                .id,
            event_ids: acknowledgement
                .events
                .into_iter()
                .map(|event| event.id)
                .collect(),
        })
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        self.inner.write_artifact(request).await
    }

    async fn finish(&self) -> Result<EventId> {
        self.inner.finish().await
    }
}

struct ActiveEventTurn {
    thread: Arc<dyn ConversationHandle>,
    turn: Arc<dyn TurnHandle>,
    outcome: Option<HarnessTurnOutcome>,
    completion: Option<oneshot::Sender<HarnessTurnOutcome>>,
    stream: Option<mpsc::UnboundedSender<Result<ExecutionStreamEvent>>>,
}

#[derive(Default)]
pub(crate) struct HarnessEvents {
    turns: Mutex<HashMap<HarnessTurnKey, Arc<AsyncMutex<ActiveEventTurn>>>>,
}

impl HarnessEvents {
    pub(crate) fn contains(&self, key: HarnessTurnKey) -> bool {
        self.turns
            .lock()
            .expect("harness event routes poisoned")
            .contains_key(&key)
    }

    pub(crate) fn register(
        &self,
        thread: Arc<dyn ConversationHandle>,
        turn: Arc<dyn TurnHandle>,
        stream: Option<mpsc::UnboundedSender<Result<ExecutionStreamEvent>>>,
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
                stream,
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
        let result = self.turn.add_events(events.clone()).await?;
        ensure!(
            result.event_ids.len() == events.len(),
            "harness event append returned {} ids for {} events",
            result.event_ids.len(),
            events.len()
        );
        // The append returns IDs in input order. Echo the submitted payloads
        // instead of fetching every event back from storage for the ACK.
        let events = result
            .event_ids
            .into_iter()
            .zip(events)
            .map(|(id, data)| {
                Ok(Event {
                    id,
                    thread_id: self.thread.record().id,
                    session_id: Some(self.turn.record().session_id),
                    turn_id: Some(self.turn.record().id),
                    created_at: id
                        .timestamp()
                        .context("appended event id has no timestamp")?,
                    data,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if let Some(stream) = &self.stream {
            for event in &events {
                if let Some(output) = execution_stream_event(event.data.clone()) {
                    crate::shared::try_send_stream_event(stream, output);
                }
            }
        }
        Ok(HarnessEventAck { events })
    }
}

fn execution_stream_event(data: EventData) -> Option<ExecutionStreamEvent> {
    match data {
        EventData::LinguaStreamChunk { chunk } => Some(ExecutionStreamEvent::Chunk(chunk)),
        EventData::ToolRequested {
            tool_call_id,
            request,
            ..
        } => Some(ExecutionStreamEvent::ToolCall {
            tool_call_id,
            tool_name: request.function_name,
            arguments: request.arguments,
        }),
        EventData::ToolResult {
            tool_call_id,
            result,
        } => Some(ExecutionStreamEvent::ToolResult {
            tool_call_id,
            result,
        }),
        _ => None,
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

pub(crate) fn turn_stream(
    events: exoharness::EventStream,
    turn: exoharness::TurnRecord,
) -> impl futures::Stream<Item = Result<crate::ExecutionStreamEvent>> + Send {
    futures::stream::try_unfold(Some((events, None)), move |state| {
        let turn = turn.clone();
        async move {
            let Some((mut events, mut failure)) = state else {
                return Ok(None);
            };
            loop {
                let event = events.next().await.context(
                    "runtime progress disconnected; reconnect to retrieve saved history",
                )??;
                if event.turn_id != Some(turn.id) {
                    continue;
                }
                use crate::ExecutionStreamEvent as Output;
                let output = match event.data {
                    EventData::Custom {
                        event_type,
                        payload,
                    } if event_type == crate::permissions::APPROVAL_REQUESTED => {
                        Some(Output::ApprovalRequested {
                            turn: turn.clone(),
                            approval: serde_json::from_value(payload)?,
                        })
                    }
                    EventData::Error { message, .. } => {
                        failure = Some(message);
                        None
                    }
                    EventData::TurnEnded => {
                        if let Some(message) = failure {
                            bail!(message);
                        }
                        return Ok(Some((
                            Output::Completed(crate::SendResult {
                                session_id: turn.session_id,
                                turn_id: turn.id,
                                latest_event_id: event.id,
                            }),
                            None,
                        )));
                    }
                    data => execution_stream_event(data),
                };
                if let Some(output) = output {
                    return Ok(Some((output, Some((events, failure)))));
                }
            }
        }
    })
}
