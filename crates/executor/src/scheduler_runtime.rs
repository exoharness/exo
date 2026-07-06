use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use exoharness::{EnqueueTurnRequest, RunInSandboxRequest, SandboxProcess, WriteArtifactRequest};
use futures::io::{AsyncRead, AsyncReadExt};
use lingua::Message;
use lingua::universal::UserContent;
use serde::Serialize;

use crate::agent_sandbox::ensure_agent_sandbox;
use crate::conversation_sandbox::{create_conversation_sandbox, ensure_conversation_sandbox};
use crate::scheduler_store::SchedulerStore;
use crate::scheduler_types::{
    DEFAULT_COMMAND_TIMEOUT_MS, ScheduledTaskRecord, ScheduledTaskRunRecord,
    ScheduledTaskSandboxMode, now_ms,
};
use crate::shared::spawn_lease_renewal;
use crate::{Harness, Uuid7};

/// How often a scheduler waits between checks while its report turn is being
/// driven by another runner or is queued behind other turns.
const REPORT_DRIVE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy)]
pub struct SchedulerRunOptions {
    pub limit: usize,
}

impl Default for SchedulerRunOptions {
    fn default() -> Self {
        Self { limit: 10 }
    }
}

#[derive(Debug, Serialize)]
struct ScheduledTaskArtifact {
    task_id: String,
    task_name: String,
    run_id: String,
    sandbox_id: Option<String>,
    setup_command: Option<Vec<String>>,
    command: Vec<String>,
    exit_code: Option<i32>,
    setup_stdout: Option<String>,
    setup_stderr: Option<String>,
    stdout: String,
    stderr: String,
    truncated: bool,
    error: Option<String>,
}

pub async fn run_due_tasks(
    harness: Arc<dyn Harness>,
    store: &SchedulerStore,
    options: SchedulerRunOptions,
) -> Result<Vec<ScheduledTaskRunRecord>> {
    let mut due = store.due_tasks(now_ms()).await?;
    due.sort_by_key(|task| task.next_run_at_ms);
    due.truncate(options.limit);
    let runs = futures::future::try_join_all(
        due.into_iter()
            .map(|task| run_task(Arc::clone(&harness), store, task)),
    )
    .await?;
    Ok(runs.into_iter().flatten().collect())
}

