use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, ensure};
use async_trait::async_trait;
use executor::conversation_sandbox::ensure_conversation_sandbox;
use executor::execution_tracing::TurnExecutionTrace;
use executor::managed_agents::{
    HarnessModules, TypeScriptHarnessPreset, agent_config_with_modules,
};
use executor::typescript_runtime::{
    RequestContext, RuntimeRequest, RuntimeResponsePayload, TypeScriptRuntime,
};
use executor::{
    AgentConfig, AgentHarnessKind, BasicExecutor, ConversationConfig, ExecutorStreamMode,
    HarnessExecutor, ModelClient, ModelRequest, ModelResponse, ModelResponseStream, SendRequest,
    ToolRuntime,
};
use exo_managed_agents::AgentDefinition;
use exoharness::{
    AgentHandle, ConversationHandle, SandboxProvider, ToolRequest, ToolResult, TurnHandle,
};

use crate::host::{Host, HostRequest};

pub(crate) struct WorkerModules;
impl HarnessModules for WorkerModules {
    fn installation(&self) -> Result<PathBuf> {
        Ok("/bundle".into())
    }
    fn resolve(&self, path: &Path) -> Result<PathBuf> {
        ensure!(
            path == Path::new("/bundle").join(TypeScriptHarnessPreset::Codex.module_path()),
            "harness module is not bundled in this Worker: {}",
            path.display()
        );
        Ok(path.into())
    }
    fn preset_image(&self, preset: TypeScriptHarnessPreset) -> Option<String> {
        let (_, digest) = preset.sandbox_image()?.rsplit_once("@sha256:")?;
        Some(format!("{}-{}", preset.agent_slug(), digest.get(..8)?))
    }
}

pub(crate) fn worker_agent_config(
    definition: &AgentDefinition,
    sandbox: SandboxProvider,
) -> Result<AgentConfig> {
    let f = &definition.frontmatter;
    ensure!(
        matches!(f.harness.as_str(), "basic" | "codex"),
        "this Worker supports basic and codex harnesses"
    );
    ensure!(
        f.resources.is_empty()
            && f.mcp_servers.is_empty()
            && f.tools.is_empty()
            && f.adapters.is_empty()
            && !f.tool_creation,
        "resources, MCP, custom tools and adapters are not supported by this Worker"
    );
    ensure!(
        f.config.braintrust.is_none(),
        "Braintrust tracing is not configured on this Worker"
    );
    let config = agent_config_with_modules(definition, sandbox, None, None, &WorkerModules)?;
    if f.harness == "codex" {
        ensure!(
            definition.permissions().permission_policy
                == exo_managed_agents::permissions::PermissionPolicy::AlwaysAllow {}
                && f.tool_policies.is_empty(),
            "Codex currently requires always_allow permissions"
        );
        ensure!(
            config.max_output_tokens.is_none() && config.max_tool_round_trips.is_none(),
            "Codex does not implement basic-harness turn limits"
        );
    }
    ensure!(
        config.credential.is_some(),
        "config.credential must name a credential in a selected vault"
    );
    Ok(config)
}

pub(crate) struct WorkerModel(pub Arc<Host>);
#[async_trait]
impl ModelClient for WorkerModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.0.call(HostRequest::Model { request }).await
    }
    async fn complete_stream(
        &self,
        _request: ModelRequest,
    ) -> Result<Box<dyn ModelResponseStream>> {
        anyhow::bail!("Worker model streaming is not enabled")
    }
}

pub(crate) struct WorkerTools;
#[async_trait]
impl ToolRuntime for WorkerTools {
    async fn execute(
        &self,
        _agent: &dyn AgentHandle,
        thread: &dyn ConversationHandle,
        _turn: Option<&dyn TurnHandle>,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
        request: &ToolRequest,
    ) -> Result<ToolResult> {
        ensure!(request.function_name == "shell", "unsupported Worker tool");
        executor::shell_tool::execute_shell_tool(thread, agent_config, config, request).await
    }
}

