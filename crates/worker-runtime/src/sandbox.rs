use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Result, bail, ensure};
use async_trait::async_trait;
use executor::runtime_host::RuntimeHost;
use exoharness::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxAttachment, SandboxCommand,
    SandboxCommandOutput, SandboxProcessParts, SandboxRequest, SnapshotFormat, SnapshotPayload,
};
use futures::future::BoxFuture;
use futures::io::AsyncWrite;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use crate::host::{Host, HostRequest};

const SNAPSHOT_FORMAT: SnapshotFormat = SnapshotFormat::from_static("cloudflare-snapshot-v1");

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Command {
    Acquire {
        request: SandboxRequest,
        snapshot: Option<Snapshot>,
    },
    Info {
        request: SandboxRequest,
    },
    Stop {
        request: SandboxRequest,
        terminate: bool,
    },
    Exec {
        request: SandboxRequest,
        command: SandboxCommand,
    },
    Start {
        request: SandboxRequest,
        command: SandboxCommand,
    },
    Read {
        process_id: String,
        stream: OutputStream,
    },
    Write {
        process_id: String,
        bytes: Vec<u8>,
    },
    CloseInput {
        process_id: String,
    },
    Wait {
        process_id: String,
    },
    Close {
        process_id: String,
    },
    Snapshot {
        request: SandboxRequest,
    },
}
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub id: String,
}
#[derive(Deserialize)]
struct Info {
    running: bool,
}

static SNAPSHOT_FORMATS: [SnapshotFormat; 1] = [SNAPSHOT_FORMAT];

