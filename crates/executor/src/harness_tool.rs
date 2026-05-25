use std::path::PathBuf;

use crate::adapter::AdapterStore;
use crate::adapter::tools::{
    execute_create_adapter_tool, execute_delete_adapter_tool, execute_disable_adapter_tool,
    execute_list_adapters_tool, execute_send_adapter_message_tool,
};
use crate::agent_sandbox::ensure_agent_sandbox;
use crate::conversation_sandbox::{conversation_sandboxes, ensure_conversation_sandbox};
use crate::scheduler_store::SchedulerStore;
use crate::scheduler_types::{
    DEFAULT_MAX_OUTPUT_BYTES, NewScheduledTask, ScheduledTaskSandboxMode,
};
use crate::{AgentConfig, ConversationConfig, ToolRuntime};
use crate::{SandboxScope, effective_sandbox_scope};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, ConversationHandle, Result, RunInSandboxRequest, SandboxProcess, ToolRequest,
    ToolResult,
};
use futures::io::AsyncReadExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct BasicToolRuntime;

#[derive(Debug, Clone)]
pub struct ExoclawToolRuntime {
    scheduler_store: SchedulerStore,
    adapter_store: AdapterStore,
}

impl ExoclawToolRuntime {
    pub fn with_roots(
        scheduler_root: impl Into<PathBuf>,
        adapter_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            scheduler_store: SchedulerStore::new(scheduler_root),
            adapter_store: AdapterStore::new(adapter_root),
        }
    }
}

#[async_trait]
impl ToolRuntime for BasicToolRuntime {
    async fn prepare_conversation(
        &self,
        _agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
    ) -> Result<()> {
        if config.shell_program.is_some() {
            ensure_conversation_sandbox(conversation, agent_config, config).await?;
        }
        Ok(())
    }

    async fn execute(
        &self,
        _agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        _agent_config: &AgentConfig,
        config: &ConversationConfig,
        request: &ToolRequest,
    ) -> Result<ToolResult> {
        match request.function_name.as_str() {
            "shell" => execute_shell_tool(conversation, config, request).await,
            other => Err(anyhow::anyhow!(
                "tool execution is not configured for {other}"
            )),
        }
    }
}

#[async_trait]
impl ToolRuntime for ExoclawToolRuntime {
    async fn prepare_conversation(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
    ) -> Result<()> {
        match effective_sandbox_scope(agent_config, config) {
            SandboxScope::Agent => {
                ensure_agent_sandbox(agent, conversation, agent_config, config).await?;
            }
            SandboxScope::Conversation => {
                ensure_conversation_sandbox(conversation, agent_config, config).await?;
            }
        }
        Ok(())
    }

    async fn execute(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
        request: &ToolRequest,
    ) -> Result<ToolResult> {
        match request.function_name.as_str() {
            "shell" => {
                execute_exoclaw_shell_tool(agent, conversation, agent_config, config, request).await
            }
            "schedule_sandbox_task" => {
                execute_schedule_task_tool(&self.scheduler_store, request).await
            }
            "list_scheduled_tasks" => {
                execute_list_scheduled_tasks_tool(&self.scheduler_store, request).await
            }
            "cancel_scheduled_task" => {
                execute_cancel_scheduled_task_tool(conversation, &self.scheduler_store, request)
                    .await
            }
            "delete_scheduled_task" => {
                execute_delete_scheduled_task_tool(conversation, &self.scheduler_store, request)
                    .await
            }
            "create_adapter" => execute_create_adapter_tool(&self.adapter_store, request).await,
            "list_adapters" => execute_list_adapters_tool(&self.adapter_store, request).await,
            "disable_adapter" => {
                execute_disable_adapter_tool(conversation, &self.adapter_store, request).await
            }
            "delete_adapter" => {
                execute_delete_adapter_tool(conversation, &self.adapter_store, request).await
            }
            "send_adapter_message" => {
                execute_send_adapter_message_tool(agent, conversation, &self.adapter_store, request)
                    .await
            }
            other => Err(anyhow::anyhow!(
                "tool execution is not configured for {other}"
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ShellToolArguments {
    command: String,
}

#[derive(Debug, Serialize)]
struct ShellToolResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleTaskArguments {
    agent_id: String,
    conversation_id: String,
    name: String,
    schedule: String,
    sandbox_mode: Option<ScheduledTaskSandboxMode>,
    setup_command: Option<Vec<String>>,
    command: Vec<String>,
    report_prompt: String,
    max_output_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConversationScopedArguments {
    agent_id: String,
    conversation_id: String,
    include_disabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScheduledTaskIdArguments {
    agent_id: String,
    conversation_id: String,
    task_id: String,
}

async fn execute_schedule_task_tool(
    store: &SchedulerStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<ScheduleTaskArguments>(Value::Object(request.arguments.clone()))?;
    let task = store
        .create_task(NewScheduledTask {
            agent_id: args.agent_id,
            conversation_id: args.conversation_id,
            name: args.name,
            schedule: args.schedule,
            sandbox_mode: args.sandbox_mode,
            setup_command: args.setup_command,
            command: args.command,
            report_prompt: args.report_prompt,
            max_output_bytes: Some(args.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)),
        })
        .await?;
    Ok(serde_json::json!({
        "ok": true,
        "taskId": task.id,
        "name": task.name,
        "schedule": task.schedule,
        "nextRunAtMs": task.next_run_at_ms,
    }))
}

async fn execute_list_scheduled_tasks_tool(
    store: &SchedulerStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ConversationScopedArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let tasks = store
        .list_tasks_for_conversation(
            &args.agent_id,
            &args.conversation_id,
            args.include_disabled.unwrap_or(false),
        )
        .await?;
    Ok(serde_json::json!({
        "ok": true,
        "tasks": tasks,
    }))
}

async fn execute_cancel_scheduled_task_tool(
    conversation: &dyn ConversationHandle,
    store: &SchedulerStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ScheduledTaskIdArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let Some(task) = store.get_task(&args.task_id).await? else {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "scheduled task not found for this conversation",
        }));
    };
    if task.agent_id != args.agent_id || task.conversation_id != args.conversation_id {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "scheduled task not found for this conversation",
        }));
    }
    let task_sandbox_id = task.task_sandbox_id.clone();
    store.disable_task(&args.task_id).await?;
    if let Some(sandbox_id) = task_sandbox_id {
        conversation.stop_sandbox(sandbox_id).await?;
    }
    Ok(serde_json::json!({
        "ok": true,
        "taskId": args.task_id,
        "cancelled": true,
    }))
}

