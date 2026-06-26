use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use exoharness::{RunInSandboxRequest, SandboxProcess, WriteArtifactRequest};
use futures::io::{AsyncRead, AsyncReadExt};
use serde::Serialize;

use crate::agent_sandbox::ensure_agent_sandbox;
use crate::conversation_sandbox::{create_conversation_sandbox, ensure_conversation_sandbox};
use crate::conversation_wakeup::send_conversation_wakeup;
use crate::scheduler_store::SchedulerStore;
use crate::scheduler_types::{
    DEFAULT_COMMAND_TIMEOUT_MS, DEFAULT_TASK_LEASE_MS, ScheduledTaskRecord, ScheduledTaskRunRecord,
    ScheduledTaskSandboxMode, now_ms,
};
use crate::work_source::{ClaimedWork, CompletionHook, StoreWorkSource, WorkOutcome, WorkSource};
use crate::{Harness, Uuid7};

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

/// Run all tasks due in the store-backed source.
///
/// This is the original entry point and is preserved verbatim in behavior: it
/// drains the single [`SchedulerStore`] source. It is now a thin wrapper over
/// [`run_due_tasks_from_sources`] with one [`StoreWorkSource`].
pub async fn run_due_tasks(
    harness: Arc<dyn Harness>,
    store: &SchedulerStore,
    options: SchedulerRunOptions,
) -> Result<Vec<ScheduledTaskRunRecord>> {
    let sources: Vec<Box<dyn WorkSource>> = vec![Box::new(StoreWorkSource::new(store.clone()))];
    run_due_tasks_from_sources(harness, store, &sources, options).await
}

/// Run all due work drained from a list of [`WorkSource`]s.
///
/// Each source is asked to atomically claim up to `limit` units of due work
/// (leased for [`DEFAULT_TASK_LEASE_MS`]); the claimed work is then run with the
/// exact same claim/lease/run/record contract exo has always used. The
/// `store` remains the system of record for the *run* (`put_run`/`put_task`),
/// regardless of which source produced the work; a source may additionally
/// observe completion via [`ClaimedWork::on_complete`].
pub async fn run_due_tasks_from_sources(
    harness: Arc<dyn Harness>,
    store: &SchedulerStore,
    sources: &[Box<dyn WorkSource>],
    options: SchedulerRunOptions,
) -> Result<Vec<ScheduledTaskRunRecord>> {
    let claimed = claim_from_sources(sources, now_ms(), options.limit).await?;
    futures::future::try_join_all(
        claimed
            .into_iter()
            .map(|work| run_claimed_work(Arc::clone(&harness), store, work)),
    )
    .await
}

/// Drain claimed work from each source in order, sharing one `limit` budget
/// across all sources. Returns at most `limit` units. Extracted so the
/// trait-dispatch contract (ordering + budget) is unit-testable without a live
/// sandbox/harness.
async fn claim_from_sources(
    sources: &[Box<dyn WorkSource>],
    now: u64,
    limit: usize,
) -> Result<Vec<ClaimedWork>> {
    let mut claimed: Vec<ClaimedWork> = Vec::new();
    for source in sources {
        let remaining = limit.saturating_sub(claimed.len());
        if remaining == 0 {
            break;
        }
        let from_source = source
            .claim_due(now, remaining, DEFAULT_TASK_LEASE_MS)
            .await
            .with_context(|| format!("work source `{}` failed to claim", source.name()))?;
        claimed.extend(from_source);
    }
    Ok(claimed)
}

/// Run one unit of claimed work and, if the source attached a completion hook,
/// acknowledge the outcome back to the source.
async fn run_claimed_work(
    harness: Arc<dyn Harness>,
    store: &SchedulerStore,
    work: ClaimedWork,
) -> Result<ScheduledTaskRunRecord> {
    let ClaimedWork {
        task, on_complete, ..
    } = work;
    let run = run_task(harness, store, task).await?;
    notify_completion(&run, on_complete).await;
    Ok(run)
}

/// Map a finished run to a [`WorkOutcome`] and fire the source's completion
/// hook, if any. A hook failure is logged, never fatal — exo has already
/// recorded the run; acknowledging the source is best-effort.
async fn notify_completion(run: &ScheduledTaskRunRecord, on_complete: Option<CompletionHook>) {
    let Some(hook) = on_complete else {
        return;
    };
    let outcome = match &run.error {
        Some(_) => WorkOutcome::Errored,
        None => WorkOutcome::Completed {
            exit_code: run.exit_code,
        },
    };
    if let Err(error) = hook(outcome).await {
        tracing::warn!(%error, run_id = %run.id, "work source completion hook failed");
    }
}

