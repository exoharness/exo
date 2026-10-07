use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use exoharness::{AgentHandle, ConversationHandle, TurnHandle};
use futures::FutureExt;
use tokio::sync::{Notify, mpsc, watch};

use crate::execution_tracing::TurnExecutionTrace;
use crate::harness::{
    Harness, HarnessCommand, HarnessEvent, HarnessEventSink, HarnessTurnKey, HarnessTurnOutcome,
};
use crate::harness_events::HarnessTurn;
use crate::harness_executor::{ExecutorStreamMode, HarnessExecutor};
use crate::runtime_host::RuntimeHost;
use crate::{AgentConfig, ConversationConfig, ExecutionStreamEvent};

pub(crate) struct ExecutorTurn {
    pub agent: Arc<dyn AgentHandle>,
    pub thread: Arc<dyn ConversationHandle>,
    pub turn: Arc<dyn TurnHandle>,
    pub agent_config: AgentConfig,
    pub thread_config: ConversationConfig,
    pub request: crate::SendRequest,
    pub recovering: bool,
    pub stream: Option<mpsc::UnboundedSender<Result<ExecutionStreamEvent>>>,
    pub(crate) trace: Option<Arc<dyn TurnExecutionTrace>>,
}

impl ExecutorTurn {
    fn key(&self) -> HarnessTurnKey {
        HarnessTurnKey {
            agent_id: self.agent.record().id,
            thread_id: self.thread.record().id,
            turn_id: self.turn.record().id,
        }
    }
}

#[derive(Default)]
struct ActiveTurns {
    stopping: bool,
    turns: HashMap<HarnessTurnKey, watch::Sender<Option<StopReason>>>,
}

#[derive(Clone, Copy)]
enum StopReason {
    Cancel,
    Shutdown,
    Suspend,
}

pub(crate) struct ExecutorHarness {
    host: Arc<dyn RuntimeHost>,
    executor: Arc<dyn HarnessExecutor>,
    events: OnceLock<HarnessEventSink>,
    active: Arc<Mutex<ActiveTurns>>,
    idle: Arc<Notify>,
}

