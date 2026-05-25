use std::collections::{HashMap, hash_map::DefaultHasher};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SandboxKey {
    ConversationSandbox {
        conversation_id: String,
        sandbox_id: String,
    },
}

impl fmt::Display for SandboxKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConversationSandbox {
                conversation_id,
                sandbox_id,
            } => write!(f, "conversation:{conversation_id}:{sandbox_id}"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxLifecycleConfig {
    pub idle_ttl: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxMountAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SandboxMount {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub access: SandboxMountAccess,
    pub internal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxNetworkPolicy {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SandboxSpec {
    pub image: String,
    pub mounts: Vec<SandboxMount>,
    pub network: SandboxNetworkPolicy,
    pub default_workdir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRequest {
    pub key: SandboxKey,
    pub spec: SandboxSpec,
    pub lifecycle: SandboxLifecycleConfig,
}

#[derive(Debug, Clone)]
pub struct SandboxCommand {
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
    pub display_argv: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct SandboxCommandOutput {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub command: Vec<String>,
    pub cwd: String,
}

/// Opaque blob produced by `ManagedSandboxHandle::snapshot` and consumed by
/// `ManagedSandboxBackend::acquire_from_snapshot`. The `kind` tag is the
/// contract: a snapshot produced by one backend can only be restored by a
/// backend that knows how to interpret that kind.
#[derive(Debug, Clone)]
pub struct SnapshotPayload {
    pub kind: SnapshotKind,
    pub bytes: Bytes,
}

/// Tag identifying the on-disk format of a snapshot payload. Backends both
/// produce and consume a specific kind; the conversation layer just hands the
/// bytes back to the same backend type that produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    /// `docker save` output: a tar of OCI image layers + manifest, loadable
    /// with `docker load`.
    DockerImageTar,
    /// Reference to a named snapshot held inside Daytona's own snapshot
    /// registry. The payload bytes are a small JSON manifest pointing at
    /// the Daytona snapshot name; the actual filesystem state stays on
    /// Daytona. Bytes-by-reference, not bytes-by-value — restoring is a
    /// `POST /sandbox { snapshot: <name> }` call, not a tarball load.
    DaytonaSnapshot,
}

#[async_trait]
pub trait ManagedSandboxHandle: Send + Sync {
    fn id(&self) -> &str;

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput>;

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts>;

    async fn stop(&self) -> Result<()>;

    /// Capture the sandbox's current state as an opaque blob. Returns an
    /// error if this backend doesn't (yet) support snapshotting.
    async fn snapshot(&self) -> Result<SnapshotPayload>;
}

#[async_trait]
pub trait ManagedSandboxBackend: Send + Sync {
    /// Bring up a sandbox for `request.key`. Reuses an in-process warm
    /// sandbox if one matches; otherwise creates fresh from
    /// `request.spec.image`. Cross-process reuse goes through
    /// [`Self::try_resume`].
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>>;

    /// Reattach to a sandbox from a previous exo process (e.g. a stopped
    /// docker container labelled with `exo.sandbox.key`). Starts it if
    /// stopped. Returns `Ok(None)` if no match. Backends with no
    /// cross-process identity (local-process) always return `Ok(None)`.
    async fn try_resume(
        &self,
        request: SandboxRequest,
    ) -> Result<Option<Arc<dyn ManagedSandboxHandle>>>;

    /// Acquire a sandbox initialised from a previously-captured snapshot.
    /// The request is honoured for mounts, network, lifecycle, etc., but the
    /// container's filesystem is sourced from the payload instead of
    /// `request.spec.image`. Returns an error if this backend can't restore
    /// the supplied `payload.kind`.
    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>>;
}

pub const DEFAULT_SANDBOX_IMAGE: &str = "docker.io/library/ubuntu:24.04";
pub const SANDBOX_HOME_DIR: &str = "/home/exo";
pub const SANDBOX_MAIN_MOUNT_DIR: &str = "/home/exo/workspace";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerCliFlavor {
    AppleContainer,
    Docker,
}

impl ContainerCliFlavor {
    fn default_binary(self) -> &'static str {
        match self {
            Self::AppleContainer => "container",
            Self::Docker => "docker",
        }
    }

    fn requires_system_start(self) -> bool {
        matches!(self, Self::AppleContainer)
    }
}

const DEFAULT_ENABLED_NETWORK_NAME: &str = "exo-default";
const WARM_SANDBOX_KEEPALIVE_ARGV: &[&str] = &["sleep", "infinity"];
const WARM_SANDBOX_HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(3);
const WARM_SANDBOX_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const ORPHANED_WARM_SANDBOX_MIN_AGE: Duration = Duration::from_secs(60 * 60);
pub(crate) const WARM_SANDBOX_KEY_LABEL: &str = "exo.sandbox.key";
pub(crate) const WARM_SANDBOX_SPEC_HASH_LABEL: &str = "exo.sandbox.spec-hash";
const WARM_SANDBOX_OWNER_PID_LABEL: &str = "exo.sandbox.owner-pid";
const APPLE_ABSOLUTE_TIME_UNIX_OFFSET_SECONDS: f64 = 978_307_200.0;

#[derive(Debug, Clone)]
struct WarmSandboxEntry {
    name: String,
    request: SandboxRequest,
    last_used_at: Instant,
}

#[derive(Debug, Deserialize)]
struct ContainerListItem {
    status: Option<String>,
    #[serde(rename = "startedDate")]
    started_date: Option<f64>,
    configuration: ContainerListConfiguration,
}

#[derive(Debug, Deserialize)]
struct ContainerListConfiguration {
    id: String,
    #[serde(default)]
    labels: HashMap<String, String>,
}

#[derive(Debug)]
pub struct CliContainerSandboxBackend {
    cli: ContainerCliFlavor,
    container_bin: PathBuf,
    system_started: Mutex<bool>,
    network_created: Mutex<bool>,
    warm_sandboxes: Arc<Mutex<HashMap<SandboxKey, WarmSandboxEntry>>>,
}

impl CliContainerSandboxBackend {
    pub fn apple_container() -> Self {
        Self::new(ContainerCliFlavor::AppleContainer)
    }

    pub fn docker() -> Self {
        Self::new(ContainerCliFlavor::Docker)
    }

    fn new(cli: ContainerCliFlavor) -> Self {
        Self {
            cli,
            container_bin: PathBuf::from(cli.default_binary()),
            system_started: Mutex::new(false),
            network_created: Mutex::new(false),
            warm_sandboxes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn ensure_system_started(&self) -> Result<()> {
        if !self.cli.requires_system_start() {
            return Ok(());
        }
        let mut started = self.system_started.lock().await;
        if *started {
            return Ok(());
        }

        let output = Command::new(&self.container_bin)
            .arg("system")
            .arg("start")
            .kill_on_drop(true)
            .output()
            .await?;
        if !output.status.success() {
            return Err(anyhow!(
                "failed to start container system: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        if let Err(error) = reap_orphaned_warm_sandboxes(&self.container_bin).await {
            eprintln!("failed to reap orphaned warm sandboxes: {error}");
        }
        *started = true;
        Ok(())
    }

    async fn ensure_default_network_created(&self) -> Result<()> {
        let mut created = self.network_created.lock().await;
        if *created {
            return Ok(());
        }

        let output = Command::new(&self.container_bin)
            .arg("network")
            .arg("create")
            .arg(DEFAULT_ENABLED_NETWORK_NAME)
            .kill_on_drop(true)
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if !stderr.contains("already exists") {
                return Err(anyhow!(
                    "failed to create default container network {DEFAULT_ENABLED_NETWORK_NAME}: {stderr}"
                ));
            }
        }

        *created = true;
        Ok(())
    }

    async fn prepare_request(&self, request: SandboxRequest) -> Result<SandboxRequest> {
        self.ensure_system_started().await?;
        if matches!(request.spec.network, SandboxNetworkPolicy::Enabled) {
            self.ensure_default_network_created().await?;
        }

        let mounts = request
            .spec
            .mounts
            .into_iter()
            .map(|mount| {
                let host_path = std::fs::canonicalize(&mount.host_path)?;
                if !host_path.is_dir() {
                    bail!(
                        "sandbox mount root is not a directory: {}",
                        host_path.display()
                    );
                }
                Ok(SandboxMount { host_path, ..mount })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(SandboxRequest {
            key: request.key,
            spec: SandboxSpec {
                image: if request.spec.image.trim().is_empty() {
                    DEFAULT_SANDBOX_IMAGE.to_string()
                } else {
                    request.spec.image
                },
                mounts,
                network: request.spec.network,
                default_workdir: request.spec.default_workdir,
            },
            lifecycle: request.lifecycle,
        })
    }

    async fn reap_expired_warm_sandboxes(&self) -> Result<()> {
        let now = Instant::now();
        let expired = {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            let expired_keys = warm_sandboxes
                .iter()
                .filter_map(|(key, entry)| {
                    let ttl = entry.request.lifecycle.idle_ttl?;
                    (entry.last_used_at + ttl <= now).then(|| key.clone())
                })
                .collect::<Vec<_>>();

            expired_keys
                .into_iter()
                .filter_map(|key| warm_sandboxes.remove(&key))
                .collect::<Vec<_>>()
        };

        for entry in expired {
            cleanup_named_container(&self.container_bin, self.cli, &entry.name).await?;
        }

        Ok(())
    }
}

impl Drop for CliContainerSandboxBackend {
    fn drop(&mut self) {
        // Stop, don't delete — let the next process `try_resume` by label.
        let Ok(mut warm_sandboxes) = self.warm_sandboxes.try_lock() else {
            return;
        };
        let names = warm_sandboxes
            .drain()
            .map(|(_, entry)| entry.name)
            .collect::<Vec<_>>();
        for name in names {
            stop_named_container_blocking(&self.container_bin, self.cli, &name);
        }
    }
}

#[async_trait]
impl ManagedSandboxBackend for CliContainerSandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let request = self.prepare_request(request).await?;

        if request.lifecycle.idle_ttl.is_none() {
            return Ok(Arc::new(OneShotSandboxHandle {
                id: format!("oneshot:{}", request.key),
                container_bin: self.container_bin.clone(),
                request,
            }));
        }

        self.reap_expired_warm_sandboxes().await?;

        let replaced = {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            match warm_sandboxes.get(&request.key) {
                Some(entry) if entry.request.spec == request.spec => {
                    return Ok(Arc::new(WarmSandboxHandle {
                        id: format!("warm:{}", request.key),
                        cli: self.cli,
                        container_bin: self.container_bin.clone(),
                        request,
                        warm_sandboxes: Arc::clone(&self.warm_sandboxes),
                    }));
                }
                Some(_) => warm_sandboxes.remove(&request.key),
                None => None,
            }
        };
        if let Some(entry) = replaced {
            schedule_cleanup_named_container(self.container_bin.clone(), self.cli, entry.name);
        }

        let name = create_unique_warm_sandbox(&self.container_bin, &request).await?;

        {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            warm_sandboxes.insert(
                request.key.clone(),
                WarmSandboxEntry {
                    name: name.clone(),
                    request: request.clone(),
                    last_used_at: Instant::now(),
                },
            );
        }

        Ok(Arc::new(WarmSandboxHandle {
            id: format!("warm:{}", request.key),
            cli: self.cli,
            container_bin: self.container_bin.clone(),
            request,
            warm_sandboxes: Arc::clone(&self.warm_sandboxes),
        }))
    }

    async fn try_resume(
        &self,
        request: SandboxRequest,
    ) -> Result<Option<Arc<dyn ManagedSandboxHandle>>> {
        let request = self.prepare_request(request).await?;

        // No warm pool, no cross-process identity.
        let Some(idle_ttl) = request.lifecycle.idle_ttl else {
            return Ok(None);
        };

        // In-process warm pool first.
        {
            let warm_sandboxes = self.warm_sandboxes.lock().await;
            if let Some(entry) = warm_sandboxes.get(&request.key)
                && entry.request.spec == request.spec
            {
                return Ok(Some(Arc::new(WarmSandboxHandle {
                    id: format!("warm:{}", request.key),
                    cli: self.cli,
                    container_bin: self.container_bin.clone(),
                    request,
                    warm_sandboxes: Arc::clone(&self.warm_sandboxes),
                })));
            }
        }

        // Cross-process: look up by label; stale spec-hashes are reaped.
        let spec_hash = sandbox_spec_hash(&request.spec);
        let key_str = request.key.to_string();
        let Some(existing) =
            find_resumable_container(&self.container_bin, self.cli, &key_str, &spec_hash).await?
        else {
            return Ok(None);
        };

        // Drop containers idle past the TTL.
        if !existing.is_running
            && let Some(finished_at) = existing.finished_at
            && let Ok(idle_for) = SystemTime::now().duration_since(finished_at)
            && idle_for > idle_ttl
        {
            cleanup_named_container(&self.container_bin, self.cli, &existing.name).await?;
            return Ok(None);
        }

        if !existing.is_running {
            start_named_container(&self.container_bin, self.cli, &existing.name).await?;
        }

        {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            warm_sandboxes.insert(
                request.key.clone(),
                WarmSandboxEntry {
                    name: existing.name.clone(),
                    request: request.clone(),
                    last_used_at: Instant::now(),
                },
            );
        }

        Ok(Some(Arc::new(WarmSandboxHandle {
            id: format!("warm:{}", request.key),
            cli: self.cli,
            container_bin: self.container_bin.clone(),
            request,
            warm_sandboxes: Arc::clone(&self.warm_sandboxes),
        })))
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        if request.lifecycle.idle_ttl.is_none() {
            bail!("restore-from-snapshot requires a warm sandbox lifecycle (idle_ttl must be set)");
        }
        match (self.cli, payload.kind) {
            (ContainerCliFlavor::Docker, SnapshotKind::DockerImageTar) => {}
            (_, SnapshotKind::DaytonaSnapshot) => bail!(
                "DaytonaSnapshot payloads can only be restored by the Daytona sandbox backend; \
                 switch to --sandbox-backend daytona to rewind this snapshot"
            ),
            (ContainerCliFlavor::AppleContainer, _) => bail!(
                "restore-from-snapshot is not yet implemented for the apple-container backend"
            ),
        }

        let image_tag = docker_load_image(&self.container_bin, &payload.bytes).await?;

        // Build a fresh request that points at the loaded image. Mounts,
        // network policy, lifecycle, and key are all preserved from the
        // original request so the restored sandbox is otherwise identical.
        let mut request = self.prepare_request(request).await?;
        request.spec.image = image_tag;

        // Evict any pre-existing warm container for this key — we want a
        // fresh container booted from the restored image, not a reuse of
        // whatever was running before.
        let replaced = {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            warm_sandboxes.remove(&request.key)
        };
        if let Some(entry) = replaced {
            schedule_cleanup_named_container(self.container_bin.clone(), self.cli, entry.name);
        }

        let name = create_unique_warm_sandbox(&self.container_bin, &request).await?;
        {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            warm_sandboxes.insert(
                request.key.clone(),
                WarmSandboxEntry {
                    name: name.clone(),
                    request: request.clone(),
                    last_used_at: Instant::now(),
                },
            );
        }

        Ok(Arc::new(WarmSandboxHandle {
            id: format!("warm:{}", request.key),
            cli: self.cli,
            container_bin: self.container_bin.clone(),
            request,
            warm_sandboxes: Arc::clone(&self.warm_sandboxes),
        }))
    }
}

struct OneShotSandboxHandle {
    id: String,
    container_bin: PathBuf,
    request: SandboxRequest,
}

#[async_trait]
impl ManagedSandboxHandle for OneShotSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        exec_one_shot(
            &self.container_bin,
            &self.request.spec,
            network_name_for_policy(self.request.spec.network),
            command,
        )
        .await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        start_one_shot_process(
            &self.container_bin,
            &self.request.spec,
            network_name_for_policy(self.request.spec.network),
            command,
        )
        .await
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!(
            "snapshot is not supported for one-shot sandboxes (set a positive idle_ttl to enable warm sandbox + snapshotting)"
        )
    }
}

struct WarmSandboxHandle {
    id: String,
    cli: ContainerCliFlavor,
    container_bin: PathBuf,
    request: SandboxRequest,
    warm_sandboxes: Arc<Mutex<HashMap<SandboxKey, WarmSandboxEntry>>>,
}

#[async_trait]
impl ManagedSandboxHandle for WarmSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let name = ensure_warm_sandbox_ready(
            &self.container_bin,
            self.cli,
            &self.request,
            &self.warm_sandboxes,
        )
        .await?;
        touch_warm_sandbox(&self.warm_sandboxes, &self.request.key).await;
        let output = exec_warm(&self.container_bin, &name, &self.request.spec, command).await;
        touch_warm_sandbox(&self.warm_sandboxes, &self.request.key).await;
        output
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        let name = ensure_warm_sandbox_ready(
            &self.container_bin,
            self.cli,
            &self.request,
            &self.warm_sandboxes,
        )
        .await?;
        touch_warm_sandbox(&self.warm_sandboxes, &self.request.key).await;
        start_warm_process(&self.container_bin, &name, &self.request.spec, command).await
    }

    async fn stop(&self) -> Result<()> {
        let removed = {
            let mut warm_sandboxes = self.warm_sandboxes.lock().await;
            warm_sandboxes.remove(&self.request.key)
        };

        if let Some(entry) = removed {
            cleanup_named_container(&self.container_bin, self.cli, &entry.name).await
        } else {
            Ok(())
        }
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        match self.cli {
            ContainerCliFlavor::Docker => {
                let name = ensure_warm_sandbox_ready(
                    &self.container_bin,
                    self.cli,
                    &self.request,
                    &self.warm_sandboxes,
                )
                .await?;
                touch_warm_sandbox(&self.warm_sandboxes, &self.request.key).await;
                docker_snapshot_container(&self.container_bin, &name).await
            }
            // The Apple `container` CLI exposes `container image save` and a
            // `container commit`-style flow on its roadmap but neither is in
            // the released versions we target today. When it lands, the path
            // will mirror docker_snapshot_container: produce a single tarball
            // and tag it with a new SnapshotKind variant (e.g.
            // AppleContainerImageTar). Until then, fail explicitly so callers
            // know to choose Docker for snapshot-using flows.
            ContainerCliFlavor::AppleContainer => bail!(
                "snapshot is not yet implemented for the apple-container backend; \
                 use --sandbox-backend docker for snapshot-using flows"
            ),
        }
    }
}

#[derive(Debug, Default)]
pub struct LocalProcessSandboxBackend;

impl LocalProcessSandboxBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ManagedSandboxBackend for LocalProcessSandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        Ok(Arc::new(LocalProcessSandboxHandle {
            id: format!("local:{}", request.key),
            request,
        }))
    }

    async fn try_resume(
        &self,
        _request: SandboxRequest,
    ) -> Result<Option<Arc<dyn ManagedSandboxHandle>>> {
        // Local-process "sandboxes" don't outlive the exo invocation.
        Ok(None)
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("restore-from-snapshot is not supported by the local-process sandbox backend")
    }
}

struct LocalProcessSandboxHandle {
    id: String,
    request: SandboxRequest,
}

#[async_trait]
impl ManagedSandboxHandle for LocalProcessSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        if command.argv.is_empty() {
            bail!("sandbox command requires at least one argv entry");
        }
        let cwd = command
            .cwd
            .clone()
            .unwrap_or_else(|| self.request.spec.default_workdir.clone());
        let mut process = Command::new(&command.argv[0]);
        process.args(&command.argv[1..]);
        process.envs(&command.env);
        if let Some(workdir) = resolve_local_workdir(&self.request.spec, &cwd) {
            process.current_dir(workdir);
        }
        process.kill_on_drop(true);
        run_command(process, command, cwd).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        if command.argv.is_empty() {
            bail!("sandbox command requires at least one argv entry");
        }
        let cwd = command
            .cwd
            .clone()
            .unwrap_or_else(|| self.request.spec.default_workdir.clone());
        let mut process = Command::new(&command.argv[0]);
        process.args(&command.argv[1..]);
        process.envs(&command.env);
        if let Some(workdir) = resolve_local_workdir(&self.request.spec, &cwd) {
            process.current_dir(workdir);
        }
        process.kill_on_drop(true);
        spawn_sandbox_process(process, command).await
    }

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        // The local-process backend runs commands directly on the host; there
        // is no container filesystem to capture. A meaningful implementation
        // would tar up the writable mounts, but the semantics differ enough
        // from container snapshots (no isolated filesystem, no rollback of
        // host-side state) that we don't pretend to support it.
        bail!("snapshot is not supported by the local-process sandbox backend")
    }
}

