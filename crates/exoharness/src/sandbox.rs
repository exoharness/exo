use crate::{DurableFileSystem, ResourceScope, SandboxAttachment, SandboxId};
pub use crate::{EgressPolicy, SandboxNetworkPolicy};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{HashMap, hash_map::DefaultHasher};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "basic-backend")]
mod native;
#[cfg(feature = "aws-agentcore")]
pub(crate) use native::validate_durable_file_systems;
#[cfg(feature = "basic-backend")]
pub use native::*;
#[cfg(feature = "basic-backend")]
pub(crate) use native::{WARM_SANDBOX_KEY_LABEL, WARM_SANDBOX_SPEC_HASH_LABEL};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxLifecycleConfig {
    pub idle_ttl: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SandboxMountAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxMount {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub access: SandboxMountAccess,
    pub internal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressListenConfig {
    pub bind_address: std::net::Ipv4Addr,
    pub advertised_address: std::net::Ipv4Addr,
    #[serde(default)]
    pub http_port: u16,
    #[serde(default)]
    pub https_port: u16,
    #[serde(default)]
    pub dns_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxEgressProxy {
    pub http: std::net::SocketAddrV4,
    pub https: std::net::SocketAddrV4,
    pub dns: std::net::SocketAddrV4,
}

impl SandboxEgressProxy {
    pub fn validate(&self) -> Result<()> {
        for endpoint in [self.http, self.https, self.dns] {
            if endpoint.port() == 0
                || endpoint.ip().is_unspecified()
                || endpoint.ip().is_loopback()
                || endpoint.ip().is_multicast()
                || endpoint.ip().is_broadcast()
            {
                bail!("egress proxy requires concrete, host-reachable IPv4 endpoints");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<crate::SandboxResourceShape>,
    pub mounts: Vec<SandboxMount>,
    pub durable_file_systems: Vec<DurableFileSystem>,
    pub policy: EgressPolicy,
    pub default_workdir: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tcp_ports: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxRequest {
    pub sandbox_id: SandboxId,
    #[serde(default)]
    pub scope: ResourceScope,
    pub spec: SandboxSpec,
    pub lifecycle: SandboxLifecycleConfig,
    pub provider_state: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxCommand {
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
    pub display_argv: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxCommandOutput {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub command: Vec<String>,
    pub cwd: String,
}

/// Opaque blob produced by `ManagedSandboxHandle::snapshot` and consumed by
/// `ManagedSandboxBackend::acquire_from_snapshot`. The `format` identifier is
/// the contract: a snapshot produced by one backend can only be restored by a
/// backend that declares support for that format.
#[derive(Debug, Clone)]
pub struct SnapshotPayload {
    pub format: SnapshotFormat,
    pub bytes: Bytes,
}

/// Open identifier for the on-disk format of a snapshot payload.
///
/// Portable formats use shared names that any backend can declare support for.
/// Backend-private references use namespaced names. Format evolution is
/// represented by a new, versioned identifier rather than a core enum variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SnapshotFormat(Cow<'static, str>);

#[allow(non_upper_case_globals)]
impl SnapshotFormat {
    /// `docker save` output: a tar of OCI image layers + manifest.
    pub const DockerImageTar: Self = Self::from_static("docker-image-tar");
    /// Portable content-addressed workspace chunks plus a manifest.
    pub const WorkspaceChunksV1: Self = Self::from_static("workspace-chunks-v1");
    /// Reference to a named snapshot in Daytona's registry.
    pub const DaytonaRef: Self = Self::from_static("daytona-ref");
    /// Reference to an E2B snapshot template id.
    pub const E2bRef: Self = Self::from_static("e2b-ref");
    /// Reference to a Sprites checkpoint id.
    pub const SpritesRef: Self = Self::from_static("sprites-ref");
    /// Reference to a `.smolmachine` pack on the local disk.
    pub const SmolvmMachinePack: Self = Self::from_static("smolvm-machine-pack");
    /// Reference to an immutable bundle in a Firecracker host's private root.
    pub const FirecrackerHostRef: Self = Self::from_static("firecracker-host-ref");
    pub const FirecrackerFilesystemRef: Self = Self::from_static("firecracker-filesystem-ref-v1");

    pub const fn from_static(format: &'static str) -> Self {
        Self(Cow::Borrowed(format))
    }

    pub fn new(format: impl Into<Cow<'static, str>>) -> Self {
        Self(format.into())
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }

    fn from_serialized(format: String) -> Self {
        match format.as_str() {
            "docker-image-tar" | "DockerImageTar" | "docker_image_tar" => Self::DockerImageTar,
            "workspace-chunks-v1" => Self::WorkspaceChunksV1,
            "daytona-ref" | "DaytonaSnapshot" | "daytona_snapshot" => Self::DaytonaRef,
            "e2b-ref" | "E2bSnapshot" | "e2b_snapshot" => Self::E2bRef,
            "sprites-ref" | "SpritesSnapshot" | "sprites_snapshot" => Self::SpritesRef,
            "smolvm-machine-pack" | "SmolMachinePack" | "smol_machine_pack" => {
                Self::SmolvmMachinePack
            }
            "firecracker-host-ref" | "FirecrackerSnapshot" | "firecracker_snapshot" => {
                Self::FirecrackerHostRef
            }
            _ => Self(Cow::Owned(format)),
        }
    }
}

impl<'de> Deserialize<'de> for SnapshotFormat {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from_serialized)
    }
}

impl fmt::Display for SnapshotFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SnapshotFormat {
    type Err = std::convert::Infallible;

    fn from_str(format: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(Cow::Owned(format.to_string())))
    }
}

#[async_trait]
pub trait ManagedSandboxHandle: Send + Sync {
    fn id(&self) -> &str;

    fn provider_state(&self) -> Option<Value> {
        None
    }

    /// The image the sandbox is actually running when the backend substituted
    /// the requested one (e.g. a snapshot restore boots from a freshly loaded
    /// tag). Callers persist this so every process resolves the same warm
    /// sandbox after a restore.
    fn effective_image(&self) -> Option<String> {
        None
    }

    async fn command_environment(&self) -> Result<HashMap<String, String>> {
        Ok(HashMap::new())
    }

    async fn is_running(&self) -> Result<Option<bool>> {
        Ok(None)
    }

    /// Keep this sandbox available during a turn. Backends whose lifecycle
    /// already covers turn activity do not need a separate guard.
    async fn activity(&self) -> Result<crate::SandboxActivity> {
        Ok(crate::SandboxActivity::noop())
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput>;

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts>;

    /// Start a process whose ID remains usable through this sandbox's process API.
    /// Once started, dropping a wait or output request must not cancel the process.
    async fn start_managed_process(
        &self,
        _command: &SandboxCommand,
        _stdin: crate::SandboxProcessStdin,
    ) -> Result<crate::SandboxProcessId> {
        bail!("sandbox backend does not support managed processes")
    }

    async fn write_process_input(&self, _process_id: &str, _data: &[u8]) -> Result<()> {
        bail!("sandbox backend does not support managed processes")
    }

    async fn close_process_input(&self, _process_id: &str) -> Result<()> {
        bail!("sandbox backend does not support managed processes")
    }

    /// Events strictly after `after`, ordered by cursor, with current status.
    /// A terminal status guarantees that the terminal event is available.
    async fn process_events(
        &self,
        _process_id: &str,
        _after: u64,
    ) -> Result<crate::GetSandboxProcessEventsResult> {
        bail!("sandbox backend does not support managed processes")
    }

    async fn wait_process(&self, _process_id: &str) -> Result<crate::SandboxProcessStatus> {
        bail!("sandbox backend does not support managed processes")
    }

    async fn cancel_process(&self, _process_id: &str) -> Result<crate::SandboxProcessStatus> {
        bail!("sandbox backend does not support managed processes")
    }

    async fn start_terminal(
        &self,
        _command: &SandboxCommand,
        _size: crate::SandboxTerminalSize,
    ) -> Result<crate::SandboxTerminalParts> {
        bail!("sandbox backend does not support terminal sessions")
    }

    fn supports_tcp(&self) -> bool {
        false
    }

    async fn connect_tcp(&self, _port: u16) -> Result<Option<BoxSandboxTcpStream>> {
        Ok(None)
    }

    fn live_resource_ceiling(&self) -> Option<crate::SandboxResourceShape> {
        None
    }

    async fn expand_resources(
        &self,
        _resources: crate::SandboxResourceShape,
    ) -> Result<crate::SandboxResourceShape> {
        bail!("sandbox handle does not support live resource expansion")
    }

    async fn stop(&self) -> Result<()>;

    /// Relinquish lifecycle ownership without stopping the sandbox and return
    /// the descriptor required to attach to it elsewhere.
    async fn detach(&self) -> Result<SandboxAttachment>;

    /// Capture the requested state and leave the source usable. Reject unsupported
    /// kinds or writable mounts before capture; never silently omit filesystem state.
    async fn snapshot(&self, kind: crate::SnapshotKind) -> Result<SnapshotPayload>;

    async fn snapshot_template(&self, _kind: crate::SnapshotKind) -> Result<SnapshotPayload> {
        bail!("sandbox handle does not support template capture")
    }

    async fn delete_snapshot(&self, _payload: SnapshotPayload) -> Result<()> {
        bail!("sandbox handle does not support snapshot deletion")
    }
}

pub trait SandboxTcpStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}

impl<T> SandboxTcpStream for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}

pub type BoxSandboxTcpStream = Pin<Box<dyn SandboxTcpStream>>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SandboxImageConfiguration {
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub env: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub healthcheck: Option<SandboxImageHealthcheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SandboxImageHealthcheck {
    pub test: Vec<String>,
    pub interval: Option<u64>,
    pub timeout: Option<u64>,
    pub start_period: Option<u64>,
    pub retries: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedSandboxImage {
    pub image: String,
    pub configuration: SandboxImageConfiguration,
}

#[async_trait]
pub trait ManagedSandboxBackend: Send + Sync {
    #[cfg(feature = "basic-backend")]
    fn with_external_proxy(
        &self,
        _proxy: crate::egress::ExternalProxyConfig,
    ) -> Result<Arc<dyn ManagedSandboxBackend>> {
        bail!("sandbox backend does not support external proxies")
    }

    async fn materialize_resources(
        &self,
        _request: crate::resources::MaterializeResourcesRequest,
    ) -> Result<Vec<crate::FileSystemMount>> {
        bail!("sandbox backend does not support filesystem resources")
    }

    async fn remove_thread_resources(
        &self,
        _agent: crate::AgentId,
        _thread: crate::ThreadId,
    ) -> Result<()> {
        bail!("sandbox backend does not support filesystem resources")
    }

    fn is_local(&self) -> bool;

    /// Whether stopping leaves provider-owned disks that deletion must reclaim.
    fn retains_disk_when_stopped(&self) -> bool {
        false
    }

    /// Whether stopping an uncached sandbox needs persisted provider state.
    /// Backends addressing resources directly by sandbox ID can skip loading it.
    fn stop_requires_provider_state(&self) -> bool {
        true
    }

    /// Formats this backend can consume in `acquire_from_snapshot`.
    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat];

    async fn resolve_image(&self, _image: &str) -> Result<ResolvedSandboxImage> {
        bail!("sandbox backend does not expose image configuration")
    }

    /// Enforce `request.spec.policy` before returning a usable handle. Attach,
    /// restore, and fork must provide the same guarantee or reject the policy.
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>>;

    /// Connect to a published port. Backends whose acquisition changes lifecycle
    /// ownership should inspect the existing sandbox without restarting it.
    async fn connect_tcp(
        &self,
        request: SandboxRequest,
        port: u16,
    ) -> Result<Option<BoxSandboxTcpStream>> {
        self.acquire(request).await?.connect_tcp(port).await
    }

    /// Stop a persisted sandbox when no in-process handle is available. Backends
    /// with stable resource IDs can override this to avoid starting it first.
    async fn stop(&self, request: SandboxRequest) -> Result<()> {
        self.acquire(request).await?.stop().await
    }

    /// Reconnect to an existing sandbox without provisioning a replacement.
    /// `request` is available when the caller retained the acquisition context;
    /// remote backends may resolve `sandbox_id` without it. Return `None` when the
    /// sandbox is gone or reconnection is unsupported. Reuse `previous` when its
    /// connection is unchanged so any process bookkeeping remains intact.
    async fn resolve_existing(
        &self,
        _sandbox_id: &str,
        _request: Option<&SandboxRequest>,
        _previous: &Arc<dyn ManagedSandboxHandle>,
    ) -> Result<Option<Arc<dyn ManagedSandboxHandle>>> {
        Ok(None)
    }

    /// Stop the sandbox identified by `sandbox_id`, even if `previous` is stale.
    /// Backends with independently changing allocations should override this to
    /// target the current allocation without acquiring or resuming a sandbox.
    async fn stop_existing(
        &self,
        _sandbox_id: &str,
        previous: &Arc<dyn ManagedSandboxHandle>,
    ) -> Result<()> {
        previous.stop().await
    }

    /// Terminate an existing sandbox. Backends that can resolve IDs independently
    /// may override this to work without the original acquisition request.
    async fn terminate_existing(
        &self,
        _sandbox_id: &str,
        request: Option<&SandboxRequest>,
    ) -> Result<()> {
        self.terminate(
            request
                .context("sandbox termination requires its acquisition request")?
                .clone(),
        )
        .await
    }

    /// Whether an operation failure requires reconnecting before the next operation.
    /// Ordinary command failures and missing process IDs must not invalidate a handle.
    /// Callers must not replay an operation whose outcome is ambiguous.
    fn invalidates_handle(&self, _error: &anyhow::Error) -> bool {
        false
    }

    async fn attach(
        &self,
        request: SandboxRequest,
        attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>>;

    /// Acquire a sandbox initialised from a previously-captured snapshot.
    /// The request is honoured for mounts, network, lifecycle, etc., but the
    /// container's filesystem is sourced from the payload instead of
    /// `request.spec.image`. Returns an error if this backend can't restore
    /// the supplied `payload.format`.
    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>>;

    /// Capture the requested state and release execution resources. Restore it
    /// with acquire_from_snapshot; unsupported kinds must leave the source intact.
    /// The returned snapshot has its own lifetime and is deleted with delete_snapshot.
    async fn suspend(
        &self,
        _request: SandboxRequest,
        _kind: crate::SnapshotKind,
    ) -> Result<SnapshotPayload> {
        bail!("sandbox backend does not support suspension")
    }

    /// Permanently destroy the sandbox addressed by `request` and any retained
    /// backend state. Unlike stopping a handle, termination must be idempotent.
    async fn terminate(&self, _request: SandboxRequest) -> Result<()> {
        bail!("sandbox backend does not support explicit termination")
    }

    async fn delete_snapshot(&self, _payload: SnapshotPayload) -> Result<()> {
        bail!("sandbox backend does not support snapshot deletion")
    }

    /// Copy the current state of `source` to `target`.
    async fn fork_sandbox(
        &self,
        _source: SandboxRequest,
        _target: SandboxRequest,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("sandbox backend does not support forking")
    }
}

pub const DEFAULT_SANDBOX_IMAGE: &str = crate::sandbox_provider::DEFAULT_DOCKER_IMAGE;
pub const SANDBOX_HOME_DIR: &str = "/home/exo";
pub const SANDBOX_MAIN_MOUNT_DIR: &str = "/home/exo/workspace";

pub(crate) fn sandbox_spec_hash(spec: &SandboxSpec) -> String {
    let mut hasher = DefaultHasher::new();
    spec.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
