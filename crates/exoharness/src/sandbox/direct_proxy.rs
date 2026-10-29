use super::*;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::SandboxAttachment;
use crate::egress::{DirectExternalProxy, ExternalProxyConfig};

/// Gives any backend an external proxy by installing it directly in each
/// sandbox: the proxy CA joins the guest trust bundle whenever a handle is
/// produced, and every command the handle runs carries the proxy environment.
/// Backends with an in-sandbox egress proxy implement `with_external_proxy`
/// themselves; backends whose guests must reach the proxy directly return this.
pub struct DirectExternalProxyBackend {
    inner: Arc<dyn ManagedSandboxBackend>,
    proxy: ExternalProxyConfig,
}

impl DirectExternalProxyBackend {
    pub fn wrap(
        inner: Arc<dyn ManagedSandboxBackend>,
        proxy: ExternalProxyConfig,
    ) -> Result<Arc<dyn ManagedSandboxBackend>> {
        proxy.validate()?;
        Ok(Arc::new(Self { inner, proxy }))
    }

    async fn install(
        &self,
        sandbox_id: &str,
        handle: Arc<dyn ManagedSandboxHandle>,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let egress = DirectExternalProxy::new(
            self.proxy.clone(),
            format!(
                "/tmp/exo-egress-{}.pem",
                super::native::stable_fnv1a_hex(sandbox_id)
            ),
        );
        egress.install(handle.as_ref()).await?;
        Ok(Arc::new(DirectExternalProxyHandle {
            inner: handle,
            egress,
        }))
    }
}

#[async_trait]
impl ManagedSandboxBackend for DirectExternalProxyBackend {
    fn with_external_proxy(
        &self,
        proxy: ExternalProxyConfig,
    ) -> Result<Arc<dyn ManagedSandboxBackend>> {
        Self::wrap(Arc::clone(&self.inner), proxy)
    }

    async fn materialize_resources(
        &self,
        request: crate::resources::MaterializeResourcesRequest,
    ) -> Result<Vec<crate::FileSystemMount>> {
        self.inner.materialize_resources(request).await
    }

    async fn remove_thread_resources(
        &self,
        agent: crate::AgentId,
        thread: crate::ThreadId,
    ) -> Result<()> {
        self.inner.remove_thread_resources(agent, thread).await
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }

    fn capabilities(&self) -> SandboxBackendCapabilities {
        self.inner.capabilities()
    }

    fn retains_disk_when_stopped(&self) -> bool {
        self.inner.retains_disk_when_stopped()
    }

    fn stop_requires_provider_state(&self) -> bool {
        self.inner.stop_requires_provider_state()
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        self.inner.consumable_snapshot_formats()
    }

    async fn resolve_image(&self, image: &str) -> Result<ResolvedSandboxImage> {
        self.inner.resolve_image(image).await
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let handle = self.inner.acquire(request.clone()).await?;
        self.install(request.sandbox_id.as_str(), handle).await
    }

    async fn connect_tcp(
        &self,
        request: SandboxRequest,
        port: u16,
    ) -> Result<Option<BoxSandboxTcpStream>> {
        self.inner.connect_tcp(request, port).await
    }

    async fn stop(&self, request: SandboxRequest) -> Result<()> {
        self.inner.stop(request).await
    }

    async fn resolve_existing(
        &self,
        sandbox_id: &str,
        request: Option<&SandboxRequest>,
        previous: &Arc<dyn ManagedSandboxHandle>,
    ) -> Result<Option<Arc<dyn ManagedSandboxHandle>>> {
        match self
            .inner
            .resolve_existing(sandbox_id, request, previous)
            .await?
        {
            Some(handle) if Arc::ptr_eq(&handle, previous) => Ok(Some(handle)),
            Some(handle) => Ok(Some(self.install(sandbox_id, handle).await?)),
            None => Ok(None),
        }
    }

    async fn lookup(
        &self,
        sandbox_id: &str,
        request: Option<&SandboxRequest>,
    ) -> Result<Option<Arc<dyn ManagedSandboxHandle>>> {
        match self.inner.lookup(sandbox_id, request).await? {
            Some(handle) => Ok(Some(self.install(sandbox_id, handle).await?)),
            None => Ok(None),
        }
    }

    async fn stop_existing(
        &self,
        sandbox_id: &str,
        previous: &Arc<dyn ManagedSandboxHandle>,
    ) -> Result<()> {
        self.inner.stop_existing(sandbox_id, previous).await
    }

    async fn terminate_existing(
        &self,
        sandbox_id: &str,
        request: Option<&SandboxRequest>,
    ) -> Result<()> {
        self.inner.terminate_existing(sandbox_id, request).await
    }

    fn invalidates_handle(&self, error: &anyhow::Error) -> bool {
        self.inner.invalidates_handle(error)
    }