fn resolve_local_workdir(spec: &SandboxSpec, cwd: &str) -> Option<PathBuf> {
    let cwd_path = PathBuf::from(cwd);
    if cwd_path.is_absolute() {
        if let Some(mount) = spec.mounts.iter().find(|mount| mount.guest_path == cwd) {
            return Some(mount.host_path.clone());
        }
        return cwd_path.exists().then_some(cwd_path);
    }

    spec.mounts
        .iter()
        .find(|mount| mount.guest_path == cwd)
        .map(|mount| mount.host_path.clone())
}

async fn touch_warm_sandbox(
    warm_sandboxes: &Arc<Mutex<HashMap<SandboxKey, WarmSandboxEntry>>>,
    key: &SandboxKey,
) {
    let mut warm_sandboxes = warm_sandboxes.lock().await;
    if let Some(entry) = warm_sandboxes.get_mut(key) {
        entry.last_used_at = Instant::now();
    }
}

async fn create_named_warm_sandbox(
    container_bin: &Path,
    request: &SandboxRequest,
    name: &str,
) -> Result<()> {
    let mut process = Command::new(container_bin);
    process
        .arg("run")
        .arg("--detach")
        .arg("--name")
        .arg(name)
        .arg("--label")
        .arg(format!("{WARM_SANDBOX_KEY_LABEL}={}", request.key))
        .arg("--label")
        .arg(format!(
            "{WARM_SANDBOX_SPEC_HASH_LABEL}={}",
            sandbox_spec_hash(&request.spec)
        ))
        .arg("--label")
        .arg(format!(
            "{WARM_SANDBOX_OWNER_PID_LABEL}={}",
            std::process::id()
        ))
        .arg("--workdir")
        .arg(&request.spec.default_workdir);

    configure_network_args(
        &mut process,
        request.spec.network,
        Some(DEFAULT_ENABLED_NETWORK_NAME),
    );
    configure_mount_args(&mut process, &request.spec.mounts);

    process.arg(&request.spec.image);
    process.args(WARM_SANDBOX_KEEPALIVE_ARGV);
    process.kill_on_drop(true);

    let output = process.output().await?;
    if !output.status.success() {
        let stderr = render_command_error(&output.stderr);
        return Err(anyhow!("failed to start warm sandbox {}: {}", name, stderr));
    }

    Ok(())
}

