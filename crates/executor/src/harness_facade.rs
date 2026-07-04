use std::sync::Arc;
use std::time::Duration;

use crate::{AgentConfig, ConversationConfig, ExecutionStreamHandle, SendRequest, SendResult};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, AgentRecord, BasicTurnCoordinator, ConversationHandle, ConversationLease,
    ConversationRecord, EnqueueTurnRequest, ExoHarness, NewAgentRequest, NewConversationRequest,
    PendingTurn, Result, SessionId, TurnCoordinator, TurnId,
};
use lingua::Message;

use crate::harness_helpers::{
    get_conversation_model_override, materialize_conversation_messages,
    put_conversation_model_override, resolve_agent_handle, resolve_conversation_handle,
};
use crate::harness_types::{
    CreateAgentRequest, CreateConversationRequest, Harness, HarnessAgent, HarnessConversation,
};

/// How often a waiting sender re-attempts to claim the conversation and check
/// that its turn reached the head of the queue.
const CLAIM_RETRY_INTERVAL: Duration = Duration::from_millis(50);

#[async_trait]
pub(crate) trait HarnessRuntime: Send + Sync + Clone + 'static {
    async fn get_agent_config(&self, agent: &dyn AgentHandle) -> Result<AgentConfig>;
    async fn put_agent_config(&self, agent: &dyn AgentHandle, config: AgentConfig) -> Result<()>;
    async fn get_conversation_config(
        &self,
        conversation: &dyn ConversationHandle,
    ) -> Result<ConversationConfig>;
    async fn put_conversation_config(
        &self,
        conversation: &dyn ConversationHandle,
        config: ConversationConfig,
    ) -> Result<()>;
    /// Execute a claimed pending turn: begin it through the coordinator
    /// (fenced by the lease), run the executor, and finish it. The caller
    /// completes the turn and releases the lease.
    async fn execute_pending_turn(
        &self,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        coordinator: Arc<dyn TurnCoordinator>,
        lease: ConversationLease,
        pending: PendingTurn,
    ) -> Result<SendResult>;
    /// Streaming variant. The spawned turn task owns completion: it pops the
    /// turn and releases the lease after the turn finishes.
    async fn execute_pending_turn_stream(
        &self,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        coordinator: Arc<dyn TurnCoordinator>,
        lease: ConversationLease,
        pending: PendingTurn,
    ) -> Result<ExecutionStreamHandle>;
    async fn flush_tracing(&self) -> Result<()>;
}

/// The result of a turn that already finished, reconstructed from its
/// durable `turn_ended` event. Lets a sender whose turn was executed by
/// another driver (a worker draining the queue, a deduplicated producer)
/// return the same ids the executing driver saw.
pub(crate) async fn finished_turn_result(
    conversation: &dyn ConversationHandle,
    turn_id: TurnId,
) -> Result<Option<SendResult>> {
    let events = conversation
        .get_events(Some(exoharness::EventQuery {
            turn_id: Some(turn_id),
            types: Some(vec![exoharness::EventKind::TURN_ENDED]),
            ..Default::default()
        }))
        .await?
        .events;
    let Some(ended) = events.last() else {
        return Ok(None);
    };
    Ok(Some(SendResult {
        session_id: ended
            .session_id
            .ok_or_else(|| anyhow::anyhow!("turn_ended event is missing a session id"))?,
        turn_id,
        latest_event_id: ended.id,
    }))
}

pub(crate) trait SharedHarnessBacked: Send + Sync {
    type Runtime: HarnessRuntime;

    fn shared_harness(&self) -> &SharedHarness<Self::Runtime>;
}