/// Run one due task. All coordination goes through the harness's turn
/// coordinator:
///
/// - The conversation lease is the run mutex. If the conversation is leased
///   (an interactive turn, or another scheduler runner), the task is skipped
///   this tick and stays due — no scheduler-specific locks.
/// - The report is a queued turn with an occurrence-scoped dedupe key, so a
///   racing runner that re-fires the same occurrence attaches to the same
///   pending turn instead of duplicating the report.
/// - Occurrence bookkeeping advances only after the report turn completes:
///   a crash anywhere re-fires the occurrence (at-least-once), and the
///   dedupe key collapses the report while the earlier one is still pending.
///
/// Returns `None` when the task was skipped.
pub async fn run_task(
    harness: Arc<dyn Harness>,
    store: &SchedulerStore,
    task: ScheduledTaskRecord,
) -> Result<Option<ScheduledTaskRunRecord>> {
    let coordinator = harness.turn_coordinator();
    let conversation_id = task
        .conversation_id
        .parse::<exoharness::ConversationId>()
        .with_context(|| {
            format!(
                "scheduled task conversation id is not valid: {}",
                task.conversation_id
            )
        })?;
    let Some(lease) = coordinator.claim_conversation(conversation_id).await? else {
        return Ok(None);
    };
    // Re-read under the lease: another runner may have completed this
    // occurrence between the due scan and our claim.
    let current = store.get_task(&task.id).await?;
    let Some(mut task) = current.filter(|task| task.is_due(now_ms())) else {
        release_lease(coordinator.as_ref(), &lease).await;
        return Ok(None);
    };
    let occurrence_ms = task.next_run_at_ms;

    let started_at_ms = now_ms();
    let run_id = Uuid7::now().to_string();
    let renewal = spawn_lease_renewal(Arc::clone(&coordinator), lease.clone());
    let run_result = run_task_inner(Arc::clone(&harness), &mut task, &run_id).await;
    renewal.abort();
    let finished_at_ms = now_ms();

    let (mut run, result_artifact_id, report_prompt) = match run_result {
        Ok(output) => {
            let stdout_bytes = output.stdout.len() as u64;
            let stderr_bytes = output.stderr.len() as u64;
            (
                ScheduledTaskRunRecord {
                    id: run_id,
                    task_id: task.id.clone(),
                    started_at_ms,
                    finished_at_ms,
                    exit_code: output.exit_code,
                    stdout_bytes,
                    stderr_bytes,
                    truncated: output.truncated,
                    result_artifact_id: output.result_artifact_id.clone(),
                    error: output.error.clone(),
                },
                output.result_artifact_id,
                Some(output.report_prompt),
            )
        }
        Err(error) => (
            ScheduledTaskRunRecord {
                id: run_id,
                task_id: task.id.clone(),
                started_at_ms,
                finished_at_ms,
                exit_code: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
                truncated: false,
                result_artifact_id: None,
                error: Some(error.to_string()),
            },
            None,
            None,
        ),
    };

    // The command is done: the lease's remaining job (serializing the report)
    // belongs to the drive loop below, which claims it per head turn.
    release_lease(coordinator.as_ref(), &lease).await;

    if let Some(prompt) = report_prompt {
        drive_report_turn(
            harness.as_ref(),
            coordinator.as_ref(),
            &task,
            conversation_id,
            occurrence_ms,
            prompt,
        )
        .await?;
    }

    task.mark_completed(&run, result_artifact_id, finished_at_ms)?;
    if run
        .error
        .as_deref()
        .is_some_and(is_missing_task_owner_error)
    {
        task.enabled = false;
        task.updated_at_ms = finished_at_ms;
    }
    run.task_id = task.id.clone();
    store.put_run(&run).await?;
    store.put_task(&task).await?;
    Ok(Some(run))
}

async fn release_lease(
    coordinator: &dyn exoharness::TurnCoordinator,
    lease: &exoharness::ConversationLease,
) {
    if let Err(error) = coordinator.release_idle(lease).await {
        tracing::error!(
            %error,
            conversation_id = %lease.conversation_id,
            "failed to release conversation lease"
        );
    }
}

/// Enqueue the report as a durable turn — deduplicated per occurrence, so a
/// racing runner attaches to the same pending turn — and drive the queue
/// until it completes. Foreign turns drained along the way fail softly.
async fn drive_report_turn(
    harness: &dyn Harness,
    coordinator: &dyn exoharness::TurnCoordinator,
    task: &ScheduledTaskRecord,
    conversation_id: exoharness::ConversationId,
    occurrence_ms: u64,
    prompt: String,
) -> Result<()> {
    let enqueued = coordinator
        .enqueue_turn(
            conversation_id,
            EnqueueTurnRequest {
                input: vec![Message::User {
                    content: UserContent::String(prompt),
                }],
                session_id: None,
                not_before: None,
                dedupe_key: Some(format!("scheduled-task:{}:{occurrence_ms}", task.id)),
            },
        )
        .await?;
    let conversation = harness
        .get_agent(&task.agent_id)
        .await?
        .ok_or_else(|| anyhow!("scheduled task agent does not exist: {}", task.agent_id))?
        .get_conversation(&task.conversation_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "scheduled task conversation does not exist: {}",
                task.conversation_id
            )
        })?;
    let result = loop {
        match conversation.run_next_pending_turn().await {
            Ok(Some(result)) if result.turn_id == enqueued.turn.id => break result,
            // A foreign turn was drained; keep going.
            Ok(Some(_)) => continue,
            // Nothing ran: the lease is held elsewhere, the queue drained
            // without our turn, or another driver already executed it.
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    %error,
                    "queued turn failed while driving a scheduled task report"
                );
            }
        }
        if let Some(result) = crate::harness_facade::finished_turn_result(
            conversation.exoharness_handle().as_ref(),
            enqueued.turn.id,
        )
        .await?
        {
            break result;
        }
        tokio::time::sleep(REPORT_DRIVE_INTERVAL).await;
    };
    conversation.close_session(result.session_id).await?;
    Ok(())
}

