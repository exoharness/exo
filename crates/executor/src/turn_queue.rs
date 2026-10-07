use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, anyhow, ensure};
use exoharness::turn_coordinator::{
    QueuedTurn, TurnAuthority, TurnControl, TurnControlOutcome, TurnLease, TurnOptions, TurnQueue,
    TurnSubmission, TurnThread,
};
use exoharness::{
    AgentHandle, EventData, EventQuery, EventQueryDirection, ThreadHandle, TurnRecord, Uuid7,
};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::harness::{Harness, HarnessCommand, HarnessTurnKey};
use crate::runtime_host::{RuntimeHost, TaskGroup};
use crate::{
    ExecutionStreamEvent, ExecutionStreamHandle, LocalProvider, Runtime, SendRequest, TurnWork,
};

#[cfg(all(test, feature = "native"))]
mod tests;

#[derive(Debug)]
pub(crate) struct TurnSuspended;
impl std::fmt::Display for TurnSuspended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("turn suspended")
    }
}
impl std::error::Error for TurnSuspended {}

struct TurnContext {
    runtime: Runtime,
    observers: Vec<mpsc::UnboundedSender<Result<ExecutionStreamEvent>>>,
}

type ActiveTurn = (
    Arc<crate::harness_adapter::ExecutorHarness>,
    exoharness::TurnId,
);

pub(crate) struct TurnQueueRuntime {
    pub(crate) admission: tokio::sync::RwLock<()>,
    pub(crate) coordinator: Arc<dyn TurnQueue<TurnWork>>,
    contexts: Mutex<HashMap<HarnessTurnKey, TurnContext>>,
    pub(crate) active: Mutex<HashMap<TurnThread, ActiveTurn>>,
    tasks: Mutex<TaskGroup>,
    host: Arc<dyn RuntimeHost>,
    pub(crate) draining: AtomicBool,
}

impl TurnQueueRuntime {
    fn register_observer(
        &self,
        key: HarnessTurnKey,
        runtime: &Runtime,
        observer: mpsc::UnboundedSender<Result<ExecutionStreamEvent>>,
    ) {
        self.contexts
            .lock()
            .expect("turn contexts poisoned")
            .entry(key)
            .or_insert_with(|| TurnContext {
                runtime: runtime.clone(),
                observers: Vec::new(),
            })
            .observers
            .push(observer);
    }

    fn active_harness(
        &self,
        key: HarnessTurnKey,
    ) -> Option<Arc<crate::harness_adapter::ExecutorHarness>> {
        self.active
            .lock()
            .expect("active queue owners poisoned")
            .get(&TurnThread {
                agent_id: key.agent_id,
                thread_id: key.thread_id,
            })
            .filter(|(_, id)| *id == key.turn_id)
            .map(|(harness, _)| harness.clone())
    }

    async fn signal(&self, key: HarnessTurnKey, control: TurnControl) -> Result<()> {
        let command = match control {
            TurnControl::Run => HarnessCommand::ResumeTurn { key },
            TurnControl::Cancel => HarnessCommand::CancelTurn { key },
            TurnControl::Suspend => HarnessCommand::SuspendTurn { key },
        };
        if let Some(harness) = self.active_harness(key) {
            harness.submit(command).await?;
        }
        Ok(())
    }

    pub(crate) fn new(
        host: Arc<dyn RuntimeHost>,
        coordinator: Arc<dyn TurnQueue<TurnWork>>,
    ) -> Self {
        Self {
            admission: Default::default(),
            coordinator,
            contexts: Mutex::default(),
            active: Mutex::default(),
            tasks: Mutex::new(TaskGroup::new(host.clone())),
            host,
            draining: AtomicBool::new(false),
        }
    }

    pub(crate) fn broadcast(&self, key: HarnessTurnKey, event: Result<ExecutionStreamEvent>) {
        let mut contexts = self.contexts.lock().expect("turn contexts poisoned");
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
        if matches!(&event, Ok(ExecutionStreamEvent::Completed(_)) | Err(_)) {
            contexts.remove(&key);
        }
    }

