use crate::{ExoHttpProcessTransport, RunInSandboxRequest, StartSandboxProcessRequest};
use anyhow::anyhow;
use async_trait::async_trait;
use tokio::io::{AsyncReadExt as TokioAsyncReadExt, AsyncWriteExt as TokioAsyncWriteExt};
use tokio::sync::oneshot;
use tokio::time::{self, Duration};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::HTTP_EXOHARNESS_TRACING_TARGET;
use crate::HttpExoHarness;
use crate::ResourceScope;
use crate::protocol::{Request, Response};
use crate::{
    CloseSandboxProcessInputRequest, Result, SandboxId, SandboxProcess, SandboxProcessEvent,
    SandboxProcessEventQuery, SandboxProcessId, SandboxProcessParts, SandboxProcessStatus,
    WriteSandboxProcessInputRequest,
};

struct LiveHttpSandboxProcess {
    parts: Option<SandboxProcessParts>,
}

impl SandboxProcess for LiveHttpSandboxProcess {
    fn into_parts(mut self: Box<Self>) -> SandboxProcessParts {
        self.parts
            .take()
            .expect("live HTTP sandbox process parts already consumed")
    }
}

fn spawn_http_sandbox_process_event_poller(
    harness: HttpExoHarness,
    scope: ResourceScope,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
    mut stdout: tokio::io::DuplexStream,
    mut stderr: tokio::io::DuplexStream,
    wait_tx: oneshot::Sender<Result<i32>>,
) {
    tokio::spawn(async move {
        let mut cursor = None;
        let mut wait_tx = Some(wait_tx);
        loop {
            let query = SandboxProcessEventQuery {
                sandbox_id: sandbox_id.clone(),
                process_id: process_id.clone(),
                after: cursor,
                limit: None,
                follow: None,
            };
            let response = harness
                .request(Request::GetSandboxProcessEvents { scope, query })
                .await;
            let result = match response {
                Ok(Response::SandboxProcessEvents { result }) => result,
                Ok(response) => {
                    send_http_sandbox_process_wait_result(
                        &mut wait_tx,
                        Err(anyhow!(
                            "expected sandbox_process_events response, got {}",
                            response.kind()
                        )),
                    );
                    return;
                }
                Err(error) => {
                    send_http_sandbox_process_wait_result(&mut wait_tx, Err(error));
                    return;
                }
            };

            for event in result.events {
                cursor = Some(event.cursor());
                match event {
                    SandboxProcessEvent::Stdout { data, .. } => {
                        if let Err(error) = stdout.write_all(&data).await {
                            send_http_sandbox_process_wait_result(&mut wait_tx, Err(error.into()));
                            return;
                        }
                    }
                    SandboxProcessEvent::Stderr { data, .. } => {
                        if let Err(error) = stderr.write_all(&data).await {
                            send_http_sandbox_process_wait_result(&mut wait_tx, Err(error.into()));
                            return;
                        }
                    }
                    SandboxProcessEvent::Exit { exit_code, .. } => {
                        send_http_sandbox_process_wait_result(&mut wait_tx, Ok(exit_code));
                        return;
                    }
                    SandboxProcessEvent::Error { message, .. } => {
                        send_http_sandbox_process_wait_result(&mut wait_tx, Err(anyhow!(message)));
                        return;
                    }
                    SandboxProcessEvent::Cancelled { .. } => {
                        send_http_sandbox_process_wait_result(
                            &mut wait_tx,
                            Err(anyhow!("sandbox process was cancelled")),
                        );
                        return;
                    }
                }
            }

            match result.status {
                SandboxProcessStatus::Running => {
                    time::sleep(Duration::from_millis(50)).await;
                }
                SandboxProcessStatus::Exited { exit_code } => {
                    send_http_sandbox_process_wait_result(&mut wait_tx, Ok(exit_code));
                    return;
                }
                SandboxProcessStatus::Failed { message } => {
                    send_http_sandbox_process_wait_result(&mut wait_tx, Err(anyhow!(message)));
                    return;
                }
                SandboxProcessStatus::Cancelled => {
                    send_http_sandbox_process_wait_result(
                        &mut wait_tx,
                        Err(anyhow!("sandbox process was cancelled")),
                    );
                    return;
                }
            }
        }
    });
}

