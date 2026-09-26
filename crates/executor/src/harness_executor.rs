use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, AgentRecord, BeginTurnRequest, ConversationHandle, ExoHarness, NewAgentRequest,
    NewConversationRequest, Result, TurnHandle,
};
use tokio::sync::{OnceCell, mpsc};
use tokio_stream::{StreamExt, wrappers::UnboundedReceiverStream};

use crate::braintrust::{BraintrustRuntimeConfig, BraintrustTracer};
use crate::conversation_wakeup::conversation_send_lock;
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
use crate::shared::{
    AGENT_CONFIG_CACHE_NAME, CONVERSATION_CONFIG_CACHE_NAME, cache_agent_config,
    cache_conversation_config, finalize_turn, get_or_load_cached,
};
use crate::{
    AgentConfig, ConversationConfig, ConversationModelConfig, CreateAgentRequest,
    CreateConversationRequest, ExecutionStreamEvent, ExecutionStreamHandle, Provider, SendRequest,
    SendResult,
};

#[derive(Clone, Copy)]
pub(crate) enum ExecutorStreamMode<'a> {
    Disabled,
    Enabled(&'a mpsc::UnboundedSender<Result<ExecutionStreamEvent>>),
}

#[async_trait]
pub(crate) trait HarnessExecutor: Send + Sync + 'static {
    fn name(&self) -> &'static str;

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
}

#[derive(Clone)]
pub struct Runtime {
    provider: Arc<dyn Provider>,
    initialized: Arc<OnceCell<()>>,
    events: Arc<HarnessEvents>,
    finalizers: Arc<tokio::sync::Mutex<tokio::task::JoinSet<()>>>,
    tracer: Arc<dyn ExecutionTracer>,
    agent_config_cache: Arc<RwLock<HashMap<exoharness::AgentId, AgentConfig>>>,
    conversation_config_cache: Arc<RwLock<HashMap<exoharness::ConversationId, ConversationConfig>>>,
}

impl Runtime {
    pub fn new(
        provider: impl Provider + 'static,
        runtime_config: Option<BraintrustRuntimeConfig>,
    ) -> Self {
        Self {
            provider: Arc::new(provider),
            initialized: Arc::default(),
            events: Arc::default(),
            finalizers: Arc::default(),
            tracer: Arc::new(BraintrustTracer::new(runtime_config)),
            agent_config_cache: Arc::new(RwLock::new(HashMap::new())),
            conversation_config_cache: Arc::new(RwLock::new(HashMap::new())),
        }
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
    ) -> Result<(exoharness::TurnRecord, ExecutionStreamHandle)> {
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
        let (agent_config, mut thread_config) = tokio::try_join!(
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
            self.get_conversation_config(thread.as_ref()),
        )?;
        if let Some(definition) = exo_managed_agents::load_definition(agent.as_ref()).await? {
            thread_config.permissions = definition.permissions();
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
        let turn = thread
            .begin_turn(BeginTurnRequest {
                session_id: request.session_id,
                input: request.input.clone(),
            })
            .await?;
        let key = HarnessTurnKey {
            thread_id: thread.record().id,
            turn_id: turn.record().id,
        };
        let record = turn.record().clone();
        let mut completion = self
            .events
            .register(Arc::clone(&thread), Arc::clone(&turn))?;
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
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut finalizers = self.finalizers.lock().await;
        while let Some(result) = finalizers.try_join_next() {
            if let Err(error) = result {
                tracing::error!(%error, "harness turn finalization panicked");
            }
        }
        let submission = provider
            .harness
            .submit(HarnessCommand::StartTurn(ExecutorTurn {
                agent,
                thread,
                turn: Arc::clone(&turn),
                agent_config,
                thread_config,
                request,
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
            let result = match outcome {
                Ok(HarnessTurnOutcome::Completed(_)) => Ok(()),
                Ok(HarnessTurnOutcome::Failed(error)) => Err(error),
                Ok(HarnessTurnOutcome::Cancelled) => Err(anyhow!("harness turn cancelled")),
                Err(error) => Err(anyhow!("harness stopped without completion: {error}")),
            };
            let latest_event_id = finalize_turn(turn.as_ref(), result).await;
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
        get_or_load_cached(
            &self.agent_config_cache,
            agent.record().id,
            AGENT_CONFIG_CACHE_NAME,
            || load_agent_config(agent),
        )
        .await
    }

    pub async fn put_agent_config(
        &self,
        agent: &dyn AgentHandle,
        config: AgentConfig,
    ) -> Result<()> {
        store_agent_config(agent, &config).await?;
        cache_agent_config(&self.agent_config_cache, agent.record().id, config);
        Ok(())
    }

    pub async fn get_conversation_config(
        &self,
        conversation: &dyn ConversationHandle,
    ) -> Result<ConversationConfig> {
        get_or_load_cached(
            &self.conversation_config_cache,
            conversation.record().id,
            CONVERSATION_CONFIG_CACHE_NAME,
            || load_conversation_config(conversation),
        )
        .await
    }

    pub async fn put_conversation_config(
        &self,
        conversation: &dyn ConversationHandle,
        config: ConversationConfig,
    ) -> Result<()> {
        store_conversation_config(conversation, &config).await?;
        cache_conversation_config(
            &self.conversation_config_cache,
            conversation.record().id,
            config,
        );
        Ok(())
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

    pub(crate) async fn cancel_turn(&self, key: HarnessTurnKey) -> Result<bool> {
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
        while let Some(result) = finalizers.join_next().await {
            result?;
        }
        let flush = self.tracer.flush().await;
        shutdown?;
        flush
    }
}

impl Runtime {
    pub async fn update_managed_agent(
        &self,
        agent: &Arc<dyn AgentHandle>,
        definition: &exo_managed_agents::AgentDefinition,
    ) -> Result<exoharness::ArtifactVersion> {
        let previous = exo_managed_agents::load_definition(agent.as_ref()).await?;
        let previous_spec_mounts = previous
            .as_ref()
            .and_then(|definition| definition.frontmatter.sandbox.as_ref())
            .map(|sandbox| sandbox.mounts.as_slice())
            .unwrap_or_default();
        let external_mounts: Vec<_> = crate::find_agent_config(agent.as_ref())
            .await?
            .into_iter()
            .flat_map(|config| config.sandbox.mounts)
            .filter(|mount| !previous_spec_mounts.contains(mount))
            .collect();
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
        self.agent_config_cache
            .write()
            .expect("agent config cache poisoned")
            .remove(&agent.record().id);
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
        self.agent_config_cache
            .write()
            .expect("agent config cache poisoned")
            .remove(&agent.record().id);
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
        self.conversation_config_cache
            .write()
            .expect("conversation config cache poisoned")
            .remove(&opened.thread.record().id);
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
        let conversation_config = ConversationConfig {
            sandbox_image: request.sandbox_image.or(agent_config.sandbox.image),
            sandbox_provider: Some(
                request
                    .sandbox_provider
                    .unwrap_or(agent_config.sandbox.provider),
            ),
            shell_program: request
                .shell_program
                .or(default_conversation_config.shell_program),
            mounts: default_conversation_config.mounts,
            durable_file_systems: default_conversation_config.durable_file_systems,
            sandbox_scope: default_conversation_config.sandbox_scope,
            permissions: default_conversation_config.permissions,
            environment: default_conversation_config.environment,
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
