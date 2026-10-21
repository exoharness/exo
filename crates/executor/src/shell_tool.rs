use crate::{AgentConfig, ConversationConfig, ShellToolArguments, ShellToolResult};
use anyhow::Result;
use exoharness::{
    ConversationHandle, RunInSandboxRequest, SandboxProcess, ToolRequest, ToolResult,
};
use futures::io::AsyncReadExt;
use serde_json::Value;

pub async fn read_shell_process(process: Box<dyn SandboxProcess>) -> Result<ToolResult> {
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut stderr = parts.stderr;
    drop(parts.stdin);

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let (stdout_result, stderr_result, wait_result) = futures::join!(
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

pub async fn execute_shell_tool(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<ShellToolArguments>(Value::Object(request.arguments.clone()))?;
    let program = config
        .shell_program
        .clone()
        .ok_or_else(|| anyhow::anyhow!("shell tool is not enabled for this conversation"))?;
    let sandbox_id = ensure_shell_sandbox(conversation, agent_config, config).await?;
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![program, "-lc".to_string(), args.command],
            env: Default::default(),
        })
        .await?;
    read_shell_process(process).await
}

pub(crate) async fn ensure_shell_sandbox(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> Result<String> {
    crate::conversation_sandbox::ensure_conversation_sandbox(
        conversation,
        agent_config,
        config,
        config.shell_program.as_deref(),
    )
    .await
}
