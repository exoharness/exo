//! macOS transport for the Linux Firecracker backend.
//!
//! Firecracker only runs on Linux/KVM. On Apple silicon that supports nested
//! virtualization, Lima can provide the Linux/KVM boundary while the Exo caller
//! remains a native macOS process:
//! https://developer.apple.com/documentation/virtualization/vzgenericplatformconfiguration/isnestedvirtualizationsupported
//! https://github.com/lima-vm/lima/blob/master/templates/default.yaml

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};

use anyhow::{Context as AnyhowContext, Result, anyhow, bail};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, ReadBuf};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::io::StreamReader;
use tokio_util::sync::PollSender;

use crate::egress::{
    EgressCredentialResolver, EgressRuntime, PublicUpstreamResolver, SandboxEgress,
    UpstreamResolver,
};
use crate::sandbox::{
    BoxSandboxTcpStream, ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand,
    SandboxCommandOutput, SnapshotFormat, SnapshotPayload,
};
use crate::{FirecrackerRequest, SandboxRequest};
use crate::{SandboxAttachment, SandboxProcessParts};

use super::FirecrackerLimaConfig;
use super::firecracker::FirecrackerConfig;
use super::firecracker_bridge::{
    FirecrackerBridgeClientFrame, FirecrackerBridgeRequest, FirecrackerBridgeResponse,
    FirecrackerBridgeServerFrame, FirecrackerBridgeStreamChannel, STREAM_CHUNK_BYTES, read_frame,
    write_frame,
};

static CONSUMABLE_SNAPSHOT_FORMATS: [SnapshotFormat; 1] = [SnapshotFormat::FirecrackerHostRef];

const BRIDGE_FRAME_QUEUE_DEPTH: usize = 16;
const BRIDGE_STREAM_QUEUE_DEPTH: usize = 16;
static LIMA_ONE_SHOT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
// The bridge runs as root inside Lima. Install the locally built executable to
// a root-owned path before sudo executes it so the build user cannot swap it.
const BRIDGE_INSTALL_PATH: &str = "/usr/local/libexec/exo-firecracker-bridge";

#[derive(Clone)]
pub struct LimaFirecrackerSandboxBackend {
    egress: Arc<EgressRuntime<LimaFirecrackerSandboxHandle>>,
    config: FirecrackerConfig,
    bridge: Arc<LimaBridgeManager>,
}