fn is_missing_task_owner_error(error: &str) -> bool {
    error.starts_with("scheduled task agent does not exist:")
        || error.starts_with("scheduled task conversation does not exist:")
}

struct TaskOutput {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    result_artifact_id: Option<String>,
    error: Option<String>,
    report_prompt: String,
}

async fn run_task_inner(
    harness: Arc<dyn Harness>,
    task: &mut ScheduledTaskRecord,
    run_id: &str,
) -> Result<TaskOutput> {
    let agent = harness
        .get_agent(&task.agent_id)
        .await?
        .ok_or_else(|| anyhow!("scheduled task agent does not exist: {}", task.agent_id))?;
    let conversation = agent
        .get_conversation(&task.conversation_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "scheduled task conversation does not exist: {}",
                task.conversation_id
            )
        })?;
    let agent_config = agent.config().await?;
    let conversation_config = conversation.config().await?;
    let conversation_handle = conversation.exoharness_handle();
    let agent_handle = agent.exoharness_handle();
    let sandbox = resolve_task_sandbox(
        task,
        agent_handle.as_ref(),
        std::sync::Arc::clone(&conversation_handle),
        &agent_config,
        &conversation_config,
    )
    .await?;
    let command_result: Result<CommandOutput> = async {
        let process = sandbox
            .run_in_sandbox(
                agent_handle.as_ref(),
                task.setup_command
                    .clone()
                    .unwrap_or_else(|| task.command.clone()),
            )
            .await?;
        let setup_output =
            read_process_output(process, task.max_output_bytes, DEFAULT_COMMAND_TIMEOUT_MS).await?;
        if task.setup_command.is_none() {
            return Ok(CommandOutput {
                sandbox_id: sandbox.sandbox_id().to_string(),
                setup: None,
                main: setup_output,
                error: None,
            });
        }
        if setup_output.exit_code != Some(0) {
            return Ok(CommandOutput {
                sandbox_id: sandbox.sandbox_id().to_string(),
                setup: Some(setup_output),
                main: ProcessOutput::empty(),
                error: Some("setup command exited non-zero".to_string()),
            });
        }
        let process = sandbox
            .run_in_sandbox(agent_handle.as_ref(), task.command.clone())
            .await?;
        let main_output =
            read_process_output(process, task.max_output_bytes, DEFAULT_COMMAND_TIMEOUT_MS).await?;
        Ok(CommandOutput {
            sandbox_id: sandbox.sandbox_id().to_string(),
            setup: Some(setup_output),
            main: main_output,
            error: None,
        })
    }
    .await;

    let (exit_code, stdout, stderr, truncated, error, setup, sandbox_id) = match command_result {
        Ok(output) => {
            let truncated =
                output.main.truncated || output.setup.as_ref().is_some_and(|setup| setup.truncated);
            (
                output.main.exit_code,
                output.main.stdout,
                output.main.stderr,
                truncated,
                output.error,
                output.setup,
                Some(output.sandbox_id),
            )
        }
        Err(error) => (
            None,
            Vec::new(),
            Vec::new(),
            false,
            Some(error.to_string()),
            None,
            None,
        ),
    };

    let artifact = ScheduledTaskArtifact {
        task_id: task.id.clone(),
        task_name: task.name.clone(),
        run_id: run_id.to_string(),
        sandbox_id,
        setup_command: task.setup_command.clone(),
        command: task.command.clone(),
        exit_code,
        setup_stdout: setup
            .as_ref()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned()),
        setup_stderr: setup
            .as_ref()
            .map(|output| String::from_utf8_lossy(&output.stderr).into_owned()),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated,
        error: error.clone(),
    };
    let artifact_version = conversation_handle
        .write_artifact(WriteArtifactRequest {
            path: format!("scheduled-tasks/{}/{run_id}.json", task.name),
            contents: serde_json::to_vec_pretty(&artifact)?,
        })
        .await?;
    let artifact_id = artifact_version.artifact_id.to_string();
    let report_prompt = if let Some(error) = &error {
        error_wakeup_prompt(task, run_id, error, &artifact_version)
    } else {
        wakeup_prompt(
            task,
            run_id,
            exit_code.expect("completed scheduled command has exit code"),
            truncated,
            &artifact_version,
            &stdout,
            &stderr,
        )
    };

    Ok(TaskOutput {
        exit_code,
        stdout,
        stderr,
        truncated,
        result_artifact_id: Some(artifact_id),
        error,
        report_prompt,
    })
}