async fn create_unique_warm_sandbox(
    container_bin: &Path,
    request: &SandboxRequest,
) -> Result<String> {
    for _ in 0..4 {
        let name = new_warm_container_name(&request.key);
        match create_named_warm_sandbox(container_bin, request, &name).await {
            Ok(()) => return Ok(name),
            Err(err) if is_already_exists_error(&err.to_string()) => continue,
            Err(err) => return Err(err),
        }
    }

    Err(anyhow!(
        "failed to allocate a unique warm sandbox name for {}",
        request.key
    ))
}

async fn ensure_warm_sandbox_ready(
    container_bin: &Path,
    cli: ContainerCliFlavor,
    request: &SandboxRequest,
    warm_sandboxes: &Arc<Mutex<HashMap<SandboxKey, WarmSandboxEntry>>>,
) -> Result<String> {
    let healthcheck = SandboxCommand {
        argv: vec!["/bin/true".to_string()],
        env: HashMap::new(),
        display_argv: Some(vec!["/bin/true".to_string()]),
        cwd: None,
        timeout: Some(WARM_SANDBOX_HEALTHCHECK_TIMEOUT),
    };

    let mut warm_sandboxes = warm_sandboxes.lock().await;
    let current_name = match warm_sandboxes.get_mut(&request.key) {
        Some(entry) if entry.request.spec == request.spec => {
            entry.last_used_at = Instant::now();
            entry.name.clone()
        }
        Some(_) => {
            let stale = warm_sandboxes
                .remove(&request.key)
                .expect("entry disappeared while locked");
            schedule_cleanup_named_container(container_bin.to_path_buf(), cli, stale.name);
            let name = create_unique_warm_sandbox(container_bin, request).await?;
            warm_sandboxes.insert(
                request.key.clone(),
                WarmSandboxEntry {
                    name: name.clone(),
                    request: request.clone(),
                    last_used_at: Instant::now(),
                },
            );
            return Ok(name);
        }
        None => {
            let name = create_unique_warm_sandbox(container_bin, request).await?;
            warm_sandboxes.insert(
                request.key.clone(),
                WarmSandboxEntry {
                    name: name.clone(),
                    request: request.clone(),
                    last_used_at: Instant::now(),
                },
            );
            return Ok(name);
        }
    };

    let healthy = matches!(
        exec_warm(container_bin, &current_name, &request.spec, &healthcheck).await,
        Ok(output) if output.ok
    );
    if healthy {
        return Ok(current_name);
    }

    let replacement_name = create_unique_warm_sandbox(container_bin, request).await?;
    warm_sandboxes.insert(
        request.key.clone(),
        WarmSandboxEntry {
            name: replacement_name.clone(),
            request: request.clone(),
            last_used_at: Instant::now(),
        },
    );
    drop(warm_sandboxes);
    schedule_cleanup_named_container(container_bin.to_path_buf(), cli, current_name);
    Ok(replacement_name)
}