#[async_trait]
impl<T> Harness for T
where
    T: SharedHarnessBacked,
{
    fn exoharness_handle(&self) -> Arc<dyn ExoHarness> {
        self.shared_harness().exoharness_handle()
    }

    fn turn_coordinator(&self) -> Arc<dyn TurnCoordinator> {
        self.shared_harness().turn_coordinator()
    }

    async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        self.shared_harness().list_agents().await
    }

    async fn get_agent(&self, agent_ref: &str) -> Result<Option<Arc<dyn HarnessAgent>>> {
        self.shared_harness().get_agent(agent_ref).await
    }

    async fn create_agent(&self, request: CreateAgentRequest) -> Result<Arc<dyn HarnessAgent>> {
        self.shared_harness().create_agent(request).await
    }

    async fn delete_agent(&self, agent_ref: &str) -> Result<bool> {
        self.shared_harness().delete_agent(agent_ref).await
    }

    async fn flush_tracing(&self) -> Result<()> {
        self.shared_harness().flush_tracing().await
    }
}

pub(crate) struct SharedHarness<R> {
    exoharness: Arc<dyn ExoHarness>,
    runtime: R,
    coordinator: Arc<dyn TurnCoordinator>,
}

impl<R> SharedHarness<R>
where
    R: HarnessRuntime,
{
    pub(crate) fn new(exoharness: Arc<dyn ExoHarness>, runtime: R) -> Self {
        // Prefer the substrate's own coordinator (durable backends share it
        // across every process on the same store). Backends without one get
        // process-local coordination, matching their pre-coordinator scope.
        let coordinator: Arc<dyn TurnCoordinator> =
            exoharness.turn_coordinator().unwrap_or_else(|| {
                Arc::new(BasicTurnCoordinator::in_memory(
                    Arc::clone(&exoharness),
                    exoharness::DEFAULT_LEASE_TTL,
                ))
            });
        Self {
            exoharness,
            runtime,
            coordinator,
        }
    }

    pub(crate) fn exoharness_handle(&self) -> Arc<dyn ExoHarness> {
        Arc::clone(&self.exoharness)
    }

    pub(crate) fn turn_coordinator(&self) -> Arc<dyn TurnCoordinator> {
        Arc::clone(&self.coordinator)
    }

    pub(crate) async fn list_agents(&self) -> Result<Vec<AgentRecord>> {
        let agents = self.exoharness.list_agents().await?;
        Ok(agents
            .into_iter()
            .map(|agent| agent.record().clone())
            .collect())
    }

    pub(crate) async fn get_agent(&self, agent_ref: &str) -> Result<Option<Arc<dyn HarnessAgent>>> {
        let Some(agent) = resolve_agent_handle(self.exoharness.as_ref(), agent_ref).await? else {
            return Ok(None);
        };
        Ok(Some(self.wrap_agent(agent)))
    }

    pub(crate) async fn create_agent(
        &self,
        request: CreateAgentRequest,
    ) -> Result<Arc<dyn HarnessAgent>> {
        let name = request.name.clone().unwrap_or_else(|| request.slug.clone());
        let config = AgentConfig {
            instructions: Vec::new(),
            harness: request.harness,
            typescript: request.typescript,
            enable_agent_tool_creation: request.enable_agent_tool_creation,
            sandbox_image: request.sandbox_image,
            sandbox_provider: request.sandbox_provider,
            enable_networking: request.enable_networking,
            model: request.model,
            max_output_tokens: request.max_output_tokens,
            max_tool_round_trips: request.max_tool_round_trips,
            braintrust: request.braintrust,
        };
        let agent = self
            .exoharness
            .new_agent(NewAgentRequest {
                slug: request.slug,
                name,
            })
            .await?;
        self.runtime
            .put_agent_config(agent.as_ref(), config)
            .await?;
        Ok(self.wrap_agent(agent))
    }

    pub(crate) async fn delete_agent(&self, agent_ref: &str) -> Result<bool> {
        let Some(agent) = resolve_agent_handle(self.exoharness.as_ref(), agent_ref).await? else {
            return Ok(false);
        };
        let deleted = self.exoharness.delete_agent(&agent.record().id).await?;
        Ok(deleted)
    }

    pub(crate) async fn flush_tracing(&self) -> Result<()> {
        self.runtime.flush_tracing().await
    }

    fn wrap_agent(&self, agent: Arc<dyn AgentHandle>) -> Arc<dyn HarnessAgent> {
        Arc::new(SharedHarnessAgent {
            agent,
            runtime: self.runtime.clone(),
            coordinator: Arc::clone(&self.coordinator),
        })
    }
}

