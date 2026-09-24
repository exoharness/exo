use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use exoharness::{AgentHandle, ConversationHandle, TurnHandle};
use futures::FutureExt;
use tokio::sync::{Notify, mpsc, oneshot};

use crate::execution_tracing::TurnExecutionTrace;
use crate::harness::{
    Harness, HarnessCommand, HarnessEvent, HarnessEventSink, HarnessTurnKey, HarnessTurnOutcome,
};
use crate::harness_executor::{ExecutorStreamMode, HarnessExecutor};
use crate::{AgentConfig, ConversationConfig, ExecutionStreamEvent};

pub(crate) struct ExecutorTurn {
    pub agent: Arc<dyn AgentHandle>,
    pub thread: Arc<dyn ConversationHandle>,
    pub turn: Arc<dyn TurnHandle>,
    pub agent_config: AgentConfig,
    pub thread_config: ConversationConfig,
    pub request: crate::SendRequest,
    pub stream: Option<mpsc::UnboundedSender<Result<ExecutionStreamEvent>>>,
    pub(crate) trace: Option<Arc<dyn TurnExecutionTrace>>,
}

impl ExecutorTurn {
    fn key(&self) -> HarnessTurnKey {
        HarnessTurnKey {
            thread_id: self.thread.record().id,
            turn_id: self.turn.record().id,
        }
    }
}

#[derive(Default)]
struct ActiveTurns {
    stopping: bool,
    turns: HashMap<HarnessTurnKey, Option<oneshot::Sender<()>>>,
}

pub(crate) struct ExecutorHarness {
    executor: Arc<dyn HarnessExecutor>,
    events: OnceLock<HarnessEventSink>,
    active: Arc<Mutex<ActiveTurns>>,
    idle: Arc<Notify>,
}

impl ExecutorHarness {
    pub(crate) fn new(executor: Arc<dyn HarnessExecutor>) -> Self {
        Self {
            executor,
            events: OnceLock::new(),
            active: Arc::default(),
            idle: Arc::default(),
        }
    }
}

#[async_trait]
impl Harness<ExecutorTurn> for ExecutorHarness {
    fn name(&self) -> &'static str {
        self.executor.name()
    }

    async fn init(&self, events: HarnessEventSink) -> Result<()> {
        self.events
            .set(events)
            .map_err(|_| anyhow!("harness is already initialized"))
    }

    async fn shutdown(&self) -> Result<()> {
        {
            let mut active = self.active.lock().expect("active harness turns poisoned");
            active.stopping = true;
            for cancel in active.turns.values_mut() {
                if let Some(cancel) = cancel.take()
                    && cancel.send(()).is_err()
                {
                    tracing::debug!("harness turn already stopped during shutdown");
                }
            }
        }
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self
                .active
                .lock()
                .expect("active harness turns poisoned")
                .turns
                .is_empty()
            {
                break;
            }
            idle.await;
        }
        self.executor.shutdown().await
    }

    async fn submit(&self, command: HarnessCommand<ExecutorTurn>) -> Result<()> {
        let events = self
            .events
            .get()
            .ok_or_else(|| anyhow!("harness is not initialized"))?
            .clone();
        let work = match command {
            HarnessCommand::CancelTurn { key } => {
                let mut active = self.active.lock().expect("active harness turns poisoned");
                if let Some(cancel) = active.turns.get_mut(&key).and_then(Option::take)
                    && cancel.send(()).is_err()
                {
                    tracing::debug!(?key, "harness turn already stopped before cancellation");
                }
                return Ok(());
            }
            HarnessCommand::StartTurn(work) => work,
        };
        let key = work.key();
        let (cancel, cancelled) = oneshot::channel();
        {
            let mut active = self.active.lock().expect("active harness turns poisoned");
            if active.stopping {
                bail!("harness is shutting down");
            }
            if active.turns.contains_key(&key) {
                bail!("harness turn is already active: {key:?}");
            }
            active.turns.insert(key, Some(cancel));
        }
        let active = Arc::clone(&self.active);
        let idle = Arc::clone(&self.idle);
        let executor = self.executor.clone();
        tokio::spawn(async move {
            let execution = executor.execute_turn(
                work.agent.as_ref(),
                Arc::clone(&work.thread),
                Arc::clone(&work.turn),
                &work.agent_config,
                &work.thread_config,
                &work.request,
                work.stream
                    .as_ref()
                    .map(ExecutorStreamMode::Enabled)
                    .unwrap_or(ExecutorStreamMode::Disabled),
                work.trace.as_deref(),
            );
            let outcome = tokio::select! {
                result = std::panic::AssertUnwindSafe(execution).catch_unwind() => match result {
                    Ok(Ok(())) => HarnessTurnOutcome::Completed(None),
                    Ok(Err(error)) => HarnessTurnOutcome::Failed(error),
                    Err(_) => HarnessTurnOutcome::Failed(anyhow!("harness turn task panicked")),
                },
                _ = cancelled => HarnessTurnOutcome::Cancelled,
            };
            let outcome = if matches!(outcome, HarnessTurnOutcome::Cancelled) {
                match std::panic::AssertUnwindSafe(
                    executor.cancel_turn(work.thread.as_ref(), &work.agent_config),
                )
                .catch_unwind()
                .await
                {
                    Ok(Ok(())) => outcome,
                    Ok(Err(error)) => {
                        HarnessTurnOutcome::Failed(error.context("failed to cancel harness turn"))
                    }
                    Err(_) => HarnessTurnOutcome::Failed(anyhow!("harness cancellation panicked")),
                }
            } else {
                outcome
            };
            if let Err(error) = events
                .emit(HarnessEvent::TurnFinished {
                    key,
                    outcome,
                    events: Vec::new(),
                })
                .await
            {
                tracing::error!(?key, %error, "failed to emit harness completion");
            }
            drop(work);
            if let Err(error) = events.emit(HarnessEvent::ExecutionStopped { key }).await {
                tracing::error!(?key, %error, "failed to release harness execution");
            }
            active
                .lock()
                .expect("active harness turns poisoned")
                .turns
                .remove(&key);
            idle.notify_waiters();
        });
        Ok(())
    }
}