impl ExecutorHarness {
    pub(crate) async fn wait_for_turn(&self, key: HarnessTurnKey) {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if !self
                .active
                .lock()
                .expect("active harness turns poisoned")
                .turns
                .contains_key(&key)
            {
                return;
            }
            idle.await;
        }
    }

    pub(crate) fn is_active(&self, key: HarnessTurnKey) -> bool {
        self.active
            .lock()
            .expect("active harness turns poisoned")
            .turns
            .get(&key)
            .is_some_and(|control| matches!(*control.borrow(), None | Some(StopReason::Suspend)))
    }

    pub(crate) fn new(executor: Arc<dyn HarnessExecutor>, host: Arc<dyn RuntimeHost>) -> Self {
        Self {
            host,
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
            for cancel in active.turns.values() {
                if !matches!(*cancel.borrow(), Some(StopReason::Cancel)) {
                    cancel.send_replace(Some(StopReason::Shutdown));
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
            HarnessCommand::CancelTurn { key }
            | HarnessCommand::SuspendTurn { key }
            | HarnessCommand::ResumeTurn { key } => {
                let reason = match command {
                    HarnessCommand::CancelTurn { .. } => Some(StopReason::Cancel),
                    HarnessCommand::SuspendTurn { .. } => Some(StopReason::Suspend),
                    _ => None,
                };
                let active = self.active.lock().expect("active harness turns poisoned");
                if let Some(cancel) = active.turns.get(&key)
                    && !matches!(
                        *cancel.borrow(),
                        Some(StopReason::Cancel | StopReason::Shutdown)
                    )
                {
                    cancel.send_replace(reason);
                }
                return Ok(());
            }
            HarnessCommand::StartTurn(work) => work,
        };
        let key = work.key();
        let (cancel, mut cancelled) = watch::channel(None);
        {
            let mut active = self.active.lock().expect("active harness turns poisoned");
            if active.stopping {
                bail!("harness is shutting down");
            }
            if active.turns.contains_key(&key) {
                bail!("harness turn is already active: {key:?}");
            }
            active.turns.insert(key, cancel);
        }
        let active = Arc::clone(&self.active);
        let idle = Arc::clone(&self.idle);
        let executor = self.executor.clone();
        self.host.spawn(Box::pin(async move {
            let turn = Arc::new(HarnessTurn::new(
                Arc::clone(&work.turn),
                events.clone(),
                key,
            ));
            let resumable = executor.can_suspend_turn(&work.agent_config);
            let reconciles_tools = executor.can_reconcile_unresolved_tool_call(&work.agent_config);
            let execution = async {
                if work.recovering && !reconciles_tools {
                    restore_recovery_tools(executor.as_ref(), &work, turn.as_ref()).await?;
                }
                let stream = work
                    .stream
                    .as_ref()
                    .map(ExecutorStreamMode::Enabled)
                    .unwrap_or(ExecutorStreamMode::Disabled);
                if work.recovering {
                    executor
                        .resume_turn(
                            work.agent.as_ref(),
                            Arc::clone(&work.thread),
                            turn.clone(),
                            &work.agent_config,
                            &work.thread_config,
                            &work.request,
                            stream,
                            work.trace.as_deref(),
                        )
                        .await
                } else {
                    executor
                        .execute_turn(
                            work.agent.as_ref(),
                            Arc::clone(&work.thread),
                            turn.clone(),
                            &work.agent_config,
                            &work.thread_config,
                            &work.request,
                            stream,
                            work.trace.as_deref(),
                        )
                        .await
                }
            };
            let outcome = {
                let mut execution =
                    Box::pin(std::panic::AssertUnwindSafe(execution).catch_unwind());
                let mut suspending = false;
                loop {
                    tokio::select! {
                        biased;
                        reason = cancelled.changed() => {
                            let reason = if reason.is_err() {
                                Some(StopReason::Cancel)
                            } else {
                                *cancelled.borrow_and_update()
                            };
                            match reason {
                                Some(StopReason::Shutdown) => break HarnessTurnOutcome::Interrupted,
                                Some(StopReason::Cancel) => break HarnessTurnOutcome::Cancelled,
                                Some(StopReason::Suspend) if !resumable => {},
                                Some(StopReason::Suspend) if reconciles_tools => {
                                    break HarnessTurnOutcome::Suspended;
                                }
                                Some(StopReason::Suspend) => suspending = true,
                                None => suspending = false,
                            }
                        },
                        boundary = turn.suspension_boundary(), if suspending => {
                            drop(execution);
                            drop(boundary);
                            break HarnessTurnOutcome::Suspended;
                        },
                        result = &mut execution => break match result {
                            Ok(Ok(())) => HarnessTurnOutcome::Completed(None),
                            Ok(Err(error)) => HarnessTurnOutcome::Failed(error),
                            Err(_) => HarnessTurnOutcome::Failed(
                                anyhow!("harness turn task panicked")
                            ),
                        },
                    }
                }
            };
            let outcome = if matches!(
                outcome,
                HarnessTurnOutcome::Cancelled
                    | HarnessTurnOutcome::Interrupted
                    | HarnessTurnOutcome::Suspended
            ) {
                match std::panic::AssertUnwindSafe(
                    executor.cancel_turn(work.thread.as_ref(), &work.agent_config),
                )
                .catch_unwind()
                .await
                {
                    Ok(Ok(())) => outcome,
                    Ok(Err(error))
                        if matches!(outcome,
                            HarnessTurnOutcome::Interrupted | HarnessTurnOutcome::Suspended
                        ) => {
                        tracing::error!(?key, %error, "failed to clean up interrupted harness turn");
                        outcome
                    }
                    Ok(Err(error)) => {
                        HarnessTurnOutcome::Failed(error.context("failed to cancel harness turn"))
                    }
                    Err(_)
                        if matches!(outcome,
                            HarnessTurnOutcome::Interrupted | HarnessTurnOutcome::Suspended
                        ) => {
                        tracing::error!(?key, "harness cleanup panicked during shutdown");
                        outcome
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
        }));
        Ok(())
    }
}

async fn restore_recovery_tools(
    executor: &dyn HarnessExecutor,
    work: &ExecutorTurn,
    turn: &HarnessTurn,
) -> Result<()> {
    use exoharness::{EventData, EventKind, EventQuery, EventQueryDirection};
    let events = work
        .thread
        .get_events(Some(EventQuery {
            turn_id: Some(work.turn.record().id),
            types: Some(vec![
                EventKind::TOOL_REQUESTED,
                EventKind::TOOL_RESULT,
                EventKind::custom(crate::basic::BASIC_TOOL_ROUND),
                EventKind::custom(crate::permissions::APPROVAL_REQUESTED),
                EventKind::custom(crate::permissions::APPROVAL_RESPONSE),
            ]),
            direction: Some(EventQueryDirection::Asc),
            ..Default::default()
        }))
        .await?
        .events;
    let mut tool_round = None;
    let mut pending = Vec::new();
    let mut approvals = Vec::new();
    let mut responses = HashSet::new();
    for event in events {
        match event.data {
            EventData::Custom {
                event_type,
                payload,
            } if event_type == crate::basic::BASIC_TOOL_ROUND => {
                tool_round =
                    Some(serde_json::from_value::<crate::basic::BasicToolRound>(payload)?.round);
            }
            EventData::Custom {
                event_type,
                payload,
            } if event_type == crate::permissions::APPROVAL_REQUESTED => {
                approvals
                    .push(serde_json::from_value::<crate::permissions::ApprovalRequest>(payload)?);
            }
            EventData::Custom {
                event_type,
                payload,
            } if event_type == crate::permissions::APPROVAL_RESPONSE => {
                responses.insert(
                    serde_json::from_value::<crate::permissions::ApprovalResponse>(payload)?
                        .approval_id,
                );
            }
            EventData::ToolRequested {
                tool_call_id,
                request,
                ..
            } => pending.push((tool_round, tool_call_id, request)),
            EventData::ToolResult { tool_call_id, .. } => {
                pending.retain(|(_, id, _)| *id != tool_call_id)
            }
            _ => {}
        }
    }
    if let Some((round, tool_call_id, request)) = pending.first() {
        let pending_approval = executor.can_resume_pending_approval(&work.agent_config)
            && approvals.iter().any(|approval| {
                approval.tool_call_id.as_deref() == Some(tool_call_id.as_str())
                    && approval.round == *round
                    && round.is_some()
                    && approval.request == *request
                    && !responses.contains(&approval.approval_id)
            });
        anyhow::ensure!(
            pending_approval,
            "cannot safely resume unresolved tool call `{tool_call_id}` (`{}`) for turn {}",
            request.function_name,
            work.turn.record().id
        );
    }
    turn.restore_pending_tools(pending.into_iter().map(|(_, id, _)| id).collect())
        .await;
    Ok(())
}