struct SharedHarnessAgent<R> {
    agent: Arc<dyn AgentHandle>,
    runtime: R,
    coordinator: Arc<dyn TurnCoordinator>,
}

#[async_trait]
impl<R> HarnessAgent for SharedHarnessAgent<R>
where
    R: HarnessRuntime,
{
    fn record(&self) -> &AgentRecord {
        self.agent.record()
    }

    fn exoharness_handle(&self) -> Arc<dyn AgentHandle> {
        Arc::clone(&self.agent)
    }

    async fn config(&self) -> Result<AgentConfig> {
        self.runtime.get_agent_config(self.agent.as_ref()).await
    }

    async fn put_config(&self, config: AgentConfig) -> Result<()> {
        self.runtime
            .put_agent_config(self.agent.as_ref(), config)
            .await
    }

    async fn list_conversations(&self) -> Result<Vec<ConversationRecord>> {
        let conversations = self
            .agent
            .list_conversations(exoharness::ListConversationsRequest::default())
            .await?
            .conversations;
        Ok(conversations
            .into_iter()
            .map(|conversation| conversation.record().clone())
            .collect())
    }

    async fn get_conversation(
        &self,
        conversation_ref: &str,
    ) -> Result<Option<Arc<dyn HarnessConversation>>> {
        let Some(conversation) =
            resolve_conversation_handle(self.agent.as_ref(), conversation_ref).await?
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(SharedHarnessConversation {
            agent: Arc::clone(&self.agent),
            conversation,
            runtime: self.runtime.clone(),
            coordinator: Arc::clone(&self.coordinator),
        })))
    }

    async fn create_conversation(
        &self,
        request: CreateConversationRequest,
    ) -> Result<Arc<dyn HarnessConversation>> {
        let agent_config = self.config().await?;
        let conversation = self
            .agent
            .new_conversation(NewConversationRequest {
                slug: request.slug,
                name: request.name,
            })
            .await?;
        let default_conversation_config = ConversationConfig::default();
        let conversation_config = ConversationConfig {
            sandbox_image: request.sandbox_image.or(agent_config.sandbox_image),
            sandbox_provider: Some(
                request
                    .sandbox_provider
                    .unwrap_or(agent_config.sandbox_provider),
            ),
            shell_program: request
                .shell_program
                .or(default_conversation_config.shell_program),
            mounts: default_conversation_config.mounts,
            durable_file_systems: default_conversation_config.durable_file_systems,
            sandbox_scope: default_conversation_config.sandbox_scope,
        };
        self.runtime
            .put_conversation_config(conversation.as_ref(), conversation_config)
            .await?;
        Ok(Arc::new(SharedHarnessConversation {
            agent: Arc::clone(&self.agent),
            conversation,
            runtime: self.runtime.clone(),
            coordinator: Arc::clone(&self.coordinator),
        }))
    }

    async fn delete_conversation(&self, conversation_ref: &str) -> Result<bool> {
        let Some(conversation) =
            resolve_conversation_handle(self.agent.as_ref(), conversation_ref).await?
        else {
            return Ok(false);
        };
        let deleted = self
            .agent
            .delete_conversation(&conversation.record().id)
            .await?;
        Ok(deleted)
    }
}

struct SharedHarnessConversation<R> {
    agent: Arc<dyn AgentHandle>,
    conversation: Arc<dyn ConversationHandle>,
    runtime: R,
    coordinator: Arc<dyn TurnCoordinator>,
}

