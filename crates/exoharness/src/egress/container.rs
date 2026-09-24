use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex as AsyncMutex;

use super::{
    EgressCredentialResolver, EgressIdentity, PublicUpstreamResolver, State,
    explicit::ExplicitProxy,
};
use crate::{
    CliContainerSandboxBackend, ManagedSandboxBackend, ManagedSandboxHandle, SandboxAttachment,
    SandboxCommand, SandboxCommandOutput, SandboxNetworkPolicy, SandboxProcessParts,
    SandboxProvider, SandboxRequest, SnapshotFormat, SnapshotPayload,
};

pub(crate) struct CredentialContainerBackend {
    inner: Arc<dyn ManagedSandboxBackend>,
    provider: SandboxProvider,
    resolver: Arc<dyn EgressCredentialResolver>,
    sandboxes: Mutex<HashMap<String, SandboxEntry>>,
}

type SandboxEntry = Arc<AsyncMutex<Option<Arc<CredentialSandbox>>>>;

impl CredentialContainerBackend {
    pub(crate) fn new(
        provider: SandboxProvider,
        resolver: Arc<dyn EgressCredentialResolver>,
    ) -> Self {
        let inner = if provider == SandboxProvider::Docker {
            CliContainerSandboxBackend::docker()
        } else {
            CliContainerSandboxBackend::apple_container()
        };
        Self {
            inner: Arc::new(inner),
            provider,
            resolver,
            sandboxes: Mutex::new(HashMap::new()),
        }
    }

    fn sandbox_entry(&self, sandbox_id: &str) -> SandboxEntry {
        self.sandboxes
            .lock()
            .expect("credential sandbox registry poisoned")
            .entry(sandbox_id.to_owned())
            .or_default()
            .clone()
    }

    fn remove_entry(&self, sandbox_id: &str, entry: &SandboxEntry) {
        let mut sandboxes = self
            .sandboxes
            .lock()
            .expect("credential sandbox registry poisoned");
        if Arc::strong_count(entry) == 2 {
            sandboxes.remove(sandbox_id);
        }
    }

    async fn proxy_address(&self) -> Result<(Ipv4Addr, String)> {
        if self.provider == SandboxProvider::Docker && cfg!(target_os = "macos") {
            return Ok((Ipv4Addr::LOCALHOST, "host.docker.internal".into()));
        }
        #[derive(Deserialize)]
        struct AppleNetwork {
            status: AppleStatus,
        }
        #[derive(Deserialize)]
        struct AppleStatus {
            #[serde(rename = "ipv4Gateway")]
            gateway: String,
        }
        #[derive(Deserialize)]
        struct DockerNetwork {
            #[serde(rename = "IPAM")]
            ipam: DockerIpam,
        }
        #[derive(Deserialize)]
        struct DockerIpam {
            #[serde(rename = "Config")]
            config: Vec<DockerNetworkConfig>,
        }
        #[derive(Deserialize)]
        struct DockerNetworkConfig {
            #[serde(rename = "Gateway")]
            gateway: Option<String>,
        }
        let docker = self.provider == SandboxProvider::Docker;
        let output = tokio::process::Command::new(if docker { "docker" } else { "container" })
            .args([
                "network",
                "inspect",
                crate::sandbox::DEFAULT_ENABLED_NETWORK_NAME,
            ])
            .kill_on_drop(true)
            .output()
            .await?;
        ensure!(
            output.status.success(),
            "cannot inspect sandbox network: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let gateway = if docker {
            serde_json::from_slice::<Vec<DockerNetwork>>(&output.stdout)?
                .into_iter()
                .flat_map(|network| network.ipam.config)
                .find_map(|config| config.gateway)
        } else {
            serde_json::from_slice::<Vec<AppleNetwork>>(&output.stdout)?
                .into_iter()
                .next()
                .map(|network| network.status.gateway)
        }
        .context("sandbox network has no host gateway")?;
        let address = gateway
            .parse::<Ipv4Addr>()
            .context("sandbox host gateway must be IPv4")?;
        Ok((address, gateway))
    }
}

