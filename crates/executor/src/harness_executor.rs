use std::collections::HashSet;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, AgentRecord, BeginTurnRequest, ConversationHandle, EventData, EventKind,
    EventQuery, EventQueryDirection, ExoHarness, NewAgentRequest, NewConversationRequest, Result,
    TurnHandle, TurnRecord,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use tokio::sync::{Notify, OnceCell, mpsc};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::conversation_lock::conversation_send_lock;
use crate::execution_tracing::{ExecutionTracer, TurnExecutionTrace};
use crate::harness::{
    Harness, HarnessCommand, HarnessEventSink, HarnessTurnKey, HarnessTurnOutcome,
};
use crate::harness_adapter::ExecutorTurn;
use crate::harness_config::{
    load_agent_config, load_conversation_config, store_agent_config, store_conversation_config,
};
use crate::harness_events::HarnessEvents;
use crate::harness_helpers::{
    get_conversation_model_override, resolve_agent_handle, resolve_conversation_handle,
};
use crate::runtime_host::TaskGroup;
use crate::shared::finalize_turn;
use crate::{
    AgentConfig, ConversationConfig, ConversationModelConfig, CreateAgentRequest,
    CreateConversationRequest, ExecutionStreamEvent, ExecutionStreamHandle, Provider, SendRequest,
    SendResult,
};

pub(crate) const RUNTIME_TURN_WORK: &str = "exo.runtime.turn_work";
pub(crate) const RUNTIME_TURN_COMPLETED: &str = "exo.runtime.turn_completed";

pub type RecoveryRuntimeResolver = Arc<dyn Fn(String) -> Result<Arc<Runtime>> + Send + Sync>;

#[derive(Default)]
struct RecoveryGate {
    state: Mutex<RecoveryGateState>,
    changed: Notify,
}

#[derive(Default)]
struct RecoveryGateState {
    running: bool,
    done: bool,
    indexed: bool,
    pending_threads: HashSet<exoharness::ThreadId>,
    new_threads: HashSet<exoharness::ThreadId>,
}

impl RecoveryGate {
    fn begin(&self) {
        let mut state = self.state.lock().expect("recovery gate poisoned");
        if !state.done {
            state.running = true;
        }
    }

    fn index(&self, pending_threads: HashSet<exoharness::ThreadId>) {
        let mut state = self.state.lock().expect("recovery gate poisoned");
        if state.running {
            state.pending_threads = pending_threads;
            state.indexed = true;
            self.changed.notify_waiters();
        }
    }

    fn thread_done(&self, thread: exoharness::ThreadId) {
        let mut state = self.state.lock().expect("recovery gate poisoned");
        if state.running {
            state.pending_threads.remove(&thread);
            self.changed.notify_waiters();
        }
    }

    fn new_thread(&self, thread: exoharness::ThreadId) {
        let mut state = self.state.lock().expect("recovery gate poisoned");
        if state.running {
            state.new_threads.insert(thread);
            self.changed.notify_waiters();
        }
    }

    fn is_new_thread(&self, thread: exoharness::ThreadId) -> bool {
        self.state
            .lock()
            .expect("recovery gate poisoned")
            .new_threads
            .contains(&thread)
    }

    fn finish(&self) {
        let mut state = self.state.lock().expect("recovery gate poisoned");
        state.running = false;
        state.done = true;
        state.pending_threads.clear();
        state.new_threads.clear();
        self.changed.notify_waiters();
    }

    async fn wait(&self, thread: exoharness::ThreadId) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let ready = {
                let state = self.state.lock().expect("recovery gate poisoned");
                !state.running
                    || state.done
                    || (state.indexed && !state.pending_threads.contains(&thread))
                    || state.new_threads.contains(&thread)
            };
            if ready {
                return;
            }
            changed.await;
        }
    }
}

#[cfg(test)]
mod recovery_gate_tests {
    use super::RecoveryGate;
    use std::collections::HashSet;
    use std::time::Duration;

    #[tokio::test]
    async fn new_threads_skip_the_scan_while_old_threads_wait_for_their_turn() {
        let gate = RecoveryGate::default();
        let old = exoharness::Uuid7::now();
        let new = exoharness::Uuid7::now();
        gate.begin();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), gate.wait(old))
                .await
                .is_err()
        );
        gate.new_thread(new);
        tokio::time::timeout(Duration::from_millis(20), gate.wait(new))
            .await
            .expect("new thread should not wait for recovery");
        gate.index(HashSet::from([old]));
        tokio::time::timeout(
            Duration::from_millis(20),
            gate.wait(exoharness::Uuid7::now()),
        )
        .await
        .expect("a thread without unfinished turns should proceed");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), gate.wait(old))
                .await
                .is_err()
        );
        gate.thread_done(old);
        tokio::time::timeout(Duration::from_millis(20), gate.wait(old))
            .await
            .expect("scanned thread should proceed");
        gate.finish();
        tokio::time::timeout(
            Duration::from_millis(20),
            gate.wait(exoharness::Uuid7::now()),
        )
        .await
        .expect("an unscanned thread should proceed after the scan ends");
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RecoverableTurn {
    pub(crate) agent_config: AgentConfig,
    pub(crate) thread_config: ConversationConfig,
    pub(crate) request: SendRequest,
}

impl RecoverableTurn {
    pub(crate) fn event(&self) -> Result<EventData> {
        Ok(EventData::Custom {
            event_type: RUNTIME_TURN_WORK.to_owned(),
            payload: serde_json::to_value(self)?,
        })
    }
}

#[derive(Clone, Copy)]
pub enum ExecutorStreamMode<'a> {
    Disabled,
    Enabled(&'a mpsc::UnboundedSender<Result<ExecutionStreamEvent>>),
}