pub(crate) struct CloudflareBackend(pub Arc<Host>);
impl CloudflareBackend {
    async fn acquire(
        &self,
        request: SandboxRequest,
        snapshot: Option<Snapshot>,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        ensure!(
            matches!(request.scope, exoharness::ResourceScope::Thread { .. }),
            "Cloudflare sandboxes require a thread scope"
        );
        ensure!(
            request.spec.image.is_empty() || request.spec.image == "cloudflare/debian-trixie",
            "custom Cloudflare sandbox images are not supported"
        );
        ensure!(
            request.spec.mounts.is_empty()
                && request.spec.durable_file_systems.is_empty()
                && request.spec.tcp_ports.is_empty()
                && request.spec.resources.is_none(),
            "Cloudflare sandbox mounts, durable filesystems, TCP ports and resource overrides are not supported"
        );
        ensure!(
            request.lifecycle.idle_ttl.is_some_and(|ttl| !ttl.is_zero()),
            "Cloudflare sandboxes require a positive idle timeout"
        );
        self.0
            .call::<()>(HostRequest::Sandbox {
                command: Command::Acquire {
                    request: request.clone(),
                    snapshot,
                },
            })
            .await?;
        Ok(Arc::new(CloudflareHandle {
            host: self.0.clone(),
            request,
        }))
    }
}
#[async_trait]
impl ManagedSandboxBackend for CloudflareBackend {
    fn is_local(&self) -> bool {
        false
    }
    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &SNAPSHOT_FORMATS
    }
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        self.acquire(request, None).await
    }
    async fn resolve_existing(
        &self,
        _id: &str,
        _request: Option<&SandboxRequest>,
        previous: &Arc<dyn ManagedSandboxHandle>,
    ) -> Result<Option<Arc<dyn ManagedSandboxHandle>>> {
        Ok(previous
            .is_running()
            .await?
            .unwrap_or(false)
            .then(|| previous.clone()))
    }
    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Cloudflare sandbox attachment is not supported")
    }
    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        ensure!(
            payload.format == SNAPSHOT_FORMAT,
            "unsupported Cloudflare snapshot format"
        );
        self.acquire(request, Some(serde_json::from_slice(&payload.bytes)?))
            .await
    }
    async fn terminate(&self, request: SandboxRequest) -> Result<()> {
        self.0
            .call(HostRequest::Sandbox {
                command: Command::Stop {
                    request,
                    terminate: true,
                },
            })
            .await
    }
}
struct CloudflareHandle {
    host: Arc<Host>,
    request: SandboxRequest,
}
#[async_trait]
impl ManagedSandboxHandle for CloudflareHandle {
    fn id(&self) -> &str {
        &self.request.sandbox_id
    }
    fn effective_image(&self) -> Option<String> {
        Some("cloudflare/debian-trixie".into())
    }
    async fn command_environment(&self) -> Result<HashMap<String, String>> {
        Ok(HashMap::new())
    }
    async fn is_running(&self) -> Result<Option<bool>> {
        let info: Info = self
            .host
            .call(HostRequest::Sandbox {
                command: Command::Info {
                    request: self.request.clone(),
                },
            })
            .await?;
        Ok(Some(info.running))
    }
    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        self.host
            .call(HostRequest::Sandbox {
                command: Command::Exec {
                    request: self.request.clone(),
                    command: command.clone(),
                },
            })
            .await
    }
    async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
        let id: String = self
            .host
            .call(HostRequest::Sandbox {
                command: Command::Start {
                    request: self.request.clone(),
                    command: command.clone(),
                },
            })
            .await?;
        let reader = |output| {
            let host = self.host.clone();
            let id = id.clone();
            Box::pin(
                futures::stream::try_unfold((host, id), move |(host, id)| async move {
                    let bytes: Option<Vec<u8>> = host
                        .call(HostRequest::Sandbox {
                            command: Command::Read {
                                process_id: id.clone(),
                                stream: output,
                            },
                        })
                        .await
                        .map_err(std::io::Error::other)?;
                    Ok(bytes.map(|bytes| (bytes, (host, id))))
                })
                .boxed()
                .into_async_read(),
            ) as exoharness::BoxAsyncRead
        };
        let host = self.host.clone();
        let guard = ProcessGuard {
            host: host.clone(),
            id: id.clone(),
            finished: false,
        };
        let wait_id = id.clone();
        Ok(SandboxProcessParts {
            stdout: reader(OutputStream::Stdout),
            stderr: reader(OutputStream::Stderr),
            stdin: Box::pin(ProcessInput {
                host: host.clone(),
                id,
                pending: None,
                closing: false,
            }),
            wait: Box::pin(async move {
                let mut guard = guard;
                let exit = host
                    .call(HostRequest::Sandbox {
                        command: Command::Wait {
                            process_id: wait_id,
                        },
                    })
                    .await;
                guard.finished = exit.is_ok();
                exit
            }),
        })
    }
    async fn stop(&self) -> Result<()> {
        self.host
            .call(HostRequest::Sandbox {
                command: Command::Stop {
                    request: self.request.clone(),
                    terminate: false,
                },
            })
            .await
    }
    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("Cloudflare sandbox detach is not supported")
    }
    async fn snapshot(&self) -> Result<SnapshotPayload> {
        let snapshot: Snapshot = self
            .host
            .call(HostRequest::Sandbox {
                command: Command::Snapshot {
                    request: self.request.clone(),
                },
            })
            .await?;
        Ok(SnapshotPayload {
            format: SNAPSHOT_FORMAT,
            bytes: serde_json::to_vec(&snapshot)?.into(),
        })
    }
}
struct ProcessGuard {
    host: Arc<Host>,
    id: String,
    finished: bool,
}
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if !self.finished {
            let host = self.host.clone();
            let id = self.id.clone();
            self.host.spawn(Box::pin(async move {
                if let Err(error) = host
                    .call::<()>(HostRequest::Sandbox {
                        command: Command::Close { process_id: id },
                    })
                    .await
                {
                    tracing::warn!(%error, "failed to close sandbox process");
                }
            }));
        }
    }
}
struct ProcessInput {
    host: Arc<Host>,
    id: String,
    pending: Option<BoxFuture<'static, std::io::Result<usize>>>,
    closing: bool,
}
impl Drop for ProcessInput {
    fn drop(&mut self) {
        if !self.closing {
            let host = self.host.clone();
            let id = self.id.clone();
            self.host.spawn(Box::pin(async move {
                if let Err(error) = host
                    .call::<()>(HostRequest::Sandbox {
                        command: Command::CloseInput { process_id: id },
                    })
                    .await
                {
                    tracing::warn!(%error, "failed to close sandbox process input");
                }
            }));
        }
    }
}
impl ProcessInput {
    fn poll_command(
        &mut self,
        cx: &mut TaskContext<'_>,
        command: Command,
        length: usize,
    ) -> Poll<std::io::Result<usize>> {
        let host = self.host.clone();
        let pending = self.pending.get_or_insert_with(|| {
            Box::pin(async move {
                host.call::<()>(HostRequest::Sandbox { command })
                    .await
                    .map_err(std::io::Error::other)?;
                Ok(length)
            })
        });
        let result = pending.as_mut().poll(cx);
        if result.is_ready() {
            self.pending = None;
        }
        result
    }
}
impl AsyncWrite for ProcessInput {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.closing {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        let command = Command::Write {
            process_id: self.id.clone(),
            bytes: bytes.to_vec(),
        };
        self.poll_command(cx, command, bytes.len())
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        if let Some(pending) = self.pending.as_mut() {
            let result = std::task::ready!(pending.as_mut().poll(cx));
            self.pending = None;
            result?;
        }
        Poll::Ready(Ok(()))
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        if !self.closing {
            std::task::ready!(self.as_mut().poll_flush(cx))?;
            self.closing = true;
        }
        let command = Command::CloseInput {
            process_id: self.id.clone(),
        };
        self.poll_command(cx, command, 0).map_ok(|_| ())
    }
}
