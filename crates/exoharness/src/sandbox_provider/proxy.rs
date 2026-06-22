//! Proxy sandbox backend: forwards each `exec` to an HTTP endpoint owned by a
//! host process, which runs the command in the real sandbox on exo's behalf.
//!
//! This is the seam that lets exo run as a *host-side* agent in frameworks where
//! the sandbox has no inbound network and runs no agent code (e.g. Horizon on
//! Harbor). exo runs on the networked host (so model calls work); its shell tool
//! calls back here, and the host bridge dispatches the command into the sandbox
//! via the framework's own `exec` (e.g. Harbor `environment.exec`).
//!
//! Protocol — `POST {exec_url}` with JSON `{command, argv, cwd, env, timeout_secs}`
//! and a JSON reply `{exit_code, stdout, stderr}`. Stateless: each request is one
//! command; there is no remote acquire/stop step (the host owns the sandbox).

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

const PIPE_BUFFER_SIZE: usize = 64 * 1024;

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxRequest, SnapshotPayload,
};
use crate::sandbox_provider::shell_quote;

pub struct ProxySandboxBackend {
    client: reqwest::Client,
    exec_url: String,
}

impl ProxySandboxBackend {
    pub fn new(exec_url: String) -> Result<Self> {
        let exec_url = exec_url.trim().to_string();
        if exec_url.is_empty() {
            bail!("proxy sandbox backend requires a non-empty exec URL (EXO_PROXY_EXEC_URL)");
        }
        let client = reqwest::Client::builder()
            .build()
            .context("building proxy sandbox HTTP client")?;
        Ok(Self { client, exec_url })
    }
}

#[async_trait]
impl ManagedSandboxBackend for ProxySandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        // No remote provisioning: the host already owns a live sandbox and is
        // serving the exec endpoint. The handle just carries the request spec.
        Ok(Arc::new(ProxySandboxHandle {
            id: format!("proxy:{}", request.key),
            client: self.client.clone(),
            exec_url: self.exec_url.clone(),
            request,
        }))
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("restore-from-snapshot is not supported by the proxy sandbox backend")
    }
}

struct ProxySandboxHandle {
    id: String,
    client: reqwest::Client,
    exec_url: String,
    request: SandboxRequest,
}

#[async_trait]
impl ManagedSandboxHandle for ProxySandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let (cwd, reply) = self.post_exec(command).await?;
        Ok(SandboxCommandOutput {
            ok: reply.exit_code == 0,
            exit_code: Some(reply.exit_code),
            stdout: reply.stdout,
            stderr: reply.stderr,
            command: command
                .display_argv
                .clone()
                .unwrap_or_else(|| command.argv.clone()),
            cwd,
        })
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        // The proxy protocol is one-shot, so we run the command to completion and
        // then present its buffered output as an already-finished "stream". This is
        // sufficient for the non-interactive shell tool (which uses this path); true
        // streaming/PTY would require a streaming host protocol.
        let (_cwd, reply) = self.post_exec(command).await?;

        let (stdout_reader, mut stdout_writer) = tokio::io::duplex(PIPE_BUFFER_SIZE);
        let (stderr_reader, mut stderr_writer) = tokio::io::duplex(PIPE_BUFFER_SIZE);
        let (stdin_reader, stdin_writer) = tokio::io::duplex(PIPE_BUFFER_SIZE);

        // Feed buffered output, then drop the writers to signal EOF. Spawned so a
        // payload larger than the pipe buffer can't deadlock on a slow reader.
        let out = reply.stdout.into_bytes();
        let err = reply.stderr.into_bytes();
        tokio::spawn(async move {
            let _ = stdout_writer.write_all(&out).await;
        });
        tokio::spawn(async move {
            let _ = stderr_writer.write_all(&err).await;
        });
        // The command already ran; drain any stdin the caller writes so its writes
        // don't error on a dropped reader.
        tokio::spawn(async move {
            let mut reader = stdin_reader;
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
        });

        let exit_code = reply.exit_code;
        let wait: BoxFuture<'static, Result<i32>> = Box::pin(async move { Ok(exit_code) });
        Ok(crate::SandboxProcessParts {
            stdout: Box::pin(stdout_reader.compat()),
            stderr: Box::pin(stderr_reader.compat()),
            stdin: Box::pin(stdin_writer.compat_write()),
            wait,
        })
    }

    async fn stop(&self) -> Result<()> {
        // The host owns the sandbox lifecycle; nothing to stop from here.
        Ok(())
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!("snapshots are not supported by the proxy sandbox backend")
    }
}

impl ProxySandboxHandle {
    /// POST one command to the host exec endpoint; returns the resolved cwd and reply.
    async fn post_exec(&self, command: &SandboxCommand) -> Result<(String, ProxyExecResponse)> {
        if command.argv.is_empty() {
            bail!("sandbox command requires at least one argv entry");
        }
        let cwd = command
            .cwd
            .clone()
            .unwrap_or_else(|| self.request.spec.default_workdir.clone());
        let rendered = command
            .argv
            .iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let body = ProxyExecRequest {
            command: rendered,
            argv: command.argv.clone(),
            cwd: cwd.clone(),
            env: command.env.clone(),
            timeout_secs: command.timeout.map(|t| t.as_secs()),
        };
        let response = self
            .client
            .post(&self.exec_url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("posting exec to proxy endpoint {}", self.exec_url))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("proxy exec endpoint returned an error ({status}): {text}");
        }
        let reply: ProxyExecResponse = response
            .json()
            .await
            .context("decoding proxy exec response")?;
        Ok((cwd, reply))
    }
}

#[derive(Debug, Serialize)]
struct ProxyExecRequest {
    /// Shell-quoted single string (convenience for `environment.exec`).
    command: String,
    /// Raw argv, for hosts that prefer to exec without a shell.
    argv: Vec<String>,
    cwd: String,
    env: std::collections::HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ProxyExecResponse {
    #[serde(default)]
    exit_code: i32,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
}

/// Default env var the CLI reads the proxy exec URL from.
pub const PROXY_EXEC_URL_ENV: &str = "EXO_PROXY_EXEC_URL";