#[async_trait]
pub trait HarnessExecutor: Send + Sync + 'static {
    fn with_state(&self, _state: Arc<dyn ExoHarness>) -> Result<Arc<dyn HarnessExecutor>> {
        anyhow::bail!("this executor does not support caller-scoped execution")
    }

    async fn reset_thread(&self, _thread: exoharness::ThreadId) -> Result<()> {
        Ok(())
    }

    fn name(&self) -> &'static str;

    fn can_resume_pending_approval(&self, _config: &AgentConfig) -> bool {
        false
    }

    fn can_reconcile_unresolved_tool_call(&self, _config: &AgentConfig) -> bool {
        false
    }

    fn agent_config(
        &self,
        _definition: &exo_managed_agents::AgentDefinition,
    ) -> Result<AgentConfig> {
        Err(anyhow!("this executor does not configure managed agents"))
    }

    async fn resolve_thread_harness(
        &self,
        _thread: &dyn ConversationHandle,
        _config: &AgentConfig,
        _harness: &str,
    ) -> Result<crate::ConversationHarnessConfig> {
        Err(anyhow!(
            "this executor does not configure managed harnesses"
        ))
    }

    async fn configure_managed_thread(
        &self,
        _agent: &dyn AgentHandle,
        _thread: &dyn ConversationHandle,
        _agent_config: &AgentConfig,
        _thread_config: &ConversationConfig,
    ) -> Result<usize> {
        Ok(0)
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn cancel_turn(
        &self,
        _thread: &dyn ConversationHandle,
        _config: &AgentConfig,
    ) -> Result<()> {
        Ok(())
    }

    async fn prepare_conversation(
        &self,
        _agent: &dyn AgentHandle,
        _conversation: &dyn ConversationHandle,
        _agent_config: &AgentConfig,
        _conversation_config: &ConversationConfig,
    ) -> Result<()> {
        Ok(())
    }

    async fn execute_turn(
        &self,
        agent: &dyn AgentHandle,
        conversation: Arc<dyn ConversationHandle>,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        request: &SendRequest,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()>;

    // A harness must opt in after it can reconstruct execution progress without
    // repeating tool or sandbox side effects from the interrupted turn.
    async fn resume_turn(
        &self,
        _agent: &dyn AgentHandle,
        _conversation: Arc<dyn ConversationHandle>,
        _turn: Arc<dyn TurnHandle>,
        _agent_config: &AgentConfig,
        _conversation_config: &ConversationConfig,
        _request: &SendRequest,
        _stream_mode: ExecutorStreamMode<'_>,
        _turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        anyhow::bail!(
            "{} harness cannot safely resume an unfinished turn",
            self.name()
        )
    }
}

type ShutdownHook = Arc<dyn Fn() -> BoxFuture<'static, Result<()>> + Send + Sync>;

#[derive(Clone)]
pub struct Runtime {
    provider: Arc<dyn Provider>,
    initialized: Arc<OnceCell<()>>,
    events: Arc<HarnessEvents>,
    finalizers: Arc<tokio::sync::Mutex<TaskGroup>>,
    tracer: Arc<dyn ExecutionTracer>,
    recovery: Arc<OnceCell<()>>,
    recovery_gate: Arc<RecoveryGate>,
    recovery_agent_concurrency: Arc<AtomicUsize>,
    recovery_thread_concurrency: Arc<AtomicUsize>,
    shutdown_hook: Option<ShutdownHook>,
}

impl Runtime {
    pub fn with_caller(&self, caller: exoharness::access::Caller) -> Result<Self> {
        let mut scoped = self.clone();
        scoped.provider = self.provider.with_caller(caller)?;
        scoped.initialized = Arc::default();
        scoped.finalizers = Arc::new(tokio::sync::Mutex::new(TaskGroup::new(
            scoped.provider.runtime_host(),
        )));
        scoped.recovery = Arc::default();
        // The root runtime owns cleanup shared with caller-scoped runtimes.
        scoped.shutdown_hook = None;
        Ok(scoped)
    }

    pub fn with_tracer(
        provider: impl Provider + 'static,
        tracer: Arc<dyn ExecutionTracer>,
    ) -> Self {
        let host = provider.runtime_host();
        Self {
            provider: Arc::new(provider),
            initialized: Arc::default(),
            events: Arc::default(),
            finalizers: Arc::new(tokio::sync::Mutex::new(TaskGroup::new(host))),
            tracer,
            recovery: Arc::default(),
            recovery_gate: Arc::default(),
            recovery_agent_concurrency: Arc::new(AtomicUsize::new(4)),
            recovery_thread_concurrency: Arc::new(AtomicUsize::new(4)),
            shutdown_hook: None,
        }
    }

    /// Run cleanup after execution and finalizers have drained. Caller-scoped
    /// runtimes leave this hook with their root runtime.
    pub fn with_shutdown_hook<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.shutdown_hook = Some(Arc::new(move || Box::pin(hook())));
        self
    }

    pub fn set_recovery_concurrency(&self, agents: NonZeroUsize, threads: NonZeroUsize) {
        self.recovery_agent_concurrency
            .store(agents.get(), Ordering::Relaxed);
        self.recovery_thread_concurrency
            .store(threads.get(), Ordering::Relaxed);
    }

    pub async fn recover_unfinished_turns(&self) -> Result<()> {
        self.recover_unfinished_turns_with_resolver(None).await
    }

    pub async fn recover_unfinished_turns_with_resolver(
        &self,
        resolver: Option<RecoveryRuntimeResolver>,
    ) -> Result<()> {
        let result = self
            .recovery
            .get_or_try_init(|| async {
                self.provider
                    .recover_unfinished_turns(self.clone(), resolver)
                    .await
            })
            .await;
        self.recovery_gate.finish();
        result.map(|_| ())
    }

    pub fn begin_recovery_scan(&self) {
        self.recovery_gate.begin();
    }

    pub async fn approval_response(
        &self,
        agent: exoharness::AgentId,
        thread: exoharness::ThreadId,
        turn: exoharness::TurnId,
        body: &exo_managed_agents::http::protocol::ApprovalResponseBody,
    ) -> Result<exoharness::EventId> {
        self.provider
            .approval_response(agent, thread, turn, body)
            .await
    }

    pub async fn frontend_tool_result(
        &self,
        agent: exoharness::AgentId,
        thread: exoharness::ThreadId,
        turn: exoharness::TurnId,
        body: &exo_managed_agents::http::protocol::FrontendToolResultBody,
    ) -> Result<exoharness::EventId> {
        self.provider
            .frontend_tool_result(agent, thread, turn, body)
            .await
    }

    pub(crate) async fn is_turn_active(
        &self,
        thread: &dyn exoharness::ThreadHandle,
        turn: exoharness::TurnId,
    ) -> Result<bool> {
        self.provider.is_turn_active(thread, turn).await
    }

    pub async fn reconnect_turn(
        &self,
        thread: &dyn exoharness::ThreadHandle,
    ) -> Result<Option<(exoharness::TurnRecord, ExecutionStreamHandle)>> {
        let latest = thread
            .get_events(Some(exoharness::EventQuery {
                direction: Some(exoharness::EventQueryDirection::Desc),
                limit: Some(1),
                types: Some(vec![
                    exoharness::EventKind::TURN_STARTED,
                    exoharness::EventKind::TURN_ENDED,
                ]),
                ..Default::default()
            }))
            .await?
            .events
            .into_iter()
            .next();
        let Some(event) = latest else {
            return Ok(None);
        };
        if !matches!(event.data, exoharness::EventData::TurnStarted { .. }) {
            return Ok(None);
        }
        let turn = exoharness::TurnRecord {
            id: event.turn_id.context("turn event is missing a turn id")?,
            session_id: event
                .session_id
                .context("turn event is missing a session id")?,
        };
        if !self.is_turn_active(thread, turn.id).await? {
            return Ok(None);
        }
        let events = crate::permissions::approval_events(
            thread,
            exoharness::EventQuery {
                turn_id: Some(turn.id),
                session_id: Some(turn.session_id),
                ..Default::default()
            },
        )
        .await?;
        if events
            .iter()
            .any(|event| matches!(event.data, exoharness::EventData::TurnEnded))
        {
            return Ok(None);
        }
        let after = events.last().map(|event| event.id).unwrap_or(event.id);
        let pending = crate::permissions::pending_from_events(events)?;
        let initial: Vec<_> = pending
            .into_iter()
            .map(|approval| {
                Ok(ExecutionStreamEvent::ApprovalRequested {
                    turn: turn.clone(),
                    approval,
                })
            })
            .collect();
        let live = thread
            .watch_events(std::ops::Bound::Excluded(after))
            .await?;
        let stream = crate::harness_events::turn_stream(live, turn.clone());
        Ok(Some((
            turn,
            ExecutionStreamHandle::new(futures::stream::iter(initial).chain(stream)),
        )))
    }

    pub(crate) async fn recover_local_turns(
        &self,
        provider: &crate::LocalProvider,
        resolver: Option<RecoveryRuntimeResolver>,
    ) -> Result<()> {
        let agents = provider.state.list_agents().await?;
        let listings = agents
            .into_iter()
            .map(|agent| async move {
                let result = agent
                    .list_threads(exoharness::ListThreadsRequest {
                        unfinished_only: true,
                        ..Default::default()
                    })
                    .await;
                (agent, result)
            })
            .collect::<Vec<_>>();
        let pages = futures::stream::iter(listings)
            .buffer_unordered(self.recovery_agent_concurrency.load(Ordering::Relaxed))
            .collect::<Vec<_>>()
            .await;
        let mut threads = Vec::new();
        for (agent, result) in pages {
            match result {
                Ok(page) => threads.extend(page.threads.into_iter().filter_map(|thread| {
                    (!self.recovery_gate.is_new_thread(thread.record().id))
                        .then_some((Arc::clone(&agent), thread))
                })),
                Err(error) => {
                    tracing::error!(agent_id = %agent.record().id, %error, "failed to list unfinished threads");
                }
            }
        }
        self.recovery_gate.index(
            threads
                .iter()
                .map(|(_, thread)| thread.record().id)
                .collect(),
        );
        let recoveries = threads
            .into_iter()
            .map(|(agent, thread)| {
                let resolver = resolver.clone();
                async move {
                    let thread_id = thread.record().id;
                    let result = self
                        .recover_local_thread(provider, agent, thread, resolver)
                        .await;
                    self.recovery_gate.thread_done(thread_id);
                    if let Err(error) = result {
                        tracing::error!(%thread_id, %error, "failed to recover thread");
                    }
                }
            })
            .collect::<Vec<_>>();
        futures::stream::iter(recoveries)
            .buffer_unordered(self.recovery_thread_concurrency.load(Ordering::Relaxed))
            .collect::<Vec<_>>()
            .await;
        Ok(())
    }

    async fn recover_local_thread(
        &self,
        provider: &crate::LocalProvider,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ConversationHandle>,
        resolver: Option<RecoveryRuntimeResolver>,
    ) -> Result<()> {
        let events = thread
            .get_events(Some(EventQuery {
                direction: Some(EventQueryDirection::Asc),
                types: Some(vec![
                    EventKind::custom(RUNTIME_TURN_WORK),
                    EventKind::custom(RUNTIME_TURN_COMPLETED),
                    EventKind::TURN_STARTED,
                    EventKind::TURN_ENDED,
                    EventKind::TOOL_REQUESTED,
                    EventKind::TOOL_RESULT,
                    EventKind::custom(crate::basic::BASIC_TOOL_ROUND),
                    EventKind::ERROR,
                    EventKind::custom(crate::permissions::APPROVAL_REQUESTED),
                    EventKind::custom(crate::permissions::APPROVAL_RESPONSE),
                ]),
                ..Default::default()
            }))
            .await?
            .events;
        let mut work = Vec::new();
        let mut ended = std::collections::HashSet::new();
        let mut completed = std::collections::HashSet::new();
        let mut failed = std::collections::HashSet::new();
        let mut callers = std::collections::HashMap::new();
        let mut tool_requests = std::collections::HashMap::<
            _,
            Vec<(Option<u32>, String, exoharness::ToolRequest)>,
        >::new();
        let mut tool_rounds = std::collections::HashMap::new();
        let mut invalid_tool_rounds = std::collections::HashSet::new();
        let mut approval_requests =
            std::collections::HashMap::<_, Vec<crate::permissions::ApprovalRequest>>::new();
        let mut approval_responses =
            std::collections::HashMap::<_, std::collections::HashSet<String>>::new();
        for event in events {
            match event.data {
                EventData::TurnStarted { user_id } => {
                    if let Some(turn_id) = event.turn_id {
                        callers.insert(turn_id, user_id);
                    }
                }
                EventData::Custom {
                    event_type,
                    payload,
                } if event_type == RUNTIME_TURN_WORK => {
                    if let (Some(id), Some(session_id)) = (event.turn_id, event.session_id) {
                        work.push((
                            TurnRecord { id, session_id },
                            serde_json::from_value::<RecoverableTurn>(payload),
                        ));
                    } else {
                        tracing::error!(thread_id = %thread.record().id, "turn work has no turn or session id");
                    }
                }
                EventData::TurnEnded => {
                    if let Some(turn_id) = event.turn_id {
                        ended.insert(turn_id);
                    }
                }
                EventData::Custom { event_type, .. } if event_type == RUNTIME_TURN_COMPLETED => {
                    if let Some(turn_id) = event.turn_id {
                        completed.insert(turn_id);
                    }
                }
                EventData::Custom {
                    event_type,
                    payload,
                } if event_type == crate::permissions::APPROVAL_REQUESTED => {
                    if let Some(turn_id) = event.turn_id {
                        match serde_json::from_value::<crate::permissions::ApprovalRequest>(payload)
                        {
                            Ok(approval) => {
                                approval_requests.entry(turn_id).or_default().push(approval)
                            }
                            Err(error) => {
                                tracing::error!(%turn_id, %error, "invalid saved approval request")
                            }
                        }
                    }
                }
                EventData::Custom {
                    event_type,
                    payload,
                } if event_type == crate::permissions::APPROVAL_RESPONSE => {
                    if let Some(turn_id) = event.turn_id {
                        match serde_json::from_value::<crate::permissions::ApprovalResponse>(
                            payload,
                        ) {
                            Ok(response) => {
                                approval_responses
                                    .entry(turn_id)
                                    .or_default()
                                    .insert(response.approval_id);
                            }
                            Err(error) => {
                                tracing::error!(%turn_id, %error, "invalid saved approval response")
                            }
                        }
                    }
                }
                EventData::Error { .. } => {
                    if let Some(turn_id) = event.turn_id {
                        failed.insert(turn_id);
                    }
                }
                EventData::Custom {
                    event_type,
                    payload,
                } if event_type == crate::basic::BASIC_TOOL_ROUND => {
                    if let Some(turn_id) = event.turn_id {
                        match serde_json::from_value::<crate::basic::BasicToolRound>(payload) {
                            Ok(marker) => {
                                tool_rounds.insert(turn_id, marker.round);
                            }
                            Err(error) => {
                                tracing::error!(%turn_id, %error, "invalid saved tool round");
                                invalid_tool_rounds.insert(turn_id);
                            }
                        }
                    }
                }
                EventData::ToolRequested {
                    tool_call_id,
                    request,
                    ..
                } => {
                    if let Some(turn_id) = event.turn_id {
                        tool_requests.entry(turn_id).or_default().push((
                            tool_rounds.get(&turn_id).copied(),
                            tool_call_id,
                            request,
                        ));
                    }
                }
                EventData::ToolResult { tool_call_id, .. } => {
                    if let Some(turn_id) = event.turn_id
                        && let Some(pending) = tool_requests.get_mut(&turn_id)
                        && let Some(index) =
                            pending.iter().position(|(_, id, _)| *id == tool_call_id)
                    {
                        pending.remove(index);
                    }
                }
                _ => {}
            }
        }
        // Turns on the same thread still need to be resumed in event order.
        let mut thread_error = None;
        for (turn, work) in work {
            if ended.contains(&turn.id) {
                continue;
            }
            let turn_id = turn.id;
            let result = async {
                if provider.is_turn_active(thread.as_ref(), turn_id).await? {
                    return Ok(());
                }
                if completed.contains(&turn_id) || failed.contains(&turn_id) {
                    thread.turn_handle(turn.clone()).await?.finish().await?;
                    return Ok(());
                }
                let work = work?;
                anyhow::ensure!(
                    !invalid_tool_rounds.contains(&turn_id),
                    "turn has an invalid saved tool round"
                );
                if let Some((round, tool_call_id, request)) =
                    tool_requests.get(&turn_id).and_then(|requests| requests.first())
                {
                    let pending_approval = provider.executor.can_resume_pending_approval(&work.agent_config)
                        && approval_requests.get(&turn_id).is_some_and(|approvals| {
                            approvals.iter().any(|approval| {
                                approval.tool_call_id.as_deref() == Some(tool_call_id.as_str())
                                    && approval.round == *round
                                    && round.is_some()
                                    && approval.request == *request
                                    && !approval_responses.get(&turn_id).is_some_and(|responses| responses.contains(&approval.approval_id))
                            })
                        });
                    if !pending_approval
                        && !crate::frontend_tools::contains(&work.agent_config, &request.function_name)
                        && !provider
                            .executor
                            .can_reconcile_unresolved_tool_call(&work.agent_config)
                    {
                        anyhow::bail!(
                            "cannot safely resume unresolved tool call `{tool_call_id}` (`{}`) for turn {turn_id}",
                            request.function_name
                        );
                    }
                }
                let caller = callers
                    .get(&turn_id)
                    .context("turn has no recorded caller")?;
                let runtime = match caller {
                    Some(principal) => resolver
                        .as_ref()
                        .context("caller-scoped recovery is unavailable")?(
                        principal.clone()
                    )?,
                    None => Arc::new(self.clone()),
                };
                if let Some(principal) = caller {
                    anyhow::ensure!(
                        runtime
                            .exoharness_handle()
                            .caller()
                            .map(|c| c.principal.as_str())
                            == Some(principal.as_str()),
                        "recovery runtime has the wrong caller"
                    );
                }
                let scoped_agent = runtime
                    .exoharness_handle()
                    .get_agent(&agent.record().id)
                    .await?
                    .context("recovery caller cannot access agent")?;
                let scoped_thread = scoped_agent
                    .get_thread(&thread.record().id)
                    .await?
                    .context("recovery caller cannot access thread")?;
                let mut stream = runtime
                    .provider
                    .resume_turn(
                        runtime.as_ref(),
                        scoped_agent,
                        scoped_thread,
                        turn.clone(),
                        work.request,
                        work.agent_config,
                        work.thread_config,
                    )
                    .await?;
                runtime.provider.runtime_host().spawn(Box::pin(async move {
                    while let Some(event) = stream.next().await {
                        if let Err(error) = event {
                            tracing::error!(%turn_id, %error, "recovered turn failed");
                        }
                    }
                }));
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                tracing::error!(%turn_id, %error, "failed to recover turn");
                match thread.turn_handle(turn).await {
                    Ok(handle) => {
                        if let Err(finalize_error) =
                            finalize_turn(handle.as_ref(), Err(error)).await
                        {
                            tracing::error!(%turn_id, %finalize_error, "failed to end unrecoverable turn");
                            thread_error = Some(finalize_error);
                        }
                    }
                    Err(handle_error) => {
                        tracing::error!(%turn_id, %handle_error, "failed to load unrecoverable turn");
                        thread_error = Some(handle_error);
                    }
                }
            }
        }
        thread_error.map_or(Ok(()), Err)
    }

    pub async fn start_turn(
        &self,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ConversationHandle>,
        request: SendRequest,
        streaming: bool,
        config_override: Option<AgentConfig>,
    ) -> Result<(exoharness::TurnRecord, ExecutionStreamHandle)> {
        let (receipt, response) = tokio::sync::oneshot::channel();
        self.provider
            .harness()
            .submit(HarnessCommand::StartTurn(crate::provider::ProviderTurn {
                agent,
                thread,
                request,
                streaming,
                config_override,
                receipt,
                runtime: self.clone(),
            }))
            .await?;
        response
            .await
            .map_err(|error| anyhow!("provider stopped before acknowledging the turn: {error}"))
    }

    pub(crate) async fn start_local_turn(
        &self,
        provider: &crate::LocalProvider,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ConversationHandle>,
        request: SendRequest,
        streaming: bool,
        config_override: Option<AgentConfig>,
        recovery: Option<(TurnRecord, ConversationConfig)>,
    ) -> Result<(exoharness::TurnRecord, ExecutionStreamHandle)> {
        if recovery.is_none() {
            self.recovery_gate.wait(thread.record().id).await;
        }
        self.initialized
            .get_or_try_init(|| {
                provider
                    .harness
                    .init(HarnessEventSink::new(self.events.clone()))
            })
            .await?;
        let guard = conversation_send_lock(&thread.record().id.to_string())
            .lock_owned()
            .await;
        if recovery.is_none() && thread.activate_caller().await? {
            provider.executor.reset_thread(thread.record().id).await?;
        }
        let apply_thread_harness = config_override.is_none() && recovery.is_none();
        let (mut agent_config, mut thread_config) = tokio::try_join!(
            async {
                if let Some(config) = config_override {
                    return Ok(config);
                }
                let (mut config, model) = tokio::try_join!(
                    self.get_agent_config(agent.as_ref()),
                    get_conversation_model_override(thread.as_ref()),
                )?;
                apply_conversation_model_override(&mut config, model);
                Ok::<_, anyhow::Error>(config)
            },
            async {
                match recovery.as_ref() {
                    Some((_, config)) => Ok(config.clone()),
                    None => self.get_conversation_config(thread.as_ref()).await,
                }
            },
        )?;
        if apply_thread_harness {
            crate::managed_agents::apply_thread_harness(
                &mut agent_config,
                thread_config.harness.as_ref(),
            )?;
        }
        if recovery.is_none() {
            if let Some(definition) = exo_managed_agents::load_definition(agent.as_ref()).await? {
                thread_config.permissions = definition.permissions();
            }
            if thread.caller().is_some() {
                thread_config.sandbox_scope = Some(crate::SandboxScope::Conversation);
            }
            if !thread_config.resources.is_empty() {
                thread_config
                    .materialize_resources(thread.as_ref(), &agent_config)
                    .await?;
                let mut locations = String::from("Filesystem resources for this thread:\n");
                for resource in &thread_config.resources {
                    let resource = &resource.definition;
                    let mode = match resource.mode {
                        exoharness::FileSystemMountMode::ReadOnly => "read-only",
                        exoharness::FileSystemMountMode::ReadWrite => "read-write",
                    };
                    locations.push_str(&format!(
                        "- {}: {:?} ({mode})\n",
                        resource.name, resource.mount_path
                    ));
                }
                agent_config
                    .instructions
                    .push(crate::harness_helpers::system_message(&locations));
            }
        }
        provider
            .executor
            .prepare_conversation(
                agent.as_ref(),
                thread.as_ref(),
                &agent_config,
                &thread_config,
            )
            .await?;
        // Reconnect must not see the saved turn before it is registered as live.
        let mut live_turns = provider.live_turns.write().await;
        let recovering = recovery.is_some();
        let turn = match recovery {
            Some((record, _)) => thread.turn_handle(record).await?,
            None => {
                let work = RecoverableTurn {
                    agent_config: agent_config.clone(),
                    thread_config: thread_config.clone(),
                    request: request.clone(),
                };
                thread
                    .begin_turn(BeginTurnRequest {
                        session_id: request.session_id,
                        input: request.input.clone(),
                        initial_events: vec![work.event()?],
                    })
                    .await?
            }
        };
        let key = HarnessTurnKey {
            thread_id: thread.record().id,
            turn_id: turn.record().id,
        };
        let record = turn.record().clone();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut completion = self.events.register(
            Arc::clone(&thread),
            Arc::clone(&turn),
            streaming.then(|| event_tx.clone()),
        )?;
        let live_turn = Arc::new(());
        live_turns.retain(|_, turn| turn.strong_count() > 0);
        live_turns.insert(key, Arc::downgrade(&live_turn));
        drop(live_turns);
        let trace: Option<Arc<dyn TurnExecutionTrace>> = self
            .tracer
            .start_turn(
                agent_config.braintrust.as_ref(),
                agent.record(),
                thread.record(),
                &agent_config,
                turn.record().session_id,
                turn.record().id,
                streaming,
            )
            .await
            .map(Arc::from);
        let mut finalizers = self.finalizers.lock().await;
        while let Some(result) = finalizers.try_join_next() {
            if let Err(error) = result {
                tracing::error!(%error, "harness turn finalization panicked");
            }
        }
        let completion_thread = Arc::clone(&thread);
        let submission = provider
            .harness
            .submit(HarnessCommand::StartTurn(ExecutorTurn {
                agent,
                thread,
                turn: Arc::clone(&turn),
                agent_config,
                thread_config,
                request,
                recovering,
                stream: streaming.then(|| event_tx.clone()),
                trace: trace.clone(),
            }))
            .await;
        if let Err(error) = submission {
            self.events.remove(key);
            let error = finalize_turn(turn.as_ref(), Err(error))
                .await
                .expect_err("failed submission");
            if let Some(trace) = trace {
                trace.finish_error(&error).await;
            }
            return Err(error);
        }
        let harness = Arc::clone(&provider.harness);
        finalizers.spawn(async move {
            let _guard = guard;
            let outcome = tokio::select! {
                outcome = &mut completion => outcome,
                () = event_tx.closed() => {
                    if let Err(error) = harness.submit(HarnessCommand::CancelTurn { key }).await {
                        tracing::error!(?key, %error, "failed to cancel disconnected harness turn");
                    }
                    completion.await
                },
            };
            let latest_event_id = match outcome {
                Ok(HarnessTurnOutcome::Completed(_)) => {
                    let marked = completion_thread
                        .get_events(Some(EventQuery {
                            turn_id: Some(key.turn_id),
                            types: Some(vec![EventKind::custom(RUNTIME_TURN_COMPLETED)]),
                            ..Default::default()
                        }))
                        .await
                        .map(|result| !result.events.is_empty());
                    match marked {
                        Ok(true) => finalize_turn(turn.as_ref(), Ok(())).await,
                        Ok(false) => {
                            match turn
                                .add_events(vec![EventData::Custom {
                                    event_type: RUNTIME_TURN_COMPLETED.to_owned(),
                                    payload: serde_json::Value::Null,
                                }])
                                .await
                            {
                                Ok(_) => finalize_turn(turn.as_ref(), Ok(())).await,
                                Err(error) => finalize_turn(turn.as_ref(), Err(error)).await,
                            }
                        }
                        Err(error) => finalize_turn(turn.as_ref(), Err(error)).await,
                    }
                }
                Ok(HarnessTurnOutcome::Failed(error)) => {
                    finalize_turn(turn.as_ref(), Err(error)).await
                }
                Ok(HarnessTurnOutcome::Cancelled) => {
                    finalize_turn(turn.as_ref(), Err(anyhow!("harness turn cancelled"))).await
                }
                Ok(HarnessTurnOutcome::Interrupted) => {
                    Err(anyhow!("harness turn interrupted by shutdown"))
                }
                Err(error) => {
                    finalize_turn(
                        turn.as_ref(),
                        Err(anyhow!("harness stopped without completion: {error}")),
                    )
                    .await
                }
            };
            drop(live_turn);
            if let Some(trace) = trace {
                match &latest_event_id {
                    Ok(id) => trace.finish_success(Some(*id)).await,
                    Err(error) => trace.finish_error(error).await,
                }
            }
            let result = latest_event_id.map(|latest_event_id| {
                ExecutionStreamEvent::Completed(SendResult {
                    session_id: turn.record().session_id,
                    turn_id: key.turn_id,
                    latest_event_id,
                })
            });
            if event_tx.send(result).is_err() {
                tracing::debug!(?key, "harness stream closed before completion delivery");
            }
        });
        Ok((
            record,
            ExecutionStreamHandle::new(UnboundedReceiverStream::new(event_rx)),
        ))
    }
}

