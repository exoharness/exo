use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{Result, anyhow};
use futures::channel::{mpsc, oneshot};
use futures::io::AsyncWrite;
use futures::{Sink, SinkExt, StreamExt, TryStreamExt};

use super::client::HttpExoHarness;
use crate::runtime_host::RuntimeHost;
use crate::{
    CancelSandboxProcessRequest, CloseSandboxProcessInputRequest, ResourceScope, SandboxId,
    SandboxProcess, SandboxProcessEvent, SandboxProcessEventQuery, SandboxProcessId,
    SandboxProcessParts, SandboxProcessStatus, WriteSandboxProcessInputRequest,
};

struct LiveHttpSandboxProcess(SandboxProcessParts);
impl SandboxProcess for LiveHttpSandboxProcess {
    fn into_parts(self: Box<Self>) -> SandboxProcessParts {
        self.0
    }
}

pub(super) fn open(
    harness: HttpExoHarness,
    host: Arc<dyn RuntimeHost>,
    scope: ResourceScope,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
) -> Box<dyn SandboxProcess> {
    let guard = CancelProcess {
        harness: harness.clone(),
        host: host.clone(),
        scope,
        sandbox_id: sandbox_id.clone(),
        process_id: process_id.clone(),
        finished: false,
    };
    let (stdout_tx, stdout_rx) = mpsc::channel(8);
    let (stderr_tx, stderr_rx) = mpsc::channel(8);
    let (stdin_tx, mut stdin_rx) = mpsc::channel(8);
    let (wait_tx, wait_rx) = oneshot::channel();
    let reader = |rx: mpsc::Receiver<Vec<u8>>| {
        Box::pin(rx.map(Ok::<_, std::io::Error>).into_async_read()) as crate::BoxAsyncRead
    };
    let output_harness = harness.clone();
    let output_sandbox = sandbox_id.clone();
    let output_process = process_id.clone();
    let host_for_polls = host.clone();
    host.spawn(Box::pin(async move {
        let result = poll_events(
            output_harness,
            scope,
            output_sandbox,
            output_process,
            host_for_polls,
            stdout_tx,
            stderr_tx,
        )
        .await;
        if wait_tx.send(result).is_err() {
            tracing::trace!("HTTP sandbox process waiter closed");
        }
    }));
    host.spawn(Box::pin(async move {
        let result: Result<()> = async {
            while let Some(data) = stdin_rx.next().await {
                super::client::http_write_sandbox_process_input(
                    &harness,
                    scope,
                    WriteSandboxProcessInputRequest {
                        sandbox_id: sandbox_id.clone(),
                        process_id: process_id.clone(),
                        data,
                    },
                )
                .await?;
            }
            super::client::http_close_sandbox_process_input(
                &harness,
                scope,
                CloseSandboxProcessInputRequest {
                    sandbox_id,
                    process_id,
                },
            )
            .await
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, "HTTP sandbox process stdin failed");
        }
    }));
    Box::new(LiveHttpSandboxProcess(SandboxProcessParts {
        stdout: reader(stdout_rx),
        stderr: reader(stderr_rx),
        stdin: Box::pin(ProcessInput(stdin_tx)),
        wait: Box::pin(async move {
            let mut guard = guard;
            let result = wait_rx
                .await
                .map_err(|_| anyhow!("HTTP sandbox process poller stopped"))?;
            // Losing the observer to a transport error does not stop the process.
            guard.finished = true;
            result
        }),
    }))
}

async fn poll_events(
    harness: HttpExoHarness,
    scope: ResourceScope,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
    host: Arc<dyn RuntimeHost>,
    mut stdout: mpsc::Sender<Vec<u8>>,
    mut stderr: mpsc::Sender<Vec<u8>>,
) -> Result<i32> {
    let mut cursor = None;
    loop {
        let result = super::client::http_get_sandbox_process_events(
            &harness,
            scope,
            SandboxProcessEventQuery {
                sandbox_id: sandbox_id.clone(),
                process_id: process_id.clone(),
                after: cursor,
                limit: None,
                follow: Some(true),
            },
        )
        .await?;
        let empty = result.events.is_empty();
        for event in result.events {
            cursor = Some(event.cursor());
            match event {
                SandboxProcessEvent::Stdout { data, .. } => stdout.send(data).await?,
                SandboxProcessEvent::Stderr { data, .. } => stderr.send(data).await?,
                SandboxProcessEvent::Exit { exit_code, .. } => return Ok(exit_code),
                SandboxProcessEvent::Error { message, .. } => return Err(anyhow!(message)),
                SandboxProcessEvent::Cancelled { .. } => {
                    return Err(anyhow!("sandbox process was cancelled"));
                }
            }
        }
        match result.status {
            SandboxProcessStatus::Running => {
                // An HTTP transport may return an empty page without long-polling.
                if empty {
                    host.sleep(std::time::Duration::from_millis(50)).await?;
                }
            }
            SandboxProcessStatus::Exited { exit_code } => return Ok(exit_code),
            SandboxProcessStatus::Failed { message } => return Err(anyhow!(message)),
            SandboxProcessStatus::Cancelled => {
                return Err(anyhow!("sandbox process was cancelled"));
            }
        }
    }
}

struct ProcessInput(mpsc::Sender<Vec<u8>>);
impl AsyncWrite for ProcessInput {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        futures::ready!(Pin::new(&mut self.0).poll_ready(cx)).map_err(std::io::Error::other)?;
        Pin::new(&mut self.0)
            .start_send(bytes.to_vec())
            .map_err(std::io::Error::other)?;
        Poll::Ready(Ok(bytes.len()))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0)
            .poll_flush(cx)
            .map_err(std::io::Error::other)
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0)
            .poll_close(cx)
            .map_err(std::io::Error::other)
    }
}

// Attached processes must stop when their caller drops the wait future, including
// cancellation during resource preparation before the executor is initialized.
struct CancelProcess {
    harness: HttpExoHarness,
    host: Arc<dyn RuntimeHost>,
    scope: ResourceScope,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
    finished: bool,
}
impl Drop for CancelProcess {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let harness = self.harness.clone();
        let scope = self.scope;
        let request = CancelSandboxProcessRequest {
            sandbox_id: self.sandbox_id.clone(),
            process_id: self.process_id.clone(),
            signal: None,
        };
        let task = Box::pin(async move {
            if let Err(error) =
                super::client::http_cancel_sandbox_process(&harness, scope, request).await
            {
                tracing::warn!(%error, "failed to cancel abandoned HTTP sandbox process");
            }
        });
        self.host.spawn(task);
    }
}