pub(crate) struct WorkerExecutor {
    pub coordinator: Arc<dyn exoharness::turn_coordinator::TurnQueue<executor::TurnWork>>,
    pub host: Arc<Host>,
    pub basic: BasicExecutor<WorkerModel, WorkerTools>,
    pub state: Arc<dyn exoharness::ExoHarness>,
    pub sandbox_default: SandboxProvider,
    pub harnesses: Mutex<HashMap<exoharness::ThreadId, Arc<WorkerHarness>>>,
}

pub(crate) struct WorkerHarness {
    runtime: TypeScriptRuntime,
    turn: Mutex<Option<Arc<WorkerTurn>>>,
}

struct WorkerTurn {
    agent: Arc<dyn AgentHandle>,
    thread: Arc<dyn ConversationHandle>,
    turn: Arc<dyn TurnHandle>,
    config: AgentConfig,
    thread_config: ConversationConfig,
    stream: Option<tokio::sync::mpsc::UnboundedSender<Result<executor::ExecutionStreamEvent>>>,
}
impl WorkerTurn {
    fn context(&self) -> RequestContext<'_> {
        RequestContext {
            agent: self.agent.as_ref(),
            conversation: self.thread.as_ref(),
            turn: self.turn.as_ref(),
            agent_config: &self.config,
            conversation_config: &self.thread_config,
            stream: self
                .stream
                .as_ref()
                .map_or(ExecutorStreamMode::Disabled, ExecutorStreamMode::Enabled),
        }
    }
}

struct ActiveTurn<'a>(&'a WorkerHarness);
impl Drop for ActiveTurn<'_> {
    fn drop(&mut self) {
        *self.0.turn.lock().expect("Worker turn poisoned") = None;
    }
}

#[async_trait]
impl HarnessExecutor for WorkerExecutor {
    fn name(&self) -> &'static str {
        "worker"
    }
    fn agent_config(&self, definition: &AgentDefinition) -> Result<AgentConfig> {
        worker_agent_config(definition, self.sandbox_default.clone())
    }
    fn can_resume_pending_approval(&self, config: &AgentConfig) -> bool {
        config.harness == AgentHarnessKind::Basic
    }
    fn can_reconcile_unresolved_tool_call(&self, config: &AgentConfig) -> bool {
        config.harness == AgentHarnessKind::TypeScript
    }
    async fn prepare_conversation(
        &self,
        agent: &dyn AgentHandle,
        thread: &dyn ConversationHandle,
        config: &AgentConfig,
        thread_config: &ConversationConfig,
    ) -> Result<()> {
        ensure_conversation_sandbox(thread, config, thread_config, None).await?;
        if config.harness == AgentHarnessKind::Basic {
            self.basic
                .prepare_conversation(agent, thread, config, thread_config)
                .await?;
        }
        Ok(())
    }
    async fn cancel_turn(
        &self,
        thread: &dyn ConversationHandle,
        _config: &AgentConfig,
    ) -> Result<()> {
        let harness = self
            .harnesses
            .lock()
            .expect("Worker harnesses poisoned")
            .get(&thread.record().id)
            .cloned();
        if let Some(harness) = harness {
            harness.runtime.shutdown().await?;
        }
        Ok(())
    }
    async fn execute_turn(
        &self,
        agent: &dyn AgentHandle,
        thread: Arc<dyn ConversationHandle>,
        turn: Arc<dyn TurnHandle>,
        config: &AgentConfig,
        thread_config: &ConversationConfig,
        request: &SendRequest,
        stream: ExecutorStreamMode<'_>,
        trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        self.run(
            agent,
            thread,
            turn,
            config,
            thread_config,
            request,
            stream,
            trace,
            false,
        )
        .await
    }
    async fn resume_turn(
        &self,
        agent: &dyn AgentHandle,
        thread: Arc<dyn ConversationHandle>,
        turn: Arc<dyn TurnHandle>,
        config: &AgentConfig,
        thread_config: &ConversationConfig,
        request: &SendRequest,
        stream: ExecutorStreamMode<'_>,
        trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        self.run(
            agent,
            thread,
            turn,
            config,
            thread_config,
            request,
            stream,
            trace,
            true,
        )
        .await
    }
}

