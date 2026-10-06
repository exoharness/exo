use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, anyhow, ensure};
use exoharness::turn_coordinator::{
    CancelAuthority, CancelTurnOutcome, QueuedTurn, StoredTurnCoordinator, TurnAttention,
    TurnCoordinator, TurnQueueDiscovery, TurnThread,
};
use exoharness::{
    AgentHandle, EventData, EventQuery, EventQueryDirection, ThreadHandle, TurnRecord, Uuid7,
};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::harness::{Harness, HarnessCommand, HarnessTurnKey};
use crate::harness_executor::RecoveryRuntimeResolver;
use crate::runtime_host::{RuntimeHost, TaskGroup};
use crate::{
    ExecutionStreamEvent, ExecutionStreamHandle, LocalProvider, Runtime, SendRequest, TurnWork,
};

#[cfg(all(test, feature = "native"))]
mod tests;

#[derive(Debug, Clone, Default)]
pub struct TurnOptions {
    pub idempotency_key: Option<String>,
    pub attention: TurnAttention,
}

struct TurnContext {
    thread: TurnThread,
    runtime: Runtime,
    observers: Vec<mpsc::UnboundedSender<Result<ExecutionStreamEvent>>>,
}

type ActiveTurn = (
    Arc<crate::harness_adapter::ExecutorHarness>,
    exoharness::TurnId,
);

pub(crate) struct TurnQueueRuntime {
    pub(crate) admission: tokio::sync::RwLock<()>,
    resolver: Mutex<Option<RecoveryRuntimeResolver>>,
    pub coordinator: Arc<dyn TurnCoordinator<TurnWork>>,
    pub discovery: Option<Arc<dyn TurnQueueDiscovery>>,
    contexts: Mutex<HashMap<HarnessTurnKey, TurnContext>>,
    drains: Mutex<HashMap<TurnThread, Uuid7>>,
    pub(crate) active: Mutex<HashMap<TurnThread, ActiveTurn>>,
    tasks: Mutex<TaskGroup>,
    host: Arc<dyn RuntimeHost>,
    pub draining: AtomicBool,
}

impl TurnQueueRuntime {
    pub fn new(host: Arc<dyn RuntimeHost>) -> Self {
        let coordinator = Arc::new(StoredTurnCoordinator::in_memory());
        Self {
            admission: Default::default(),
            resolver: Mutex::default(),
            coordinator: coordinator.clone(),
            discovery: Some(coordinator),
            contexts: Mutex::default(),
            drains: Mutex::default(),
            active: Mutex::default(),
            tasks: Mutex::new(TaskGroup::new(host.clone())),
            host,
            draining: AtomicBool::new(false),
        }
    }

    pub(crate) fn broadcast(&self, key: HarnessTurnKey, event: Result<ExecutionStreamEvent>) {
        let mut contexts = self.contexts.lock().expect("turn contexts poisoned");
        if matches!(&event, Ok(ExecutionStreamEvent::Completed(_)) | Err(_)) {
            if let Some(context) = contexts.remove(&key) {
                for observer in context.observers {
                    if observer
                        .send(match &event {
                            Ok(event) => Ok(event.clone()),
                            Err(error) => Err(anyhow!(error.to_string())),
                        })
                        .is_err()
                    {
                        tracing::debug!(?key, "turn observer disconnected");
                    }
                }
            }
            return;
        }
        if let Some(context) = contexts.get_mut(&key) {
            context.observers.retain(|observer| {
                observer
                    .send(match &event {
                        Ok(event) => Ok(event.clone()),
                        Err(error) => Err(anyhow!(error.to_string())),
                    })
                    .is_ok()
            });
        }
    }

    fn replay_completion(
        &self,
        key: HarnessTurnKey,
        observer: &mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
        result: Result<ExecutionStreamEvent>,
    ) {
        let mut contexts = self.contexts.lock().expect("turn contexts poisoned");
        if let Some(context) = contexts.get_mut(&key) {
            // A concurrent completion may already have delivered and removed it.
            if let Some(index) = context
                .observers
                .iter()
                .position(|sender| sender.same_channel(observer))
            {
                let sender = context.observers.remove(index);
                if sender.send(result).is_err() {
                    tracing::debug!(?key, "duplicate turn observer disconnected");
                }
            }
            if context.observers.is_empty() {
                contexts.remove(&key);
            }
        }
    }

