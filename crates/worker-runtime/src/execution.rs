use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use executor::execution_tracing::{ExecutionTracer, TurnExecutionTrace};
use executor::managed_agents::{
    HarnessModules, TypeScriptHarnessPreset, agent_config_with_modules,
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
use serde::Deserialize;

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
    fn preset_image(&self, _preset: TypeScriptHarnessPreset) -> Option<String> {
        None
    }
}

pub(crate) fn worker_agent_config(definition: &AgentDefinition) -> Result<AgentConfig> {
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
    let config = agent_config_with_modules(
        definition,
        SandboxProvider::from_static("cloudflare"),
        None,
        None,
        &WorkerModules,
    )?;
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

pub(crate) struct WorkerTools(pub Arc<Host>);
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}
#[async_trait]
impl ToolRuntime for WorkerTools {
    async fn execute(
        &self,
        agent: &dyn AgentHandle,
        thread: &dyn ConversationHandle,
        _turn: Option<&dyn TurnHandle>,
        _agent_config: &AgentConfig,
        config: &ConversationConfig,
        request: &ToolRequest,
    ) -> Result<ToolResult> {
        ensure!(request.function_name == "shell", "unsupported Worker tool");
        let args: executor::ShellToolArguments =
            serde_json::from_value(serde_json::to_value(&request.arguments)?)?;
        let program = config
            .shell_program
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("shell tool is not enabled for this conversation"))?;
        let response: ExecOutput = self
            .0
            .call(HostRequest::Exec {
                agent_id: agent.record().id,
                thread_id: thread.record().id,
                command: vec![program.clone(), "-lc".into(), args.command],
            })
            .await?;
        Ok(serde_json::to_value(executor::ShellToolResult {
            stdout: response.stdout,
            stderr: response.stderr,
            exit_code: response.exit_code,
        })?)
    }
}

pub(crate) struct WorkerExecutor {
    pub host: Arc<Host>,
    pub basic: BasicExecutor<WorkerModel, WorkerTools>,
}
#[derive(Deserialize)]
pub(crate) struct UnitResponse {}

#[async_trait]
impl HarnessExecutor for WorkerExecutor {
    fn name(&self) -> &'static str {
        "worker"
    }
    fn agent_config(&self, definition: &AgentDefinition) -> Result<AgentConfig> {
        worker_agent_config(definition)
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
        validate_conversation(thread_config)?;
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
        let _response: UnitResponse = self
            .host
            .call(HostRequest::StopSandbox {
                thread_id: thread.record().id,
            })
            .await?;
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
        validate_conversation(thread_config)?;
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
        let _response: UnitResponse = self
            .host
            .call(HostRequest::Harness {
                agent_id: agent.record().id,
                thread_id: thread.record().id,
                turn: turn.record().clone(),
                agent_config: config.clone(),
                conversation_config: thread_config.clone(),
                request: request.clone(),
                recovering,
            })
            .await?;
        Ok(())
    }
}

pub(crate) struct WorkerTracer;
#[async_trait]
impl ExecutionTracer for WorkerTracer {
    async fn flush(&self) -> Result<()> {
        Ok(())
    }
    async fn start_turn(
        &self,
        _config: Option<&executor::BraintrustTracingConfig>,
        _agent: &exoharness::AgentRecord,
        _thread: &exoharness::ConversationRecord,
        _agent_config: &AgentConfig,
        _session: exoharness::SessionId,
        _turn: exoharness::TurnId,
        _streamed: bool,
    ) -> Option<Box<dyn TurnExecutionTrace>> {
        None
    }
}

fn validate_conversation(config: &ConversationConfig) -> Result<()> {
    ensure!(
        config.resources.is_empty()
            && config.resource_mounts.is_empty()
            && config.mounts.is_empty()
            && config.durable_file_systems.is_empty(),
        "filesystem resources and mounts are not supported by this host"
    );
    ensure!(
        config
            .sandbox_image
            .as_deref()
            .is_none_or(|image| image == "cloudflare/debian-trixie")
            && config
                .sandbox_provider
                .as_ref()
                .is_none_or(|provider| provider.as_str() == "cloudflare"),
        "this host uses the standard Cloudflare sandbox image"
    );
    if let Some(environment) = &config.environment {
        let sandbox = &environment.config;
        ensure!(
            sandbox.provider.as_str() == "cloudflare"
                && sandbox.image == "cloudflare/debian-trixie"
                && sandbox.resources.is_none()
                && sandbox
                    .default_workdir
                    .as_deref()
                    .is_none_or(|path| path == "/workspace")
                && sandbox
                    .file_system_mounts
                    .as_ref()
                    .is_none_or(Vec::is_empty)
                && sandbox
                    .durable_file_systems
                    .as_ref()
                    .is_none_or(Vec::is_empty)
                && sandbox.tcp_ports.is_empty()
                && sandbox.idle_seconds.is_none_or(|idle| idle == 300),
            "this host supports standard-image environments with network policies; custom execution settings are not supported"
        );
    }
    Ok(())
}