async fn exec_one_shot(
    container_bin: &Path,
    spec: &SandboxSpec,
    network_name: Option<&'static str>,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }

    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());

    let mut process = Command::new(container_bin);
    process.arg("run").arg("--rm").arg("--workdir").arg(&cwd);
    configure_network_args(&mut process, spec.network, network_name);
    configure_mount_args(&mut process, &spec.mounts);
    configure_env_args(&mut process, &command.env);
    process.arg(&spec.image);
    process.args(&command.argv);
    process.kill_on_drop(true);

    run_command(process, command, cwd).await
}

async fn start_one_shot_process(
    container_bin: &Path,
    spec: &SandboxSpec,
    network_name: Option<&'static str>,
    command: &SandboxCommand,
) -> Result<crate::SandboxProcessParts> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }

    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());

    let mut process = Command::new(container_bin);
    process
        .arg("run")
        .arg("--rm")
        .arg("--interactive")
        .arg("--workdir")
        .arg(&cwd);
    configure_network_args(&mut process, spec.network, network_name);
    configure_mount_args(&mut process, &spec.mounts);
    configure_env_args(&mut process, &command.env);
    process.arg(&spec.image);
    process.args(&command.argv);
    process.kill_on_drop(true);

    spawn_sandbox_process(process, command).await
}