fn apply_conversation_model_override(
    agent_config: &mut AgentConfig,
    model_override: Option<ConversationModelConfig>,
) {
    if let Some(config) = model_override {
        agent_config.model = config.model;
        agent_config.max_output_tokens = config.max_output_tokens;
    }
}

impl Runtime {
    pub async fn get_agent_config(&self, agent: &dyn AgentHandle) -> Result<AgentConfig> {
        load_agent_config(agent).await
    }

    pub(crate) async fn resolve_thread_harness(
        &self,
        thread: &dyn ConversationHandle,
        config: &AgentConfig,
        harness: &str,
    ) -> Result<crate::ConversationHarnessConfig> {
        self.provider
            .resolve_thread_harness(thread, config, harness)
            .await
    }

    pub async fn get_thread_agent_config(
        &self,
        agent: &dyn AgentHandle,
        thread: &dyn ConversationHandle,
    ) -> Result<AgentConfig> {
        let (mut config, thread_config, model) = tokio::try_join!(
            self.get_agent_config(agent),
            self.get_conversation_config(thread),
            get_conversation_model_override(thread),
        )?;
        apply_conversation_model_override(&mut config, model);
        crate::managed_agents::apply_thread_harness(&mut config, thread_config.harness.as_ref())?;
        Ok(config)
    }