    pub async fn shutdown(&self, root: Arc<crate::harness_adapter::ExecutorHarness>) -> Result<()> {
        let mut errors = Vec::new();
        let active: Vec<_> = self
            .active
            .lock()
            .expect("active queue owners poisoned")
            .values()
            .map(|(provider, _)| provider.clone())
            .collect();
        let mut active = active;
        if !active.iter().any(|harness| Arc::ptr_eq(harness, &root)) {
            active.push(root);
        }
        let mut seen: Vec<Arc<crate::harness_adapter::ExecutorHarness>> = Vec::new();
        for harness in active {
            if !seen.iter().any(|previous| Arc::ptr_eq(previous, &harness)) {
                if let Err(error) = harness.shutdown().await {
                    errors.push(error);
                }
                seen.push(harness);
            }
        }
        let mut tasks = std::mem::replace(
            &mut *self.tasks.lock().expect("turn tasks poisoned"),
            TaskGroup::new(self.host.clone()),
        );
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                errors.push(error);
            }
        }
        let contexts = std::mem::take(&mut *self.contexts.lock().expect("turn contexts poisoned"));
        for (key, context) in contexts {
            for observer in context.observers {
                if observer
                    .send(Err(anyhow!("turn {} interrupted by shutdown", key.turn_id)))
                    .is_err()
                {
                    tracing::debug!(?key, "turn observer disconnected during shutdown");
                }
            }
        }
        ensure!(
            errors.is_empty(),
            "turn queue shutdown failed: {}",
            errors
                .iter()
                .map(|error| format!("{error:#}"))
                .collect::<Vec<_>>()
                .join("; ")
        );
        Ok(())
    }
}

impl LocalProvider {
    /// Drain a known thread after a host wakeup, such as a Durable Object alarm.
    /// Global queue discovery is unnecessary when the host already knows it.
    pub async fn wake_turn_queue(&self, runtime: &Runtime, thread: TurnThread) -> Result<()> {
        runtime.drain_queue(self, thread, None).await
    }

    /// Select queue persistence explicitly. Caller-scoped providers share this
    /// queue; sandbox ownership remains with the state implementation.
    pub fn with_turn_coordinator(
        mut self,
        coordinator: Arc<dyn TurnCoordinator<TurnWork>>,
        discovery: Option<Arc<dyn TurnQueueDiscovery>>,
    ) -> Self {
        let mut turns = TurnQueueRuntime::new(self.host.clone());
        turns.coordinator = coordinator;
        turns.discovery = discovery;
        self.turns = Arc::new(turns);
        self
    }

    pub(crate) async fn cancel_queued_turn(&self, key: HarnessTurnKey) -> Result<bool> {
        let thread = {
            let contexts = self.turns.contexts.lock().expect("turn contexts poisoned");
            contexts.get(&key).map(|context| TurnThread {
                agent_id: context.thread.agent_id,
                thread_id: key.thread_id,
            })
        };
        let thread = thread.or_else(|| {
            self.turns
                .drains
                .lock()
                .expect("turn drains poisoned")
                .keys()
                .find(|scope| scope.thread_id == key.thread_id)
                .copied()
        });
        let thread = match thread {
            Some(thread) => Some(thread),
            None => match &self.turns.discovery {
                Some(discovery) => discovery
                    .pending_threads()
                    .await?
                    .into_iter()
                    .find(|thread| thread.thread_id == key.thread_id),
                None => None,
            },
        };
        let Some(thread) = thread else {
            return Ok(false);
        };
        let authority = match self.state.caller() {
            Some(caller) => CancelAuthority::Submitter(caller.principal.clone()),
            None => CancelAuthority::ThreadOwner,
        };
        match self
            .turns
            .coordinator
            .cancel(thread, key.turn_id, authority)
            .await?
        {
            CancelTurnOutcome::Running => {
                let provider = self
                    .turns
                    .active
                    .lock()
                    .expect("active queue owners poisoned")
                    .get(&thread)
                    .map(|(provider, _)| provider.clone());
                if let Some(provider) = provider
                    && provider.is_active(key)
                {
                    provider.submit(HarnessCommand::CancelTurn { key }).await?;
                }
                Ok(true)
            }
            CancelTurnOutcome::Queued => Ok(true),
            CancelTurnOutcome::NotAccessible => {
                anyhow::bail!("only the submitter or thread owner may cancel this turn")
            }
            CancelTurnOutcome::NotFound => Ok(false),
        }
    }
}

