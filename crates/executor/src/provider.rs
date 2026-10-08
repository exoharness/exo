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
use crate::harness_adapter::{ExecutorHarness, ExecutorTurn};
use crate::harness_executor::{HarnessExecutor, RecoveryRuntimeResolver};
use crate::runtime_host::{RuntimeHost, TaskGroup};
use crate::{AgentConfig, ExecutionStreamHandle, Runtime, SendRequest};

#[async_trait]
pub trait Provider: AgentBackend {
    async fn preview_endpoint(
        &self,
        _agent: &dyn AgentHandle,
        _thread: &dyn ThreadHandle,
    ) -> Result<Option<exo_managed_agents::http::protocol::PreviewEndpoint>> {
        Ok(None)
    }
    fn runtime_host(&self) -> Arc<dyn RuntimeHost>;

    fn with_caller(&self, _caller: exoharness::access::Caller) -> Result<Arc<dyn Provider>> {
        anyhow::bail!("this provider does not support caller-scoped execution")
    }

    fn harness(&self) -> &dyn Harness<ProviderTurn>;

    async fn resolve_thread_harness(
        &self,
        _thread: &dyn ThreadHandle,
        _config: &AgentConfig,
        _harness: &str,
    ) -> Result<crate::ConversationHarnessConfig> {
        anyhow::bail!("this provider does not configure managed harnesses")
    }

    async fn recover_unfinished_turns(
        &self,
        _runtime: Runtime,
        _resolver: Option<RecoveryRuntimeResolver>,
    ) -> Result<()> {
        Ok(())
    }

    async fn resume_turn(
        &self,
        _runtime: &Runtime,
        _agent: Arc<dyn AgentHandle>,
        _thread: Arc<dyn ThreadHandle>,
        _turn: TurnRecord,
        _request: SendRequest,
        _agent_config: AgentConfig,
        _thread_config: crate::ConversationConfig,
    ) -> Result<ExecutionStreamHandle> {
        anyhow::bail!("this provider does not support turn recovery")
    }

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

    async fn frontend_tool_result(
        &self,
        _agent: exoharness::AgentId,
        _thread: exoharness::ThreadId,
        _turn: exoharness::TurnId,
        _body: &exo_managed_agents::http::protocol::FrontendToolResultBody,
    ) -> Result<exoharness::EventId> {
        anyhow::bail!("this provider does not support client tools")
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
    host: Arc<dyn RuntimeHost>,
    pub(crate) state: Arc<dyn ExoHarness>,
    pub(crate) executor: Arc<dyn HarnessExecutor>,
    pub(crate) harness: Arc<ExecutorHarness>,
    pub(crate) managed: crate::managed_agents::LocalAgentSetup,
    pub(crate) resource_preparations: Arc<Mutex<TaskGroup>>,
    // One provider-wide lock serializes the pending check and decision write so
    // concurrent responses cannot accept the same approval twice.
    approval_responses: tokio::sync::Mutex<()>,
    frontend_responses: Arc<tokio::sync::Mutex<()>>,
    pub(crate) live_turns:
        Arc<tokio::sync::RwLock<HashMap<crate::harness::HarnessTurnKey, Weak<()>>>>,
}

impl LocalProvider {
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
            host,
            approval_responses: Default::default(),
            frontend_responses: Default::default(),
            live_turns: Arc::default(),
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
    #[cfg(feature = "native")]
    async fn preview_endpoint(
        &self,
        _agent: &dyn AgentHandle,
        thread: &dyn ThreadHandle,
    ) -> Result<Option<exo_managed_agents::http::protocol::PreviewEndpoint>> {
        let Some(port) = crate::load_conversation_config(thread).await?.preview_port else {
            return Ok(None);
        };
        let endpoint = crate::PreviewEndpoint {
            domain: "localhost".into(),
            port,
        };
        let Some(previews) = crate::previews_for(thread.record(), &endpoint)? else {
            return Ok(None);
        };
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(1))
            .build()?
            .get(format!("http://127.0.0.1:{port}"))
            .header("Host", previews.page.trim_start_matches("http://"))
            .send()
            .await;
        Ok(response
            .ok()
            .filter(|response| response.status().is_success())
            .map(|_| endpoint))
    }
    fn runtime_host(&self) -> Arc<dyn RuntimeHost> {
        self.host.clone()
    }

    async fn resolve_thread_harness(
        &self,
        thread: &dyn ThreadHandle,
        config: &AgentConfig,
        harness: &str,
    ) -> Result<crate::ConversationHarnessConfig> {
        self.executor
            .resolve_thread_harness(thread, config, harness)
            .await
    }

    async fn recover_unfinished_turns(
        &self,
        runtime: Runtime,
        resolver: Option<RecoveryRuntimeResolver>,
    ) -> Result<()> {
        runtime.recover_local_turns(self, resolver).await
    }

    async fn resume_turn(
        &self,
        runtime: &Runtime,
        agent: Arc<dyn AgentHandle>,
        thread: Arc<dyn ThreadHandle>,
        turn: TurnRecord,
        request: SendRequest,
        agent_config: AgentConfig,
        thread_config: crate::ConversationConfig,
    ) -> Result<ExecutionStreamHandle> {
        runtime
            .start_local_turn(
                self,
                agent,
                thread,
                request,
                false,
                Some(agent_config),
                Some((turn, thread_config)),
            )
            .await
            .map(|(_, stream)| stream)
    }

    fn with_caller(&self, caller: exoharness::access::Caller) -> Result<Arc<dyn Provider>> {
        let state = self.state.with_caller(caller)?;
        let executor = self.executor.with_state(state.clone())?;
        let mut provider = Self::with_host(state, executor, self.host.clone())
            .with_managed_agents(self.managed.clone());
        provider.live_turns = self.live_turns.clone();
        provider.resource_preparations = self.resource_preparations.clone();
        provider.frontend_responses = self.frontend_responses.clone();
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

    async fn frontend_tool_result(
        &self,
        agent: exoharness::AgentId,
        thread: exoharness::ThreadId,
        turn: exoharness::TurnId,
        body: &exo_managed_agents::http::protocol::FrontendToolResultBody,
    ) -> Result<exoharness::EventId> {
        use anyhow::{Context, ensure};
        let agent = self
            .state
            .get_agent(&agent)
            .await?
            .context("agent not found")?;
        let thread = agent
            .get_thread(&thread)
            .await?
            .context("thread not found")?;
        if let Some(caller) = self.state.caller() {
            ensure!(
                crate::permissions::turn_caller(thread.as_ref(), turn)
                    .await?
                    .as_deref()
                    == Some(caller.principal.as_str()),
                "only the caller who started this turn may submit client tool results"
            );
        }
        let _guard = self.frontend_responses.lock().await;
        crate::frontend_tools::respond(
            thread.as_ref(),
            TurnRecord {
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
                        None,
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