async fn execute_delete_scheduled_task_tool(
    conversation: &dyn ConversationHandle,
    store: &SchedulerStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ScheduledTaskIdArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let Some(task) = store.get_task(&args.task_id).await? else {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "scheduled task not found for this conversation",
        }));
    };
    if task.agent_id != args.agent_id || task.conversation_id != args.conversation_id {
        return Ok(serde_json::json!({
            "ok": false,
            "error": "scheduled task not found for this conversation",
        }));
    }
    if let Some(sandbox_id) = task.task_sandbox_id.clone() {
        conversation.stop_sandbox(sandbox_id).await?;
    }
    store.delete_task(&args.task_id).await?;
    Ok(serde_json::json!({
        "ok": true,
        "taskId": args.task_id,
        "deleted": true,
        "runsDeleted": true,
    }))
}

async fn execute_shell_tool(
    conversation: &dyn ConversationHandle,
    config: &ConversationConfig,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<ShellToolArguments>(Value::Object(request.arguments.clone()))?;
    let program = config
        .shell_program
        .clone()
        .ok_or_else(|| anyhow::anyhow!("shell tool is not enabled for this conversation"))?;
    let sandbox_id = conversation_sandboxes(conversation)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("shell sandbox is not available for this conversation"))?
        .id;
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![program, "-lc".to_string(), args.command],
            env: Default::default(),
        })
        .await?;
    read_shell_process(process).await
}

async fn read_shell_process(process: Box<dyn SandboxProcess>) -> Result<ToolResult> {
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut stderr = parts.stderr;
    drop(parts.stdin);

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let (stdout_result, stderr_result, wait_result) = tokio::join!(
        stdout.read_to_end(&mut stdout_bytes),
        stderr.read_to_end(&mut stderr_bytes),
        parts.wait,
    );
    stdout_result?;
    stderr_result?;
    let exit_code = wait_result?;

    Ok(serde_json::to_value(ShellToolResult {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        exit_code,
    })?)
}

async fn execute_exoclaw_shell_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    request: &ToolRequest,
) -> Result<ToolResult> {
    if effective_sandbox_scope(agent_config, config) == SandboxScope::Conversation {
        return execute_shell_tool(conversation, config, request).await;
    }

    let args =
        serde_json::from_value::<ShellToolArguments>(Value::Object(request.arguments.clone()))?;
    let program = config
        .shell_program
        .clone()
        .ok_or_else(|| anyhow::anyhow!("shell tool is not enabled for this conversation"))?;
    let agent_sandbox = ensure_agent_sandbox(agent, conversation, agent_config, config).await?;
    let process = agent_sandbox
        .conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: agent_sandbox.sandbox_id,
            command: vec![program, "-lc".to_string(), args.command],
            env: Default::default(),
        })
        .await?;
    read_shell_process(process).await
}