impl LimaFirecrackerSandboxBackend {
    async fn terminate_request(&self, request: SandboxRequest) -> Result<()> {
        match self
            .request(FirecrackerBridgeRequest::Terminate {
                config: self.config.clone(),
                request: request.into(),
            })
            .await?
        {
            FirecrackerBridgeResponse::Unit => Ok(()),
            _ => bail!("Firecracker Lima bridge returned the wrong response to terminate"),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_egress(
        mut self,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        upstream: Arc<dyn UpstreamResolver>,
    ) -> Self {
        self.egress = Arc::new(EgressRuntime::new(resolver, upstream));
        self
    }

    pub fn shutdown_egress(&self) {
        self.egress.shutdown();
    }

    async fn egress_transport(
        &self,
        allowed_hosts: &[String],
    ) -> Result<Arc<dyn crate::egress::EgressTransport>> {
        let connection = self.bridge.connection().await?;
        let FirecrackerBridgeResponse::Egress {
            listener_id,
            endpoints,
        } = connection
            .request(FirecrackerBridgeRequest::EgressCreate {
                listen: self.config.egress_listen,
                allowed_hosts: allowed_hosts.to_vec(),
            })
            .await?
        else {
            bail!("Lima bridge did not return egress listeners");
        };
        Ok(Arc::new(LimaEgressTransport::new(
            connection,
            listener_id,
            endpoints,
        )))
    }

    async fn acquire_request(
        &self,
        request: FirecrackerRequest,
    ) -> Result<LimaFirecrackerSandboxHandle> {
        // A one-shot Firecracker handle destroys its VM after the command. Do
        // not eagerly acquire it here and then acquire a second VM when the
        // command crosses the bridge.
        if request.lifecycle.idle_ttl.is_none() {
            let sequence = LIMA_ONE_SHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            return Ok(LimaFirecrackerSandboxHandle {
                egress: None,
                id: format!("firecracker-lima-oneshot:{sequence}"),
                provider_state: None,
                effective_image: None,
                source_ipv4: None,
                request,
                config: self.config.clone(),
                bridge: self.bridge.clone(),
            });
        }
        let response = self
            .request(FirecrackerBridgeRequest::Acquire {
                config: self.config.clone(),
                request: request.clone(),
            })
            .await?;
        let FirecrackerBridgeResponse::Handle {
            id,
            provider_state,
            effective_image,
            source_ipv4,
        } = response
        else {
            bail!("Firecracker Lima bridge returned the wrong response to acquire");
        };
        self.bound_handle(request, id, provider_state, effective_image, source_ipv4)
    }

    pub async fn new(config: FirecrackerConfig, lima: FirecrackerLimaConfig) -> Result<Self> {
        Self::new_with_egress(config, lima, None, Arc::new(PublicUpstreamResolver)).await
    }

    pub(crate) async fn new_with_egress(
        config: FirecrackerConfig,
        lima: FirecrackerLimaConfig,
        resolver: Option<Arc<dyn EgressCredentialResolver>>,
        upstream: Arc<dyn UpstreamResolver>,
    ) -> Result<Self> {
        let build_bridge = lima.bridge_binary.is_none();
        let bridge = Arc::new(LimaBridgeManager::new(
            lima.limactl,
            lima.instance,
            lima.bridge_binary
                .unwrap_or_else(|| PathBuf::from(BRIDGE_INSTALL_PATH)),
            build_bridge,
        ));
        bridge.prepare_bridge(&lima.target_dir).await?;
        bridge.connection().await?;
        Ok(Self {
            config,
            bridge,
            egress: Arc::new(EgressRuntime::new(resolver, upstream)),
        })
    }

    async fn request(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<FirecrackerBridgeResponse> {
        self.bridge.request(request).await
    }

    fn bound_handle(
        &self,
        mut request: FirecrackerRequest,
        id: String,
        provider_state: Option<serde_json::Value>,
        effective_image: Option<String>,
        source_ipv4: Option<std::net::Ipv4Addr>,
    ) -> Result<LimaFirecrackerSandboxHandle> {
        let effective_image = effective_image
            .context("Firecracker Lima bridge did not return a resolved image for its handle")?;
        request.spec.image.clone_from(&effective_image);
        request.provider_state.clone_from(&provider_state);
        Ok(LimaFirecrackerSandboxHandle {
            egress: None,
            id,
            provider_state,
            effective_image: Some(effective_image),
            source_ipv4,
            request,
            config: self.config.clone(),
            bridge: self.bridge.clone(),
        })
    }
}

#[async_trait]
impl ManagedSandboxBackend for LimaFirecrackerSandboxBackend {
    fn is_local(&self) -> bool {
        true
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &CONSUMABLE_SNAPSHOT_FORMATS
    }

    async fn resolve_image(&self, image: &str) -> Result<crate::ResolvedSandboxImage> {
        match self
            .request(FirecrackerBridgeRequest::ResolveImage {
                config: self.config.clone(),
                image: image.to_owned(),
            })
            .await?
        {
            FirecrackerBridgeResponse::Image(image) => Ok(image),
            _ => bail!("Firecracker Lima bridge returned the wrong response to resolve_image"),
        }
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let terminate = self.terminate_request(request.clone());
        self.egress
            .acquire(
                request.clone(),
                |hosts| async move { self.egress_transport(&hosts).await },
                |egress| async move {
                    let mut handle = self
                        .acquire_request(FirecrackerRequest {
                            sandbox: request,
                            egress_proxy: egress.as_ref().map(|egress| egress.endpoints()),
                        })
                        .await?;
                    if let Some(egress) = egress {
                        let source = handle
                            .source_ipv4
                            .context("Firecracker sandbox has no egress source")?;
                        egress.initialize(&handle, source).await?;
                        handle.egress = Some(egress);
                    }
                    Ok(handle)
                },
                terminate,
            )
            .await
            .map(|handle| crate::with_process_management(handle))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Firecracker sandboxes do not support external attachments")
    }

    async fn delete_snapshot(&self, payload: SnapshotPayload) -> Result<()> {
        self.bridge
            .delete_snapshot(self.config.clone(), payload)
            .await
    }

    async fn terminate(&self, request: SandboxRequest) -> Result<()> {
        let id = request.sandbox_id.clone();
        self.egress
            .terminate(&id, self.terminate_request(request))
            .await
    }

    async fn fork_sandbox(
        &self,
        source: SandboxRequest,
        target: SandboxRequest,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        source
            .spec
            .policy
            .validate_basic("Firecracker snapshot source")?;
        target
            .spec
            .policy
            .validate_basic("Firecracker snapshot target")?;
        if target.lifecycle.idle_ttl.is_none() {
            bail!("Firecracker Lima forks require a managed sandbox lifecycle");
        }
        let response = self
            .request(FirecrackerBridgeRequest::Fork {
                config: self.config.clone(),
                source: source.into(),
                target: target.clone().into(),
            })
            .await?;
        let FirecrackerBridgeResponse::Handle {
            id,
            provider_state,
            effective_image,
            source_ipv4,
        } = response
        else {
            bail!("Firecracker Lima bridge returned the wrong response to fork");
        };
        Ok(crate::with_process_management(Arc::new(
            self.bound_handle(
                target.into(),
                id,
                provider_state,
                effective_image,
                source_ipv4,
            )?,
        )))
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        request
            .spec
            .policy
            .validate_basic("Firecracker snapshot restore")?;
        if request.lifecycle.idle_ttl.is_none() {
            bail!("Firecracker Lima snapshot restores require a managed sandbox lifecycle");
        }
        let response = self
            .request(FirecrackerBridgeRequest::AcquireFromSnapshot {
                config: self.config.clone(),
                request: request.clone().into(),
                format: payload.format,
                payload: BASE64.encode(payload.bytes),
            })
            .await?;
        let FirecrackerBridgeResponse::Handle {
            id,
            provider_state,
            effective_image,
            source_ipv4,
        } = response
        else {
            bail!("Firecracker Lima bridge returned the wrong response to snapshot restore");
        };
        Ok(crate::with_process_management(Arc::new(
            self.bound_handle(
                request.into(),
                id,
                provider_state,
                effective_image,
                source_ipv4,
            )?,
        )))
    }
}

struct LimaFirecrackerSandboxHandle {
    egress: Option<Arc<SandboxEgress>>,
    source_ipv4: Option<std::net::Ipv4Addr>,
    id: String,
    provider_state: Option<serde_json::Value>,
    effective_image: Option<String>,
    request: FirecrackerRequest,
    config: FirecrackerConfig,
    bridge: Arc<LimaBridgeManager>,
}

#[async_trait]
impl ManagedSandboxHandle for LimaFirecrackerSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn provider_state(&self) -> Option<serde_json::Value> {
        self.provider_state.clone()
    }

    fn effective_image(&self) -> Option<String> {
        self.effective_image.clone()
    }

    async fn is_running(&self) -> Result<Option<bool>> {
        match self
            .bridge
            .request(FirecrackerBridgeRequest::IsRunning {
                config: self.config.clone(),
                request: self.request.sandbox.clone(),
            })
            .await?
        {
            FirecrackerBridgeResponse::Running { running } => Ok(running),
            _ => bail!("Firecracker Lima bridge returned the wrong response to is_running"),
        }
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let command = SandboxEgress::prepare_command(self.egress.as_deref(), command)?;
        let command = command.as_ref();
        let response = self
            .bridge
            .request(FirecrackerBridgeRequest::Exec {
                config: self.config.clone(),
                request: self.request.clone(),
                command: command.clone(),
            })
            .await?;
        let FirecrackerBridgeResponse::Exec { output } = response else {
            bail!("Firecracker Lima bridge returned the wrong response to exec");
        };
        Ok(output)
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
        let command = SandboxEgress::prepare_command(self.egress.as_deref(), command)?;
        let command = command.as_ref();
        self.bridge
            .start_process(FirecrackerBridgeRequest::StartProcess {
                config: self.config.clone(),
                request: self.request.clone(),
                command: command.clone(),
            })
            .await
    }

    fn supports_tcp(&self) -> bool {
        true
    }

    async fn connect_tcp(&self, port: u16) -> Result<Option<BoxSandboxTcpStream>> {
        if self.request.lifecycle.idle_ttl.is_none() {
            bail!("one-shot Firecracker Lima sandboxes do not support TCP connections");
        }
        let stream = self
            .bridge
            .connect_tcp(FirecrackerBridgeRequest::ConnectTcp {
                config: self.config.clone(),
                request: self.request.clone(),
                port,
            })
            .await?;
        Ok(Some(Box::pin(stream)))
    }

    async fn stop(&self) -> Result<()> {
        if self.request.lifecycle.idle_ttl.is_none() {
            bail!("one-shot Firecracker Lima sandboxes cannot be stopped independently");
        }
        match self
            .bridge
            .request(FirecrackerBridgeRequest::Stop {
                config: self.config.clone(),
                request: self.request.clone(),
            })
            .await?
        {
            FirecrackerBridgeResponse::Unit => {
                if let Some(egress) = &self.egress {
                    egress.close();
                }
                Ok(())
            }
            _ => bail!("Firecracker Lima bridge returned the wrong response to stop"),
        }
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("Firecracker sandboxes cannot be detached")
    }

    async fn delete_snapshot(&self, payload: SnapshotPayload) -> Result<()> {
        self.bridge
            .delete_snapshot(self.config.clone(), payload)
            .await
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        if self.egress.is_some() {
            bail!(super::firecracker::PROXIED_SNAPSHOT_UNSUPPORTED);
        }
        if self.request.lifecycle.idle_ttl.is_none() {
            bail!("one-shot Firecracker Lima sandboxes cannot be snapshotted");
        }
        let response = self
            .bridge
            .request(FirecrackerBridgeRequest::Snapshot {
                config: self.config.clone(),
                request: self.request.clone(),
            })
            .await?;
        let FirecrackerBridgeResponse::Snapshot { format, payload } = response else {
            bail!("Firecracker Lima bridge returned the wrong response to snapshot");
        };
        let bytes = BASE64
            .decode(payload)
            .context("decoding Firecracker snapshot bridge response")?;
        Ok(SnapshotPayload {
            format,
            bytes: Bytes::from(bytes),
        })
    }
}

struct LimaBridgeManager {
    limactl: PathBuf,
    instance: String,
    bridge_binary: PathBuf,
    build_bridge: bool,
    connection: Mutex<Option<Arc<LimaBridgeConnection>>>,
}

impl LimaBridgeManager {
    async fn delete_snapshot(
        &self,
        config: FirecrackerConfig,
        payload: SnapshotPayload,
    ) -> Result<()> {
        match self
            .request(FirecrackerBridgeRequest::DeleteSnapshot {
                config,
                format: payload.format,
                payload: BASE64.encode(payload.bytes),
            })
            .await?
        {
            FirecrackerBridgeResponse::Unit => Ok(()),
            _ => bail!("Firecracker Lima bridge returned the wrong response to delete snapshot"),
        }
    }

    fn new(limactl: PathBuf, instance: String, bridge_binary: PathBuf, build_bridge: bool) -> Self {
        Self {
            limactl,
            instance,
            bridge_binary,
            build_bridge,
            connection: Mutex::new(None),
        }
    }

    async fn prepare_bridge(&self, target_dir: &Path) -> Result<()> {
        // `limactl start <name>` silently provisions a brand-new VM from
        // template:default when no instance with that name exists — and that
        // template mounts the user's whole $HOME into the VM. A typo'd
        // instance name must fail loudly rather than hand a root Firecracker
        // stack an implicit home-directory mount; only the curated instance
        // from the README (writable mount limited to the Exo checkout) is
        // acceptable.
        let instances = Command::new(&self.limactl)
            .arg("list")
            .arg("--format")
            .arg("{{.Name}}")
            .output()
            .await
            .context("listing Lima instances")?;
        if !instances.status.success() {
            bail!(
                "listing Lima instances failed ({}): {}",
                instances.status,
                String::from_utf8_lossy(&instances.stderr).trim()
            );
        }
        if !String::from_utf8_lossy(&instances.stdout)
            .lines()
            .any(|name| name.trim() == self.instance)
        {
            bail!(
                "Lima instance {:?} does not exist; create the dedicated Firecracker VM first \
                 (see support/firecracker/README.md, macOS development)",
                self.instance
            );
        }
        self.run_checked(
            Command::new(&self.limactl).arg("start").arg(&self.instance),
            "starting the Firecracker Lima VM",
        )
        .await?;
        self.check_kvm_access().await?;
        if !self.build_bridge {
            return Ok(());
        }
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("resolving the Exo source root")?;
        let mut command = Command::new(&self.limactl);
        // Keep the one-time Linux build viable in the 4 GiB development VM:
        // GNU ld retained multiple GiB while linking Exo, whereas LLVM lld is
        // designed for lower-memory parallel linking. Debug info and incremental
        // state are not useful for this executable-only bridge cache.
        // https://lld.llvm.org/
        command
            .arg("shell")
            .arg(&self.instance)
            .arg("--")
            .arg("env")
            .arg(format!("CARGO_TARGET_DIR={}", target_dir.display()))
            .arg("CARGO_BUILD_JOBS=1")
            .arg("CARGO_INCREMENTAL=0")
            .arg("CARGO_PROFILE_DEV_DEBUG=0")
            .arg("RUSTFLAGS=-C linker=clang -C link-arg=-fuse-ld=lld")
            .arg("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(source_root.join("Cargo.toml"))
            .arg("--package")
            .arg("exoharness")
            .arg("--features")
            .arg("firecracker")
            .arg("--example")
            .arg("firecracker-bridge");
        self.run_checked(&mut command, "building the Exo Firecracker bridge in Lima")
            .await?;
        // Copy the unprivileged build output to a root-owned path and execute
        // only that: sudo must never run a binary that the build user (or any
        // other uid, via the sticky-bit /var/tmp target directory) can still
        // replace after this point. This also lets a hardened sudoers restrict
        // the lima user to exactly this root-owned path.
        let mut install = Command::new(&self.limactl);
        install
            .arg("shell")
            .arg(&self.instance)
            .arg("--")
            .arg("sudo")
            .arg("-n")
            .arg("install")
            .arg("-o")
            .arg("root")
            .arg("-g")
            .arg("root")
            .arg("-m")
            .arg("0755")
            .arg(target_dir.join("debug/examples/firecracker-bridge"))
            .arg(BRIDGE_INSTALL_PATH);
        self.run_checked(
            &mut install,
            "installing the Exo Firecracker bridge as root in Lima",
        )
        .await
    }

    async fn check_kvm_access(&self) -> Result<()> {
        for mode in ["-r", "-w"] {
            let mut command = Command::new(&self.limactl);
            command
                .arg("shell")
                .arg(&self.instance)
                .arg("--")
                .arg("sudo")
                .arg("-n")
                .arg("test")
                .arg(mode)
                .arg("/dev/kvm");
            self.run_checked(&mut command, "checking Lima /dev/kvm access")
                .await
                .with_context(|| {
                    format!(
                        "Lima instance {:?} does not provide read/write /dev/kvm access; \
                         Firecracker on macOS requires Apple M3 or newer and a VZ Lima VM \
                         created with --nested-virt",
                        self.instance
                    )
                })?;
        }
        Ok(())
    }

    async fn run_checked(&self, command: &mut Command, description: &str) -> Result<()> {
        let output = command.output().await?;
        if output.status.success() {
            return Ok(());
        }
        bail!(
            "{description} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }

    fn bridge_command(&self) -> Command {
        let mut command = Command::new(&self.limactl);
        // The direct backend deliberately uses Firecracker's jailer and host
        // network setup, both of which require root inside the Lima VM. `-n`
        // prevents an invisible password prompt from hanging the host process.
        // https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md
        command
            .arg("shell")
            .arg(&self.instance)
            .arg("--")
            .arg("sudo")
            .arg("-n")
            .arg(&self.bridge_binary)
            .arg("firecracker-bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    async fn connection(&self) -> Result<Arc<LimaBridgeConnection>> {
        let mut connection = self.connection.lock().await;
        if let Some(active) = connection.as_ref()
            && !active.is_closed()
        {
            return Ok(Arc::clone(active));
        }
        let active = Arc::new(LimaBridgeConnection::spawn(self.bridge_command()).await?);
        *connection = Some(Arc::clone(&active));
        Ok(active)
    }

    async fn invalidate(&self, failed: &Arc<LimaBridgeConnection>) {
        let mut connection = self.connection.lock().await;
        if connection
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, failed))
        {
            *connection = None;
        }
    }

    async fn request(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<FirecrackerBridgeResponse> {
        self.with_connection(|connection| Box::pin(connection.request(request)))
            .await
    }

    async fn start_process(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<SandboxProcessParts> {
        self.with_connection(|connection| Box::pin(connection.start_process(request)))
            .await
    }

    async fn connect_tcp(&self, request: FirecrackerBridgeRequest) -> Result<LimaTcpStream> {
        self.with_connection(|connection| Box::pin(connection.connect_tcp(request)))
            .await
    }

    async fn with_connection<T: Send>(
        &self,
        operation: impl for<'a> FnOnce(&'a LimaBridgeConnection) -> BoxFuture<'a, Result<T>>,
    ) -> Result<T> {
        let connection = self.connection().await?;
        let result = operation(&connection).await;
        if result.is_err() && connection.is_closed() {
            self.invalidate(&connection).await;
        }
        // A closed connection makes the result ambiguous: the bridge may have
        // completed the request before its response was lost. Replaying exec,
        // start_process, or fork would duplicate side effects, so the next
        // call reconnects but this one remains at-most-once.
        result
    }
}

impl Drop for LimaBridgeManager {
    fn drop(&mut self) {
        if let Ok(connection) = self.connection.try_lock()
            && let Some(connection) = connection.as_ref()
        {
            connection.shutdown();
        }
    }
}

type RpcResult = std::result::Result<FirecrackerBridgeResponse, String>;
type OpenResult = std::result::Result<(), String>;
type ExitResult = std::result::Result<i32, String>;
type BridgeReadResult = io::Result<Bytes>;
type BridgeReadStream = StreamReader<ReceiverStream<BridgeReadResult>, Bytes>;

struct LimaBridgeConnection {
    outgoing: mpsc::Sender<FirecrackerBridgeClientFrame>,
    state: Arc<LimaBridgeClientState>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct LimaBridgeClientState {
    requests: StdMutex<HashMap<u64, oneshot::Sender<RpcResult>>>,
    streams: StdMutex<HashMap<u64, ClientStreamRoutes>>,
    closed: AtomicBool,
    close_reason: StdMutex<Option<String>>,
}

#[derive(Default)]
struct ClientStreamRoutes {
    opened: Option<oneshot::Sender<OpenResult>>,
    stdout: Option<mpsc::Sender<BridgeReadResult>>,
    stderr: Option<mpsc::Sender<BridgeReadResult>>,
    tcp: Option<mpsc::Sender<BridgeReadResult>>,
    exited: Option<oneshot::Sender<ExitResult>>,
}

impl LimaBridgeConnection {
    async fn spawn(mut command: Command) -> Result<Self> {
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .context("opening Firecracker bridge stdin")?;
        let mut stdout = child
            .stdout
            .take()
            .context("opening Firecracker bridge stdout")?;
        let mut stderr = child
            .stderr
            .take()
            .context("opening Firecracker bridge stderr")?;
        let state = Arc::new(LimaBridgeClientState::default());
        let (outgoing, mut outgoing_receiver) = mpsc::channel(BRIDGE_FRAME_QUEUE_DEPTH);

        let writer_state = Arc::clone(&state);
        tokio::spawn(async move {
            while let Some(frame) = outgoing_receiver.recv().await {
                if let Err(error) = write_frame(&mut stdin, &frame).await {
                    writer_state.fail(format!("writing Firecracker Lima bridge: {error:#}"));
                    return;
                }
            }
        });

        let reader_state = Arc::clone(&state);
        let reader_outgoing = outgoing.clone();
        tokio::spawn(async move {
            loop {
                match read_frame::<FirecrackerBridgeServerFrame>(&mut stdout).await {
                    Ok(frame) => {
                        if let Err(error) = reader_state.handle_frame(frame, &reader_outgoing) {
                            reader_state
                                .fail(format!("decoding Firecracker Lima bridge: {error:#}"));
                            return;
                        }
                    }
                    Err(error) => {
                        reader_state.fail(format!("reading Firecracker Lima bridge: {error:#}"));
                        return;
                    }
                }
            }
        });

        tokio::spawn(async move {
            let mut lines = BufReader::new(&mut stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(message)) => tracing::info!(
                        target: "exo_firecracker_lima_bridge",
                        %message,
                        "Firecracker Lima bridge"
                    ),
                    Ok(None) => return,
                    Err(error) => {
                        tracing::debug!(%error, "failed to drain Firecracker Lima bridge stderr");
                        return;
                    }
                }
            }
        });

        let wait_state = Arc::clone(&state);
        tokio::spawn(async move {
            let reason = match child.wait().await {
                Ok(status) => format!("Firecracker Lima bridge exited with {status}"),
                Err(error) => format!("waiting for Firecracker Lima bridge: {error}"),
            };
            wait_state.fail(reason);
        });

        Ok(Self {
            outgoing,
            state,
            next_id: AtomicU64::new(1),
        })
    }

    fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn send(&self, frame: FirecrackerBridgeClientFrame) -> Result<()> {
        self.outgoing.send(frame).await.map_err(|_| {
            anyhow!(
                "Firecracker Lima bridge is closed: {}",
                self.state.close_reason()
            )
        })
    }

    async fn request(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<FirecrackerBridgeResponse> {
        let id = self.next_id();
        let (sender, receiver) = oneshot::channel();
        self.state.insert_request(id, sender)?;
        if let Err(error) = self
            .send(FirecrackerBridgeClientFrame::Request {
                id,
                request: Box::new(request),
            })
            .await
        {
            self.state.remove_request(id);
            return Err(error);
        }
        receiver
            .await
            .map_err(|_| anyhow!("Firecracker Lima bridge dropped request {id}"))?
            .map_err(|message| anyhow!(message))
    }

    async fn open_stream(
        &self,
        id: u64,
        request: FirecrackerBridgeRequest,
        opened: oneshot::Receiver<OpenResult>,
        kind: &str,
    ) -> Result<()> {
        if let Err(error) = self
            .send(FirecrackerBridgeClientFrame::Request {
                id,
                request: Box::new(request),
            })
            .await
        {
            self.state.remove_stream(id);
            return Err(error);
        }
        opened
            .await
            .map_err(|_| anyhow!("Firecracker Lima bridge dropped {kind} open {id}"))?
            .map_err(|message| anyhow!(message))
    }

    async fn start_process(
        &self,
        request: FirecrackerBridgeRequest,
    ) -> Result<SandboxProcessParts> {
        let id = self.next_id();
        let (opened_sender, opened_receiver) = oneshot::channel();
        let (stdout_sender, stdout_receiver) = mpsc::channel(BRIDGE_STREAM_QUEUE_DEPTH);
        let (stderr_sender, stderr_receiver) = mpsc::channel(BRIDGE_STREAM_QUEUE_DEPTH);
        let (exit_sender, exit_receiver) = oneshot::channel();
        self.state.insert_stream(
            id,
            ClientStreamRoutes {
                opened: Some(opened_sender),
                stdout: Some(stdout_sender),
                stderr: Some(stderr_sender),
                exited: Some(exit_sender),
                ..ClientStreamRoutes::default()
            },
        )?;
        let cancel = BridgeStreamCancel::new(id, self.outgoing.clone(), Arc::clone(&self.state));
        self.open_stream(id, request, opened_receiver, "process")
            .await?;

        let wait = Box::pin(async move {
            let mut cancel = cancel;
            let result = exit_receiver
                .await
                .map_err(|_| anyhow!("Firecracker Lima bridge dropped process exit {id}"))?
                .map_err(|message| anyhow!(message));
            cancel.disarm();
            result
        });
        Ok(SandboxProcessParts {
            stdout: Box::pin(bridge_read_stream(stdout_receiver).compat()),
            stderr: Box::pin(bridge_read_stream(stderr_receiver).compat()),
            stdin: Box::pin(BridgeWriteStream::new(id, self.outgoing.clone()).compat_write()),
            wait,
        })
    }

    async fn connect_tcp(&self, request: FirecrackerBridgeRequest) -> Result<LimaTcpStream> {
        let id = self.next_id();
        let (opened_sender, opened_receiver) = oneshot::channel();
        let (tcp_sender, tcp_receiver) = mpsc::channel(BRIDGE_STREAM_QUEUE_DEPTH);
        self.state.insert_stream(
            id,
            ClientStreamRoutes {
                opened: Some(opened_sender),
                tcp: Some(tcp_sender),
                ..ClientStreamRoutes::default()
            },
        )?;
        let mut cancel =
            BridgeStreamCancel::new(id, self.outgoing.clone(), Arc::clone(&self.state));
        self.open_stream(id, request, opened_receiver, "TCP")
            .await?;
        cancel.disarm();
        Ok(LimaTcpStream {
            id,
            reader: bridge_read_stream(tcp_receiver),
            writer: BridgeWriteStream::new(id, self.outgoing.clone()),
            state: Arc::clone(&self.state),
        })
    }

    fn shutdown(&self) {
        if self
            .outgoing
            .try_send(FirecrackerBridgeClientFrame::Shutdown)
            .is_err()
        {
            tracing::debug!("Firecracker Lima bridge was already closed during shutdown");
        }
    }
}

impl LimaBridgeClientState {
    fn close_reason(&self) -> String {
        self.close_reason
            .lock()
            .ok()
            .and_then(|reason| reason.clone())
            .unwrap_or_else(|| "connection closed".to_string())
    }

    fn fail(&self, reason: String) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut close_reason) = self.close_reason.lock() {
            *close_reason = Some(reason.clone());
        }
        if let Ok(mut requests) = self.requests.lock() {
            for (_, sender) in requests.drain() {
                if sender.send(Err(reason.clone())).is_err() {
                    tracing::debug!("Firecracker bridge request receiver already closed");
                }
            }
        }
        if let Ok(mut streams) = self.streams.lock() {
            for (_, routes) in streams.drain() {
                routes.fail(reason.clone());
            }
        }
    }

    fn handle_frame(
        &self,
        frame: FirecrackerBridgeServerFrame,
        outgoing: &mpsc::Sender<FirecrackerBridgeClientFrame>,
    ) -> Result<()> {
        match frame {
            FirecrackerBridgeServerFrame::Response { id, result } => {
                let sender = self
                    .requests
                    .lock()
                    .map_err(|_| anyhow!("Firecracker bridge request lock is poisoned"))?
                    .remove(&id)
                    .context("Firecracker bridge response has an unknown request id")?;
                if sender.send(result).is_err() {
                    tracing::debug!(
                        id,
                        "Firecracker bridge request receiver closed before response"
                    );
                }
            }
            FirecrackerBridgeServerFrame::StreamOpened { id } => {
                let mut streams = self
                    .streams
                    .lock()
                    .map_err(|_| anyhow!("Firecracker bridge stream lock is poisoned"))?;
                let Some(routes) = streams.get_mut(&id) else {
                    return Ok(());
                };
                let sender = routes
                    .opened
                    .take()
                    .context("Firecracker bridge opened a stream twice")?;
                if sender.send(Ok(())).is_err() {
                    tracing::debug!(id, "Firecracker bridge stream opener was dropped");
                }
            }
            FirecrackerBridgeServerFrame::StreamData { id, channel, data } => {
                self.send_stream_data(id, channel, Bytes::from(BASE64.decode(data)?), outgoing)?;
            }
            FirecrackerBridgeServerFrame::StreamClosed { id, channel } => {
                self.close_stream_channel(id, channel)?;
                if channel == FirecrackerBridgeStreamChannel::Tcp {
                    self.remove_stream(id);
                }
            }
            FirecrackerBridgeServerFrame::ProcessExited { id, exit_code } => {
                let Some(mut routes) = self.remove_stream(id) else {
                    return Ok(());
                };
                routes.close_outputs();
                if let Some(sender) = routes.exited.take()
                    && sender.send(Ok(exit_code)).is_err()
                {
                    tracing::debug!(id, "Firecracker bridge process waiter was dropped");
                }
            }
            FirecrackerBridgeServerFrame::StreamError { id, message } => {
                if let Some(routes) = self.remove_stream(id) {
                    routes.fail(message);
                }
            }
        }
        Ok(())
    }

    fn send_stream_data(
        &self,
        id: u64,
        channel: FirecrackerBridgeStreamChannel,
        data: Bytes,
        outgoing: &mpsc::Sender<FirecrackerBridgeClientFrame>,
    ) -> Result<()> {
        let sender = {
            let streams = self
                .streams
                .lock()
                .map_err(|_| anyhow!("Firecracker bridge stream lock is poisoned"))?;
            let Some(routes) = streams.get(&id) else {
                return Ok(());
            };
            match channel {
                FirecrackerBridgeStreamChannel::Stdout => routes.stdout.as_ref(),
                FirecrackerBridgeStreamChannel::Stderr => routes.stderr.as_ref(),
                FirecrackerBridgeStreamChannel::Tcp => routes.tcp.as_ref(),
            }
            .context("Firecracker bridge data used the wrong stream channel")?
            .clone()
        };
        match sender.try_send(Ok(data)) {
            Ok(()) => return Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    id,
                    ?channel,
                    "canceling Firecracker bridge stream whose reader stopped draining"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!(id, ?channel, "Firecracker bridge stream reader was dropped");
            }
        }
        if let Some(routes) = self.remove_stream(id) {
            routes.fail("Firecracker bridge stream reader stopped draining".to_string());
        }
        send_bridge_frame_on_drop(outgoing, FirecrackerBridgeClientFrame::StreamCancel { id });
        Ok(())
    }

    fn close_stream_channel(&self, id: u64, channel: FirecrackerBridgeStreamChannel) -> Result<()> {
        let mut streams = self
            .streams
            .lock()
            .map_err(|_| anyhow!("Firecracker bridge stream lock is poisoned"))?;
        let Some(routes) = streams.get_mut(&id) else {
            return Ok(());
        };
        let sender = match channel {
            FirecrackerBridgeStreamChannel::Stdout => &mut routes.stdout,
            FirecrackerBridgeStreamChannel::Stderr => &mut routes.stderr,
            FirecrackerBridgeStreamChannel::Tcp => &mut routes.tcp,
        };
        sender
            .take()
            .context("Firecracker bridge closed the wrong stream channel")?;
        Ok(())
    }

    // Both insert paths re-check `closed` after inserting: fail() drains the
    // maps exactly once, so an entry inserted after that drain would never be
    // completed and its caller would wait forever (requests have no timeout so
    // long-running execs can finish). fail() stores `closed` before draining,
    // which makes this check sufficient to close the race in every
    // interleaving: either fail() sees our entry, or we see `closed`.
    fn insert_request(&self, id: u64, sender: oneshot::Sender<RpcResult>) -> Result<()> {
        self.requests
            .lock()
            .map_err(|_| anyhow!("Firecracker bridge request lock is poisoned"))?
            .insert(id, sender);
        if self.closed.load(Ordering::Acquire) {
            self.remove_request(id);
            bail!("Firecracker Lima bridge is closed: {}", self.close_reason());
        }
        Ok(())
    }

    fn insert_stream(&self, id: u64, routes: ClientStreamRoutes) -> Result<()> {
        if self
            .streams
            .lock()
            .map_err(|_| anyhow!("Firecracker bridge stream lock is poisoned"))?
            .insert(id, routes)
            .is_some()
        {
            bail!("duplicate Firecracker bridge stream id {id}");
        }
        if self.closed.load(Ordering::Acquire) {
            self.remove_stream(id);
            bail!("Firecracker Lima bridge is closed: {}", self.close_reason());
        }
        Ok(())
    }

    fn remove_request(&self, id: u64) {
        if let Ok(mut requests) = self.requests.lock() {
            requests.remove(&id);
        }
    }

    fn remove_stream(&self, id: u64) -> Option<ClientStreamRoutes> {
        self.streams
            .lock()
            .ok()
            .and_then(|mut streams| streams.remove(&id))
    }
}

impl ClientStreamRoutes {
    fn close_outputs(&mut self) {
        self.stdout = None;
        self.stderr = None;
    }

    fn fail(mut self, message: String) {
        if let Some(sender) = self.opened.take()
            && sender.send(Err(message.clone())).is_err()
        {
            tracing::debug!("Firecracker bridge stream opener already closed");
        }
        for sender in [self.stdout.take(), self.stderr.take(), self.tcp.take()]
            .into_iter()
            .flatten()
        {
            if sender
                .try_send(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    message.clone(),
                )))
                .is_err()
            {
                tracing::debug!("Firecracker bridge stream reader already closed");
            }
        }
        if let Some(sender) = self.exited.take()
            && sender.send(Err(message)).is_err()
        {
            tracing::debug!("Firecracker bridge process waiter already closed");
        }
    }
}

