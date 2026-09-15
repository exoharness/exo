use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use futures::{
    future::join_all,
    io::{AsyncReadExt, AsyncWriteExt},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    BoxAsyncRead, BoxAsyncWrite, BoxSandboxTcpStream, GetSandboxProcessEventsResult,
    ManagedSandboxHandle, SandboxAttachment, SandboxCommand, SandboxCommandOutput,
    SandboxProcessEvent, SandboxProcessId, SandboxProcessParts, SandboxProcessStatus,
    SandboxProcessStdin, SandboxTerminalParts, SandboxTerminalSize, SnapshotPayload, Uuid7,
};

/// Give a stream-backed provider ID-based process operations. Records live for
/// the lifetime of the returned sandbox handle; they cannot survive a host
/// process restart. Providers with their own process service implement the
/// operations directly instead of installing this adapter.
pub fn with_process_management(
    handle: Arc<dyn ManagedSandboxHandle>,
) -> Arc<dyn ManagedSandboxHandle> {
    Arc::new(ProcessManagedSandbox {
        handle,
        processes: ProcessRegistry::default(),
    })
}

struct ProcessManagedSandbox {
    handle: Arc<dyn ManagedSandboxHandle>,
    processes: ProcessRegistry,
}

#[async_trait]
impl ManagedSandboxHandle for ProcessManagedSandbox {
    fn id(&self) -> &str {
        self.handle.id()
    }
    fn provider_state(&self) -> Option<serde_json::Value> {
        self.handle.provider_state()
    }
    fn effective_image(&self) -> Option<String> {
        self.handle.effective_image()
    }
    async fn is_running(&self) -> Result<Option<bool>> {
        self.handle.is_running().await
    }
    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        self.handle.exec(command).await
    }
    async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
        self.handle.start_process(command).await
    }
    async fn start_managed_process(
        &self,
        command: &SandboxCommand,
        stdin: SandboxProcessStdin,
    ) -> Result<SandboxProcessId> {
        let parts = self.handle.start_process(command).await?;
        Ok(self.processes.start(parts, stdin))
    }
    async fn write_process_input(&self, id: &str, data: &[u8]) -> Result<()> {
        self.processes.get(id)?.write_input(data).await
    }
    async fn close_process_input(&self, id: &str) -> Result<()> {
        self.processes.get(id)?.stdin.lock().await.take();
        Ok(())
    }
    async fn process_events(&self, id: &str, after: u64) -> Result<GetSandboxProcessEventsResult> {
        Ok(self.processes.get(id)?.events(after))
    }
    async fn wait_process(&self, id: &str) -> Result<SandboxProcessStatus> {
        Ok(self.processes.get(id)?.wait().await)
    }
    async fn cancel_process(&self, id: &str) -> Result<SandboxProcessStatus> {
        let process = self.processes.get(id)?;
        process.cancel.cancel();
        Ok(process.wait().await)
    }
    async fn start_terminal(
        &self,
        command: &SandboxCommand,
        size: SandboxTerminalSize,
    ) -> Result<SandboxTerminalParts> {
        self.handle.start_terminal(command, size).await
    }
    fn supports_tcp(&self) -> bool {
        self.handle.supports_tcp()
    }
    async fn connect_tcp(&self, port: u16) -> Result<Option<BoxSandboxTcpStream>> {
        self.handle.connect_tcp(port).await
    }
    async fn stop(&self) -> Result<()> {
        self.handle.stop().await?;
        self.processes.cancel_all().await;
        Ok(())
    }
    async fn detach(&self) -> Result<SandboxAttachment> {
        self.handle.detach().await
    }
    async fn snapshot(&self) -> Result<SnapshotPayload> {
        self.handle.snapshot().await
    }
    async fn delete_snapshot(&self, payload: SnapshotPayload) -> Result<()> {
        self.handle.delete_snapshot(payload).await
    }
}

#[derive(Default)]
struct ProcessRegistry(Mutex<HashMap<SandboxProcessId, Arc<Process>>>);