    async fn attach(
        &self,
        request: SandboxRequest,
        attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let handle = self.inner.attach(request.clone(), attachment).await?;
        self.install(request.sandbox_id.as_str(), handle).await
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let handle = self
            .inner
            .acquire_from_snapshot(request.clone(), payload)
            .await?;
        self.install(request.sandbox_id.as_str(), handle).await
    }

    async fn suspend(
        &self,
        request: SandboxRequest,
        kind: crate::SnapshotKind,
    ) -> Result<SnapshotPayload> {
        self.inner.suspend(request, kind).await
    }

    async fn terminate(&self, request: SandboxRequest) -> Result<()> {
        self.inner.terminate(request).await
    }

    async fn delete_snapshot(&self, payload: SnapshotPayload) -> Result<()> {
        self.inner.delete_snapshot(payload).await
    }

    async fn fork_sandbox(
        &self,
        source: SandboxRequest,
        target: SandboxRequest,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let sandbox_id = target.sandbox_id.clone();
        let handle = self.inner.fork_sandbox(source, target).await?;
        self.install(sandbox_id.as_str(), handle).await
    }
}

struct DirectExternalProxyHandle {
    inner: Arc<dyn ManagedSandboxHandle>,
    egress: DirectExternalProxy,
}

#[async_trait]
impl ManagedSandboxHandle for DirectExternalProxyHandle {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn provider_state(&self) -> Option<Value> {
        self.inner.provider_state()
    }

    fn effective_image(&self) -> Option<String> {
        self.inner.effective_image()
    }

    async fn command_environment(&self) -> Result<HashMap<String, String>> {
        let mut environment = self.inner.command_environment().await?;
        environment.extend(self.egress.environment()?);
        Ok(environment)
    }

    async fn is_running(&self) -> Result<Option<bool>> {
        self.inner.is_running().await
    }

    fn allocation_identity(&self) -> Option<String> {
        self.inner.allocation_identity()
    }

    fn recovery_point(&self) -> Option<SandboxRecoveryPoint> {
        self.inner.recovery_point()
    }

    fn prepared_repositories(&self) -> bool {
        self.inner.prepared_repositories()
    }

    fn validity(&self) -> HandleValidity {
        self.inner.validity()
    }

    async fn activity(&self) -> Result<crate::SandboxActivity> {
        self.inner.activity().await
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        self.inner
            .exec(&*DirectExternalProxy::prepare_command(
                Some(&self.egress),
                command,
            )?)
            .await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        self.inner
            .start_process(&*DirectExternalProxy::prepare_command(
                Some(&self.egress),
                command,
            )?)
            .await
    }

    async fn start_managed_process(
        &self,
        command: &SandboxCommand,
        stdin: crate::SandboxProcessStdin,
    ) -> Result<crate::SandboxProcessId> {
        self.inner
            .start_managed_process(
                &*DirectExternalProxy::prepare_command(Some(&self.egress), command)?,
                stdin,
            )
            .await
    }

    async fn write_process_input(&self, process_id: &str, data: &[u8]) -> Result<()> {
        self.inner.write_process_input(process_id, data).await
    }

    async fn close_process_input(&self, process_id: &str) -> Result<()> {
        self.inner.close_process_input(process_id).await
    }

    async fn process_events(
        &self,
        process_id: &str,
        after: u64,
    ) -> Result<crate::GetSandboxProcessEventsResult> {
        self.inner.process_events(process_id, after).await
    }

    async fn wait_process(&self, process_id: &str) -> Result<crate::SandboxProcessStatus> {
        self.inner.wait_process(process_id).await
    }

    async fn cancel_process(&self, process_id: &str) -> Result<crate::SandboxProcessStatus> {
        self.inner.cancel_process(process_id).await
    }

    async fn start_terminal(
        &self,
        command: &SandboxCommand,
        size: crate::SandboxTerminalSize,
    ) -> Result<crate::SandboxTerminalParts> {
        self.inner
            .start_terminal(
                &*DirectExternalProxy::prepare_command(Some(&self.egress), command)?,
                size,
            )
            .await
    }

    fn supports_tcp(&self) -> bool {
        self.inner.supports_tcp()
    }

    async fn connect_tcp(&self, port: u16) -> Result<Option<BoxSandboxTcpStream>> {
        self.inner.connect_tcp(port).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn connect_websocket(
        &self,
        port: u16,
        path: &str,
    ) -> Result<Option<BoxSandboxWebSocket>> {
        self.inner.connect_websocket(port, path).await
    }

    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        self.inner.detach().await
    }

    async fn snapshot(&self, kind: crate::SnapshotKind) -> Result<SnapshotPayload> {
        self.inner.snapshot(kind).await
    }

    async fn snapshot_template(&self, kind: crate::SnapshotKind) -> Result<SnapshotPayload> {
        self.inner.snapshot_template(kind).await
    }

    async fn delete_snapshot(&self, payload: SnapshotPayload) -> Result<()> {
        self.inner.delete_snapshot(payload).await
    }
}