async fn exec_warm(
    container_bin: &Path,
    name: &str,
    spec: &SandboxSpec,
    command: &SandboxCommand,
) -> Result<SandboxCommandOutput> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }

    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());

    let mut process = Command::new(container_bin);
    process.arg("exec").arg("--workdir").arg(&cwd);
    configure_env_args(&mut process, &command.env);
    process.arg(name);
    process.args(&command.argv);
    process.kill_on_drop(true);

    run_command(process, command, cwd).await
}

async fn start_warm_process(
    container_bin: &Path,
    name: &str,
    spec: &SandboxSpec,
    command: &SandboxCommand,
) -> Result<crate::SandboxProcessParts> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }

    let cwd = command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone());

    let mut process = Command::new(container_bin);
    process
        .arg("exec")
        .arg("--interactive")
        .arg("--workdir")
        .arg(&cwd);
    configure_env_args(&mut process, &command.env);
    process.arg(name);
    process.args(&command.argv);
    process.kill_on_drop(true);

    spawn_sandbox_process(process, command).await
}

async fn run_command(
    mut process: Command,
    command: &SandboxCommand,
    cwd: String,
) -> Result<SandboxCommandOutput> {
    let output = match command.timeout {
        Some(timeout) => match time::timeout(timeout, process.output()).await {
            Ok(output) => output?,
            Err(_) => {
                return Err(anyhow!(
                    "sandbox command timed out after {}s: {}",
                    timeout.as_secs(),
                    command
                        .display_argv
                        .as_ref()
                        .unwrap_or(&command.argv)
                        .join(" ")
                ));
            }
        },
        None => process.output().await?,
    };

    Ok(SandboxCommandOutput {
        ok: output.status.success(),
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        command: command
            .display_argv
            .clone()
            .unwrap_or_else(|| command.argv.clone()),
        cwd,
    })
}

