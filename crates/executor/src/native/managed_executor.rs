use crate::execution_tracing::TurnExecutionTrace;
use crate::harness_executor::{ExecutorStreamMode, HarnessExecutor};
use crate::managed_mcp::{PreparedMcp, connect_mcp};
use crate::native::config;
use crate::{
    AgentConfig, AgentHarnessKind, BasicExecutor, BasicToolRuntime, ConversationConfig,
    ExoToolRuntime, McpToolRuntime, RouterModelClient, SendRequest,
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, BasicExoHarnessConfig, ExoHarness, ThreadHandle, ThreadId, TurnHandle,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::OnceCell;

pub(crate) struct ManagedExecutor {
    state: Arc<dyn ExoHarness>,
    config: BasicExoHarnessConfig,
    env: HashMap<String, String>,
    pricing: Arc<cost::PricingTable>,
    workspace: PathBuf,
    threads: crate::executor_cache::ExecutorCache<ThreadExecutor>,
}

struct ThreadExecutor {
    executor: Arc<dyn HarnessExecutor>,
    mcp: PreparedMcp,
}

#[async_trait]
impl crate::executor_cache::CachedExecutor for ThreadExecutor {
    async fn shutdown(&self) -> Result<()> {
        let stopped = self.executor.shutdown().await;
        let closed = self.mcp.tools.close().await;
        stopped?;
        closed
    }
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
            threads: Default::default(),
        })
    }

    async fn thread(
        &self,
        agent: &dyn AgentHandle,
        thread: &dyn ThreadHandle,
        config: &AgentConfig,
        thread_config: &ConversationConfig,
    ) -> Result<Arc<OnceCell<crate::executor_cache::Entry<ThreadExecutor>>>> {
        let cell = self
            .threads
            .get_or_try_init(thread.record().id, config, || async {
                let mcp = connect_mcp(agent, thread, &exo_mcp::NativeMcpConnector).await?;
                validate_thread(&mcp, &self.config, config, thread_config)?;
                mcp.configure_thread(thread).await?;
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
                Ok::<_, anyhow::Error>(ThreadExecutor { executor, mcp })
            })
            .await?;
        let prepared = cell.get().context("managed executor was not initialized")?;
        prepared.mcp.validate_configuration(agent).await?;
        validate_thread(&prepared.mcp, &self.config, config, thread_config)?;
        Ok(cell)
    }
}

#[async_trait]
impl HarnessExecutor for ManagedExecutor {
    async fn reset_thread(&self, thread: ThreadId) -> Result<()> {
        self.threads.reset(thread).await
    }

    fn with_state(&self, state: Arc<dyn ExoHarness>) -> Result<Arc<dyn HarnessExecutor>> {
        Ok(Arc::new(Self::new(
            state,
            self.config.clone(),
            self.env.clone(),
            self.pricing.clone(),
        )?))
    }

    fn name(&self) -> &'static str {
        "local"
    }

    fn can_resume_pending_approval(&self, config: &AgentConfig) -> bool {
        config.harness == AgentHarnessKind::Basic
    }

    fn can_suspend_turn(&self, config: &AgentConfig) -> bool {
        matches!(
            config.harness,
            AgentHarnessKind::Basic | AgentHarnessKind::TypeScript
        )
    }

    fn can_reconcile_unresolved_tool_call(&self, config: &AgentConfig) -> bool {
        config.harness == AgentHarnessKind::TypeScript
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

    async fn resolve_thread_harness(
        &self,
        thread: &dyn ThreadHandle,
        config: &AgentConfig,
        harness: &str,
    ) -> Result<crate::ConversationHarnessConfig> {
        config::resolve_thread_harness(thread, config, harness).await
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
            .thread(agent, thread.as_ref(), agent_config, thread_config)
            .await?;
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

    async fn resume_turn(
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
            .thread(agent, thread.as_ref(), agent_config, thread_config)
            .await?;
        cell.get()
            .context("managed executor was not initialized")?
            .executor
            .resume_turn(
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
        let cell = self.threads.get(thread.record().id);
        if let Some(cell) = cell
            && let Some(prepared) = cell.get()
        {
            prepared.executor.cancel_turn(thread, config).await?;
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        self.threads.shutdown().await
    }
}

fn validate_thread(
    mcp: &crate::managed_mcp::PreparedMcp,
    store: &exoharness::BasicExoHarnessConfig,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> Result<()> {
    let provider = config
        .sandbox_provider
        .as_ref()
        .unwrap_or(&agent_config.sandbox.provider);
    if *provider != exoharness::SandboxProvider::LocalProcess {
        for mount in agent_config
            .sandbox
            .mounts
            .iter()
            .chain(config.mounts.iter())
        {
            store.validate_secret_mount(std::path::Path::new(&mount.host_path))?;
        }
    } else if let Some(selection) = &mcp.selection
        && (selection.vaults.len() > 1 || selection.bindings.iter().any(|b| b.secret.is_some()))
    {
        anyhow::bail!(
            "vault-backed chat requires an isolated sandbox; local-process can read host credentials"
        );
    }
    Ok(())
}