    pub async fn put_agent_config(
        &self,
        agent: &dyn AgentHandle,
        config: AgentConfig,
    ) -> Result<()> {
        store_agent_config(agent, &config).await
    }

    pub async fn get_conversation_config(
        &self,
        conversation: &dyn ConversationHandle,
    ) -> Result<ConversationConfig> {
        load_conversation_config(conversation).await
    }

    pub async fn put_conversation_config(
        &self,
        conversation: &dyn ConversationHandle,
        config: ConversationConfig,
    ) -> Result<()> {
        store_conversation_config(conversation, &config).await
    }

    pub async fn send(
        &self,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        request: SendRequest,
    ) -> Result<SendResult> {
        let (_, mut events) = self
            .start_turn(agent, conversation, request, false, None)
            .await?;
        while let Some(event) = events.next().await {
            if let ExecutionStreamEvent::Completed(result) = event? {
                return Ok(result);
            }
        }
        Err(anyhow!("harness stopped without completion"))
    }

    pub async fn send_stream(
        &self,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        request: SendRequest,
    ) -> Result<ExecutionStreamHandle> {
        self.start_turn(agent, conversation, request, true, None)
            .await
            .map(|(_, stream)| stream)
    }

    pub async fn cancel_turn(&self, key: HarnessTurnKey) -> Result<bool> {
        if !self.events.contains(key) {
            return Ok(false);
        }
        self.cancel(key).await?;
        Ok(true)
    }