fn spawn_http_sandbox_process_stdin_forwarder(
    harness: HttpExoHarness,
    scope: ResourceScope,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
    mut stdin: tokio::io::DuplexStream,
) {
    tokio::spawn(async move {
        let mut buffer = vec![0; 8192];
        loop {
            match stdin.read(&mut buffer).await {
                Ok(0) => {
                    let request = CloseSandboxProcessInputRequest {
                        sandbox_id: sandbox_id.clone(),
                        process_id: process_id.clone(),
                    };
                    let response = harness
                        .request(Request::CloseSandboxProcessInput { scope, request })
                        .await;
                    if let Err(error) = response {
                        tracing::warn!(
                            target: HTTP_EXOHARNESS_TRACING_TARGET,
                            error = %error,
                            "failed to close HTTP sandbox process stdin"
                        );
                    }
                    return;
                }
                Ok(length) => {
                    let request = WriteSandboxProcessInputRequest {
                        sandbox_id: sandbox_id.clone(),
                        process_id: process_id.clone(),
                        data: buffer[..length].to_vec(),
                    };
                    let response = harness
                        .request(Request::WriteSandboxProcessInput { scope, request })
                        .await;
                    if let Err(error) = response {
                        tracing::warn!(
                            target: HTTP_EXOHARNESS_TRACING_TARGET,
                            error = %error,
                            "failed to write HTTP sandbox process stdin"
                        );
                        return;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: HTTP_EXOHARNESS_TRACING_TARGET,
                        error = %error,
                        "failed to read HTTP sandbox process stdin pipe"
                    );
                    return;
                }
            }
        }
    });
}

fn send_http_sandbox_process_wait_result(
    sender: &mut Option<oneshot::Sender<Result<i32>>>,
    result: Result<i32>,
) {
    if let Some(sender) = sender.take() {
        match sender.send(result) {
            Ok(()) => {}
            Err(_result) => {}
        }
    }
}

pub(super) struct HttpProcessTransport;

#[async_trait]
impl ExoHttpProcessTransport for HttpProcessTransport {
    async fn run_in_sandbox(
        &self,
        harness: &HttpExoHarness,
        scope: ResourceScope,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        let sandbox_id = request.id;
        let process = match harness
            .request(Request::StartSandboxProcess {
                scope,
                request: StartSandboxProcessRequest {
                    sandbox_id: sandbox_id.clone(),
                    name: None,
                    command: request.command,
                    env: request.env,
                    cwd: None,
                    mode: Default::default(),
                    stdin: Default::default(),
                    output: Default::default(),
                    lifecycle: Default::default(),
                },
            })
            .await?
        {
            Response::SandboxProcess { process } => process,
            response => anyhow::bail!("expected sandbox_process response, got {}", response.kind()),
        };
        let (stdout_reader, stdout_writer) = tokio::io::duplex(64 * 1024);
        let (stderr_reader, stderr_writer) = tokio::io::duplex(64 * 1024);
        let (stdin_reader, stdin_writer) = tokio::io::duplex(64 * 1024);
        let (wait_tx, wait_rx) = oneshot::channel();
        spawn_http_sandbox_process_event_poller(
            harness.clone(),
            scope,
            sandbox_id.clone(),
            process.id.clone(),
            stdout_writer,
            stderr_writer,
            wait_tx,
        );
        spawn_http_sandbox_process_stdin_forwarder(
            harness.clone(),
            scope,
            sandbox_id,
            process.id,
            stdin_reader,
        );
        Ok(Box::new(LiveHttpSandboxProcess {
            parts: Some(SandboxProcessParts {
                stdout: Box::pin(stdout_reader.compat()),
                stderr: Box::pin(stderr_reader.compat()),
                stdin: Box::pin(stdin_writer.compat_write()),
                wait: Box::pin(async move {
                    wait_rx
                        .await
                        .unwrap_or_else(|_| Err(anyhow!("HTTP sandbox process poller stopped")))
                }),
            }),
        }))
    }
}