async fn spawn_sandbox_process(
    mut process: Command,
    command: &SandboxCommand,
) -> Result<crate::SandboxProcessParts> {
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process.spawn().with_context(|| {
        format!(
            "failed to start sandbox command: {}",
            command
                .display_argv
                .as_ref()
                .unwrap_or(&command.argv)
                .join(" ")
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("sandbox process did not expose stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("sandbox process did not expose stderr"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("sandbox process did not expose stdin"))?;

    Ok(crate::SandboxProcessParts {
        stdout: Box::pin(stdout.compat()),
        stderr: Box::pin(stderr.compat()),
        stdin: Box::pin(stdin.compat_write()),
        wait: wait_for_child(child),
    })
}

fn wait_for_child(mut child: Child) -> BoxFuture<'static, crate::Result<i32>> {
    Box::pin(async move {
        let status = child.wait().await?;
        Ok(status.code().unwrap_or_default())
    })
}

fn configure_network_args(
    process: &mut Command,
    policy: SandboxNetworkPolicy,
    network_name: Option<&str>,
) {
    match policy {
        SandboxNetworkPolicy::Disabled => {
            process.arg("--network").arg("none");
        }
        SandboxNetworkPolicy::Enabled => {
            if let Some(network_name) = network_name {
                process.arg("--network").arg(network_name);
            }
        }
    }
}

fn configure_mount_args(process: &mut Command, mounts: &[SandboxMount]) {
    for mount in mounts {
        let mut volume = format!("{}:{}", mount.host_path.display(), mount.guest_path);
        if matches!(mount.access, SandboxMountAccess::ReadOnly) {
            volume.push_str(":ro");
        }
        process.arg("--volume").arg(volume);
    }
}

fn configure_env_args(process: &mut Command, env: &HashMap<String, String>) {
    for (key, value) in env {
        process.arg("--env").arg(format!("{key}={value}"));
    }
}

async fn cleanup_named_container(
    container_bin: &Path,
    cli: ContainerCliFlavor,
    name: &str,
) -> Result<()> {
    match cli {
        ContainerCliFlavor::AppleContainer => {
            let stop = run_container_admin_command(
                container_bin,
                WARM_SANDBOX_CLEANUP_TIMEOUT,
                ["stop", name],
            )
            .await?;
            if !stop.status.success() {
                let stderr = String::from_utf8_lossy(&stop.stderr).trim().to_string();
                if !is_missing_container_error(&stderr) {
                    return Err(anyhow!("failed to stop warm sandbox {}: {}", name, stderr));
                }
            }

            let delete = run_container_admin_command(
                container_bin,
                WARM_SANDBOX_CLEANUP_TIMEOUT,
                ["delete", name],
            )
            .await?;
            if !delete.status.success() {
                let stderr = String::from_utf8_lossy(&delete.stderr).trim().to_string();
                if !is_missing_container_error(&stderr) {
                    return Err(anyhow!(
                        "failed to delete warm sandbox {}: {}",
                        name,
                        stderr
                    ));
                }
            }
        }
        ContainerCliFlavor::Docker => {
            // `docker rm -f` is SIGKILL + remove in one shot. Avoids racing
            // the daemon's default 10s SIGTERM grace against our cleanup
            // timeout, which otherwise leaves Exited containers behind.
            let rm = run_container_admin_command(
                container_bin,
                WARM_SANDBOX_CLEANUP_TIMEOUT,
                ["rm", "-f", name],
            )
            .await?;
            if !rm.status.success() {
                let stderr = String::from_utf8_lossy(&rm.stderr).trim().to_string();
                if !is_missing_container_error(&stderr) {
                    return Err(anyhow!(
                        "failed to delete warm sandbox {}: {}",
                        name,
                        stderr
                    ));
                }
            }
        }
    }

    Ok(())
}

async fn reap_orphaned_warm_sandboxes(container_bin: &Path) -> Result<()> {
    let output = run_container_admin_command(
        container_bin,
        WARM_SANDBOX_CLEANUP_TIMEOUT,
        ["list", "--format", "json"],
    )
    .await?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to list warm sandboxes: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let now = apple_absolute_time_now()?;
    let containers: Vec<ContainerListItem> = serde_json::from_slice(&output.stdout)?;
    for container in containers {
        if !container
            .configuration
            .labels
            .contains_key(WARM_SANDBOX_KEY_LABEL)
        {
            continue;
        }
        if let Some(owner_pid) = container
            .configuration
            .labels
            .get(WARM_SANDBOX_OWNER_PID_LABEL)
            && owner_pid_is_alive(owner_pid)
        {
            continue;
        }
        if container.status.as_deref() != Some("running") {
            continue;
        }
        let Some(started_date) = container.started_date else {
            continue;
        };
        if now - started_date < ORPHANED_WARM_SANDBOX_MIN_AGE.as_secs_f64() {
            continue;
        }
        if let Err(error) = cleanup_named_container(
            container_bin,
            ContainerCliFlavor::AppleContainer,
            &container.configuration.id,
        )
        .await
        {
            eprintln!(
                "failed to clean up orphaned warm sandbox {}: {error}",
                container.configuration.id
            );
        }
    }

    Ok(())
}

fn apple_absolute_time_now() -> Result<f64> {
    let unix_now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(unix_now.as_secs_f64() - APPLE_ABSOLUTE_TIME_UNIX_OFFSET_SECONDS)
}

fn owner_pid_is_alive(pid: &str) -> bool {
    if pid.parse::<u32>().is_err() {
        return false;
    }
    std::process::Command::new("ps")
        .arg("-p")
        .arg(pid)
        .arg("-o")
        .arg("pid=")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Stop without deleting, so the next process can `try_resume` by label.
/// For destructive cleanup use `cleanup_named_container`.
fn stop_named_container_blocking(container_bin: &Path, cli: ContainerCliFlavor, name: &str) {
    let _ = cli; // both flavors accept `stop <name>`
    // `-t 0` = SIGKILL immediately on Docker (no 10s SIGTERM grace).
    run_container_admin_command_blocking(container_bin, ["stop", "-t", "0", name]);
}

fn schedule_cleanup_named_container(container_bin: PathBuf, cli: ContainerCliFlavor, name: String) {
    tokio::spawn(async move {
        if let Err(error) = cleanup_named_container(&container_bin, cli, &name).await {
            eprintln!("failed to clean up warm sandbox {name}: {error}");
        }
    });
}

async fn run_container_admin_command<const N: usize>(
    container_bin: &Path,
    timeout: Duration,
    args: [&str; N],
) -> Result<std::process::Output> {
    let mut command = Command::new(container_bin);
    command.args(args).kill_on_drop(true);
    match time::timeout(timeout, command.output()).await {
        Ok(output) => Ok(output?),
        Err(_) => Err(anyhow!(
            "container {} timed out after {}s",
            args.join(" "),
            timeout.as_secs()
        )),
    }
}

fn run_container_admin_command_blocking<const N: usize>(container_bin: &Path, args: [&str; N]) {
    let Ok(mut child) = std::process::Command::new(container_bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };

    let deadline = Instant::now() + WARM_SANDBOX_CLEANUP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                if let Err(error) = child.kill() {
                    eprintln!("failed to kill timed out container admin command: {error}");
                }
                if let Err(error) = child.wait() {
                    eprintln!("failed to wait for timed out container admin command: {error}");
                }
                return;
            }
            Err(_) => return,
        }
    }
}

fn is_missing_container_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("not found") || lower.contains("no such")
}