    pub async fn cancel(&self, key: HarnessTurnKey) -> Result<()> {
        self.provider
            .harness()
            .submit(HarnessCommand::CancelTurn { key })
            .await
    }

    pub async fn flush_tracing(&self) -> Result<()> {
        self.tracer.flush().await
    }

    pub async fn shutdown(&self) -> Result<()> {
        let shutdown = self.provider.harness().shutdown().await;
        let mut finalizers = self.finalizers.lock().await;
        let mut finalizer_error = None;
        while let Some(result) = finalizers.join_next().await {
            if let Err(error) = result {
                tracing::error!(?error, "runtime finalizer failed");
                finalizer_error = Some(error);
            }
        }
        drop(finalizers);
        let flush = self.tracer.flush().await;
        let cleanup = match &self.shutdown_hook {
            Some(hook) => hook().await,
            None => Ok(()),
        };
        shutdown?;
        if let Some(error) = finalizer_error {
            return Err(error);
        }
        flush?;
        cleanup
    }
}

impl Runtime {
    pub async fn update_managed_agent(
        &self,
        agent: &Arc<dyn AgentHandle>,
        definition: &exo_managed_agents::AgentDefinition,
    ) -> Result<exoharness::ArtifactVersion> {
        let previous = exo_managed_agents::load_definition(agent.as_ref()).await?;
        let external_mounts = crate::find_agent_config(agent.as_ref())
            .await?
            .map(|config| config.sandbox.mounts)
            .unwrap_or_default();
        let version = agent
            .write_artifact(exoharness::WriteArtifactRequest {
                path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
                contents: definition.source().as_bytes().to_vec(),
            })
            .await?;
        if let Err(error) = self.provider.configure_agent(agent, definition).await {
            agent
                .write_artifact(exoharness::WriteArtifactRequest {
                    path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
                    contents: previous
                        .map(|definition| definition.source().as_bytes().to_vec())
                        .unwrap_or_default(),
                })
                .await
                .with_context(|| {
                    format!(
                        "updating agent configuration failed: {error:#}; restoring previous definition"
                    )
                })?;
            return Err(error);
        }
        if !external_mounts.is_empty() {
            let mut config = self.get_agent_config(agent.as_ref()).await?;
            for mount in external_mounts {
                config
                    .sandbox
                    .mounts
                    .retain(|other| other.mount_path != mount.mount_path);
                config.sandbox.mounts.push(mount);
            }
            self.put_agent_config(agent.as_ref(), config).await?;
        }
        Ok(version)
    }