fn bridge_read_stream(receiver: mpsc::Receiver<BridgeReadResult>) -> BridgeReadStream {
    StreamReader::new(ReceiverStream::new(receiver))
}

struct BridgeWriteStream {
    id: u64,
    outgoing: mpsc::Sender<FirecrackerBridgeClientFrame>,
    poll_sender: PollSender<FirecrackerBridgeClientFrame>,
    closed: bool,
}

impl BridgeWriteStream {
    fn new(id: u64, outgoing: mpsc::Sender<FirecrackerBridgeClientFrame>) -> Self {
        Self {
            id,
            poll_sender: PollSender::new(outgoing.clone()),
            outgoing,
            closed: false,
        }
    }

    fn poll_send(
        &mut self,
        context: &mut Context<'_>,
        frame: FirecrackerBridgeClientFrame,
    ) -> Poll<io::Result<()>> {
        match self.poll_sender.poll_reserve(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Firecracker bridge closed",
            ))),
            Poll::Ready(Ok(())) => Poll::Ready(self.poll_sender.send_item(frame).map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "Firecracker bridge closed")
            })),
        }
    }
}

impl AsyncWrite for BridgeWriteStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Firecracker bridge stream input is closed",
            )));
        }
        let written = buffer.len().min(STREAM_CHUNK_BYTES);
        let frame = FirecrackerBridgeClientFrame::StreamInput {
            id: self.id,
            data: BASE64.encode(&buffer[..written]),
        };
        match self.poll_send(context, frame) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(written)),
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.closed {
            let id = self.id;
            match self.poll_send(
                context,
                FirecrackerBridgeClientFrame::StreamInputClosed { id },
            ) {
                Poll::Ready(Ok(())) => self.closed = true,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for BridgeWriteStream {
    fn drop(&mut self) {
        if !self.closed {
            send_bridge_frame_on_drop(
                &self.outgoing,
                FirecrackerBridgeClientFrame::StreamInputClosed { id: self.id },
            );
        }
    }
}

struct BridgeStreamCancel {
    id: u64,
    outgoing: mpsc::Sender<FirecrackerBridgeClientFrame>,
    state: Arc<LimaBridgeClientState>,
    armed: bool,
}

impl BridgeStreamCancel {
    fn new(
        id: u64,
        outgoing: mpsc::Sender<FirecrackerBridgeClientFrame>,
        state: Arc<LimaBridgeClientState>,
    ) -> Self {
        Self {
            id,
            outgoing,
            state,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BridgeStreamCancel {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.state.remove_stream(self.id);
        send_bridge_frame_on_drop(
            &self.outgoing,
            FirecrackerBridgeClientFrame::StreamCancel { id: self.id },
        );
    }
}

struct LimaTcpStream {
    id: u64,
    reader: BridgeReadStream,
    writer: BridgeWriteStream,
    state: Arc<LimaBridgeClientState>,
}

impl AsyncRead for LimaTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(context, buffer)
    }
}

impl AsyncWrite for LimaTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.writer).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.writer).poll_shutdown(context)
    }
}