impl WorkerExecutor {
    pub fn emit_stream(
        &self,
        thread: exoharness::ThreadId,
        event: executor::TypeScriptStreamEvent,
    ) -> Result<()> {
        let harnesses = self.harnesses.lock().expect("Worker harnesses poisoned");
        let harness = harnesses
            .get(&thread)
            .ok_or_else(|| anyhow::anyhow!("TypeScript harness is not active"))?;
        let turn = harness.turn.lock().expect("Worker turn poisoned");
        let turn = turn
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("TypeScript turn is not active"))?;
        if let Some(stream) = &turn.stream
            && stream
                .send(Ok(executor::to_execution_stream_event(event)))
                .is_err()
        {
            tracing::trace!("turn stream closed before progress arrived");
        }
        Ok(())
    }

    pub async fn request_runtime(
        &self,
        thread: exoharness::ThreadId,
        request: RuntimeRequest,
    ) -> Result<RuntimeResponsePayload> {
        let harness = self
            .harnesses
            .lock()
            .expect("Worker harnesses poisoned")
            .get(&thread)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("TypeScript harness is not active"))?;
        let turn = harness.turn.lock().expect("Worker turn poisoned").clone();
        harness
            .runtime
            .handle_request(turn.as_ref().map(|turn| turn.context()), request)
            .await
    }

    async fn run(
        &self,
        agent: &dyn AgentHandle,
        thread: Arc<dyn ConversationHandle>,
        turn: Arc<dyn TurnHandle>,
        config: &AgentConfig,
        thread_config: &ConversationConfig,
        request: &SendRequest,
        stream: ExecutorStreamMode<'_>,
        trace: Option<&dyn TurnExecutionTrace>,
        recovering: bool,
    ) -> Result<()> {
        let sandbox_id =
            ensure_conversation_sandbox(thread.as_ref(), config, thread_config, None).await?;
        let _activity = thread.sandbox_activity(sandbox_id).await?;
        if config.harness == AgentHarnessKind::Basic {
            if recovering {
                return self
                    .basic
                    .resume_turn(
                        agent,
                        thread,
                        turn,
                        config,
                        thread_config,
                        request,
                        stream,
                        trace,
                    )
                    .await;
            }
            return self
                .basic
                .execute_turn(
                    agent,
                    thread,
                    turn,
                    config,
                    thread_config,
                    request,
                    stream,
                    trace,
                )
                .await;
        }
        ensure!(
            config.harness == AgentHarnessKind::TypeScript,
            "harness is not supported by this Worker"
        );
        let harness = self
            .harnesses
            .lock()
            .expect("Worker harnesses poisoned")
            .entry(thread.record().id)
            .or_insert_with(|| {
                let host = self.host.clone();
                Arc::new(WorkerHarness {
                    runtime: TypeScriptRuntime::new(
                        thread.clone(),
                        Arc::new(WorkerTools),
                        self.host.clone(),
                        Arc::new(move |event| {
                            host.events
                                .lock()
                                .expect("Worker events poisoned")
                                .push(event);
                            Ok(())
                        }),
                    ),
                    turn: Mutex::default(),
                })
            })
            .clone();
        let turn = Arc::new(WorkerTurn {
            agent: self
                .state
                .get_agent(&agent.record().id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("agent not found"))?,
            thread,
            turn,
            config: config.clone(),
            thread_config: thread_config.clone(),
            stream: match stream {
                ExecutorStreamMode::Enabled(tx) => Some(tx.clone()),
                ExecutorStreamMode::Disabled => None,
            },
        });
        let payload = harness
            .runtime
            .init(&turn.context(), request, recovering, None)
            .await?;
        *harness.turn.lock().expect("Worker turn poisoned") = Some(turn);
        let active_turn = ActiveTurn(harness.as_ref());
        let result = self.host.call::<()>(HostRequest::Harness { payload }).await;
        drop(active_turn);
        if result.is_err() {
            harness.runtime.shutdown().await?;
        }
        result?;
        Ok(())
    }
}
