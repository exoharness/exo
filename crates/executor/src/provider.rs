use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use crate::{BasicExecutor, ModelClient, ToolRuntime};
use async_trait::async_trait;
use exo_managed_agents::AgentBackend;
use exoharness::{AgentHandle, ExoHarness, Result, ThreadHandle, TurnRecord};
use tokio::sync::oneshot;

use crate::harness::{Harness, HarnessCommand};
use crate::harness_adapter::ExecutorHarness;
use crate::harness_executor::HarnessExecutor;
use crate::runtime_host::{RuntimeHost, TaskGroup};
use crate::{AgentConfig, ExecutionStreamHandle, Runtime, SendRequest};

#[async_trait]
pub trait Provider: AgentBackend {
    fn runtime_host(&self) -> Arc<dyn RuntimeHost>;

    fn with_caller(&self, _caller: exoharness::access::Caller) -> Result<Arc<dyn Provider>> {
        anyhow::bail!("this provider does not support caller-scoped execution")
    }

    fn harness(&self) -> &dyn Harness<ProviderTurn>;
    async fn cancel_turn(
        &self,
        runtime: &Runtime,
        key: crate::harness::HarnessTurnKey,
    ) -> Result<bool>;
    async fn suspend_turn(
        &self,
        _runtime: &Runtime,
        _key: crate::harness::HarnessTurnKey,
    ) -> Result<bool> {
        anyhow::bail!("this provider does not support turn suspension")
    }
    async fn resume_suspended_turn(
        &self,
        _runtime: &Runtime,
        _key: crate::harness::HarnessTurnKey,
    ) -> Result<ExecutionStreamHandle> {
        anyhow::bail!("this provider does not support turn resumption")
    }
    async fn execute_turn(
        &self,
        _runtime: &Runtime,
        _agent: Arc<dyn AgentHandle>,
        _thread: Arc<dyn ThreadHandle>,
        _turn: TurnRecord,
        _work: crate::TurnWork,
        _recovering: bool,
    ) -> Result<ExecutionStreamHandle> {
        anyhow::bail!("this provider does not execute accepted queue entries")
    }

    async fn recover_unfinished_turns(&self, _runtime: Runtime) -> Result<()> {
        Ok(())
    }

    async fn is_turn_active(&self, key: crate::harness::HarnessTurnKey) -> Result<bool>;

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
    pub options: crate::TurnOptions,
    pub agent: Arc<dyn AgentHandle>,
    pub thread: Arc<dyn ThreadHandle>,
    pub request: SendRequest,
    pub streaming: bool,
    pub config_override: Option<AgentConfig>,
    pub receipt: oneshot::Sender<(TurnRecord, ExecutionStreamHandle)>,
    pub(crate) runtime: Runtime,
}

#[derive(Clone)]
pub struct LocalProvider {
    pub(crate) host: Arc<dyn RuntimeHost>,
    pub(crate) state: Arc<dyn ExoHarness>,
    pub(crate) executor: Arc<dyn HarnessExecutor>,
    pub(crate) harness: Arc<ExecutorHarness>,
    pub(crate) managed: crate::managed_agents::LocalAgentSetup,
    pub(crate) resource_preparations: Arc<Mutex<TaskGroup>>,
    // One provider-wide lock serializes the pending check and decision write so
    // concurrent responses cannot accept the same approval twice.
    approval_responses: Arc<tokio::sync::Mutex<()>>,
    pub(crate) live_turns:
        Arc<tokio::sync::RwLock<HashMap<crate::harness::HarnessTurnKey, Weak<()>>>>,
    pub(crate) turns: Arc<crate::turn_queue::TurnQueueRuntime>,
}

impl LocalProvider {
    /// Use an in-memory turn queue. To recover across process restarts, supply
    /// a persistent coordinator with `with_turn_coordinator()`.
    pub fn with_host(
        state: Arc<dyn ExoHarness>,
        executor: Arc<dyn HarnessExecutor>,
        host: Arc<dyn RuntimeHost>,
    ) -> Self {
        Self {
            state,
            harness: Arc::new(ExecutorHarness::new(Arc::clone(&executor), host.clone())),
            executor,
            managed: Default::default(),
            resource_preparations: Arc::new(Mutex::new(TaskGroup::new(host.clone()))),
            host: host.clone(),
            approval_responses: Default::default(),
            live_turns: Arc::default(),
            turns: {
                let coordinator =
                    Arc::new(exoharness::turn_coordinator::StoredTurnCoordinator::in_memory());
                Arc::new(crate::turn_queue::TurnQueueRuntime::new(
                    host.clone(),
                    coordinator,
                ))
            },
        }
    }