impl Drop for LimaTcpStream {
    fn drop(&mut self) {
        self.state.remove_stream(self.id);
        send_bridge_frame_on_drop(
            &self.writer.outgoing,
            FirecrackerBridgeClientFrame::StreamCancel { id: self.id },
        );
    }
}

fn send_bridge_frame_on_drop(
    outgoing: &mpsc::Sender<FirecrackerBridgeClientFrame>,
    frame: FirecrackerBridgeClientFrame,
) {
    match outgoing.try_send(frame) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::debug!("Firecracker bridge already closed while dropping a stream");
        }
        Err(mpsc::error::TrySendError::Full(frame)) => {
            let outgoing = outgoing.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if outgoing.send(frame).await.is_err() {
                        tracing::debug!("Firecracker bridge closed while dropping a stream");
                    }
                });
            }
        }
    }
}

struct LimaEgressTransport {
    connection: Arc<LimaBridgeConnection>,
    listener_id: String,
    endpoints: crate::SandboxEgressProxy,
    http: Mutex<mpsc::Receiver<Result<BoxSandboxTcpStream>>>,
    https: Mutex<mpsc::Receiver<Result<BoxSandboxTcpStream>>>,
    cancel: tokio_util::sync::CancellationToken,
    cleanup: Shared<BoxFuture<'static, Result<(), Arc<anyhow::Error>>>>,
}