impl Runtime {
    pub(crate) async fn accept_local_turn(
        &self,
        provider: &LocalProvider,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ThreadHandle>,
        request: SendRequest,
        streaming: bool,
        config_override: Option<crate::AgentConfig>,
        options: TurnOptions,
    ) -> Result<(TurnRecord, ExecutionStreamHandle)> {
        ensure!(
            !provider.turns.draining.load(Ordering::SeqCst),
            "runtime is shutting down"
        );
        self.wait_for_recovery(thread.record().id).await;
        let _admission = provider.turns.admission.read().await;
        ensure!(
            !provider.turns.draining.load(Ordering::SeqCst),
            "runtime is shutting down"
        );
        let scope = TurnThread {
            agent_id: agent.record().id,
            thread_id: thread.record().id,
        };
        if let Some(caller) = thread.caller() {
            caller
                .check(exoharness::ResourceScope::Thread {
                    agent_id: scope.agent_id,
                    thread_id: scope.thread_id,
                })
                .await?;
        }
        let override_supplied = config_override.is_some();
        let mut agent_config = match config_override {
            Some(config) => config,
            None => self.get_agent_config(agent.as_ref()).await?,
        };
        if !override_supplied
            && let Some(model) = crate::get_conversation_model_override(thread.as_ref()).await?
        {
            agent_config.model = model.model;
            agent_config.max_output_tokens = model.max_output_tokens;
        }
        let work = TurnWork {
            streaming,
            agent_config,
            thread_config: self.get_conversation_config(thread.as_ref()).await?,
            request,
        };
        let principal = thread.caller().map(|caller| caller.principal.clone());
        let accepted = provider
            .turns
            .coordinator
            .enqueue(
                scope,
                QueuedTurn {
                    turn: TurnRecord {
                        id: Uuid7::now(),
                        session_id: work.request.session_id.unwrap_or_else(Uuid7::now),
                    },
                    work,
                    principal,
                    idempotency_key: options.idempotency_key,
                    attention: options.attention,
                    started: false,
                    cancelled: false,
                },
            )
            .await?;
        let key = HarnessTurnKey::new(scope.thread_id, accepted.turn.id);
        let (sender, receiver) = mpsc::unbounded_channel();
        {
            let mut contexts = provider
                .turns
                .contexts
                .lock()
                .expect("turn contexts poisoned");
            contexts
                .entry(key)
                .or_insert_with(|| TurnContext {
                    thread: scope,
                    runtime: self.clone(),
                    observers: Vec::new(),
                })
                .observers
                .push(sender.clone());
        }
        // Attach before checking completion so a racing acknowledgment cannot
        // leave a duplicate request observing a turn that has already finished.
        if accepted.duplicate
            && let JournalStatus::Finished(result) =
                journal_status(thread.as_ref(), &accepted.turn).await?
        {
            provider.turns.replay_completion(
                key,
                &sender,
                result.map(ExecutionStreamEvent::Completed),
            );
        }
        drop(sender);
        if let Some(turn_id) = accepted.interrupted {
            provider
                .cancel_queued_turn(HarnessTurnKey::new(scope.thread_id, turn_id))
                .await?;
        }
        self.spawn_queue(provider, scope, None).await?;
        Ok((
            accepted.turn,
            ExecutionStreamHandle::new(UnboundedReceiverStream::new(receiver)),
        ))
    }

    pub(crate) async fn drain_queue(
        &self,
        provider: &LocalProvider,
        scope: TurnThread,
        resolver: Option<RecoveryRuntimeResolver>,
    ) -> Result<()> {
        let _admission = provider.turns.admission.read().await;
        ensure!(
            !provider.turns.draining.load(Ordering::SeqCst),
            "runtime is shutting down"
        );
        self.spawn_queue(provider, scope, resolver).await
    }

