use std::{collections::HashMap, path::PathBuf, sync::Arc};

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
    fn harness(&self) -> &dyn Harness<ProviderTurn>;

    fn temporary(&self, _state: Arc<dyn ExoHarness>) -> Result<Runtime> {
        anyhow::bail!("this provider does not support temporary agents")
    }

    async fn cleanup(&self) -> Result<()> {
        Ok(())
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
}

impl LocalProvider {
    pub(crate) fn new(state: Arc<dyn ExoHarness>, executor: Arc<dyn HarnessExecutor>) -> Self {
        Self {
            state,
            harness: Arc::new(ExecutorHarness::new(Arc::clone(&executor))),
            executor,
            managed: Default::default(),
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
    fn temporary(&self, state: Arc<dyn ExoHarness>) -> Result<Runtime> {
        let executor = self.executor.fork(state.clone())?;
        Ok(Runtime::new(
            Self::new(state, executor).with_managed_agents(
                crate::managed_agents::LocalAgentSetup {
                    temporary: true,
                    ..Default::default()
                },
            ),
            None,
        ))
    }

    fn harness(&self) -> &dyn Harness<ProviderTurn> {
        self
    }

    async fn cleanup(&self) -> Result<()> {
        if self.managed.temporary {
            for agent in self.state.list_agents().await? {
                self.state.delete_agent(&agent.record().id).await?;
            }
        }
        Ok(())
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