    pub fn basic_with_host<M: ModelClient + 'static, T: ToolRuntime + 'static>(
        state: Arc<dyn ExoHarness>,
        model: Arc<M>,
        tools: Arc<T>,
        pricing: Arc<cost::PricingTable>,
        host: Arc<dyn RuntimeHost>,
    ) -> Self {
        Self::with_host(
            state,
            Arc::new(BasicExecutor::with_pricing(model, tools, pricing)),
            host,
        )
    }
}

#[async_trait]
impl Provider for LocalProvider {
    async fn execute_turn(
        &self,
        runtime: &Runtime,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ThreadHandle>,
        turn: TurnRecord,
        work: crate::TurnWork,
        recovering: bool,
    ) -> Result<ExecutionStreamHandle> {
        runtime
            .start_local_turn(self, agent, thread, turn, work, recovering)
            .await
    }
    async fn cancel_turn(
        &self,
        runtime: &Runtime,
        key: crate::harness::HarnessTurnKey,
    ) -> Result<bool> {
        runtime
            .control_local_turn(self, key, exoharness::turn_coordinator::TurnControl::Cancel)
            .await
    }
    async fn suspend_turn(
        &self,
        runtime: &Runtime,
        key: crate::harness::HarnessTurnKey,
    ) -> Result<bool> {
        runtime
            .control_local_turn(
                self,
                key,
                exoharness::turn_coordinator::TurnControl::Suspend,
            )
            .await
    }
    async fn resume_suspended_turn(
        &self,
        runtime: &Runtime,
        key: crate::harness::HarnessTurnKey,
    ) -> Result<ExecutionStreamHandle> {
        runtime.resume_local_turn(self, key).await
    }
    fn runtime_host(&self) -> Arc<dyn RuntimeHost> {
        self.host.clone()
    }

    async fn recover_unfinished_turns(&self, runtime: Runtime) -> Result<()> {
        runtime.recover_local_turns(self).await
    }

    fn with_caller(&self, caller: exoharness::access::Caller) -> Result<Arc<dyn Provider>> {
        let state = self.state.with_caller(caller)?;
        let executor = self.executor.with_state(state.clone())?;
        let mut provider = Self::with_host(state, executor, self.host.clone())
            .with_managed_agents(self.managed.clone());
        provider.live_turns = self.live_turns.clone();
        provider.resource_preparations = self.resource_preparations.clone();
        provider.turns = self.turns.clone();
        Ok(Arc::new(provider))
    }

    async fn is_turn_active(&self, key: crate::harness::HarnessTurnKey) -> Result<bool> {
        Ok(self
            .live_turns
            .read()
            .await
            .get(&key)
            .is_some_and(|turn_state| turn_state.strong_count() > 0))
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
                .is_active(crate::harness::HarnessTurnKey::new(agent, thread, turn)),
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
        if self.state.caller().is_none() {
            let _admission = self.turns.admission.write().await;
            let _active = self
                .turns
                .active
                .lock()
                .expect("active queue owners poisoned");
            self.turns
                .draining
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let mut preparations = std::mem::replace(
            &mut *self
                .resource_preparations
                .lock()
                .expect("resource preparations poisoned"),
            TaskGroup::new(self.host.clone()),
        );
        while let Some(result) = preparations.join_next().await {
            result?;
        }
        if self.state.caller().is_none() {
            self.turns.shutdown(self.harness.clone()).await
        } else {
            self.harness.shutdown().await
        }
    }

    async fn submit(&self, command: HarnessCommand<ProviderTurn>) -> Result<()> {
        match command {
            HarnessCommand::StartTurn(work) => {
                let result = work
                    .runtime
                    .accept_local_turn(
                        self,
                        work.agent,
                        work.thread,
                        work.request,
                        work.streaming,
                        work.config_override,
                        work.options,
                    )
                    .await?;
                if work.receipt.send(result).is_err() {
                    tracing::debug!("local turn receipt receiver disconnected");
                }
                Ok(())
            }
            HarnessCommand::CancelTurn { .. }
            | HarnessCommand::SuspendTurn { .. }
            | HarnessCommand::ResumeTurn { .. } => {
                anyhow::bail!("use the Runtime API to control queued turns")
            }
        }
    }
}