    async fn spawn_queue(
        &self,
        provider: &LocalProvider,
        scope: TurnThread,
        resolver: Option<RecoveryRuntimeResolver>,
    ) -> Result<()> {
        let resolver = {
            let mut saved = provider
                .turns
                .resolver
                .lock()
                .expect("queue resolver poisoned");
            if resolver.is_some() {
                *saved = resolver.clone();
            }
            resolver.or_else(|| saved.clone())
        };
        let Some(lease) = provider.turns.coordinator.claim(scope).await? else {
            return Ok(());
        };
        provider
            .turns
            .drains
            .lock()
            .expect("turn drains poisoned")
            .insert(scope, lease.token());
        let runtime = self.clone();
        let provider = provider.clone();
        let turns = provider.turns.clone();
        let tasks = turns.clone();
        let mut group = tasks.tasks.lock().expect("turn tasks poisoned");
        while let Some(result) = group.try_join_next() {
            if let Err(error) = result {
                tracing::error!(%error, "turn queue task failed");
            }
        }
        group.spawn(async move {
            let result = tokio::select! {
                result = runtime.run_queue(&provider, &lease, resolver) => result,
                () = lease.revoked() => {
                    let active = turns.active.lock().expect("active queue owners poisoned").get(&scope).cloned();
                    if let Some((active, turn_id)) = active {
                        if active.is_active(HarnessTurnKey::new(scope.thread_id, turn_id))
                            && let Err(error) = active.submit(HarnessCommand::CancelTurn { key: HarnessTurnKey::new(scope.thread_id, turn_id) }).await {
                                tracing::error!(%error, "failed to stop turn after losing ownership");
                        }
                        active.wait_for_turn(HarnessTurnKey::new(scope.thread_id, turn_id)).await;
                    }
                    Err(anyhow!("turn ownership lost"))
                }
            };
            {
                let mut drains = turns.drains.lock().expect("turn drains poisoned");
                if drains.get(&scope) == Some(&lease.token()) {
                    drains.remove(&scope);
                    turns.active.lock().expect("active queue owners poisoned").remove(&scope);
                }
            }
            if let Err(error) = result {
                tracing::error!(?scope, %error, "turn queue drain failed; retained for recovery");
                let keys: Vec<_> = turns.contexts.lock().expect("turn contexts poisoned").keys().filter(|key| key.thread_id == scope.thread_id).copied().collect();
                for key in keys { turns.broadcast(key, Err(anyhow!(error.to_string()))); }
            }
        });
        Ok(())
    }

    async fn run_queue(
        &self,
        provider: &LocalProvider,
        lease: &exoharness::turn_coordinator::TurnLease,
        resolver: Option<RecoveryRuntimeResolver>,
    ) -> Result<()> {
        let turns = &provider.turns;
        let scope = lease.thread;
        while !turns.draining.load(Ordering::SeqCst) {
            let Some(head) = turns.coordinator.peek(lease).await? else {
                if turns.coordinator.release_if_idle(lease).await? {
                    return Ok(());
                }
                continue;
            };
            let key = HarnessTurnKey::new(scope.thread_id, head.turn.id);
            let context = turns
                .contexts
                .lock()
                .expect("turn contexts poisoned")
                .get(&key)
                .map(|context| context.runtime.clone());
            let mut recovery_slot = if context.is_none() {
                Some(self.claim_recovery_slot().await?)
            } else {
                None
            };
            let execution = match context {
                Some(context) => context,
                None => match &head.principal {
                    Some(principal) => {
                        let execution = resolver
                            .as_ref()
                            .context("caller-scoped queue recovery is unavailable")?(
                            principal.clone(),
                        )?;
                        ensure!(
                            execution
                                .exoharness_handle()
                                .caller()
                                .is_some_and(|caller| caller.principal == *principal),
                            "queue recovery runtime has the wrong caller"
                        );
                        execution.as_ref().clone()
                    }
                    None => self.clone(),
                },
            };
            let agent = execution
                .exoharness_handle()
                .get_agent(&scope.agent_id)
                .await?
                .context("queue agent no longer exists")?;
            let thread = agent
                .get_thread(&scope.thread_id)
                .await?
                .context("queue thread no longer exists")?;
            let status = if head.started {
                journal_status(thread.as_ref(), &head.turn).await?
            } else {
                JournalStatus::Unstarted
            };
            match status {
                JournalStatus::Finished(result) => {
                    turns.broadcast(key, result.map(ExecutionStreamEvent::Completed))
                }
                JournalStatus::Running if head.cancelled => {
                    let turn = thread.turn_handle(head.turn.clone()).await?;
                    let error = anyhow!("turn cancelled before recovery");
                    persist_failure(turn.as_ref(), &error).await?;
                    turns.broadcast(key, Err(error));
                }
                JournalStatus::Running => {
                    let recovery = execution
                        .recover_local_thread(
                            provider,
                            agent,
                            thread.clone(),
                            resolver.clone(),
                            head.turn.id,
                            recovery_slot.take(),
                        )
                        .await;
                    match journal_status(thread.as_ref(), &head.turn).await? {
                        JournalStatus::Finished(result) => {
                            turns.broadcast(key, result.map(ExecutionStreamEvent::Completed))
                        }
                        _ => {
                            recovery?;
                            return Ok(()); // Graceful shutdown preserves the unfinished head.
                        }
                    }
                }
                JournalStatus::Unstarted => {
                    turns.coordinator.start(lease, head.turn.id).await?;
                    if turns.coordinator.cancelled(lease, head.turn.id).await? {
                        let turn = thread
                            .begin_turn(exoharness::BeginTurnRequest {
                                turn: Some(head.turn.clone()),
                                session_id: head.work.request.session_id,
                                input: head.work.request.input.clone(),
                                initial_events: vec![head.work.event()?],
                            })
                            .await?;
                        let error = anyhow!("turn cancelled before execution");
                        persist_failure(turn.as_ref(), &error).await?;
                        turns.broadcast(key, Err(error));
                    } else {
                        let execution_result = execution
                            .execute_accepted_turn(
                                agent,
                                thread.clone(),
                                head.turn.clone(),
                                head.work.clone(),
                            )
                            .await;
                        let mut stream = match execution_result {
                            Ok(stream) => stream,
                            Err(error) => {
                                if turns.draining.load(Ordering::SeqCst) {
                                    return Ok(());
                                }
                                let turn = match journal_status(thread.as_ref(), &head.turn).await?
                                {
                                    JournalStatus::Unstarted => {
                                        thread
                                            .begin_turn(exoharness::BeginTurnRequest {
                                                turn: Some(head.turn.clone()),
                                                session_id: head.work.request.session_id,
                                                input: head.work.request.input.clone(),
                                                initial_events: vec![head.work.event()?],
                                            })
                                            .await?
                                    }
                                    _ => thread.turn_handle(head.turn.clone()).await?,
                                };
                                persist_failure(turn.as_ref(), &error).await?;
                                turns.broadcast(key, Err(error));
                                turns.coordinator.acknowledge(lease, head.turn.id).await?;
                                turns
                                    .contexts
                                    .lock()
                                    .expect("turn contexts poisoned")
                                    .remove(&key);
                                continue;
                            }
                        };
                        if turns.coordinator.cancelled(lease, head.turn.id).await? {
                            let active = turns
                                .active
                                .lock()
                                .expect("active queue owners poisoned")
                                .get(&scope)
                                .cloned();
                            if let Some((harness, _)) = active
                                && harness.is_active(key)
                            {
                                harness.submit(HarnessCommand::CancelTurn { key }).await?;
                            }
                        }
                        drop(recovery_slot.take());
                        let mut finished = false;
                        while let Some(event) = stream.next().await {
                            match &event {
                                Ok(ExecutionStreamEvent::Completed(_)) => finished = true,
                                Err(_) => {
                                    finished = matches!(
                                        journal_status(thread.as_ref(), &head.turn).await?,
                                        JournalStatus::Finished(_)
                                    )
                                }
                                _ => {}
                            }
                            turns.broadcast(key, event);
                        }
                        if !finished {
                            return Ok(());
                        }
                    }
                }
            }
            turns
                .active
                .lock()
                .expect("active queue owners poisoned")
                .remove(&scope);
            turns.coordinator.acknowledge(lease, head.turn.id).await?;
            turns
                .contexts
                .lock()
                .expect("turn contexts poisoned")
                .remove(&key);
        }
        Ok(())
    }
}

