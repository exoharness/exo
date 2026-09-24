use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use exo_managed_agents::AgentBackend;
use exoharness::{AgentHandle, ExoHarness, Result, ThreadHandle, TurnRecord};
use tokio::sync::oneshot;

use crate::harness::{Harness, HarnessCommand};
use crate::harness_adapter::{ExecutorHarness, ExecutorTurn};
use crate::harness_executor::HarnessExecutor;
use crate::{
    AgentConfig, BasicExecutor, ExecutionStreamHandle, ModelClient, Runtime, SendRequest,
    ToolRuntime,
};

#[async_trait]
pub trait Provider: AgentBackend {
    fn with_caller(&self, _caller: exoharness::access::Caller) -> Result<Arc<dyn Provider>> {
        anyhow::bail!("this provider does not support caller-scoped execution")
    }

    fn harness(&self) -> &dyn Harness<ProviderTurn>;

    async fn is_turn_active(
        &self,
        thread: &dyn ThreadHandle,
        turn: exoharness::TurnId,
    ) -> Result<bool>;

    async fn approval_response(
        &self,
        _agent: exoharness::AgentId,
        _thread: exoharness::ThreadId,
        _turn: exoharness::TurnId,
        _body: &exo_managed_agents::http::protocol::ApprovalResponseBody,
    ) -> Result<exoharness::EventId> {
        anyhow::bail!("this provider does not support tool approvals")
    }
}

pub struct ProviderTurn {
    pub agent: Arc<dyn AgentHandle>,
    pub thread: Arc<dyn ThreadHandle>,
    pub request: SendRequest,
    pub streaming: bool,
    pub config_override: Option<AgentConfig>,
    pub receipt: oneshot::Sender<(TurnRecord, ExecutionStreamHandle)>,
    pub(crate) runtime: Runtime,
}

pub struct LocalProvider {
    pub(crate) state: Arc<dyn ExoHarness>,
    pub(crate) executor: Arc<dyn HarnessExecutor>,
    pub(crate) harness: Arc<ExecutorHarness>,
    pub(crate) managed: crate::managed_agents::LocalAgentSetup,
    // One provider-wide lock serializes the pending check and decision write so
    // concurrent responses cannot accept the same approval twice.
    approval_responses: tokio::sync::Mutex<()>,
    pub(crate) live_turns:
        Arc<tokio::sync::RwLock<HashMap<crate::harness::HarnessTurnKey, Weak<()>>>>,
}

impl LocalProvider {
    pub(crate) fn new(state: Arc<dyn ExoHarness>, executor: Arc<dyn HarnessExecutor>) -> Self {
        Self {
            state,
            harness: Arc::new(ExecutorHarness::new(Arc::clone(&executor))),
            executor,
            managed: Default::default(),
            approval_responses: Default::default(),
            live_turns: Arc::default(),
        }
    }

    pub fn basic<M: ModelClient + 'static, T: ToolRuntime + 'static>(
        state: Arc<dyn ExoHarness>,
        model: Arc<M>,
        tools: Arc<T>,
        pricing: Arc<cost::PricingTable>,
    ) -> Self {
        Self::new(
            state,
            Arc::new(BasicExecutor::with_pricing(model, tools, pricing)),
        )
    }

    pub fn rlm<M: ModelClient + 'static>(
        state: Arc<dyn ExoHarness>,
        model: Arc<M>,
        tools: Arc<dyn ToolRuntime>,
    ) -> Self {
        Self::new(state, Arc::new(crate::rlm::RlmExecutor { model, tools }))
    }

    pub fn typescript<T: ToolRuntime + 'static>(
        state: Arc<dyn ExoHarness>,
        workspace: PathBuf,
        env: HashMap<String, String>,
        tools: Arc<T>,
    ) -> Self {
        let executor =
            crate::typescript::TypeScriptExecutor::new(Arc::clone(&state), workspace, env, tools);
        Self::new(state, Arc::new(executor))
    }
}

#[async_trait]
impl Provider for LocalProvider {
    fn with_caller(&self, caller: exoharness::access::Caller) -> Result<Arc<dyn Provider>> {
        let state = self.state.with_caller(caller)?;
        let executor = self.executor.with_state(state.clone())?;
        let mut provider = Self::new(state, executor).with_managed_agents(self.managed.clone());
        provider.live_turns = self.live_turns.clone();
        Ok(Arc::new(provider))
    }

    async fn is_turn_active(
        &self,
        thread: &dyn ThreadHandle,
        turn: exoharness::TurnId,
    ) -> Result<bool> {
        Ok(self
            .live_turns
            .read()
            .await
            .get(&crate::harness::HarnessTurnKey::new(
                thread.record().id,
                turn,
            ))
            .is_some_and(|turn| turn.strong_count() > 0))
    }

    async fn approval_response(
        &self,
        agent: exoharness::AgentId,
        thread: exoharness::ThreadId,
        turn: exoharness::TurnId,
        body: &exo_managed_agents::http::protocol::ApprovalResponseBody,
    ) -> Result<exoharness::EventId> {
        use anyhow::Context;
        if let Some(caller) = self.state.caller() {
            let agent = self
                .state
                .get_agent(&agent)
                .await?
                .context("agent not found")?;
            let thread = agent
                .get_thread(&thread)
                .await?
                .context("thread not found")?;
            anyhow::ensure!(
                crate::permissions::turn_caller(thread.as_ref(), turn)
                    .await?
                    .as_deref()
                    == Some(caller.principal.as_str()),
                "only the caller who started this turn may approve it"
            );
        }
        let _guard = self.approval_responses.lock().await;
        anyhow::ensure!(
            self.harness
                .is_active(crate::harness::HarnessTurnKey::new(thread, turn)),
            "turn is not active"
        );
        let agent = self
            .state
            .get_agent(&agent)
            .await?
            .context("agent not found")?;
        let thread = agent
            .get_thread(&thread)
            .await?
            .context("thread not found")?;
        crate::permissions::respond(
            thread.as_ref(),
            exoharness::TurnRecord {
                id: turn,
                session_id: body.session_id,
            },
            body,
        )
        .await
    }

    fn harness(&self) -> &dyn Harness<ProviderTurn> {
        self
    }
}

#[async_trait]
impl Harness<ProviderTurn> for LocalProvider {
    fn name(&self) -> &'static str {
        self.harness.name()
    }

    async fn shutdown(&self) -> Result<()> {
        self.harness.shutdown().await
    }

    async fn submit(&self, command: HarnessCommand<ProviderTurn>) -> Result<()> {
        match command {
            HarnessCommand::StartTurn(work) => {
                let result = work
                    .runtime
                    .start_local_turn(
                        self,
                        work.agent,
                        work.thread,
                        work.request,
                        work.streaming,
                        work.config_override,
                    )
                    .await?;
                if work.receipt.send(result).is_err() {
                    tracing::debug!("local turn receipt receiver disconnected");
                }
                Ok(())
            }
            HarnessCommand::CancelTurn { key } => {
                self.harness
                    .submit(HarnessCommand::<ExecutorTurn>::CancelTurn { key })
                    .await
            }
        }
    }
}