impl LimaEgressTransport {
    fn new(
        connection: Arc<LimaBridgeConnection>,
        listener_id: String,
        endpoints: crate::SandboxEgressProxy,
    ) -> Self {
        let cancel = tokio_util::sync::CancellationToken::new();
        let receive = |tls| {
            let (sender, receiver) = mpsc::channel(8);
            let connection = connection.clone();
            let listener_id = listener_id.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    let operation = async {
                        let permit = sender.reserve().await.context("egress receiver closed")?;
                        match connection
                            .connect_tcp(FirecrackerBridgeRequest::EgressAccept {
                                listener_id: listener_id.clone(),
                                tls,
                            })
                            .await
                        {
                            Ok(stream) => {
                                permit.send(Ok(Box::pin(stream) as BoxSandboxTcpStream));
                            }
                            Err(error) => {
                                permit.send(Err(error));
                                bail!("egress bridge accept failed");
                            }
                        }
                        Ok::<_, anyhow::Error>(())
                    };
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        result = operation => if result.is_err() { cancel.cancel(); break; }
                    }
                }
            });
            Mutex::new(receiver)
        };
        let http = receive(false);
        let https = receive(true);
        let cleanup_connection = connection.clone();
        let cleanup_id = listener_id.clone();
        let cleanup_cancel = cancel.clone();
        // Drop and explicit shutdown share one RPC. Shutdown waits for it so a
        // replacement listener can immediately reuse the same ports.
        let cleanup = async move {
            match cleanup_connection
                .request(FirecrackerBridgeRequest::EgressClose {
                    listener_id: cleanup_id,
                })
                .await?
            {
                FirecrackerBridgeResponse::Unit => Ok(()),
                _ => bail!("Lima bridge did not close egress listener"),
            }
        }
        .map(|result: Result<()>| result.map_err(Arc::new))
        .boxed()
        .shared();
        let background_cleanup = cleanup.clone();
        tokio::spawn(async move {
            cleanup_cancel.cancelled().await;
            if let Err(error) = background_cleanup.await {
                tracing::debug!(%error, "egress listener cleanup failed after bridge closure");
            }
        });
        Self {
            connection,
            listener_id,
            endpoints,
            http,
            https,
            cancel,
            cleanup,
        }
    }
}