enum JournalStatus {
    Unstarted,
    Running,
    Finished(Result<crate::SendResult>),
}

async fn persist_failure(turn: &dyn exoharness::TurnHandle, error: &anyhow::Error) -> Result<()> {
    turn.add_events(vec![EventData::Error {
        message: format!("{error:#}"),
        metadata: None,
    }])
    .await?;
    turn.finish().await?;
    Ok(())
}

async fn journal_status(thread: &dyn ThreadHandle, turn: &TurnRecord) -> Result<JournalStatus> {
    let events = thread
        .get_events(Some(EventQuery {
            turn_id: Some(turn.id),
            direction: Some(EventQueryDirection::Asc),
            types: Some(vec![
                exoharness::EventKind::TURN_STARTED,
                exoharness::EventKind::TURN_ENDED,
                exoharness::EventKind::ERROR,
            ]),
            ..Default::default()
        }))
        .await?
        .events;
    let Some(ended) = events
        .iter()
        .find(|event| matches!(event.data, EventData::TurnEnded))
    else {
        return Ok(
            if events
                .iter()
                .any(|event| matches!(event.data, EventData::TurnStarted { .. }))
            {
                JournalStatus::Running
            } else {
                JournalStatus::Unstarted
            },
        );
    };
    let result = match events.iter().find_map(|event| match &event.data {
        EventData::Error { message, .. } => Some(message),
        _ => None,
    }) {
        Some(message) => Err(anyhow!(message.clone())),
        None => Ok(crate::SendResult {
            turn_id: turn.id,
            session_id: turn.session_id,
            latest_event_id: ended.id,
        }),
    };
    Ok(JournalStatus::Finished(result))
}