    pub fn exoharness_handle(&self) -> Arc<dyn ExoHarness> {
        self.provider.exoharness()
    }

    pub async fn create_managed_agent(
        &self,
        definition: &exo_managed_agents::AgentDefinition,
        name: &str,
        slug: &str,
    ) -> Result<Arc<dyn AgentHandle>> {
        exo_managed_agents::create_agent(self.provider.as_ref(), definition, name, slug).await
    }

    pub async fn open_managed_thread(
        &self,
        agent: &Arc<dyn AgentHandle>,
        reference: Option<&str>,
        request: NewConversationRequest,
        options: &exo_managed_agents::ThreadOptions,
    ) -> Result<exo_managed_agents::OpenedThread> {
        let opened = exo_managed_agents::open_thread(
            self.provider.as_ref(),
            agent,
            reference,
            request,
            options,
        )
        .await?;
        if opened.created {
            self.recovery_gate.new_thread(opened.thread.record().id);
        }
        Ok(opened)
    }

    pub async fn end_session(
        &self,
        thread: &dyn ConversationHandle,
        session_id: exoharness::SessionId,
    ) -> Result<()> {
        self.provider.end_session(thread, session_id).await
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        let agents = self.provider.exoharness().list_agents().await?;
        Ok(agents
            .into_iter()
            .map(|agent| agent.record().clone())
            .collect())
    }