async fn resolve_task_sandbox(
    task: &mut ScheduledTaskRecord,
    agent: &dyn exoharness::AgentHandle,
    conversation: std::sync::Arc<dyn exoharness::ConversationHandle>,
    agent_config: &crate::AgentConfig,
    conversation_config: &crate::ConversationConfig,
) -> Result<ResolvedTaskSandbox> {
    match task.sandbox_mode {
        ScheduledTaskSandboxMode::Agent => {
            let sandbox = ensure_agent_sandbox(agent, agent_config, conversation_config).await?;
            Ok(ResolvedTaskSandbox::Agent {
                sandbox_id: sandbox.sandbox_id,
            })
        }
        ScheduledTaskSandboxMode::Conversation => Ok(ResolvedTaskSandbox::Conversation {
            sandbox_id: ensure_conversation_sandbox(
                conversation.as_ref(),
                agent_config,
                conversation_config,
            )
            .await?,
            conversation,
        }),
        ScheduledTaskSandboxMode::TaskFresh => {
            if let Some(sandbox_id) = &task.task_sandbox_id {
                return Ok(ResolvedTaskSandbox::Conversation {
                    conversation,
                    sandbox_id: sandbox_id.clone(),
                });
            }
            let sandbox_id = create_conversation_sandbox(
                conversation.as_ref(),
                agent_config,
                conversation_config,
            )
            .await?;
            task.task_sandbox_id = Some(sandbox_id.clone());
            Ok(ResolvedTaskSandbox::Conversation {
                conversation,
                sandbox_id,
            })
        }
    }
}

enum ResolvedTaskSandbox {
    Agent {
        sandbox_id: String,
    },
    Conversation {
        conversation: std::sync::Arc<dyn exoharness::ConversationHandle>,
        sandbox_id: String,
    },
}

impl ResolvedTaskSandbox {
    fn sandbox_id(&self) -> &str {
        match self {
            Self::Agent { sandbox_id } | Self::Conversation { sandbox_id, .. } => sandbox_id,
        }
    }

    async fn run_in_sandbox(
        &self,
        agent: &dyn exoharness::AgentHandle,
        command: Vec<String>,
    ) -> Result<Box<dyn SandboxProcess>> {
        match self {
            Self::Agent { sandbox_id } => {
                agent
                    .run_in_sandbox(RunInSandboxRequest {
                        id: sandbox_id.clone(),
                        command,
                        env: Default::default(),
                    })
                    .await
            }
            Self::Conversation {
                conversation,
                sandbox_id,
            } => {
                conversation
                    .run_in_sandbox(RunInSandboxRequest {
                        id: sandbox_id.clone(),
                        command,
                        env: Default::default(),
                    })
                    .await
            }
        }
    }
}

