use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::anyhow;
use async_trait::async_trait;
use exoharness::{AgentHandle, BeginTurnRequest, ConversationHandle, Result, TurnHandle};
use tokio::sync::{OnceCell, mpsc};
use tokio_stream::{StreamExt, wrappers::UnboundedReceiverStream};

use crate::braintrust::{BraintrustRuntimeConfig, BraintrustTracer};
use crate::conversation_wakeup::conversation_send_lock;
use crate::execution_tracing::{ExecutionTracer, TurnExecutionTrace};
use crate::harness::{
    Harness, HarnessCommand, HarnessEventSink, HarnessTurnKey, HarnessTurnOutcome,
};
use crate::harness_adapter::{ExecutorHarness, ExecutorTurn};
use crate::harness_config::{
    load_agent_config, load_conversation_config, store_agent_config, store_conversation_config,
};
use crate::harness_events::HarnessEvents;
use crate::harness_facade::HarnessRuntime;
use crate::harness_helpers::get_conversation_model_override;
use crate::shared::{
    AGENT_CONFIG_CACHE_NAME, CONVERSATION_CONFIG_CACHE_NAME, cache_agent_config,
    cache_conversation_config, finalize_turn, get_or_load_cached,
};
use crate::{
    AgentConfig, ConversationConfig, ConversationModelConfig, ExecutionStreamEvent,
    ExecutionStreamHandle, SendRequest, SendResult,
};

#[derive(Clone, Copy)]
pub(crate) enum ExecutorStreamMode<'a> {
    Disabled,
    Enabled(&'a mpsc::UnboundedSender<Result<ExecutionStreamEvent>>),
}

#[async_trait]
pub(crate) trait HarnessExecutor: Send + Sync + Clone + 'static {
    type Prepared: Send + Sync + 'static;

    fn name(&self) -> &'static str;

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

    fn prepare_request(&self, request: &SendRequest) -> Result<Self::Prepared>;

    async fn execute_turn(
        &self,
        agent: &dyn AgentHandle,
        conversation: Arc<dyn ConversationHandle>,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        prepared: &Self::Prepared,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()>;
}

pub(crate) struct ExecutorHarnessRuntime<E> {
    executor: E,
    harness: Arc<ExecutorHarness<E>>,
    initialized: Arc<OnceCell<()>>,
    events: Arc<HarnessEvents>,
    finalizers: Arc<tokio::sync::Mutex<tokio::task::JoinSet<()>>>,
    tracer: Arc<dyn ExecutionTracer>,
    agent_config_cache: Arc<RwLock<HashMap<exoharness::AgentId, AgentConfig>>>,
    conversation_config_cache: Arc<RwLock<HashMap<exoharness::ConversationId, ConversationConfig>>>,
}

impl<E: HarnessExecutor> ExecutorHarnessRuntime<E> {
    pub(crate) fn new(executor: E, runtime_config: Option<BraintrustRuntimeConfig>) -> Self {
        Self {
            harness: Arc::new(ExecutorHarness::new(executor.clone())),
            executor,
            initialized: Arc::default(),
            events: Arc::default(),
            finalizers: Arc::default(),
            tracer: Arc::new(BraintrustTracer::new(runtime_config)),
            agent_config_cache: Arc::new(RwLock::new(HashMap::new())),
            conversation_config_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn start_turn(
        &self,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ConversationHandle>,
        request: SendRequest,
        streaming: bool,
    ) -> Result<ExecutionStreamHandle> {
        self.initialized
            .get_or_try_init(|| {
                self.harness
                    .init(HarnessEventSink::new(self.events.clone()))
            })
            .await?;
        let guard = conversation_send_lock(&thread.record().id.to_string())
            .lock_owned()
            .await;
        let (mut agent_config, thread_config, model_override) = tokio::try_join!(
            self.get_agent_config(agent.as_ref()),
            self.get_conversation_config(thread.as_ref()),
            get_conversation_model_override(thread.as_ref()),
        )?;
        apply_conversation_model_override(&mut agent_config, model_override);
        self.executor
            .prepare_conversation(
                agent.as_ref(),
                thread.as_ref(),
                &agent_config,
                &thread_config,
            )
            .await?;
        let prepared = self.executor.prepare_request(&request)?;
        let turn = thread
            .begin_turn(BeginTurnRequest {
                session_id: request.session_id,
                input: request.input,
            })
            .await?;
        let key = HarnessTurnKey {
            thread_id: thread.record().id,
            turn_id: turn.record().id,
        };
        let mut completion = self
            .events
            .register(Arc::clone(&thread), Arc::clone(&turn))?;
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
        let submission = self
            .harness
            .submit(HarnessCommand::StartTurn(ExecutorTurn {
                agent,
                thread,
                turn: Arc::clone(&turn),
                agent_config,
                thread_config,
                prepared,
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
        let harness = Arc::clone(&self.harness);
        let mut finalizers = self.finalizers.lock().await;
        while let Some(result) = finalizers.try_join_next() {
            if let Err(error) = result {
                tracing::error!(%error, "harness turn finalization panicked");
            }
        }
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
        Ok(ExecutionStreamHandle::new(UnboundedReceiverStream::new(
            event_rx,
        )))
    }
}

impl<E: Clone> Clone for ExecutorHarnessRuntime<E> {
    fn clone(&self) -> Self {
        Self {
            executor: self.executor.clone(),
            harness: Arc::clone(&self.harness),
            initialized: Arc::clone(&self.initialized),
            events: Arc::clone(&self.events),
            finalizers: Arc::clone(&self.finalizers),
            tracer: Arc::clone(&self.tracer),
            agent_config_cache: Arc::clone(&self.agent_config_cache),
            conversation_config_cache: Arc::clone(&self.conversation_config_cache),
        }
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

#[async_trait]
impl<E: HarnessExecutor> HarnessRuntime for ExecutorHarnessRuntime<E> {
    async fn get_agent_config(&self, agent: &dyn AgentHandle) -> Result<AgentConfig> {
        get_or_load_cached(
            &self.agent_config_cache,
            agent.record().id,
            AGENT_CONFIG_CACHE_NAME,
            || load_agent_config(agent),
        )
        .await
    }

    async fn put_agent_config(&self, agent: &dyn AgentHandle, config: AgentConfig) -> Result<()> {
        store_agent_config(agent, &config).await?;
        cache_agent_config(&self.agent_config_cache, agent.record().id, config);
        Ok(())
    }

    async fn get_conversation_config(
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

    async fn put_conversation_config(
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

    async fn send(
        &self,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        request: SendRequest,
    ) -> Result<SendResult> {
        let mut events = self.start_turn(agent, conversation, request, false).await?;
        while let Some(event) = events.next().await {
            if let ExecutionStreamEvent::Completed(result) = event? {
                return Ok(result);
            }
        }
        Err(anyhow!("harness stopped without completion"))
    }

    async fn send_stream(
        &self,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        request: SendRequest,
    ) -> Result<ExecutionStreamHandle> {
        self.start_turn(agent, conversation, request, true).await
    }

    async fn shutdown(&self) -> Result<()> {
        let shutdown = self.harness.shutdown().await;
        let mut finalizers = self.finalizers.lock().await;
        while let Some(result) = finalizers.join_next().await {
            result?;
        }
        let flush = self.tracer.flush().await;
        shutdown?;
        flush
    }
}