impl ProcessRegistry {
    fn start(
        &self,
        parts: SandboxProcessParts,
        stdin_mode: SandboxProcessStdin,
    ) -> SandboxProcessId {
        let id = Uuid7::now().to_string();
        let process = Arc::new(Process {
            stdin: AsyncMutex::new(match stdin_mode {
                SandboxProcessStdin::Open => Some(parts.stdin),
                SandboxProcessStdin::None => None,
            }),
            state: Mutex::new(ProcessState {
                events: Vec::new(),
                status: SandboxProcessStatus::Running,
            }),
            cancel: CancellationToken::new(),
            exited: CancellationToken::new(),
            notify: Notify::new(),
        });
        self.0
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::clone(&process));
        tokio::spawn(async move {
            let (output_failure, mut output_failures) = mpsc::channel(2);
            let stdout = tokio::spawn(read_output(
                Arc::clone(&process),
                parts.stdout,
                true,
                output_failure.clone(),
            ));
            let stderr = tokio::spawn(read_output(
                Arc::clone(&process),
                parts.stderr,
                false,
                output_failure,
            ));
            let mut status = tokio::select! {
                biased;
                _ = process.cancel.cancelled() => SandboxProcessStatus::Cancelled,
                Some(message) = output_failures.recv() => SandboxProcessStatus::Failed { message },
                result = parts.wait => match result {
                    Ok(exit_code) => SandboxProcessStatus::Exited { exit_code },
                    Err(error) => SandboxProcessStatus::Failed { message: error.to_string() },
                },
            };
            process.exited.cancel();
            let stdout_abort = stdout.abort_handle();
            let stderr_abort = stderr.abort_handle();
            // A background child may retain a pipe after its parent exits.
            // Drain both streams concurrently, but do not wait for it forever.
            let drained = if matches!(status, SandboxProcessStatus::Failed { .. }) {
                None
            } else {
                tokio::select! {
                    biased;
                    _ = process.cancel.cancelled() => None,
                    Some(message) = output_failures.recv() => {
                        status = SandboxProcessStatus::Failed { message };
                        None
                    },
                    result = tokio::time::timeout(Duration::from_secs(2), async {
                        tokio::join!(stdout, stderr)
                    }) => result.ok(),
                }
            };
            let status = match drained {
                Some((stdout, stderr)) => {
                    let error = stdout
                        .map_err(anyhow::Error::from)
                        .and_then(|r| r)
                        .and(stderr.map_err(anyhow::Error::from).and_then(|r| r))
                        .err();
                    match error {
                        Some(error) if !matches!(status, SandboxProcessStatus::Cancelled) => {
                            SandboxProcessStatus::Failed {
                                message: error.to_string(),
                            }
                        }
                        _ => status,
                    }
                }
                None => {
                    stdout_abort.abort();
                    stderr_abort.abort();
                    status
                }
            };
            process.stdin.lock().await.take();
            let mut state = process.state.lock().unwrap();
            let cursor = state.events.len() as u64 + 1;
            state.events.push(match &status {
                SandboxProcessStatus::Exited { exit_code } => SandboxProcessEvent::Exit {
                    cursor,
                    exit_code: *exit_code,
                },
                SandboxProcessStatus::Failed { message } => SandboxProcessEvent::Error {
                    cursor,
                    message: message.clone(),
                },
                SandboxProcessStatus::Cancelled => SandboxProcessEvent::Cancelled { cursor },
                SandboxProcessStatus::Running => unreachable!(),
            });
            state.status = status;
            drop(state);
            process.notify.notify_waiters();
        });
        id
    }

    fn get(&self, id: &str) -> Result<Arc<Process>> {
        self.0
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("sandbox process not found: {id}"))
    }

    async fn cancel_all(&self) {
        let processes: Vec<_> = self.0.lock().unwrap().values().cloned().collect();
        for process in &processes {
            process.cancel.cancel();
        }
        join_all(processes.iter().map(|process| process.wait())).await;
    }
}

impl Drop for ProcessRegistry {
    fn drop(&mut self) {
        for process in self.0.get_mut().unwrap().values() {
            process.cancel.cancel();
        }
    }
}

struct Process {
    stdin: AsyncMutex<Option<BoxAsyncWrite>>,
    state: Mutex<ProcessState>,
    cancel: CancellationToken,
    exited: CancellationToken,
    notify: Notify,
}

struct ProcessState {
    events: Vec<SandboxProcessEvent>,
    status: SandboxProcessStatus,
}