    pub async fn get_agent(&self, agent_ref: &str) -> Result<Option<Arc<dyn AgentHandle>>> {
        resolve_agent_handle(self.provider.exoharness().as_ref(), agent_ref).await
    }

    pub async fn create_agent(&self, request: CreateAgentRequest) -> Result<Arc<dyn AgentHandle>> {
        let name = request.name.clone().unwrap_or_else(|| request.slug.clone());
        let config = AgentConfig {
            frontend_tools: Vec::new(),
            resources: Vec::new(),
            instructions: Vec::new(),
            harness: request.harness,
            typescript: request.typescript,
            enable_agent_tool_creation: request.enable_agent_tool_creation,
            sandbox: crate::AgentSandboxConfig {
                image: request.sandbox_image,
                provider: request.sandbox_provider,
                mounts: Vec::new(),
                enable_networking: request.enable_networking,
                scope: request.sandbox_scope.unwrap_or_default(),
            },
            model: request.model,
            credential: request.credential,
            base_url: request.base_url,
            reasoning_effort: None,
            max_output_tokens: request.max_output_tokens,
            max_tool_round_trips: request.max_tool_round_trips,
            braintrust: request.braintrust,
        };
        let agent = self
            .provider
            .exoharness()
            .new_agent(NewAgentRequest {
                vaults: vec![],
                slug: request.slug,
                name,
            })
            .await?;
        self.put_agent_config(agent.as_ref(), config).await?;
        Ok(agent)
    }