#[async_trait]
impl ManagedSandboxBackend for CredentialContainerBackend {
    fn is_local(&self) -> bool {
        true
    }
    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        self.inner.consumable_snapshot_formats()
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let entry = self.sandbox_entry(&request.sandbox_id);
        let mut current = entry.lock().await;
        if let Some(sandbox) = current.as_ref() {
            if !sandbox.proxy.is_closed() {
                ensure!(
                    sandbox.request.spec == request.spec && sandbox.request.scope == request.scope,
                    "stop the protected sandbox before changing its configuration"
                );
                return Ok(crate::with_process_management(sandbox.clone()));
            }
            *current = None;
        }
        let sandbox_id = request.sandbox_id.clone();
        let result = async {
            if request.spec.policy.credentials.is_empty() {
                return self.inner.acquire(request).await;
            }
            ensure!(
                request.spec.policy.networking == SandboxNetworkPolicy::Unrestricted,
                "{} credential proxy requires policy.networking.unrestricted; limited networking is only enforced by Firecracker",
                self.provider
            );
            ensure!(
                request.lifecycle.idle_ttl.is_some(),
                "credential proxy requires a managed sandbox lifecycle"
            );
            let state = State::new(
                EgressIdentity {
                    sandbox_id: request.sandbox_id.clone(),
                    scope: request.scope,
                },
                request.spec.policy.clone(),
                Some(self.resolver.clone()),
                Arc::new(PublicUpstreamResolver),
            )?;
            let mut container_request = request.clone();
            container_request.spec.policy.credentials.clear();
            let inner = self.inner.acquire(container_request.clone()).await?;
            let result = async {
                let (bind_address, host) = self.proxy_address().await?;
                let listener = tokio::net::TcpListener::bind((bind_address, 0)).await?;
                let proxy = ExplicitProxy::with_listener(state, listener, &host).await?;
                let sandbox = Arc::new(CredentialSandbox {
                    request: request.clone(),
                    inner,
                    proxy,
                });
                sandbox.prepare_trust().await?;
                Ok::<_, anyhow::Error>(sandbox)
            }
            .await;
            let sandbox = match result {
                Ok(sandbox) => sandbox,
                Err(error) => {
                    if let Err(cleanup) = self.inner.terminate(container_request).await {
                        tracing::warn!(%cleanup, "failed to clean up sandbox after proxy setup failed");
                    }
                    return Err(error);
                }
            };
            *current = Some(sandbox.clone());
            Ok(crate::with_process_management(sandbox))
        }
        .await;
        if current.is_none() {
            self.remove_entry(&sandbox_id, &entry);
        }
        result
    }

    async fn attach(
        &self,
        request: SandboxRequest,
        attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        self.inner.attach(request, attachment).await
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        self.inner.acquire_from_snapshot(request, payload).await
    }

    async fn terminate(&self, request: SandboxRequest) -> Result<()> {
        let entry = self.sandbox_entry(&request.sandbox_id);
        let mut current = entry.lock().await;
        if let Some(sandbox) = current.take() {
            sandbox.proxy.close();
        }
        let sandbox_id = request.sandbox_id.clone();
        let result = self.inner.terminate(request).await;
        self.remove_entry(&sandbox_id, &entry);
        result
    }
}

impl Drop for CredentialContainerBackend {
    fn drop(&mut self) {
        for entry in self
            .sandboxes
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            match entry.try_lock() {
                Ok(current) => {
                    if let Some(sandbox) = current.as_ref() {
                        sandbox.proxy.close();
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "credential sandbox still locked during cleanup")
                }
            }
        }
    }
}

struct CredentialSandbox {
    request: SandboxRequest,
    inner: Arc<dyn ManagedSandboxHandle>,
    proxy: ExplicitProxy,
}

impl CredentialSandbox {
    async fn prepare_trust(&self) -> Result<()> {
        ensure!(
            !self.proxy.is_closed(),
            "sandbox credential proxy is closed; acquire the sandbox again"
        );
        let output = self
            .inner
            .exec(&SandboxCommand {
                argv: vec!["/bin/sh".into(), "-c".into(), super::PREPARE_TRUST.into()],
                env: HashMap::from([
                    ("EXO_EGRESS_CA_PATH".into(), self.proxy.ca_path.clone()),
                    ("EXO_EGRESS_CA_PEM".into(), self.proxy.ca_pem.clone()),
                ]),
                display_argv: None,
                cwd: None,
                timeout: Some(Duration::from_secs(30)),
            })
            .await?;
        ensure!(
            output.ok,
            "could not prepare sandbox TLS trust: {}",
            output.stderr
        );
        Ok(())
    }
}

#[async_trait]
impl ManagedSandboxHandle for CredentialSandbox {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn effective_image(&self) -> Option<String> {
        self.inner.effective_image()
    }
    async fn is_running(&self) -> Result<Option<bool>> {
        if self.proxy.is_closed() {
            return Ok(Some(false));
        }
        self.inner.is_running().await
    }
    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        self.prepare_trust().await?;
        self.inner.exec(&self.proxy.command(command)?).await
    }
    async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
        self.prepare_trust().await?;
        self.inner
            .start_process(&self.proxy.command(command)?)
            .await
    }
    async fn stop(&self) -> Result<()> {
        self.proxy.close();
        self.inner.stop().await
    }
    async fn detach(&self) -> Result<SandboxAttachment> {
        anyhow::bail!("stop the credential-protected sandbox instead of detaching it")
    }
    async fn snapshot(&self) -> Result<SnapshotPayload> {
        self.inner.snapshot().await
    }
}

#[cfg(test)]
mod tests;