#[derive(Debug)]
struct ResumableContainerInfo {
    name: String,
    is_running: bool,
    /// `None` for running containers or backends without a `FinishedAt`.
    finished_at: Option<SystemTime>,
}

/// Look up any container labelled with this `SandboxKey`. Stale spec-hash
/// containers are reaped on the way out.
async fn find_resumable_container(
    container_bin: &Path,
    cli: ContainerCliFlavor,
    key: &str,
    spec_hash: &str,
) -> Result<Option<ResumableContainerInfo>> {
    match cli {
        ContainerCliFlavor::Docker => find_docker_resumable(container_bin, key, spec_hash).await,
        ContainerCliFlavor::AppleContainer => {
            find_apple_resumable(container_bin, key, spec_hash).await
        }
    }
}

/// One row of `docker ps --format '{{json .}}'`.
#[derive(Debug, Deserialize)]
struct DockerPsItem {
    #[serde(rename = "Names")]
    names: String,
    /// `"k1=v1,k2=v2"`; parse with [`parse_docker_labels`].
    #[serde(rename = "Labels")]
    labels: String,
    #[serde(rename = "State")]
    state: String,
    #[serde(rename = "CreatedAt")]
    created_at: String,
}

fn parse_docker_labels(raw: &str) -> HashMap<&str, &str> {
    raw.split(',').filter_map(|kv| kv.split_once('=')).collect()
}

async fn find_docker_resumable(
    container_bin: &Path,
    key: &str,
    spec_hash: &str,
) -> Result<Option<ResumableContainerInfo>> {
    // `docker ps -a --filter label=...` already does an exact-match on
    // the key label server-side. We still pull every candidate so we can
    // pick the most recent if there are duplicates (concurrent races)
    // and reap any whose spec hash has drifted.
    let key_filter = format!("label={WARM_SANDBOX_KEY_LABEL}={key}");
    let mut command = Command::new(container_bin);
    command
        .arg("ps")
        .arg("-a")
        .arg("--filter")
        .arg(&key_filter)
        .arg("--format")
        .arg("{{json .}}")
        .kill_on_drop(true);
    let output = match time::timeout(WARM_SANDBOX_CLEANUP_TIMEOUT, command.output()).await {
        Ok(out) => out?,
        Err(_) => {
            return Err(anyhow!(
                "docker ps timed out while listing resumable containers"
            ));
        }
    };
    if !output.status.success() {
        return Err(anyhow!(
            "docker ps failed while listing resumable containers: {}",
            render_command_error(&output.stderr)
        ));
    }

    let stdout = std::str::from_utf8(&output.stdout).context("docker ps output is not utf-8")?;
    let mut matches: Vec<(String, String, String)> = Vec::new();
    let mut stale_names: Vec<String> = Vec::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let item: DockerPsItem = serde_json::from_str(line)
            .with_context(|| format!("decoding docker ps json line: {line}"))?;
        let hash = parse_docker_labels(&item.labels)
            .get(WARM_SANDBOX_SPEC_HASH_LABEL)
            .copied()
            .unwrap_or("");
        if hash == spec_hash {
            matches.push((item.names, item.state, item.created_at));
        } else {
            stale_names.push(item.names);
        }
    }
    for name in stale_names {
        if let Err(err) =
            cleanup_named_container(container_bin, ContainerCliFlavor::Docker, &name).await
        {
            eprintln!("failed to reap container {name} with stale spec hash: {err}");
        }
    }
    // Prefer the most-recent entry (Docker's CreatedAt sorts
    // lexicographically as ISO-8601-ish text).
    matches.sort_by(|a, b| b.2.cmp(&a.2));
    let Some((name, state, _created)) = matches.into_iter().next() else {
        return Ok(None);
    };
    let is_running = state.eq_ignore_ascii_case("running");
    let finished_at = if !is_running {
        docker_container_finished_at(container_bin, &name).await?
    } else {
        None
    };
    Ok(Some(ResumableContainerInfo {
        name,
        is_running,
        finished_at,
    }))
}

async fn docker_container_finished_at(
    container_bin: &Path,
    name: &str,
) -> Result<Option<SystemTime>> {
    let mut command = Command::new(container_bin);
    command
        .arg("inspect")
        .arg("--format")
        .arg("{{.State.FinishedAt}}")
        .arg(name)
        .kill_on_drop(true);
    let output = match time::timeout(WARM_SANDBOX_CLEANUP_TIMEOUT, command.output()).await {
        Ok(out) => out?,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        // Container disappeared between list and inspect — treat as
        // unknown, the caller will create fresh.
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // "0001-01-01T00:00:00Z" is Docker's "never finished" sentinel.
    if s.is_empty() || s.starts_with("0001-01-01") {
        return Ok(None);
    }
    Ok(parse_rfc3339_to_system_time(&s))
}

fn parse_rfc3339_to_system_time(s: &str) -> Option<SystemTime> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    let nanos = dt.timestamp_nanos_opt()?;
    if nanos < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos as u64))
}

