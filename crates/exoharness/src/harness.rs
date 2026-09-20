use std::sync::Arc;

use crate::{Error, Event, EventData, Result, ThreadId, TurnId};
use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HarnessTurnKey {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
}

impl HarnessTurnKey {
    pub fn new(thread_id: ThreadId, turn_id: TurnId) -> Self {
        Self { thread_id, turn_id }
    }
}

pub enum HarnessCommand<T> {
    StartTurn(T),
    CancelTurn { key: HarnessTurnKey },
}

#[derive(Debug)]
pub enum HarnessTurnOutcome {
    Completed(Option<String>),
    Failed(Error),
    Cancelled,
}

pub enum HarnessEvent {
    TurnEvents {
        key: HarnessTurnKey,
        events: Vec<EventData>,
    },
    TurnFinished {
        key: HarnessTurnKey,
        outcome: HarnessTurnOutcome,
        events: Vec<EventData>,
    },
    ExecutionStopped {
        key: HarnessTurnKey,
    },
}

#[derive(Debug, Default)]
pub struct HarnessEventAck {
    pub events: Vec<Event>,
}

#[async_trait]
pub trait HarnessEventHandler: Send + Sync {
    async fn emit(&self, event: HarnessEvent) -> Result<HarnessEventAck>;
}

#[derive(Clone)]
pub struct HarnessEventSink {
    handler: Arc<dyn HarnessEventHandler>,
}

impl HarnessEventSink {
    pub fn new(handler: Arc<dyn HarnessEventHandler>) -> Self {
        Self { handler }
    }

    pub async fn emit(&self, event: HarnessEvent) -> Result<HarnessEventAck> {
        self.handler.emit(event).await
    }
}

#[async_trait]
pub trait Harness<T: Send>: Send + Sync {
    fn name(&self) -> &'static str;

    async fn init(&self, _events: HarnessEventSink) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn submit(&self, command: HarnessCommand<T>) -> Result<()>
    where
        T: 'async_trait;
}