    fn stop_observers(&self, key: HarnessTurnKey, error: &anyhow::Error) {
        if let Some(context) = self
            .contexts
            .lock()
            .expect("turn contexts poisoned")
            .get_mut(&key)
        {
            for observer in context.observers.drain(..) {
                if observer
                    .send(Err(anyhow!(
                        "turn execution stopped; work retained for recovery: {error:#}"
                    )))
                    .is_err()
                {
                    tracing::debug!(?key, "stopped turn observer disconnected");
                }
            }
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

    pub(crate) async fn shutdown(
        &self,
        root: Arc<crate::harness_adapter::ExecutorHarness>,
    ) -> Result<()> {
        let mut errors = Vec::new();
        let mut active: Vec<_> = self
            .active
            .lock()
            .expect("active queue owners poisoned")
            .values()
            .map(|(provider, _)| provider.clone())
            .collect();
        active.push(root);
        active.sort_unstable_by_key(Arc::as_ptr);
        active.dedup_by(|left, right| Arc::ptr_eq(left, right));
        for harness in active {
            if let Err(error) = harness.shutdown().await {
                errors.push(error);
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
        runtime.drain_queue(self, thread).await
    }

    /// Select queue persistence explicitly. Caller-scoped providers share this
    /// queue; sandbox ownership remains with the state implementation.
    pub fn with_turn_coordinator(mut self, coordinator: Arc<dyn TurnQueue<TurnWork>>) -> Self {
        let turns = TurnQueueRuntime::new(self.host.clone(), coordinator);
        self.turns = Arc::new(turns);
        self
    }

    pub(crate) async fn cancel_queued_turn(
        &self,
        runtime: &Runtime,
        key: HarnessTurnKey,
    ) -> Result<bool> {
        let _admission = self.turns.admission.read().await;
        ensure!(
            !self.turns.draining.load(Ordering::SeqCst),
            "runtime is shutting down"
        );
        let thread = TurnThread {
            agent_id: key.agent_id,
            thread_id: key.thread_id,
        };
        let outcome = self
            .turns
            .coordinator
            .cancel(thread, key.turn_id, self.turn_authority())
            .await?;
        self.finish_queued_control(runtime, key, outcome, TurnControl::Cancel)
            .await
    }

    pub(crate) async fn set_queued_suspended(
        &self,
        runtime: &Runtime,
        key: HarnessTurnKey,
        suspended: bool,
    ) -> Result<bool> {
        let _admission = self.turns.admission.read().await;
        ensure!(
            !self.turns.draining.load(Ordering::SeqCst),
            "runtime is shutting down"
        );
        let thread = TurnThread {
            agent_id: key.agent_id,
            thread_id: key.thread_id,
        };
        if suspended {
            let Some(entry) = self.turns.coordinator.get(thread, key.turn_id).await? else {
                return Ok(false);
            };
            if !self.executor.can_suspend_turn(&entry.work.agent_config) {
                return Ok(false);
            }
        }
        let outcome = self
            .turns
            .coordinator
            .set_suspended(thread, key.turn_id, self.turn_authority(), suspended)
            .await?;
        let control = if suspended {
            TurnControl::Suspend
        } else {
            TurnControl::Run
        };
        self.finish_queued_control(runtime, key, outcome, control)
            .await
    }

    fn turn_authority(&self) -> TurnAuthority {
        match self.state.caller() {
            Some(caller) => TurnAuthority::Submitter(caller.principal.clone()),
            None => TurnAuthority::ThreadOwner,
        }
    }

    async fn finish_queued_control(
        &self,
        runtime: &Runtime,
        key: HarnessTurnKey,
        outcome: TurnControlOutcome,
        control: TurnControl,
    ) -> Result<bool> {
        match outcome {
            TurnControlOutcome::Queued | TurnControlOutcome::Running => {
                self.turns.signal(key, control).await?;
                let scope = TurnThread {
                    agent_id: key.agent_id,
                    thread_id: key.thread_id,
                };
                if let Err(error) = runtime.spawn_queue(self, scope).await {
                    self.turns.stop_observers(key, &error);
                    tracing::error!(?scope, %error, "controlled turn retained; failed to wake queue");
                }
                Ok(true)
            }
            TurnControlOutcome::NotAccessible => {
                anyhow::bail!("only the submitter or thread owner may control this turn")
            }
            TurnControlOutcome::NotFound => Ok(false),
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
        let turn = TurnRecord {
            id: Uuid7::now(),
            session_id: work.request.session_id.unwrap_or_else(Uuid7::now),
        };
        let key = HarnessTurnKey::new(scope.agent_id, scope.thread_id, turn.id);
        let (sender, receiver) = mpsc::unbounded_channel();
        // Publish the execution identity and observer before the queue entry can
        // become visible to a live drain.
        provider.turns.register_observer(key, self, sender.clone());
        let accepted = match provider
            .turns
            .coordinator
            .enqueue(
                scope,
                TurnSubmission {
                    turn,
                    work,
                    principal,
                    options,
                },
            )
            .await
        {
            Ok(accepted) => accepted,
            Err(error) => {
                provider
                    .turns
                    .contexts
                    .lock()
                    .expect("turn contexts poisoned")
                    .remove(&key);
                return Err(error);
            }
        };
        let accepted_key = HarnessTurnKey::new(scope.agent_id, scope.thread_id, accepted.turn.id);
        if accepted.duplicate {
            provider
                .turns
                .contexts
                .lock()
                .expect("turn contexts poisoned")
                .remove(&key);
            provider
                .turns
                .register_observer(accepted_key, self, sender.clone());
            match journal_status(thread.as_ref(), &accepted.turn).await {
                Ok(JournalStatus::Finished(result)) => provider.turns.replay_completion(
                    accepted_key,
                    &sender,
                    result.map(ExecutionStreamEvent::Completed),
                ),
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(
                        ?accepted_key, %error,
                        "accepted duplicate retained; failed to read completion"
                    );
                }
            }
        }
        // A durable acceptance must not be reported as a failed turn if waking
        // its worker fails. Hosts must arrange another durable wakeup.
        if let Err(error) = self.spawn_queue(provider, scope).await {
            provider.turns.stop_observers(accepted_key, &error);
            tracing::error!(?scope, %error, "accepted turn retained; failed to wake queue");
        }
        Ok((
            accepted.turn,
            ExecutionStreamHandle::new(UnboundedReceiverStream::new(receiver)),
        ))
    }

    pub(crate) async fn resume_local_turn(
        &self,
        provider: &LocalProvider,
        key: HarnessTurnKey,
    ) -> Result<ExecutionStreamHandle> {
        let (sender, receiver) = mpsc::unbounded_channel();
        provider.turns.register_observer(key, self, sender.clone());
        match provider.set_queued_suspended(self, key, false).await {
            Ok(true) => {}
            result => {
                let mut contexts = provider
                    .turns
                    .contexts
                    .lock()
                    .expect("turn contexts poisoned");
                if let Some(context) = contexts.get_mut(&key) {
                    context
                        .observers
                        .retain(|observer| !observer.same_channel(&sender));
                    if context.observers.is_empty() {
                        contexts.remove(&key);
                    }
                }
                result?;
                anyhow::bail!("turn is no longer queued");
            }
        }
        Ok(ExecutionStreamHandle::new(UnboundedReceiverStream::new(
            receiver,
        )))
    }

    pub(crate) async fn drain_queue(
        &self,
        provider: &LocalProvider,
        scope: TurnThread,
    ) -> Result<()> {
        let _admission = provider.turns.admission.read().await;
        ensure!(
            !provider.turns.draining.load(Ordering::SeqCst),
            "runtime is shutting down"
        );
        self.spawn_queue(provider, scope).await
    }

    async fn spawn_queue(&self, provider: &LocalProvider, scope: TurnThread) -> Result<()> {
        let Some(lease) = provider.turns.coordinator.claim(scope).await? else {
            return Ok(());
        };
        let initial_head = provider.turns.coordinator.peek(&lease).await?;
        let runtime = self.clone();
        let provider = provider.clone();
        let turns = provider.turns.clone();
        let mut group = turns.tasks.lock().expect("turn tasks poisoned");
        while let Some(result) = group.try_join_next() {
            if let Err(error) = result {
                tracing::error!(%error, "turn queue task failed");
            }
        }
        let queue = turns.clone();
        group.spawn(async move {
            let mut head = initial_head
                .as_ref()
                .map(|head| HarnessTurnKey::new(scope.agent_id, scope.thread_id, head.turn.id));
            let result = runtime
                .run_queue(&provider, &lease, initial_head, &mut head)
                .await;
            if result.is_err()
                && let Some(key) = head
            {
                if let Err(stop_error) = queue.signal(key, TurnControl::Suspend).await {
                    tracing::error!(?key, %stop_error, "failed to stop queue execution");
                }
                if let Some(harness) = queue.active_harness(key) {
                    harness.wait_for_turn(key).await;
                }
            }
            if result.is_err() {
                queue
                    .active
                    .lock()
                    .expect("active queue owners poisoned")
                    .remove(&scope);
            }
            drop(lease);
            if let Err(error) = result {
                if let Some(key) = head {
                    queue.stop_observers(key, &error);
                }
                // Storage or ownership failures leave accepted work pending.
                // Observers of later entries must not see a false terminal error.
                tracing::error!(?scope, %error, "turn queue drain stopped; retained for recovery");
            }
        });
        Ok(())
    }

    async fn run_queue(
        &self,
        provider: &LocalProvider,
        lease: &TurnLease,
        initial_head: Option<QueuedTurn<TurnWork>>,
        current_head: &mut Option<HarnessTurnKey>,
    ) -> Result<()> {
        let turns = &provider.turns;
        let scope = lease.thread;
        let mut initial_head = Some(initial_head);
        while !turns.draining.load(Ordering::SeqCst) {
            let head = match initial_head.take() {
                Some(head) => head,
                None => turns.coordinator.peek(lease).await?,
            };
            *current_head = head
                .as_ref()
                .map(|head| HarnessTurnKey::new(scope.agent_id, scope.thread_id, head.turn.id));
            if head
                .as_ref()
                .is_none_or(|head| head.control == TurnControl::Suspend)
            {
                if let Some(head) = &head {
                    turns.broadcast(
                        HarnessTurnKey::new(scope.agent_id, scope.thread_id, head.turn.id),
                        Ok(ExecutionStreamEvent::Suspended(head.turn.clone())),
                    );
                }
                if turns.coordinator.release_if_idle(lease).await? {
                    return Ok(());
                }
                continue;
            }
            let head = head.expect("runnable head");
            let was_started = head.started;
            let started = turns.coordinator.start(lease, head.turn.id).await?;
            let mut head = started.head;
            let mut control = started.control;
            let key = HarnessTurnKey::new(scope.agent_id, scope.thread_id, head.turn.id);
            let root = self.root_runtime();
            let agent = root
                .exoharness_handle()
                .get_agent(&scope.agent_id)
                .await?
                .context("queue agent no longer exists")?;
            let thread = agent
                .get_thread(&scope.thread_id)
                .await?
                .context("queue thread no longer exists")?;
            head.control = control.next().await.context("turn control watch ended")??;
            if head.control == TurnControl::Suspend {
                continue;
            }
            let stream = self
                .open_queue_head(provider, thread.clone(), key, &head, was_started)
                .await;
            let mut suspended = false;
            let error = match stream {
                Err(error) => Some(error),
                Ok(None) => None,
                Ok(Some(mut stream)) => {
                    let mut error = None;
                    loop {
                        tokio::select! {
                            biased;
                            update = control.next() => {
                                let update = update.context("turn control watch ended")
                                    .and_then(|update| update);
                                match update {
                                    Ok(control) => {
                                        suspended |= control == TurnControl::Suspend;
                                        turns.signal(key, control).await?;
                                    }
                                    Err(error) => return Err(error),
                                }
                            },
                            event = stream.next() => match event {
                                Some(Ok(event)) => turns.broadcast(key, Ok(event)),
                                Some(Err(failure)) => {
                                    suspended |= failure.is::<TurnSuspended>();
                                    error = Some(failure);
                                },
                                None => break,
                            },
                        }
                    }
                    error
                }
            };
            if let Some(harness) = turns.active_harness(key) {
                harness.wait_for_turn(key).await;
            }
            turns
                .active
                .lock()
                .expect("active queue owners poisoned")
                .remove(&scope);
            match journal_status(thread.as_ref(), &head.turn).await? {
                JournalStatus::Finished(result) => {
                    turns.broadcast(key, result.map(ExecutionStreamEvent::Completed))
                }
                _ if turns.draining.load(Ordering::SeqCst) => return Ok(()),
                status => {
                    let current = turns
                        .coordinator
                        .peek(lease)
                        .await?
                        .context("queue head disappeared")?;
                    if current.control == TurnControl::Suspend {
                        continue;
                    }
                    if current.control == TurnControl::Run && suspended {
                        continue;
                    }
                    let error = error
                        .unwrap_or_else(|| anyhow!("turn execution stopped without completion"));
                    persist_head_failure(thread.as_ref(), &head, status, &error).await?;
                    turns.broadcast(key, Err(error));
                }
            }
            initial_head = Some(turns.coordinator.acknowledge(lease, head.turn.id).await?);
        }
        Ok(())
    }

    async fn open_queue_head(
        &self,
        provider: &LocalProvider,
        root_thread: Arc<dyn ThreadHandle>,
        key: HarnessTurnKey,
        head: &QueuedTurn<TurnWork>,
        was_started: bool,
    ) -> Result<Option<ExecutionStreamHandle>> {
        let turns = &provider.turns;
        let events = if was_started {
            root_thread
                .get_events(Some(crate::harness_executor::recovery_query(key.turn_id)))
                .await?
                .events
        } else {
            Vec::new()
        };
        let status = journal_status_from_events(&events, &head.turn);
        match status {
            JournalStatus::Finished(_) => return Ok(None),
            _ if head.control == TurnControl::Cancel => {
                anyhow::bail!("turn cancelled")
            }
            _ => {}
        }
        if events.iter().any(|event| {
            matches!(&event.data, EventData::Error { .. })
                || matches!(&event.data, EventData::Custom { event_type, .. }
                    if event_type == crate::harness_executor::RUNTIME_TURN_COMPLETED)
        }) {
            root_thread
                .turn_handle(head.turn.clone())
                .await?
                .finish()
                .await?;
            return Ok(None);
        }
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
            Some(runtime)
                if runtime
                    .exoharness_handle()
                    .caller()
                    .map(|caller| &caller.principal)
                    == head.principal.as_ref() =>
            {
                runtime
            }
            _ => match &head.principal {
                Some(principal) => self
                    .recovery_resolver
                    .get()
                    .context("caller-scoped queue recovery is unavailable")?(
                    principal.clone()
                )?
                .as_ref()
                .clone(),
                None => self.root_runtime(),
            },
        };
        ensure!(
            execution
                .exoharness_handle()
                .caller()
                .map(|caller| &caller.principal)
                == head.principal.as_ref(),
            "queue recovery runtime has the wrong caller"
        );
        let agent = execution
            .exoharness_handle()
            .get_agent(&key.agent_id)
            .await?
            .context("queue caller cannot access agent")?;
        let thread = agent
            .get_thread(&key.thread_id)
            .await?
            .context("queue caller cannot access thread")?;
        let stream = match status {
            JournalStatus::Running => {
                let work = TurnWork::from_events(&events)?;
                execution
                    .execute_turn(agent, thread, head.turn.clone(), work, true)
                    .await
                    .map(Some)?
            }
            JournalStatus::Unstarted => Some(
                execution
                    .execute_turn(agent, thread, head.turn.clone(), head.work.clone(), false)
                    .await?,
            ),
            JournalStatus::Finished(_) => unreachable!(),
        };
        drop(recovery_slot.take());
        Ok(stream)
    }
}

async fn persist_head_failure(
    thread: &dyn ThreadHandle,
    head: &QueuedTurn<TurnWork>,
    status: JournalStatus,
    error: &anyhow::Error,
) -> Result<()> {
    let turn = match status {
        JournalStatus::Finished(_) => return Ok(()),
        JournalStatus::Running => thread.turn_handle(head.turn.clone()).await?,
        JournalStatus::Unstarted => {
            thread
                .begin_turn(exoharness::BeginTurnRequest {
                    turn: head.turn.clone(),
                    new_session: head.work.request.session_id.is_none(),
                    input: head.work.request.input.clone(),
                    initial_events: vec![head.work.event()?],
                })
                .await?
        }
    };
    turn.add_events(vec![EventData::Error {
        message: format!("{error:#}"),
        metadata: None,
    }])
    .await?;
    turn.finish().await?;
    Ok(())
}

enum JournalStatus {
    Unstarted,
    Running,
    Finished(Result<crate::SendResult>),
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
    Ok(journal_status_from_events(&events, turn))
}

fn journal_status_from_events(events: &[exoharness::Event], turn: &TurnRecord) -> JournalStatus {
    let Some(ended) = events
        .iter()
        .find(|event| matches!(event.data, EventData::TurnEnded))
    else {
        return if events
            .iter()
            .any(|event| matches!(event.data, EventData::TurnStarted { .. }))
        {
            JournalStatus::Running
        } else {
            JournalStatus::Unstarted
        };
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
    JournalStatus::Finished(result)
}