impl Process {
    async fn write_input(&self, data: &[u8]) -> Result<()> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => bail!("sandbox process was cancelled"),
            _ = self.exited.cancelled() => bail!("sandbox process has exited"),
            result = async {
                let mut stdin = self.stdin.lock().await;
                let stdin = stdin.as_mut().ok_or_else(|| anyhow!("sandbox process stdin is closed"))?;
                stdin.write_all(data).await?;
                stdin.flush().await?;
                Ok(())
            } => result,
        }
    }

    fn events(&self, after: u64) -> GetSandboxProcessEventsResult {
        let state = self.state.lock().unwrap();
        let events: Vec<_> = state
            .events
            .iter()
            .filter(|event| event.cursor() > after)
            .cloned()
            .collect();
        GetSandboxProcessEventsResult {
            cursor: events.last().map(SandboxProcessEvent::cursor),
            events,
            status: state.status.clone(),
        }
    }

    async fn wait(&self) -> SandboxProcessStatus {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let status = self.state.lock().unwrap().status.clone();
            if !status.is_running() {
                return status;
            }
            notified.await;
        }
    }
}

async fn read_output(
    process: Arc<Process>,
    mut reader: BoxAsyncRead,
    stdout: bool,
    output_failure: mpsc::Sender<String>,
) -> Result<()> {
    let mut buffer = vec![0; 8192];
    loop {
        let length = match reader.read(&mut buffer).await {
            Ok(length) => length,
            Err(error) => {
                let _ = output_failure.send(error.to_string()).await;
                return Err(error.into());
            }
        };
        if length == 0 {
            return Ok(());
        }
        let mut state = process.state.lock().unwrap();
        if !state.status.is_running() {
            return Ok(());
        }
        let cursor = state.events.len() as u64 + 1;
        let data = buffer[..length].to_vec();
        state.events.push(if stdout {
            SandboxProcessEvent::Stdout { cursor, data }
        } else {
            SandboxProcessEvent::Stderr { cursor, data }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{future, io::Cursor};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    fn parts(
        stdout: &[u8],
        wait: futures::future::BoxFuture<'static, Result<i32>>,
    ) -> SandboxProcessParts {
        SandboxProcessParts {
            stdout: Box::pin(Cursor::new(stdout.to_vec())),
            stderr: Box::pin(Cursor::new(Vec::new())),
            stdin: Box::pin(futures::io::sink()),
            wait,
        }
    }

    #[tokio::test]
    async fn cancelled_process_retains_output_and_has_one_terminal_event() -> Result<()> {
        let registry = ProcessRegistry::default();
        let id = registry.start(
            parts(b"pending", Box::pin(future::pending())),
            SandboxProcessStdin::None,
        );
        let process = registry.get(&id)?;
        tokio::time::timeout(Duration::from_secs(1), async {
            while process.events(0).events.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        process.cancel.cancel();
        assert!(matches!(
            process.wait().await,
            SandboxProcessStatus::Cancelled
        ));
        process.cancel.cancel();
        let result = process.events(0);
        assert!(matches!(result.events.as_slice(), [
            SandboxProcessEvent::Stdout { cursor: 1, data },
            SandboxProcessEvent::Cancelled { cursor: 2 },
        ] if data == b"pending"));
        assert_eq!(result.cursor, Some(2));
        assert!(process.events(2).events.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn process_exit_is_emitted_even_if_an_output_stream_stays_open() -> Result<()> {
        let registry = ProcessRegistry::default();
        let (reader, _writer) = tokio::io::duplex(8);
        let mut pipes = parts(b"", Box::pin(async { Ok(0) }));
        pipes.stdout = Box::pin(reader.compat());
        let id = registry.start(pipes, SandboxProcessStdin::None);
        let process = registry.get(&id)?;
        let status = tokio::time::timeout(Duration::from_secs(3), process.wait()).await?;
        assert!(matches!(
            status,
            SandboxProcessStatus::Exited { exit_code: 0 }
        ));
        assert!(matches!(
            process.events(0).events.as_slice(),
            [SandboxProcessEvent::Exit {
                cursor: 1,
                exit_code: 0
            }]
        ));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_interrupts_blocked_stdin() -> Result<()> {
        let registry = ProcessRegistry::default();
        let (_reader, writer) = tokio::io::duplex(1);
        let mut pipes = parts(b"", Box::pin(future::pending()));
        pipes.stdin = Box::pin(writer.compat_write());
        let id = registry.start(pipes, SandboxProcessStdin::Open);
        let process = registry.get(&id)?;
        let write = process.write_input(b"cannot fit without a reader");
        tokio::pin!(write);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut write)
                .await
                .is_err()
        );
        process.cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut write)
                .await?
                .is_err()
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), process.wait()).await?,
            SandboxProcessStatus::Cancelled
        ));
        Ok(())
    }

    #[tokio::test]
    async fn local_backend_exposes_process_ids_and_enforces_sandbox_scope() -> Result<()> {
        use crate::{
            LocalProcessSandboxBackend, ManagedSandboxBackend, SandboxLifecycleConfig,
            SandboxNetworkPolicy, SandboxRequest, SandboxSpec,
        };
        let backend = LocalProcessSandboxBackend::new();
        let request = SandboxRequest {
            sandbox_id: Uuid7::now().to_string(),
            scope: None,
            spec: SandboxSpec {
                image: String::new(),
                resources: Default::default(),
                mounts: vec![],
                durable_file_systems: vec![],
                policy: SandboxNetworkPolicy::Unrestricted.into(),
                default_workdir: "/tmp".into(),
            },
            lifecycle: SandboxLifecycleConfig::default(),
            provider_state: None,
        };
        let sandbox = backend.acquire(request.clone()).await?;
        let other = backend
            .acquire(SandboxRequest {
                sandbox_id: Uuid7::now().to_string(),
                ..request
            })
            .await?;
        let id = sandbox
            .start_managed_process(
                &SandboxCommand {
                    argv: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "cat; printf stderr >&2; exit 7".into(),
                    ],
                    env: HashMap::new(),
                    display_argv: None,
                    cwd: None,
                    timeout: None,
                },
                SandboxProcessStdin::Open,
            )
            .await?;
        sandbox.write_process_input(&id, b"stdout").await?;
        sandbox.close_process_input(&id).await?;
        assert!(matches!(
            sandbox.wait_process(&id).await?,
            SandboxProcessStatus::Exited { exit_code: 7 }
        ));
        let result = sandbox.process_events(&id, 0).await?;
        let output: Vec<u8> = result
            .events
            .iter()
            .filter_map(|event| match event {
                SandboxProcessEvent::Stdout { data, .. } => Some(data.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect();
        assert_eq!(output, b"stdout");
        assert!(matches!(
            result.events.last(),
            Some(SandboxProcessEvent::Exit { exit_code: 7, .. })
        ));
        assert!(
            sandbox
                .process_events(&id, result.cursor.unwrap())
                .await?
                .events
                .is_empty()
        );
        assert!(other.process_events(&id, 0).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn cancellation_does_not_wait_for_inherited_output_pipes() -> Result<()> {
        let registry = ProcessRegistry::default();
        let (reader, _writer) = tokio::io::duplex(8);
        let mut pipes = parts(b"", Box::pin(future::pending()));
        pipes.stdout = Box::pin(reader.compat());
        let id = registry.start(pipes, SandboxProcessStdin::None);
        let process = registry.get(&id)?;
        process.cancel.cancel();
        let status = tokio::time::timeout(Duration::from_millis(500), process.wait()).await?;
        assert!(matches!(status, SandboxProcessStatus::Cancelled));
        Ok(())
    }

    #[tokio::test]
    async fn failed_output_terminates_a_process_that_has_not_exited() -> Result<()> {
        use futures::TryStreamExt;
        let registry = ProcessRegistry::default();
        let mut pipes = parts(b"", Box::pin(future::pending()));
        pipes.stdout = Box::pin(
            futures::stream::iter([Err::<Vec<u8>, _>(std::io::Error::other(
                "output disconnected",
            ))])
            .into_async_read(),
        );
        let id = registry.start(pipes, SandboxProcessStdin::None);
        let process = registry.get(&id)?;
        let status = tokio::time::timeout(Duration::from_millis(500), process.wait()).await?;
        assert!(
            matches!(status, SandboxProcessStatus::Failed { message } if message == "output disconnected")
        );
        assert!(matches!(
            process.events(0).events.as_slice(),
            [SandboxProcessEvent::Error { cursor: 1, .. }]
        ));
        Ok(())
    }
}