async fn read_process_output(
    process: Box<dyn SandboxProcess>,
    max_stream_bytes: u64,
    timeout_ms: u64,
) -> Result<ProcessOutput> {
    let parts = process.into_parts();
    drop(parts.stdin);

    let read_output = async {
        let (stdout_result, stderr_result, exit_result) = tokio::join!(
            read_limited(parts.stdout, max_stream_bytes),
            read_limited(parts.stderr, max_stream_bytes),
            parts.wait,
        );
        let (stdout, stdout_truncated) = stdout_result?;
        let (stderr, stderr_truncated) = stderr_result?;
        let exit_code = exit_result?;
        Result::<_>::Ok((
            stdout,
            stdout_truncated,
            stderr,
            stderr_truncated,
            exit_code,
        ))
    };
    let (stdout, stdout_truncated, stderr, stderr_truncated, exit_code) =
        tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), read_output)
            .await
            .map_err(|_| anyhow!("scheduled task command timed out after {timeout_ms}ms"))??;
    let truncated = stdout_truncated || stderr_truncated;
    Ok(ProcessOutput {
        exit_code: Some(exit_code),
        stdout,
        stderr,
        truncated,
    })
}

struct CommandOutput {
    sandbox_id: String,
    setup: Option<ProcessOutput>,
    main: ProcessOutput,
    error: Option<String>,
}

#[derive(Debug)]
struct ProcessOutput {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

impl ProcessOutput {
    fn empty() -> Self {
        Self {
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
        }
    }
}

async fn read_limited<R>(mut reader: R, max_bytes: u64) -> Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(output.len() as u64) as usize;
        if read > remaining {
            output.extend_from_slice(&buffer[..remaining]);
            truncated = true;
            continue;
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok((output, truncated))
}

fn wakeup_prompt(
    task: &ScheduledTaskRecord,
    run_id: &str,
    exit_code: i32,
    truncated: bool,
    artifact: &exoharness::ArtifactVersion,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    format!(
        "Scheduled task `{}` completed.\n\nRun id: `{}`\nExit code: {}\nResult artifact: `{}` version {} at `{}`\nOutput truncated: {}\n\nReport instructions:\n{}\n\nstdout preview:\n{}\n\nstderr preview:\n{}",
        task.name,
        run_id,
        exit_code,
        artifact.artifact_id,
        artifact.version,
        artifact.path,
        truncated,
        task.report_prompt,
        preview(stdout),
        preview(stderr),
    )
}

fn error_wakeup_prompt(
    task: &ScheduledTaskRecord,
    run_id: &str,
    error: &str,
    artifact: &exoharness::ArtifactVersion,
) -> String {
    format!(
        "Scheduled task `{}` failed.\n\nRun id: `{}`\nResult artifact: `{}` version {} at `{}`\n\nReport instructions:\n{}\n\nError:\n{}",
        task.name,
        run_id,
        artifact.artifact_id,
        artifact.version,
        artifact.path,
        task.report_prompt,
        error,
    )
}

fn preview(bytes: &[u8]) -> String {
    const PREVIEW_BYTES: usize = 4_000;
    let end = bytes.len().min(PREVIEW_BYTES);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{future::FutureExt, io::Cursor};

    struct HangingSandboxProcess;

    impl SandboxProcess for HangingSandboxProcess {
        fn into_parts(self: Box<Self>) -> exoharness::SandboxProcessParts {
            exoharness::SandboxProcessParts {
                stdout: Box::pin(Cursor::new(Vec::new())),
                stderr: Box::pin(Cursor::new(Vec::new())),
                stdin: Box::pin(Cursor::new(Vec::new())),
                wait: async {
                    futures::future::pending::<()>().await;
                    Ok(0)
                }
                .boxed(),
            }
        }
    }

    #[tokio::test]
    async fn read_process_output_times_out() {
        let error = read_process_output(Box::new(HangingSandboxProcess), 1024, 1)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }
}
