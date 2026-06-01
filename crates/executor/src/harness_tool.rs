use crate::{AgentConfig, ConversationConfig, ToolRuntime};
use async_trait::async_trait;
use exoharness::{
    ConversationHandle, CreateSandboxRequest, DEFAULT_SANDBOX_IMAGE, EventData, EventKind,
    EventQueryDirection, FileSystemMount, FileSystemMountMode, Result, RunInSandboxRequest,
    SnapshotId, StartSandboxRequest, ToolRequest, ToolResult,
};
use futures::io::AsyncReadExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct BasicToolRuntime;

#[async_trait]
impl ToolRuntime for BasicToolRuntime {
    async fn prepare_conversation(
        &self,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        config: &ConversationConfig,
    ) -> Result<()> {
        if config.shell_program.is_some() {
            ensure_shell_sandbox(conversation, agent_config, config).await?;
        }
        Ok(())
    }

    async fn execute(
        &self,
        conversation: &dyn ConversationHandle,
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
    let sandbox_id = latest_shell_sandbox(conversation)
        .await?
        .ok_or_else(|| anyhow::anyhow!("shell sandbox is not available for this conversation"))?
        .id;
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![program, "-lc".to_string(), args.command],
            env: Default::default(),
        })
        .await?;
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

pub(crate) async fn ensure_shell_sandbox(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
) -> Result<String> {
    let desired_default_workdir = config
        .mounts
        .first()
        .map(|mount| mount.mount_path.clone())
        .unwrap_or_else(|| "/".to_string());
    let desired_mounts = normalize_mounts(&config.mounts);
    let desired_image = agent_config
        .sandbox_image
        .clone()
        .unwrap_or_else(|| DEFAULT_SANDBOX_IMAGE.to_string());
    let desired_enable_networking = agent_config.enable_networking || config.enable_networking;

    // 3-tier fallback for an existing conversation sandbox:
    //   Tier 1: resume the same container/sandbox by SandboxKey.
    //           This is what `run_in_sandbox` -> backend.try_resume now does
    //           internally when the in-memory handle is missing.
    //   Tier 2: if Tier 1 failed (container is truly gone — TTL expired,
    //           server-side auto-delete, manual cleanup), restore from the
    //           latest snapshot recorded in the conversation log.
    //   Tier 3: nothing to resume, create fresh.
    if let Some(sandbox) = latest_shell_sandbox(conversation).await? {
        let Some(program) = &config.shell_program else {
            return Ok(sandbox.id);
        };

        let config_matches = sandbox.image == desired_image
            && sandbox.default_workdir == desired_default_workdir
            && sandbox.file_system_mounts == desired_mounts
            && sandbox.enable_networking == desired_enable_networking
            && sandbox.idle_seconds == 300;

        if config_matches {
            // Tier 1: try a no-op exec; the harness will resume from the
            // backend on cache miss.
            let healthcheck = conversation
                .run_in_sandbox(RunInSandboxRequest {
                    id: sandbox.id.clone(),
                    command: vec![program.clone(), "-lc".to_string(), "true".to_string()],
                    env: Default::default(),
                })
                .await;
            if healthcheck.is_ok() {
                return Ok(sandbox.id);
            }

            // Tier 2: container is gone. If we ever took a snapshot of this
            // sandbox in this conversation, restore from the latest one.
            if let Some(snapshot_id) =
                latest_snapshot_for_sandbox(conversation, &sandbox.id).await?
                && conversation
                    .start_sandbox(StartSandboxRequest {
                        id: sandbox.id.clone(),
                        snapshot_id,
                        idle_seconds: Some(sandbox.idle_seconds),
                    })
                    .await
                    .is_ok()
            {
                return Ok(sandbox.id);
            }
        }
    }

    // Tier 3: nothing reusable; create fresh.
    conversation
        .create_sandbox(CreateSandboxRequest {
            image: desired_image,
            default_workdir: Some(desired_default_workdir),
            file_system_mounts: Some(desired_mounts),
            enable_networking: Some(desired_enable_networking),
            idle_seconds: Some(300),
        })
        .await
}

async fn latest_snapshot_for_sandbox(
    conversation: &dyn ConversationHandle,
    sandbox_id: &str,
) -> Result<Option<SnapshotId>> {
    exoharness::first_matching_event(
        conversation,
        EventKind::SANDBOX_SNAPSHOTTED,
        EventQueryDirection::Desc,
        100,
        |data| match data {
            EventData::SandboxSnapshotted {
                sandbox_id: sid,
                snapshot_id,
            } if sid == sandbox_id => Some(snapshot_id),
            _ => None,
        },
    )
    .await
}

fn normalize_mounts(mounts: &[FileSystemMount]) -> Vec<FileSystemMount> {
    mounts
        .iter()
        .map(|mount| FileSystemMount {
            host_path: mount.host_path.clone(),
            mount_path: mount.mount_path.clone(),
            mode: match mount.mode {
                FileSystemMountMode::ReadOnly => FileSystemMountMode::ReadOnly,
                FileSystemMountMode::ReadWrite => FileSystemMountMode::ReadWrite,
            },
            internal: Some(mount.internal.unwrap_or(false)),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellSandboxInfo {
    id: String,
    image: String,
    default_workdir: String,
    file_system_mounts: Vec<FileSystemMount>,
    enable_networking: bool,
    idle_seconds: u64,
}

async fn latest_shell_sandbox(
    conversation: &dyn ConversationHandle,
) -> Result<Option<ShellSandboxInfo>> {
    exoharness::first_matching_event(
        conversation,
        EventKind::SANDBOX_CREATED,
        EventQueryDirection::Desc,
        1,
        |data| match data {
            EventData::SandboxCreated {
                sandbox_id,
                image,
                default_workdir,
                file_system_mounts,
                enable_networking,
                idle_seconds,
            } => Some(ShellSandboxInfo {
                id: sandbox_id,
                image,
                default_workdir,
                file_system_mounts,
                enable_networking,
                idle_seconds,
            }),
            _ => None,
        },
    )
    .await
}
