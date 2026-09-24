use super::{PreparedMcp, config, connect_mcp};
use crate::execution_tracing::TurnExecutionTrace;
use crate::harness_executor::{ExecutorStreamMode, HarnessExecutor};
use crate::{
    AgentConfig, AgentHarnessKind, BasicExecutor, BasicToolRuntime, ConversationConfig,
    ExoToolRuntime, McpToolRuntime, RouterModelClient, SendRequest,
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, BasicExoHarnessConfig, ExoHarness, ThreadHandle, ThreadId, TurnHandle,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::OnceCell;

type ThreadExecutors = HashMap<ThreadId, Arc<OnceCell<ThreadExecutor>>>;

pub(crate) struct ManagedExecutor {
    state: Arc<dyn ExoHarness>,
    config: BasicExoHarnessConfig,
    env: HashMap<String, String>,
    pricing: Arc<cost::PricingTable>,
    workspace: PathBuf,
    threads: Mutex<ThreadExecutors>,
}

struct ThreadExecutor {
    executor: Arc<dyn HarnessExecutor>,
    mcp: PreparedMcp,
    harness: AgentHarnessKind,
}

impl ManagedExecutor {
    pub(crate) fn new(
        state: Arc<dyn ExoHarness>,
        config: BasicExoHarnessConfig,
        env: HashMap<String, String>,
        pricing: Arc<cost::PricingTable>,
    ) -> Result<Self> {
        Ok(Self {
            state,
            config,
            env,
            pricing,
            workspace: config::installation()?,
            threads: Mutex::default(),
        })
    }

    async fn thread(
        &self,
        agent: &dyn AgentHandle,
        thread: &dyn ThreadHandle,
        config: &AgentConfig,
        thread_config: &ConversationConfig,
    ) -> Result<Arc<OnceCell<ThreadExecutor>>> {
        let cell = self
            .threads
            .lock()
            .expect("managed executors poisoned")
            .entry(thread.record().id)
            .or_default()
            .clone();
        cell.get_or_try_init(|| async {
            let mcp = connect_mcp(agent, thread, &self.config).await?;
            mcp.configure_thread(config, thread, thread_config).await?;
            let tools = Arc::new(McpToolRuntime::new(BasicToolRuntime, mcp.tools.clone()));
            let model = Arc::new(RouterModelClient::new(self.env.clone()));
            let executor: Arc<dyn HarnessExecutor> = match config.harness {
                AgentHarnessKind::Basic => Arc::new(BasicExecutor::with_pricing(
                    model,
                    tools,
                    self.pricing.clone(),
                )),
                AgentHarnessKind::Rlm => Arc::new(crate::rlm::RlmExecutor { model, tools }),
                AgentHarnessKind::TypeScript => {
                    Arc::new(crate::typescript::TypeScriptExecutor::new(
                        self.state.clone(),
                        config::installation()?,
                        self.env.clone(),
                        tools,
                    ))
                }
                AgentHarnessKind::Exo => {
                    let root = self
                        .config
                        .root
                        .parent()
                        .context("Exo storage root has no parent")?;
                    let tools = ExoToolRuntime::from_root(root)?;
                    Arc::new(crate::typescript::TypeScriptExecutor::new(
                        self.state.clone(),
                        self.workspace.clone(),
                        self.env.clone(),
                        Arc::new(McpToolRuntime::new(tools, mcp.tools.clone())),
                    ))
                }
            };
            Ok::<_, anyhow::Error>(ThreadExecutor {
                executor,
                mcp,
                harness: config.harness,
            })
        })
        .await?;
        let prepared = cell.get().context("managed executor was not initialized")?;
        ensure!(
            prepared.harness == config.harness,
            "thread harness changed; start a new thread"
        );
        let definition = exo_managed_agents::load_definition(agent).await?;
        let servers = match definition {
            Some(definition) => definition.resolve_mcp_servers(&()).await?,
            None => vec![],
        };
        ensure!(
            prepared.mcp.servers == servers,
            "MCP configuration changed; start a new thread"
        );
        prepared.mcp.validate_thread(config, thread_config)?;
        Ok(cell)
    }
}

#[async_trait]
impl HarnessExecutor for ManagedExecutor {
    fn name(&self) -> &'static str {
        "local"
    }

    fn agent_config(
        &self,
        definition: &exo_managed_agents::AgentDefinition,
    ) -> Result<AgentConfig> {
        let config =
            config::agent_config(definition, self.config.sandbox_default.clone(), None, None)?;
        ensure!(
            self.config
                .sandbox_backends
                .iter()
                .any(|backend| backend.provider() == config.sandbox.provider),
            "sandbox provider {:?} in the agent spec is not supported by this harness",
            config.sandbox.provider
        );
        Ok(config)
    }

    async fn configure_managed_thread(
        &self,
        agent: &dyn AgentHandle,
        thread: &dyn ThreadHandle,
        agent_config: &AgentConfig,
        thread_config: &ConversationConfig,
    ) -> Result<usize> {
        let cell = self
            .thread(agent, thread, agent_config, thread_config)
            .await?;
        let prepared = cell.get().context("managed executor was not initialized")?;
        Ok(prepared.mcp.tools.tools().len())
    }

    async fn prepare_conversation(
        &self,
        agent: &dyn AgentHandle,
        thread: &dyn ThreadHandle,
        agent_config: &AgentConfig,
        thread_config: &ConversationConfig,
    ) -> Result<()> {
        let cell = self
            .thread(agent, thread, agent_config, thread_config)
            .await?;
        cell.get()
            .context("managed executor was not initialized")?
            .executor
            .prepare_conversation(agent, thread, agent_config, thread_config)
            .await
    }

    async fn execute_turn(
        &self,
        agent: &dyn AgentHandle,
        thread: Arc<dyn ThreadHandle>,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        thread_config: &ConversationConfig,
        request: &SendRequest,
        stream: ExecutorStreamMode<'_>,
        trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        let cell = self
            .threads
            .lock()
            .expect("managed executors poisoned")
            .get(&thread.record().id)
            .cloned()
            .context("thread executor was not prepared")?;
        cell.get()
            .context("managed executor was not initialized")?
            .executor
            .execute_turn(
                agent,
                thread,
                turn,
                agent_config,
                thread_config,
                request,
                stream,
                trace,
            )
            .await
    }

    async fn cancel_turn(&self, thread: &dyn ThreadHandle, config: &AgentConfig) -> Result<()> {
        let cell = self
            .threads
            .lock()
            .expect("managed executors poisoned")
            .get(&thread.record().id)
            .cloned();
        if let Some(cell) = cell
            && let Some(prepared) = cell.get()
        {
            prepared.executor.cancel_turn(thread, config).await?;
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        let threads =
            std::mem::take(&mut *self.threads.lock().expect("managed executors poisoned"));
        let results = futures::future::join_all(threads.into_values().map(|cell| async move {
            if let Some(prepared) = cell.get() {
                let stopped = prepared.executor.shutdown().await;
                let closed = prepared.mcp.tools.close().await;
                stopped?;
                closed?;
            }
            Ok::<_, anyhow::Error>(())
        }))
        .await;
        for result in results {
            result?;
        }
        Ok(())
    }
}