    pub async fn delete_agent(&self, agent_ref: &str) -> Result<bool> {
        let Some(agent) =
            resolve_agent_handle(self.provider.exoharness().as_ref(), agent_ref).await?
        else {
            return Ok(false);
        };
        self.provider
            .exoharness()
            .delete_agent(&agent.record().id)
            .await
    }

    pub async fn get_conversation(
        &self,
        agent: &dyn AgentHandle,
        reference: &str,
    ) -> Result<Option<Arc<dyn ConversationHandle>>> {
        resolve_conversation_handle(agent, reference).await
    }

    pub async fn delete_conversation(
        &self,
        agent: &dyn AgentHandle,
        reference: &str,
    ) -> Result<bool> {
        let Some(thread) = resolve_conversation_handle(agent, reference).await? else {
            return Ok(false);
        };
        agent.delete_conversation(&thread.record().id).await
    }

    pub async fn create_conversation(
        &self,
        agent: &dyn AgentHandle,
        request: CreateConversationRequest,
    ) -> Result<Arc<dyn ConversationHandle>> {
        let agent_config = self.get_agent_config(agent).await?;
        let conversation = agent
            .new_conversation(NewConversationRequest {
                environment: None,
                vaults: request.vaults,
                slug: request.slug,
                name: request.name,
            })
            .await?;
        let default_conversation_config = ConversationConfig::default();
        let sandbox_provider = request
            .sandbox_provider
            .unwrap_or_else(|| agent_config.sandbox.provider.clone());
        let resource_mounts = match conversation
            .materialize_resources(agent_config.resources.clone(), sandbox_provider.clone())
            .await
        {
            Ok(mounts) => mounts,
            Err(error) => {
                agent
                    .delete_conversation(&conversation.record().id)
                    .await
                    .context("cleaning up thread after resource preparation failed")?;
                return Err(error);
            }
        };
        let conversation_config = ConversationConfig {
            harness: None,
            resources: agent_config.resources.clone(),
            resource_mounts,
            sandbox_image: request.sandbox_image.or(agent_config.sandbox.image),
            sandbox_provider: Some(sandbox_provider),
            shell_program: request
                .shell_program
                .or(default_conversation_config.shell_program),
            mounts: default_conversation_config.mounts,
            durable_file_systems: default_conversation_config.durable_file_systems,
            sandbox_scope: (!agent_config.resources.is_empty())
                .then_some(crate::SandboxScope::Conversation),
            permissions: default_conversation_config.permissions,
            environment: default_conversation_config.environment,
            egress_policy: default_conversation_config.egress_policy,
        };
        if let Err(error) = self
            .put_conversation_config(conversation.as_ref(), conversation_config)
            .await
        {
            agent
                .delete_conversation(&conversation.record().id)
                .await
                .with_context(|| {
                    format!("configuring thread failed ({error:#}); cleanup also failed")
                })?;
            return Err(error);
        }
        self.recovery_gate.new_thread(conversation.record().id);
        Ok(conversation)
    }
}