impl<R> SharedHarnessConversation<R>
where
    R: HarnessRuntime,
{
    /// Execute the head pending turn under a held lease and complete it,
    /// even when execution fails, so an errored turn never wedges the queue.
    /// Returns the head's result, or `None` when the queue is empty (or its
    /// head is not yet eligible).
    async fn execute_head_under_lease(
        &self,
        lease: &ConversationLease,
    ) -> Result<Option<SendResult>> {
        let Some(head) = self.coordinator.peek_turn(lease).await? else {
            return Ok(None);
        };
        let head_id = head.id;
        let result = self
            .runtime
            .execute_pending_turn(
                Arc::clone(&self.agent),
                Arc::clone(&self.conversation),
                Arc::clone(&self.coordinator),
                lease.clone(),
                head,
            )
            .await;
        if let Err(error) = self.coordinator.complete_turn(lease, head_id).await {
            tracing::error!(%error, turn_id = %head_id, "failed to complete queued turn");
        }
        result.map(Some)
    }

    /// Drive head turns under the lease until `turn_id` executes. Foreign
    /// heads (turns enqueued by producers that died, or by producers that
    /// delegate driving) are executed and completed along the way; their
    /// errors are logged, not attributed to this sender.
    async fn drive_until(
        &self,
        lease: &ConversationLease,
        turn_id: TurnId,
    ) -> Result<Option<SendResult>> {
        loop {
            let Some(head) = self.coordinator.peek_turn(lease).await? else {
                return Ok(None);
            };
            if head.id == turn_id {
                return self.execute_head_under_lease(lease).await;
            }
            match self.execute_head_under_lease(lease).await {
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        %error,
                        turn_id = %head.id,
                        "queued turn failed while draining the conversation queue"
                    );
                }
            }
        }
    }
}