pub async fn run_task(
    harness: Arc<dyn Harness>,
    store: &SchedulerStore,
    mut task: ScheduledTaskRecord,
) -> Result<ScheduledTaskRunRecord> {
    let started_at_ms = now_ms();
    let run_id = Uuid7::now().to_string();
    let run_result = run_task_inner(Arc::clone(&harness), &mut task, &run_id).await;
    let finished_at_ms = now_ms();

    let (mut run, result_artifact_id) = match run_result {
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
        ),
    };

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
    Ok(run)
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
    let prompt = if let Some(error) = &error {
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
    send_conversation_wakeup(conversation.as_ref(), prompt).await?;

    Ok(TaskOutput {
        exit_code,
        stdout,
        stderr,
        truncated,
        result_artifact_id: Some(artifact_id),
        error,
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::scheduler_types::NewScheduledTask;
    use crate::work_source::ClaimedWork;
    use futures::{future::FutureExt, io::Cursor};

    fn mock_task(name: &str) -> ScheduledTaskRecord {
        ScheduledTaskRecord::new(
            NewScheduledTask {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: name.to_string(),
                schedule: "@every 1m".to_string(),
                sandbox_mode: None,
                setup_command: None,
                command: vec!["true".to_string()],
                report_prompt: "Report.".to_string(),
                max_output_bytes: None,
            },
            1,
        )
        .unwrap()
    }

    /// A mock [`WorkSource`] that yields a fixed number of tasks per claim and
    /// records how many units it was asked for, so we can assert the loop's
    /// per-source budget arithmetic.
    struct MockSource {
        name: String,
        tasks: Vec<ScheduledTaskRecord>,
        last_limit: Arc<AtomicUsize>,
    }

    impl MockSource {
        fn new(name: &str, count: usize) -> Self {
            Self {
                name: name.to_string(),
                tasks: (0..count)
                    .map(|i| mock_task(&format!("{name}-{i}")))
                    .collect(),
                last_limit: Arc::new(AtomicUsize::new(usize::MAX)),
            }
        }
    }

    #[async_trait::async_trait]
    impl WorkSource for MockSource {
        fn name(&self) -> &str {
            &self.name
        }

        async fn claim_due(
            &self,
            _now_ms: u64,
            limit: usize,
            _lease_ms: u64,
        ) -> Result<Vec<ClaimedWork>> {
            self.last_limit.store(limit, Ordering::SeqCst);
            Ok(self
                .tasks
                .iter()
                .take(limit)
                .cloned()
                .map(|task| ClaimedWork::from_store(self.name.clone(), task))
                .collect())
        }
    }

    #[tokio::test]
    async fn claim_from_sources_drains_in_order_and_shares_budget() {
        let first = MockSource::new("first", 3);
        let second = MockSource::new("second", 5);
        let second_limit = Arc::clone(&second.last_limit);
        let sources: Vec<Box<dyn WorkSource>> = vec![Box::new(first), Box::new(second)];

        let claimed = claim_from_sources(&sources, now_ms(), 5).await.unwrap();
        // First source gives 3; second source is asked for the remaining 2.
        assert_eq!(claimed.len(), 5);
        assert_eq!(second_limit.load(Ordering::SeqCst), 2);
        assert_eq!(claimed[0].source, "first");
        assert_eq!(claimed[3].source, "second");
    }

    #[tokio::test]
    async fn claim_from_sources_stops_when_budget_exhausted() {
        let first = MockSource::new("first", 5);
        let second = MockSource::new("second", 5);
        let second_limit = Arc::clone(&second.last_limit);
        let sources: Vec<Box<dyn WorkSource>> = vec![Box::new(first), Box::new(second)];

        let claimed = claim_from_sources(&sources, now_ms(), 5).await.unwrap();
        assert_eq!(claimed.len(), 5);
        // Budget exhausted by the first source; the second is never asked.
        assert_eq!(second_limit.load(Ordering::SeqCst), usize::MAX);
    }

    #[tokio::test]
    async fn notify_completion_fires_hook_with_outcome() {
        let fired = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&fired);
        let hook: CompletionHook = Box::new(move |outcome| {
            let captured = Arc::clone(&captured);
            Box::pin(async move {
                *captured.lock().unwrap() = Some(outcome);
                Ok(())
            })
        });
        let run = ScheduledTaskRunRecord {
            id: "run".to_string(),
            task_id: "task".to_string(),
            started_at_ms: 0,
            finished_at_ms: 1,
            exit_code: Some(0),
            stdout_bytes: 0,
            stderr_bytes: 0,
            truncated: false,
            result_artifact_id: None,
            error: None,
        };
        notify_completion(&run, Some(hook)).await;
        assert_eq!(
            *fired.lock().unwrap(),
            Some(WorkOutcome::Completed { exit_code: Some(0) })
        );
    }

    #[tokio::test]
    async fn notify_completion_maps_error_outcome() {
        let fired = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&fired);
        let hook: CompletionHook = Box::new(move |outcome| {
            let captured = Arc::clone(&captured);
            Box::pin(async move {
                *captured.lock().unwrap() = Some(outcome);
                Ok(())
            })
        });
        let run = ScheduledTaskRunRecord {
            id: "run".to_string(),
            task_id: "task".to_string(),
            started_at_ms: 0,
            finished_at_ms: 1,
            exit_code: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
            truncated: false,
            result_artifact_id: None,
            error: Some("boom".to_string()),
        };
        notify_completion(&run, Some(hook)).await;
        assert_eq!(*fired.lock().unwrap(), Some(WorkOutcome::Errored));
    }

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
