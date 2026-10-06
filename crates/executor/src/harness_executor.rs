use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, AgentRecord, BeginTurnRequest, ConversationHandle, EventData, EventKind,
    EventQuery, EventQueryDirection, ExoHarness, NewAgentRequest, NewConversationRequest, Result,
    TurnHandle, TurnRecord,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use tokio::sync::{OnceCell, mpsc};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::execution_tracing::{ExecutionTracer, TurnExecutionTrace};
use crate::harness::{
    Harness, HarnessCommand, HarnessEventSink, HarnessTurnKey, HarnessTurnOutcome,
};
use crate::harness_adapter::ExecutorTurn;
use crate::harness_config::{
    load_agent_config, load_conversation_config, store_agent_config, store_conversation_config,
};
use crate::harness_events::HarnessEvents;
use crate::harness_helpers::{resolve_agent_handle, resolve_conversation_handle};
use crate::runtime_host::TaskGroup;
use crate::shared::finalize_turn;
use crate::{
    AgentConfig, ConversationConfig, CreateAgentRequest, CreateConversationRequest,
    ExecutionStreamEvent, ExecutionStreamHandle, Provider, SendRequest, SendResult,
};

pub(crate) const RUNTIME_TURN_WORK: &str = "exo.runtime.turn_work";
pub(crate) const RUNTIME_TURN_COMPLETED: &str = "exo.runtime.turn_completed";

pub type RecoveryRuntimeResolver = Arc<dyn Fn(String) -> Result<Arc<Runtime>> + Send + Sync>;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnWork {
    pub streaming: bool,
    pub agent_config: AgentConfig,
    pub thread_config: ConversationConfig,
    pub request: SendRequest,
}

impl TurnWork {
    pub(crate) fn from_events(events: &[exoharness::Event]) -> Result<Self> {
        let payload = events
            .iter()
            .find_map(|event| match &event.data {
                EventData::Custom {
                    event_type,
                    payload,
                } if event_type == RUNTIME_TURN_WORK => Some(payload),
                _ => None,
            })
            .context("unfinished turn has no recorded work")?;
        Ok(serde_json::from_value(payload.clone())?)
    }

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

    /// Whether unfinished execution can be suspended and resumed.
    fn can_suspend_turn(&self, _config: &AgentConfig) -> bool {
        false
    }

    fn agent_config(
        &self,
        _definition: &exo_managed_agents::AgentDefinition,
    ) -> Result<AgentConfig> {
        Err(anyhow!("this executor does not configure managed agents"))
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
    root_provider: Arc<dyn Provider>,
    initialized: Arc<OnceCell<()>>,
    events: Arc<HarnessEvents>,
    finalizers: Arc<tokio::sync::Mutex<TaskGroup>>,
    tracer: Arc<dyn ExecutionTracer>,
    recovery: Arc<OnceCell<()>>,
    pub(crate) recovery_resolver: Arc<OnceLock<RecoveryRuntimeResolver>>,
    recovery_capacity: Arc<OnceLock<Arc<tokio::sync::Semaphore>>>,
    shutdown_hook: Option<ShutdownHook>,
}

impl Runtime {
    pub(crate) async fn claim_recovery_slot(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        let capacity = self
            .recovery_capacity
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
            .clone();
        Ok(capacity.acquire_owned().await?)
    }

    pub(crate) fn root_runtime(&self) -> Self {
        let mut runtime = self.clone();
        runtime.provider = self.root_provider.clone();
        runtime
    }

