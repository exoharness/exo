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
    CloseSandboxProcessInputRequest, ResourceScope, SandboxId, SandboxProcess, SandboxProcessEvent,
    SandboxProcessEventQuery, SandboxProcessId, SandboxProcessParts, SandboxProcessStatus,
    WriteSandboxProcessInputRequest,
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
    host.spawn(Box::pin(async move {
        let result = poll_events(
            output_harness,
            scope,
            output_sandbox,
            output_process,
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
            wait_rx
                .await
                .map_err(|_| anyhow!("HTTP sandbox process poller stopped"))?
        }),
    }))
}

async fn poll_events(
    harness: HttpExoHarness,
    scope: ResourceScope,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
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
            SandboxProcessStatus::Running => {}
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