#[async_trait]
impl crate::egress::EgressTransport for LimaEgressTransport {
    fn endpoints(&self) -> crate::SandboxEgressProxy {
        self.endpoints
    }
    async fn bind_source(&self, source: std::net::Ipv4Addr) -> Result<()> {
        match self
            .connection
            .request(FirecrackerBridgeRequest::EgressBind {
                listener_id: self.listener_id.clone(),
                source,
            })
            .await?
        {
            FirecrackerBridgeResponse::Unit => Ok(()),
            _ => bail!("Lima bridge did not bind egress source"),
        }
    }
    async fn accept(&self, tls: bool) -> Result<BoxSandboxTcpStream> {
        let receiver = if tls { &self.https } else { &self.http };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => bail!("egress bridge closed"),
            stream = async { receiver.lock().await.recv().await } => stream.context("egress bridge closed")?,
        }
    }
    async fn shutdown(&self) -> Result<()> {
        self.cancel.cancel();
        self.cleanup
            .clone()
            .await
            .map_err(|error| anyhow!("{error:#}"))
    }

    fn is_closed(&self) -> bool {
        self.cancel.is_cancelled()
    }
    fn close(&self) {
        self.cancel.cancel();
    }
}

impl Drop for LimaEgressTransport {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod egress_cleanup_tests {
    use super::*;
    use crate::egress::EgressTransport;