    pub(crate) async fn execute_turn(
        &self,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ConversationHandle>,
        turn: TurnRecord,
        work: crate::TurnWork,
        recovering: bool,
    ) -> Result<ExecutionStreamHandle> {
        self.provider
            .execute_turn(self, agent, thread, turn, work, recovering)
            .await
    }
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
        let provider: Arc<dyn Provider> = Arc::new(provider);
        Self {
            provider: provider.clone(),
            root_provider: provider,
            initialized: Arc::default(),
            events: Arc::default(),
            finalizers: Arc::new(tokio::sync::Mutex::new(TaskGroup::new(host))),
            tracer,
            recovery: Arc::default(),
            recovery_resolver: Arc::default(),
            recovery_capacity: Arc::default(),
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

    /// Configure startup recovery before any worker acquires a recovery slot.
    pub fn set_recovery_concurrency(&self, threads: NonZeroUsize) -> Result<()> {
        self.recovery_capacity
            .set(Arc::new(tokio::sync::Semaphore::new(threads.get())))
            .map_err(|_| anyhow!("recovery concurrency is already configured or in use"))?;
        Ok(())
    }

    pub async fn recover_unfinished_turns(&self) -> Result<()> {
        self.recover_unfinished_turns_with_resolver(None).await
    }

    pub async fn recover_unfinished_turns_with_resolver(
        &self,
        resolver: Option<RecoveryRuntimeResolver>,
    ) -> Result<()> {
        if let Some(resolver) = resolver {
            self.recovery_resolver.get_or_init(|| resolver);
        }
        self.recovery
            .get_or_try_init(|| self.provider.recover_unfinished_turns(self.clone()))
            .await
            .map(|_| ())
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

    pub(crate) async fn is_turn_active(&self, key: HarnessTurnKey) -> Result<bool> {
        self.provider.is_turn_active(key).await
    }

    pub async fn reconnect_turn(
        &self,
        agent: exoharness::AgentId,
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
        if !self
            .is_turn_active(HarnessTurnKey::new(agent, thread.record().id, turn.id))
            .await?
        {
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

    pub(crate) async fn recover_local_turns(&self, provider: &crate::LocalProvider) -> Result<()> {
        for scope in provider.turns.coordinator.pending_threads().await? {
            if let Err(error) = self.drain_queue(provider, scope).await {
                tracing::error!(?scope, %error, "failed to recover queued thread");
            }
        }
        Ok(())
    }

    pub async fn start_turn(
        &self,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ConversationHandle>,
        request: SendRequest,
        streaming: bool,
        config_override: Option<AgentConfig>,
    ) -> Result<(exoharness::TurnRecord, ExecutionStreamHandle)> {
        self.start_turn_with_options(
            agent,
            thread,
            request,
            streaming,
            config_override,
            Default::default(),
        )
        .await
    }

    pub async fn start_turn_with_options(
        &self,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ConversationHandle>,
        request: SendRequest,
        streaming: bool,
        config_override: Option<AgentConfig>,
        options: crate::TurnOptions,
    ) -> Result<(exoharness::TurnRecord, ExecutionStreamHandle)> {
        let (receipt, response) = tokio::sync::oneshot::channel();
        self.provider
            .harness()
            .submit(HarnessCommand::StartTurn(crate::provider::ProviderTurn {
                options,
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
        record: TurnRecord,
        work: TurnWork,
        recovering: bool,
    ) -> Result<ExecutionStreamHandle> {
        {
            let mut active = provider
                .turns
                .active
                .lock()
                .expect("active queue owners poisoned");
            anyhow::ensure!(
                !provider.turns.draining.load(Ordering::SeqCst),
                "runtime is shutting down"
            );
            active.insert(
                exoharness::turn_coordinator::TurnThread {
                    agent_id: agent.record().id,
                    thread_id: thread.record().id,
                },
                (provider.harness.clone(), record.id),
            );
        }
        self.initialized
            .get_or_try_init(|| {
                provider
                    .harness
                    .init(HarnessEventSink::new(self.events.clone()))
            })
            .await?;
        if !recovering && thread.activate_caller().await? {
            provider.executor.reset_thread(thread.record().id).await?;
        }
        let TurnWork {
            request,
            streaming,
            mut agent_config,
            mut thread_config,
        } = work;
        if !recovering {
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
        let turn = if recovering {
            thread.turn_handle(record).await?
        } else {
            let work = TurnWork {
                streaming,
                agent_config: agent_config.clone(),
                thread_config: thread_config.clone(),
                request: request.clone(),
            };
            thread
                .begin_turn(BeginTurnRequest {
                    session_id: None,
                    turn: Some(record),
                    input: request.input.clone(),
                    initial_events: vec![work.event()?],
                })
                .await?
        };
        let key = HarnessTurnKey {
            agent_id: agent.record().id,
            thread_id: thread.record().id,
            turn_id: turn.record().id,
        };
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut completion = self.events.register(
            agent.record().id,
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
        finalizers.spawn(async move {
            let outcome = (&mut completion).await;
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
                Ok(HarnessTurnOutcome::Suspended) => Err(crate::turn_queue::TurnSuspended.into()),
                Ok(HarnessTurnOutcome::Interrupted) => Err(anyhow!("harness turn interrupted")),
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
                    Err(error) if error.is::<crate::turn_queue::TurnSuspended>() => {
                        trace.finish_success(None).await
                    }
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
        Ok(ExecutionStreamHandle::new(UnboundedReceiverStream::new(
            event_rx,
        )))
    }
}

impl Runtime {
    pub async fn get_agent_config(&self, agent: &dyn AgentHandle) -> Result<AgentConfig> {
        load_agent_config(agent).await
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
            match event? {
                ExecutionStreamEvent::Completed(result) => return Ok(result),
                ExecutionStreamEvent::Suspended(turn) => {
                    anyhow::bail!("turn {} suspended; resume it to continue", turn.id)
                }
                _ => {}
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
        self.provider.cancel_turn(self, key).await
    }

    pub async fn suspend_turn(&self, key: HarnessTurnKey) -> Result<bool> {
        self.provider.suspend_turn(self, key).await
    }

    pub async fn resume_turn(&self, key: HarnessTurnKey) -> Result<ExecutionStreamHandle> {
        self.provider.resume_suspended_turn(self, key).await
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
        slug: &str,
    ) -> Result<Arc<dyn AgentHandle>> {
        exo_managed_agents::create_agent(self.provider.as_ref(), definition, slug).await
    }

    pub async fn open_managed_thread(
        &self,
        agent: &Arc<dyn AgentHandle>,
        reference: Option<&str>,
        request: NewConversationRequest,
    ) -> Result<exo_managed_agents::OpenedThread> {
        let opened =
            exo_managed_agents::open_thread(self.provider.as_ref(), agent, reference, request)
                .await?;
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
        Ok(conversation)
    }
}

pub(crate) fn recovery_query(turn_id: exoharness::TurnId) -> EventQuery {
    EventQuery {
        turn_id: Some(turn_id),
        direction: Some(EventQueryDirection::Asc),
        types: Some(vec![
            EventKind::custom(RUNTIME_TURN_WORK),
            EventKind::custom(RUNTIME_TURN_COMPLETED),
            EventKind::TURN_STARTED,
            EventKind::TURN_ENDED,
            EventKind::ERROR,
        ]),
        ..Default::default()
    }
}