async fn find_apple_resumable(
    container_bin: &Path,
    key: &str,
    spec_hash: &str,
) -> Result<Option<ResumableContainerInfo>> {
    // Apple's `container` CLI doesn't expose a label filter; we list
    // everything and match in-process.
    let output = run_container_admin_command(
        container_bin,
        WARM_SANDBOX_CLEANUP_TIMEOUT,
        ["list", "--format", "json"],
    )
    .await?;
    if !output.status.success() {
        return Err(anyhow!(
            "container list failed: {}",
            render_command_error(&output.stderr)
        ));
    }
    let containers: Vec<ContainerListItem> = serde_json::from_slice(&output.stdout)?;
    let mut chosen: Option<ResumableContainerInfo> = None;
    let mut stale_names: Vec<String> = Vec::new();
    for c in containers {
        let labels = &c.configuration.labels;
        let Some(matched_key) = labels.get(WARM_SANDBOX_KEY_LABEL) else {
            continue;
        };
        if matched_key != key {
            continue;
        }
        let hash_matches = labels
            .get(WARM_SANDBOX_SPEC_HASH_LABEL)
            .map(|s| s == spec_hash)
            .unwrap_or(false);
        if !hash_matches {
            stale_names.push(c.configuration.id);
            continue;
        }
        // We don't have a portable finished-at signal from the apple
        // CLI's list output; trust the status field for run-state and
        // leave finished_at as None (the TTL guard simply skips its
        // staleness check when finished_at is unknown).
        let is_running = c.status.as_deref() == Some("running");
        chosen = Some(ResumableContainerInfo {
            name: c.configuration.id,
            is_running,
            finished_at: None,
        });
    }
    for name in stale_names {
        if let Err(err) =
            cleanup_named_container(container_bin, ContainerCliFlavor::AppleContainer, &name).await
        {
            eprintln!("failed to reap apple-container {name} with stale spec hash: {err}");
        }
    }
    Ok(chosen)
}

async fn start_named_container(
    container_bin: &Path,
    cli: ContainerCliFlavor,
    name: &str,
) -> Result<()> {
    let _ = cli; // both flavors take `start <name>`
    let output =
        run_container_admin_command(container_bin, WARM_SANDBOX_CLEANUP_TIMEOUT, ["start", name])
            .await?;
    if !output.status.success() {
        return Err(anyhow!(
            "failed to start container {name}: {}",
            render_command_error(&output.stderr)
        ));
    }
    Ok(())
}

fn is_already_exists_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("already exists")
}

pub(crate) fn sandbox_spec_hash(spec: &SandboxSpec) -> String {
    let mut hasher = DefaultHasher::new();
    spec.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn render_command_error(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}

fn network_name_for_policy(policy: SandboxNetworkPolicy) -> Option<&'static str> {
    matches!(policy, SandboxNetworkPolicy::Enabled).then_some(DEFAULT_ENABLED_NETWORK_NAME)
}

fn new_warm_container_name(key: &SandboxKey) -> String {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = hasher.finish();
    let generation = Uuid::new_v4().simple().to_string();
    format!("exo-{hash:016x}-{}", &generation[..8])
}

/// Capture a docker container's filesystem state as a SnapshotPayload.
///
/// Pipeline:
///   docker commit -p <container>  exo-snap-<uuid>     // image from container fs
///   docker save exo-snap-<uuid>                       // tarball on stdout
///   docker image rm exo-snap-<uuid>                   // local image no longer needed
///
/// `commit -p` pauses the container during commit to ensure a consistent
/// filesystem capture (no half-written files).
async fn docker_snapshot_container(
    container_bin: &Path,
    container_name: &str,
) -> Result<SnapshotPayload> {
    let snap_tag = format!("exo-snap-{}", Uuid::new_v4().simple());

    let commit_output = Command::new(container_bin)
        .arg("commit")
        .arg("-p")
        .arg(container_name)
        .arg(&snap_tag)
        .output()
        .await
        .with_context(|| format!("running `docker commit` for {container_name}"))?;
    if !commit_output.status.success() {
        bail!(
            "docker commit {container_name} {snap_tag} failed: {}",
            render_command_error(&commit_output.stderr)
        );
    }

    let save_output = Command::new(container_bin)
        .arg("save")
        .arg(&snap_tag)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("running `docker save {snap_tag}`"))?;
    if !save_output.status.success() {
        // Best-effort cleanup of the tag we just created.
        let _ = Command::new(container_bin)
            .arg("image")
            .arg("rm")
            .arg(&snap_tag)
            .output()
            .await;
        bail!(
            "docker save {snap_tag} failed: {}",
            render_command_error(&save_output.stderr)
        );
    }
    let bytes = Bytes::from(save_output.stdout);

    // Remove the local image tag now that the bytes are captured — the
    // canonical store of the snapshot is exoharness, not the docker daemon.
    let rm_output = Command::new(container_bin)
        .arg("image")
        .arg("rm")
        .arg(&snap_tag)
        .output()
        .await;
    if let Ok(output) = &rm_output
        && !output.status.success()
    {
        eprintln!(
            "warning: failed to remove ephemeral snapshot image {snap_tag}: {}",
            render_command_error(&output.stderr)
        );
    }

    Ok(SnapshotPayload {
        kind: SnapshotKind::DockerImageTar,
        bytes,
    })
}

/// Load a docker-save tarball back into the local docker daemon and return
/// the image reference docker assigned to it. The reference is what
/// subsequent `docker run` invocations use to start a container from this
/// snapshot's state.
async fn docker_load_image(container_bin: &Path, payload: &Bytes) -> Result<String> {
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new(container_bin)
        .arg("load")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning `docker load`")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("docker load: failed to acquire stdin"))?;
    stdin
        .write_all(payload)
        .await
        .context("writing snapshot bytes to `docker load` stdin")?;
    stdin.shutdown().await.ok();
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .context("waiting on `docker load`")?;
    if !output.status.success() {
        bail!(
            "docker load failed: {}",
            render_command_error(&output.stderr)
        );
    }

    // docker load prints lines like:
    //   Loaded image: <ref>
    //   Loaded image ID: sha256:<digest>
    // Prefer the named-tag line; fall back to image-ID.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Loaded image: ") {
            return Ok(rest.trim().to_string());
        }
    }
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Loaded image ID: ") {
            return Ok(rest.trim().to_string());
        }
    }
    bail!("docker load completed but no image reference found in output: {stdout}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_docker_labels_handles_empty_and_multi() {
        assert!(!parse_docker_labels("").contains_key("anything"));
        let labels =
            parse_docker_labels("exo.sandbox.key=conversation:c1:s1,exo.sandbox.spec-hash=abc123");
        assert_eq!(labels.get("exo.sandbox.key"), Some(&"conversation:c1:s1"));
        assert_eq!(labels.get("exo.sandbox.spec-hash"), Some(&"abc123"));
    }
}