    #[tokio::test]
    async fn failed_lima_egress_setup_terminates_the_allocated_vm() -> Result<()> {
        for fail_proxy_cleanup in [false, true] {
            let (outgoing, mut receiver) = mpsc::channel(8);
            let connection = Arc::new(LimaBridgeConnection {
                outgoing,
                state: Arc::new(LimaBridgeClientState::default()),
                next_id: AtomicU64::new(1),
            });
            let backend = LimaFirecrackerSandboxBackend {
                egress: Arc::new(EgressRuntime::new(None, Arc::new(PublicUpstreamResolver))),
                config: FirecrackerConfig::default(),
                bridge: Arc::new(LimaBridgeManager {
                    limactl: "unused-limactl".into(),
                    instance: "test".into(),
                    bridge_binary: "unused-bridge".into(),
                    build_bridge: false,
                    connection: Mutex::new(Some(connection.clone())),
                }),
            };
            let request = SandboxRequest {
                sandbox_id: "failed-egress-setup".into(),
                scope: None,
                provider_state: None,
                spec: crate::SandboxSpec {
                    image: "test".into(),
                    resources: Default::default(),
                    mounts: vec![],
                    durable_file_systems: vec![],
                    default_workdir: "/workspace".into(),
                    policy: crate::SandboxNetworkPolicy::Limited {
                        allowed_hosts: vec!["api.test".into()],
                    }
                    .into(),
                },
                lifecycle: crate::SandboxLifecycleConfig {
                    idle_ttl: Some(std::time::Duration::from_secs(60)),
                },
            };
            let reply = async {
                let mut operations = Vec::new();
                while let Some(frame) = receiver.recv().await {
                    let FirecrackerBridgeClientFrame::Request { id, request } = frame else {
                        continue;
                    };
                    let response = match *request {
                        FirecrackerBridgeRequest::EgressCreate { .. } => {
                            operations.push("create");
                            Ok(FirecrackerBridgeResponse::Egress {
                                listener_id: "listener".into(),
                                endpoints: crate::SandboxEgressProxy {
                                    http: "192.0.2.1:80".parse()?,
                                    https: "192.0.2.1:443".parse()?,
                                    dns: "192.0.2.1:53".parse()?,
                                },
                            })
                        }
                        FirecrackerBridgeRequest::EgressAccept { .. } => continue,
                        FirecrackerBridgeRequest::Acquire { request, .. } => {
                            assert_eq!(request.sandbox_id, "failed-egress-setup");
                            assert!(request.egress_proxy.is_some());
                            operations.push("acquire");
                            Ok(FirecrackerBridgeResponse::Handle {
                                id: "firecracker:test-vm".into(),
                                provider_state: None,
                                effective_image: Some("/images/test.ext4".into()),
                                source_ipv4: Some("192.0.2.2".parse()?),
                            })
                        }
                        FirecrackerBridgeRequest::EgressBind { .. } => {
                            operations.push("bind");
                            Ok(FirecrackerBridgeResponse::Unit)
                        }
                        FirecrackerBridgeRequest::Exec { .. } => {
                            operations.push("initialize");
                            Err("TLS trust preparation failed".into())
                        }
                        FirecrackerBridgeRequest::EgressClose { listener_id } => {
                            assert_eq!(listener_id, "listener");
                            operations.push("close");
                            if fail_proxy_cleanup {
                                Err("listener cleanup failed".into())
                            } else {
                                Ok(FirecrackerBridgeResponse::Unit)
                            }
                        }
                        FirecrackerBridgeRequest::Terminate { request, .. } => {
                            assert_eq!(request.sandbox_id, "failed-egress-setup");
                            operations.push("terminate");
                            Ok(FirecrackerBridgeResponse::Unit)
                        }
                        request => bail!("unexpected bridge request: {request:?}"),
                    };
                    connection.state.handle_frame(
                        FirecrackerBridgeServerFrame::Response {
                            id,
                            result: response,
                        },
                        &connection.outgoing,
                    )?;
                    if operations.last() == Some(&"terminate") {
                        assert_eq!(
                            operations,
                            [
                                "create",
                                "acquire",
                                "bind",
                                "initialize",
                                "close",
                                "terminate"
                            ]
                        );
                        return Ok::<_, anyhow::Error>(());
                    }
                }
                bail!("bridge closed before VM termination")
            };
            let (result, replied) =
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    tokio::join!(backend.acquire(request), reply)
                })
                .await?;
            replied?;
            assert_eq!(
                result.err().unwrap().to_string(),
                "TLS trust preparation failed"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_shutdown_and_drop_send_one_close_request() -> Result<()> {
        let (outgoing, mut receiver) = mpsc::channel(8);
        let connection = Arc::new(LimaBridgeConnection {
            outgoing,
            state: Arc::new(LimaBridgeClientState::default()),
            next_id: AtomicU64::new(1),
        });
        let transport = LimaEgressTransport::new(
            connection.clone(),
            "listener".into(),
            crate::SandboxEgressProxy {
                http: "192.0.2.1:80".parse()?,
                https: "192.0.2.1:443".parse()?,
                dns: "192.0.2.1:53".parse()?,
            },
        );
        let respond = async {
            while let Some(frame) = receiver.recv().await {
                if let FirecrackerBridgeClientFrame::Request { id, request } = frame
                    && let FirecrackerBridgeRequest::EgressClose { listener_id } = *request
                {
                    assert_eq!(listener_id, "listener");
                    connection.state.handle_frame(
                        FirecrackerBridgeServerFrame::Response {
                            id,
                            result: Ok(FirecrackerBridgeResponse::Unit),
                        },
                        &connection.outgoing,
                    )?;
                    return Ok::<_, anyhow::Error>(());
                }
            }
            bail!("bridge closed before listener cleanup")
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::try_join!(transport.shutdown(), transport.shutdown(), respond)
        })
        .await??;
        drop(transport);
        tokio::task::yield_now().await;
        while let Ok(frame) = receiver.try_recv() {
            if let FirecrackerBridgeClientFrame::Request { request, .. } = frame {
                assert!(!matches!(
                    *request,
                    FirecrackerBridgeRequest::EgressClose { .. }
                ));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn failed_lima_stop_preserves_egress_until_a_successful_retry() -> Result<()> {
        let (outgoing, mut receiver) = mpsc::channel(8);
        let connection = Arc::new(LimaBridgeConnection {
            outgoing,
            state: Arc::new(LimaBridgeClientState::default()),
            next_id: AtomicU64::new(1),
        });
        let bridge = Arc::new(LimaBridgeManager {
            limactl: "unused-limactl".into(),
            instance: "test".into(),
            bridge_binary: "unused-bridge".into(),
            build_bridge: false,
            connection: Mutex::new(Some(connection.clone())),
        });
        let request = SandboxRequest {
            sandbox_id: "stop-test".into(),
            scope: None,
            provider_state: None,
            spec: crate::SandboxSpec {
                image: "test".into(),
                resources: Default::default(),
                mounts: vec![],
                durable_file_systems: vec![],
                default_workdir: "/workspace".into(),
                policy: crate::SandboxNetworkPolicy::Limited {
                    allowed_hosts: vec!["api.test".into()],
                }
                .into(),
            },
            lifecycle: crate::SandboxLifecycleConfig {
                idle_ttl: Some(std::time::Duration::from_secs(60)),
            },
        };
        let transport =
            Arc::new(crate::egress::LocalEgressTransport::for_hosts(&["api.test".into()]).await?);
        let runtime = EgressRuntime::new(None, Arc::new(PublicUpstreamResolver));
        let handle = runtime
            .acquire(
                request.clone(),
                |_| async { Ok(transport.clone() as Arc<dyn EgressTransport>) },
                |egress| async {
                    Ok(LimaFirecrackerSandboxHandle {
                        egress,
                        source_ipv4: None,
                        id: request.sandbox_id.clone(),
                        provider_state: None,
                        effective_image: None,
                        request: request.into(),
                        config: FirecrackerConfig::default(),
                        bridge,
                    })
                },
                async { panic!("successful acquisition must not terminate the sandbox") },
            )
            .await?;
        for response in [
            Err("sync failed".into()),
            Ok(FirecrackerBridgeResponse::Unit),
        ] {
            let stopped = response.is_ok();
            let reply = async {
                let FirecrackerBridgeClientFrame::Request { id, request } =
                    receiver.recv().await.context("stop request missing")?
                else {
                    bail!("expected stop RPC");
                };
                anyhow::ensure!(
                    matches!(*request, FirecrackerBridgeRequest::Stop { .. }),
                    "expected stop, not terminate"
                );
                connection.state.handle_frame(
                    FirecrackerBridgeServerFrame::Response {
                        id,
                        result: response,
                    },
                    &connection.outgoing,
                )
            };
            let (result, replied) =
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    tokio::join!(handle.stop(), reply)
                })
                .await?;
            replied?;
            assert_eq!(result.is_ok(), stopped);
            assert_eq!(transport.is_closed(), stopped);
        }
        Ok(())
    }
}