#[async_trait]
impl<R> HarnessConversation for SharedHarnessConversation<R>
where
    R: HarnessRuntime,
{
    fn record(&self) -> &ConversationRecord {
        self.conversation.record()
    }

    fn exoharness_handle(&self) -> Arc<dyn ConversationHandle> {
        Arc::clone(&self.conversation)
    }

    async fn config(&self) -> Result<ConversationConfig> {
        self.runtime
            .get_conversation_config(self.conversation.as_ref())
            .await
    }

    async fn put_config(&self, config: ConversationConfig) -> Result<()> {
        self.runtime
            .put_conversation_config(self.conversation.as_ref(), config)
            .await
    }

    async fn model_override(&self) -> Result<Option<crate::ConversationModelConfig>> {
        get_conversation_model_override(self.conversation.as_ref()).await
    }

    async fn put_model_override(
        &self,
        config: Option<crate::ConversationModelConfig>,
    ) -> Result<()> {
        put_conversation_model_override(self.conversation.as_ref(), config).await
    }

    async fn messages(&self) -> Result<Vec<Message>> {
        materialize_conversation_messages(self.conversation.as_ref()).await
    }

    async fn close_session(&self, session_id: SessionId) -> Result<()> {
        self.conversation.end_session(session_id).await
    }

    async fn send(&self, request: SendRequest) -> Result<SendResult> {
        let conversation_id = self.conversation.record().id;
        let enqueued = self
            .coordinator
            .enqueue_turn(
                conversation_id,
                EnqueueTurnRequest {
                    input: request.input,
                    session_id: request.session_id,
                    ..Default::default()
                },
            )
            .await?;
        let turn_id = enqueued.turn.id;
        loop {
            // Another driver (a worker draining the queue) may have executed
            // this turn already.
            if let Some(result) = finished_turn_result(self.conversation.as_ref(), turn_id).await? {
                return Ok(result);
            }
            let Some(lease) = self.coordinator.claim_conversation(conversation_id).await? else {
                tokio::time::sleep(CLAIM_RETRY_INTERVAL).await;
                continue;
            };
            let outcome = self.drive_until(&lease, turn_id).await;
            if let Err(error) = self.coordinator.release_idle(&lease).await {
                tracing::error!(%error, %conversation_id, "failed to release conversation lease");
            }
            match outcome? {
                Some(result) => return Ok(result),
                // The queue drained without reaching this turn (it finished
                // elsewhere, or the head is delayed): loop and re-check.
                None => tokio::time::sleep(CLAIM_RETRY_INTERVAL).await,
            }
        }
    }

    async fn send_stream(&self, request: SendRequest) -> Result<ExecutionStreamHandle> {
        let conversation_id = self.conversation.record().id;
        let enqueued = self
            .coordinator
            .enqueue_turn(
                conversation_id,
                EnqueueTurnRequest {
                    input: request.input,
                    session_id: request.session_id,
                    ..Default::default()
                },
            )
            .await?;
        let turn_id = enqueued.turn.id;
        let lease = loop {
            if let Some(result) = finished_turn_result(self.conversation.as_ref(), turn_id).await? {
                // A queue worker executed this turn before we could stream
                // it; degrade to a completion-only stream.
                return Ok(completed_only_stream(result));
            }
            let Some(lease) = self.coordinator.claim_conversation(conversation_id).await? else {
                tokio::time::sleep(CLAIM_RETRY_INTERVAL).await;
                continue;
            };
            // Drain foreign heads (non-streaming) until this turn is next.
            let ready = loop {
                match self.coordinator.peek_turn(&lease).await {
                    Ok(Some(head)) if head.id == turn_id => break Ok(true),
                    Ok(Some(_)) => {
                        if let Err(error) = self.execute_head_under_lease(&lease).await {
                            tracing::warn!(
                                %error,
                                "queued turn failed while draining the conversation queue"
                            );
                        }
                    }
                    Ok(None) => break Ok(false),
                    Err(error) => break Err(error),
                }
            };
            match ready {
                Ok(true) => break lease,
                Ok(false) => {
                    // Turn finished elsewhere or the head is delayed; release
                    // and re-check from the top.
                    if let Err(error) = self.coordinator.release_idle(&lease).await {
                        tracing::error!(%error, %conversation_id, "failed to release conversation lease");
                    }
                    tokio::time::sleep(CLAIM_RETRY_INTERVAL).await;
                }
                Err(error) => {
                    let _ = self.coordinator.release_idle(&lease).await;
                    return Err(error);
                }
            }
        };
        let stream = self
            .runtime
            .execute_pending_turn_stream(
                Arc::clone(&self.agent),
                Arc::clone(&self.conversation),
                Arc::clone(&self.coordinator),
                lease.clone(),
                enqueued.turn,
            )
            .await;
        match stream {
            Ok(stream) => Ok(stream),
            Err(error) => {
                // Setup failed before the spawned turn task took ownership of
                // the lease: clean up here so the queue is not wedged.
                if let Err(error) = self.coordinator.complete_turn(&lease, turn_id).await {
                    tracing::error!(%error, %turn_id, "failed to complete queued turn");
                }
                if let Err(error) = self.coordinator.release_idle(&lease).await {
                    tracing::error!(%error, %conversation_id, "failed to release conversation lease");
                }
                Err(error)
            }
        }
    }

    async fn run_next_pending_turn(&self) -> Result<Option<SendResult>> {
        let conversation_id = self.conversation.record().id;
        let Some(lease) = self.coordinator.claim_conversation(conversation_id).await? else {
            return Ok(None);
        };
        let result = self.execute_head_under_lease(&lease).await;
        if let Err(error) = self.coordinator.release_idle(&lease).await {
            tracing::error!(%error, %conversation_id, "failed to release conversation lease");
        }
        result
    }
}

/// A stream that only reports completion, for the rare case where a queue
/// worker executed a streaming sender's turn before it could attach.
fn completed_only_stream(result: SendResult) -> ExecutionStreamHandle {
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = event_tx.send(Ok(crate::ExecutionStreamEvent::Completed(result)));
    ExecutionStreamHandle::new(tokio_stream::wrappers::UnboundedReceiverStream::new(
        event_rx,
    ))
}
