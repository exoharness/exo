//! smolvm local microVM sandbox backend.
//!
//! Each sandbox is a VM with its own guest kernel (libkrun on Hypervisor.framework
//! / KVM / WHP) — the only local backend needing no daemon that runs on macOS,
//! Linux and Windows.
//!
//! [`SmolvmExecutionMode::Auto`] picks per request from `lifecycle.idle_ttl`, the
//! signal the Docker backend already uses for warm reuse: warm holds a named
//! machine and can snapshot, one-shot boots an ephemeral VM per exec and leaves
//! durable state to the host mounts.
//!
//! Snapshots are bytes-by-reference like E2B/Daytona: the payload is a manifest
//! pointing at a `.smolmachine` pack on disk.

mod egress;
#[cfg(target_os = "macos")]
mod image_cache;

use egress::SmolvmProxy;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use bytes::Bytes;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::OnceCell;

use crate::SandboxAttachment;
#[cfg(test)]
use crate::egress::UpstreamResolver;
use crate::egress::{
    EgressCredentialResolver, EgressRuntime, PublicUpstreamResolver, SandboxEgress,
};
use crate::sandbox::{
    BoxSandboxTcpStream, ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand,
    SandboxCommandOutput, SandboxMountAccess, SandboxNetworkPolicy, SandboxRequest, SandboxSpec,
    SnapshotFormat, SnapshotPayload, WARM_SANDBOX_KEY_LABEL, run_command, spawn_sandbox_process,
};

/// Default binary name; overridable with `SMOLVM_BIN` for a non-PATH install.
const SMOLVM_BIN_ENV: &str = "SMOLVM_BIN";
const DEFAULT_SMOLVM_BIN: &str = "smolvm";
/// Also the variable smolvm itself reads, which is why the name is not ours to
/// choose; `--smolvm-boot-binary` is the discoverable way to set it.
const SMOLVM_BOOT_BIN_ENV: &str = "SMOLVM_BOOT_BINARY";

/// First smolvm release whose client drains the agent's `Progress` frames during
/// a detached start, which is what warm mode needs; see [`SmolvmExecutionMode::Warm`].
const MIN_WARM_VERSION: Version = Version::new(1, 7, 2);

/// Probed from `--help`, not the version: a build carrying `--label` still
/// reported 1.7.5, so a version gate would refuse a flag that is right there.
const LABEL_FLAG: &str = "--label";
const INTERCEPTOR_FLAG: &str = "--egress-interceptor";
const HOST_PATTERN_FLAG: &str = "--allow-host-pattern";
const EXACT_HOST_POLICY_LABEL: &str = "exo.sandbox.exact-host-policy";
const TCP_FORWARD_LABEL_PREFIX: &str = "exo.sandbox.tcp-forward.";
static CONSUMABLE_SNAPSHOT_FORMATS: [SnapshotFormat; 1] = [SnapshotFormat::SmolvmMachinePack];

/// What the installed smolvm supports. Probed once per backend.
#[derive(Debug, Clone, Copy)]
struct Capabilities {
    /// An image-backed machine can be started.
    warm: bool,
    /// `machine create --label`, used for sandbox identity and TCP forwarding.
    labels: bool,
    interceptor: bool,
    host_patterns: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SmolvmExecutionMode {
    /// Warm when the request and the installed smolvm both allow it, else one-shot.
    #[default]
    Auto,
    /// One ephemeral microVM per exec. Works on every smolvm release.
    OneShot,
    /// One persistent machine per sandbox key, joined by later execs.
    ///
    /// Needs smolvm >= 1.7.2: on 1.7.0/1.7.1 an image-backed start fails with
    /// `run container detached: unexpected response type`.
    Warm,
}

/// Everything the backend can be tuned with, so a caller configures it by
/// building a struct rather than by setting variables the type never mentions.
/// The clap args that fill this (`--smolvm-binary`, `--smolvm-boot-binary`)
/// carry `env` attributes, which keeps the historical `SMOLVM_*` variables
/// working while still listing them in `--help`.
#[derive(Debug, Clone, Default)]
pub struct SmolvmBackendConfig {
    pub mode: SmolvmExecutionMode,
    /// `smolvm` itself. `None` falls back to `SMOLVM_BIN`, then bare `smolvm`
    /// resolved through `PATH`. With the `smolvm` feature, a missing default
    /// or incompatible binary is replaced by a compatible cached/downloaded runtime.
    pub binary: Option<PathBuf>,
    /// The binary handed to smolvm as `SMOLVM_BOOT_BINARY`. `None` derives one
    /// from `binary` on first use; see [`resolve_boot_binary`].
    pub boot_binary: Option<PathBuf>,
    /// Prepared local images, normally under the harness root. None skips caching.
    pub image_cache: Option<PathBuf>,
}

/// Backend driving the `smolvm` CLI.
pub struct SmolvmSandboxBackend {
    binary_override: Option<PathBuf>,
    binary: OnceCell<PathBuf>,
    /// Configured boot binary, if the caller pinned one.
    boot_binary_override: Option<PathBuf>,
    /// Serves `_boot-vm`; arms the parent-death watchdog for ephemeral VMs.
    /// Derived on first use rather than in the constructor: deriving it walks
    /// `PATH` and stats candidates, and a constructor cannot await.
    boot_binary: OnceCell<Option<PathBuf>>,
    mode: SmolvmExecutionMode,
    #[cfg(target_os = "macos")]
    image_cache: Option<PathBuf>,
    /// Probed once: re-asking per `acquire` would spawn a process per sandbox.
    capabilities: OnceCell<Capabilities>,
    /// Last use of each warm machine acquired by this process, for TTL reaping.
    warm_seen: Mutex<HashMap<String, Instant>>,
    egress: EgressRuntime<SmolvmWarmHandle, SmolvmProxy>,
}

impl SmolvmSandboxBackend {
    /// Delegates to [`Default`], which holds the body — a reader looking for how
    /// an unconfigured backend is built finds it under the trait they expect.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_mode(mode: SmolvmExecutionMode) -> Self {
        Self::from_config(SmolvmBackendConfig {
            mode,
            ..Default::default()
        })
    }

    /// The env lookups below are the fallback for callers that build a backend
    /// without a config; anything routed through the CLI arrives on the struct.
    pub fn from_config(config: SmolvmBackendConfig) -> Self {
        let binary_override = config
            .binary
            .or_else(|| std::env::var_os(SMOLVM_BIN_ENV).map(PathBuf::from));
        let boot_binary_override = config
            .boot_binary
            .or_else(|| std::env::var_os(SMOLVM_BOOT_BIN_ENV).map(PathBuf::from));
        Self {
            binary_override,
            binary: OnceCell::new(),
            boot_binary_override,
            boot_binary: OnceCell::new(),
            mode: config.mode,
            #[cfg(target_os = "macos")]
            image_cache: config.image_cache,
            capabilities: OnceCell::new(),
            warm_seen: Mutex::new(HashMap::new()),
            egress: EgressRuntime::new(None, Arc::new(PublicUpstreamResolver)),
        }
    }

    pub fn with_credentials(mut self, resolver: Arc<dyn EgressCredentialResolver>) -> Self {
        self.egress = EgressRuntime::new(Some(resolver), Arc::new(PublicUpstreamResolver));
        self
    }

    #[cfg(test)]
    pub(crate) fn with_egress(
        mut self,
        resolver: Arc<dyn EgressCredentialResolver>,
        upstream: Arc<dyn UpstreamResolver>,
    ) -> Self {
        self.egress = EgressRuntime::new(Some(resolver), upstream);
        self
    }

    pub fn shutdown_egress(&self) {
        self.egress.shutdown();
    }

    /// Resolved once and cached: every ephemeral `acquire` needs it, and the
    /// resolution touches the filesystem.
    async fn boot_binary(&self) -> Result<&Option<PathBuf>> {
        self.boot_binary
            .get_or_try_init(|| async {
                Ok(match &self.boot_binary_override {
                    Some(explicit) => Some(explicit.clone()),
                    None => resolve_boot_binary(self.binary().await?).await,
                })
            })
            .await
    }

    /// The configured mode, which may still be `Auto` until a request resolves it.
    pub fn mode(&self) -> SmolvmExecutionMode {
        self.mode
    }

    async fn binary(&self) -> Result<&PathBuf> {
        self.binary
            .get_or_try_init(|| async {
                let binary = match &self.binary_override {
                    Some(explicit) => explicit.clone(),
                    None => match which_binary(Path::new(DEFAULT_SMOLVM_BIN)).await {
                        Some(installed) if probe_flag_at(&installed, "machine", "start", INTERCEPTOR_FLAG).await => installed,
                        _ => {
                            #[cfg(feature = "smolvm")]
                            {
                                tokio::task::spawn_blocking(|| {
                                    // The SDK stages downloads by PID; serialize callers even
                                    // if an acquire is cancelled while its blocking task runs.
                                    static INSTALL_LOCK: Mutex<()> = Mutex::new(());
                                    let _install = INSTALL_LOCK
                                        .lock()
                                        .expect("smolvm installation lock poisoned");
                                    smolmachines::bootstrap::ensure_engine()
                                        .context("provisioning the SmolVM runtime")
                                })
                                .await
                                .context("SmolVM provisioning task failed")??
                            }
                            #[cfg(not(feature = "smolvm"))]
                            {
                                bail!("no compatible SmolVM found; install SmolVM 1.19.0 or newer from https://smolmachines.com/install.sh, or configure `exo environment provider create --backend smolvm --smolvm-binary <path>`");
                            }
                        }
                    },
                };
                let output = Command::new(&binary)
                    .arg("--version")
                    .kill_on_drop(true)
                    .output()
                    .await
                    .with_context(|| {
                        format!(
                            "could not run {} --version. Install SmolVM with \
                         `curl -sSL https://smolmachines.com/install.sh | bash`, \
                         or configure its path with `exo environment provider create --backend smolvm \
                         --smolvm-binary /path/to/smolvm`",
                            binary.display()
                        )
                    })?;
                ensure!(
                    output.status.success(),
                    "{} --version failed: {}",
                    binary.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                Ok(binary)
            })
            .await
    }

    /// Probe the installed binary once and cache what it supports.
    async fn capabilities(&self) -> Result<&Capabilities> {
        self.binary().await?;
        Ok(self
            .capabilities
            .get_or_init(|| async {
                Capabilities {
                    warm: self
                        .probe_version()
                        .await
                        .is_some_and(|version| version >= MIN_WARM_VERSION),
                    labels: self.probe_flag("machine", "create", LABEL_FLAG).await,
                    interceptor: self.probe_flag("machine", "start", INTERCEPTOR_FLAG).await,
                    host_patterns: self
                        .probe_flag("machine", "create", HOST_PATTERN_FLAG)
                        .await,
                }
            })
            .await)
    }

    /// An unreadable version counts as "no"; a missing binary then fails on the
    /// first real command, which reports it properly.
    pub async fn warm_supported(&self) -> bool {
        self.capabilities().await.is_ok_and(|caps| caps.warm)
    }

    /// Whether the installed smolvm can label machines.
    async fn labels_supported(&self) -> bool {
        self.capabilities().await.is_ok_and(|caps| caps.labels)
    }

    /// Whether a subcommand advertises `flag` in its own `--help`.
    async fn probe_flag(&self, group: &str, subcommand: &str, flag: &str) -> bool {
        let Ok(binary) = self.binary().await else {
            return false;
        };
        probe_flag_at(binary, group, subcommand, flag).await
    }

    /// Check request-specific CLI requirements once, before preparing an image
    /// or creating a machine. The default binary probe above only selects which
    /// runtime to use; it does not decide which features a request needs.
    async fn require_capabilities(&self, request: &SandboxRequest) -> Result<()> {
        let protected = request.spec.policy.requires_proxy();
        let limited = matches!(
            request.spec.policy.networking,
            SandboxNetworkPolicy::Limited { .. }
        );
        if protected {
            ensure!(
                request.lifecycle.idle_ttl.is_some() && self.mode != SmolvmExecutionMode::OneShot,
                "smolvm proxy egress requires a managed warm sandbox"
            );
        }
        if !protected && request.spec.tcp_ports.is_empty() {
            return Ok(());
        }

        let caps = self.capabilities().await?;
        let mut missing = Vec::new();
        if protected && !caps.interceptor {
            missing.push(INTERCEPTOR_FLAG);
        }
        if limited && !caps.host_patterns {
            missing.push(HOST_PATTERN_FLAG);
        }
        if (limited || !request.spec.tcp_ports.is_empty()) && !caps.labels {
            missing.push(LABEL_FLAG);
        }
        ensure!(
            missing.is_empty(),
            "{} lacks SmolVM flags required by this sandbox: {}; update SmolVM or select a compatible build with `exo environment provider create --backend smolvm --smolvm-binary <path>`",
            self.binary().await?.display(),
            missing.join(", ")
        );
        Ok(())
    }

    /// The mode this request will actually run under: `idle_ttl` decides, and
    /// the installed smolvm is only a capability gate on top.
    async fn resolve_mode(&self, request: &SandboxRequest) -> SmolvmExecutionMode {
        match self.mode {
            // Explicit is a decision, not a hint.
            SmolvmExecutionMode::OneShot => return SmolvmExecutionMode::OneShot,
            SmolvmExecutionMode::Warm => return SmolvmExecutionMode::Warm,
            SmolvmExecutionMode::Auto => {}
        }
        if request.lifecycle.idle_ttl.is_none() {
            // No warm lifetime asked for: a persistent VM would outlive the caller.
            return SmolvmExecutionMode::OneShot;
        }
        if self.warm_supported().await {
            SmolvmExecutionMode::Warm
        } else {
            SmolvmExecutionMode::OneShot
        }
    }

    async fn probe_version(&self) -> Option<Version> {
        let output = Command::new(self.binary().await.ok()?)
            .arg("--version")
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        parse_version(&String::from_utf8_lossy(&output.stdout))
    }

    async fn prepare_image(&self, image: &str) -> Result<String> {
        #[cfg(target_os = "macos")]
        if let Some(cache) = &self.image_cache {
            let source = image.to_owned();
            let cache = cache.clone();
            let binary = self.binary().await?.clone();
            let boot_binary = self.boot_binary().await?.clone();
            if let Some(prepared) = tokio::task::spawn_blocking(move || {
                image_cache::prepare(&binary, boot_binary.as_deref(), &cache, &source)
            })
            .await??
            {
                return Ok(prepared.to_string_lossy().into_owned());
            }
        }
        Ok(image.to_owned())
    }

    /// Boot the machine backing `name`, creating it first when absent.
    ///
    /// Idempotent by *result*, not by pre-check: two `acquire`s for one key race,
    /// and the winner's machine is exactly what the loser wanted.
    async fn ensure_machine_started(
        &self,
        name: &str,
        spec: &SandboxSpec,
        key: &str,
        image: &str,
        egress: Option<&SandboxEgress<SmolvmProxy>>,
    ) -> Result<BTreeMap<u16, u16>> {
        let mut create = Command::new(self.binary().await?);
        create.arg("machine").arg("create").arg("--name").arg(name);
        create.arg("--image").arg(image);
        self.stamp_labels(&mut create, key).await;
        configure_spec_args(&mut create, spec)?;
        let exact_host_policy = exact_host_policy_fingerprint(&spec.policy.networking)?;
        if let Some(policy) = &exact_host_policy {
            create
                .arg(LABEL_FLAG)
                .arg(format!("{EXACT_HOST_POLICY_LABEL}={policy}"));
        }
        let (host_ports, reservations) = self.configure_tcp_forwards(&mut create, spec).await?;
        // The workload is PID 1. A shell reaps orphaned service processes;
        // sleep alone leaves zombies that can prevent build servers restarting.
        create.args([
            "--",
            "/bin/sh",
            "-c",
            "trap 'exit 0' INT TERM; while :; do sleep 86400 & wait \"$!\"; done",
        ]);
        let output = create
            .output()
            .await
            .context("spawn smolvm machine create")?;
        drop(reservations);
        let host_ports = if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !cli_says::already_exists(&stderr) {
                bail!("smolvm machine create failed: {}", stderr.trim());
            }
            self.existing_tcp_forwards(name, &spec.tcp_ports, exact_host_policy.as_deref())
                .await?
        } else {
            host_ports
        };
        if !output.status.success() && egress.is_some() {
            let mut stop = Command::new(self.binary().await?);
            stop.args(["machine", "stop", "--name", name]);
            run_checked(stop, "smolvm machine stop before replacing egress").await?;
        }

        let mut start = Command::new(self.binary().await?);
        start.arg("machine").arg("start").arg("--name").arg(name);
        if let Some(egress) = egress {
            egress.proxy.configure(&mut start);
        }
        let output = start.output().await.context("spawn smolvm machine start")?;
        if output.status.success() {
            return Ok(host_ports);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Already up is the caller's intent, not an error.
        if egress.is_none() && cli_says::already_running(&stderr) {
            return Ok(host_ports);
        }
        bail!(
            "smolvm machine start failed for '{name}': {}",
            stderr.trim()
        );
    }

    async fn existing_tcp_forwards(
        &self,
        machine: &str,
        guest_ports: &[u16],
        expected_exact_host_policy: Option<&str>,
    ) -> Result<BTreeMap<u16, u16>> {
        #[derive(Deserialize)]
        struct Status {
            labels: HashMap<String, String>,
        }
        let mut status = Command::new(self.binary().await?);
        status.args(["machine", "status", "--name", machine, "--json"]);
        let output = status
            .output()
            .await
            .context("inspect smolvm TCP forwarding")?;
        ensure!(
            output.status.success(),
            "smolvm machine status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let status: Status = serde_json::from_slice(&output.stdout)?;
        if let Some(expected) = expected_exact_host_policy {
            ensure!(
                status
                    .labels
                    .get(EXACT_HOST_POLICY_LABEL)
                    .is_some_and(|actual| actual == expected),
                "existing smolvm machine has an older or different limited host policy; terminate it before reacquiring"
            );
        }
        let mut host_ports = BTreeMap::new();
        for (label, value) in status.labels {
            if let Some(port) = label.strip_prefix(TCP_FORWARD_LABEL_PREFIX) {
                host_ports.insert(port.parse::<u16>()?, value.parse::<u16>()?);
            }
        }
        ensure!(
            host_ports.keys().copied().collect::<BTreeSet<_>>()
                == guest_ports.iter().copied().collect(),
            "existing smolvm machine has a different TCP forwarding configuration"
        );
        Ok(host_ports)
    }

    async fn configure_tcp_forwards(
        &self,
        create: &mut Command,
        spec: &SandboxSpec,
    ) -> Result<(BTreeMap<u16, u16>, Vec<TcpListener>)> {
        if spec.tcp_ports.is_empty() {
            return Ok((BTreeMap::new(), Vec::new()));
        }
        let mut host_ports = BTreeMap::new();
        let mut reservations = Vec::with_capacity(spec.tcp_ports.len());
        for &guest_port in &spec.tcp_ports {
            ensure!(guest_port != 0, "sandbox TCP ports must be nonzero");
            ensure!(
                !host_ports.contains_key(&guest_port),
                "duplicate sandbox TCP port {guest_port}"
            );
            let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .context("reserve smolvm host TCP port")?;
            let host_port = reservation.local_addr()?.port();
            create
                .arg("--port")
                .arg(format!("{host_port}:{guest_port}"));
            create.arg("--label").arg(format!(
                "{TCP_FORWARD_LABEL_PREFIX}{guest_port}={host_port}"
            ));
            host_ports.insert(guest_port, host_port);
            reservations.push(reservation);
        }
        Ok((host_ports, reservations))
    }

    /// Stop idle machines while retaining their disks for the next acquisition.
    async fn reap_idle_machines(&self, request: &SandboxRequest) {
        let Some(ttl) = request.lifecycle.idle_ttl else {
            return;
        };
        let expired: Vec<String> = {
            // Scoped so the guard is dropped before any await.
            let Ok(mut seen) = self.warm_seen.lock() else {
                return;
            };
            let now = Instant::now();
            seen.insert(request.sandbox_id.clone(), now);
            let expired: Vec<String> = seen
                .iter()
                .filter(|(name, last)| {
                    name.as_str() != request.sandbox_id && now.duration_since(**last) > ttl
                })
                .map(|(name, _)| name.clone())
                .collect();
            for name in &expired {
                seen.remove(name);
            }
            expired
        };
        for id in expired {
            let name = machine_name(&id);
            match self
                .egress
                .terminate(&id, async {
                    stop_machine(self.binary().await?, &name).await
                })
                .await
            {
                Ok(()) => tracing::info!(machine = %name, "stopped idle smolvm machine"),
                Err(error) => {
                    tracing::warn!(machine = %name, %error, "failed to reap idle smolvm machine")
                }
            }
        }
    }

    /// Record which sandbox a machine serves. The creator's PID cannot identify
    /// the owner after another process resumes a machine, so cleanup belongs to
    /// the session lifecycle and thread deletion. A no-op without `--label`.
    async fn stamp_labels(&self, command: &mut Command, key: &str) {
        if !self.labels_supported().await {
            return;
        }
        command
            .arg("--label")
            .arg(format!("{WARM_SANDBOX_KEY_LABEL}={key}"));
    }

    /// Delete if present, tolerating "not found". Deliberately not a `machine ls`
    /// pre-check: that view truncates names at 15 chars and ours are 20, so the
    /// match could never hit — and asking outright has no check-then-act race.
    async fn delete_machine_if_present(&self, name: &str) -> Result<()> {
        delete_machine_if_present(self.binary().await?, name).await
    }
}

async fn stop_machine(binary: &Path, name: &str) -> Result<()> {
    let mut stop = Command::new(binary);
    stop.args(["machine", "stop", "--name", name]);
    run_checked(stop, "smolvm machine stop").await?;
    Ok(())
}

async fn delete_machine_if_present(binary: &Path, name: &str) -> Result<()> {
    let output = Command::new(binary)
        .args(["machine", "delete", "--name", name, "--force"])
        .output()
        .await
        .context("spawn smolvm machine delete")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if cli_says::no_such_machine(&stderr) {
        return Ok(());
    }
    bail!("smolvm machine delete failed: {}", stderr.trim())
}

impl Default for SmolvmSandboxBackend {
    fn default() -> Self {
        Self::with_mode(SmolvmExecutionMode::default())
    }
}

#[async_trait]
impl ManagedSandboxBackend for SmolvmSandboxBackend {
    fn is_local(&self) -> bool {
        true
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &CONSUMABLE_SNAPSHOT_FORMATS
    }

    async fn terminate(&self, request: SandboxRequest) -> Result<()> {
        let machine = machine_name(&request.sandbox_id);
        self.egress
            .terminate(
                &request.sandbox_id,
                self.delete_machine_if_present(&machine),
            )
            .await?;
        self.warm_seen
            .lock()
            .expect("smolvm machine registry poisoned")
            .remove(&request.sandbox_id);
        Ok(())
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        self.require_capabilities(&request).await?;
        let binary = self.binary().await?;
        if self.resolve_mode(&request).await != SmolvmExecutionMode::Warm {
            ensure!(
                request.spec.tcp_ports.is_empty(),
                "smolvm TCP forwarding requires a warm sandbox"
            );
            let image = self.prepare_image(&request.spec.image).await?;
            reject_unsupported_spec(&request.spec, &image)?;
            return Ok(crate::with_process_management(Arc::new(
                SmolvmOneShotHandle {
                    id: format!("smolvm-oneshot:{}", request.sandbox_id),
                    binary: binary.clone(),
                    image,
                    boot_binary: self.boot_binary().await?.clone(),
                    request,
                },
            )));
        }
        let machine = machine_name(&request.sandbox_id);
        let handle = self
            .egress
            .acquire_with_proxy(
                request.clone(),
                SmolvmProxy::start,
                |egress| async {
                    let image = self.prepare_image(&request.spec.image).await?;
                    reject_unsupported_spec(&request.spec, &image)?;
                    let host_ports = self
                        .ensure_machine_started(
                            &machine,
                            &request.spec,
                            &request.sandbox_id,
                            &image,
                            egress.as_deref(),
                        )
                        .await?;
                    let mut handle = SmolvmWarmHandle {
                        id: format!("smolvm:{machine}"),
                        binary: binary.clone(),
                        machine: machine.clone(),
                        request: request.clone(),
                        egress: None,
                        host_ports,
                    };
                    if let Some(egress) = egress {
                        egress.initialize_trust(&handle).await?;
                        handle.egress = Some(egress);
                    }
                    Ok(handle)
                },
                self.delete_machine_if_present(&machine),
            )
            .await?;
        self.reap_idle_machines(&request).await;
        Ok(crate::with_process_management(handle))
    }

    async fn acquire_tcp(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        let machine = machine_name(&request.sandbox_id);
        let host_ports = self
            .existing_tcp_forwards(&machine, &request.spec.tcp_ports, None)
            .await?;
        Ok(Arc::new(SmolvmTcpHandle {
            id: format!("smolvm:{machine}"),
            host_ports,
        }))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        // `SandboxAttachment` only models Docker containers today.
        bail!("smolvm sandboxes cannot be attached")
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        request.spec.policy.validate_basic("smolvm")?;
        if payload.format != SnapshotFormat::SmolvmMachinePack {
            bail!(
                "smolvm backend cannot restore snapshot format {}",
                payload.format
            );
        }
        self.require_capabilities(&request).await?;
        let binary = self.binary().await?;
        if self.resolve_mode(&request).await != SmolvmExecutionMode::Warm {
            bail!(
                "smolvm snapshots require warm mode (one-shot VMs hold no state to restore); \
                 warm needs smolvm >= {MIN_WARM_VERSION}"
            );
        }

        let manifest: SmolvmSnapshotManifest =
            serde_json::from_slice(&payload.bytes).context("parse smolvm snapshot manifest")?;
        if !Path::new(&manifest.pack_path).exists() {
            bail!(
                "smolvm snapshot pack is missing at {} (packs are referenced by path, not embedded)",
                manifest.pack_path
            );
        }
        reject_unsupported_spec(&request.spec, &manifest.pack_path)?;

        let machine = machine_name(request.sandbox_id.as_str());
        // Unconditional: delete already tolerates "not found".
        self.delete_machine_if_present(&machine).await?;

        let mut create = Command::new(binary);
        create
            .arg("machine")
            .arg("create")
            .arg("--name")
            .arg(&machine)
            .arg("--from")
            .arg(&manifest.pack_path);
        // Preserve sandbox identity when importing a snapshot.
        self.stamp_labels(&mut create, request.sandbox_id.as_str())
            .await;
        configure_spec_args(&mut create, &request.spec)?;
        let (host_ports, reservations) = self
            .configure_tcp_forwards(&mut create, &request.spec)
            .await?;
        run_checked(create, "smolvm machine create --from").await?;
        drop(reservations);

        let mut start = Command::new(binary);
        start
            .arg("machine")
            .arg("start")
            .arg("--name")
            .arg(&machine);
        run_checked(start, "smolvm machine start").await?;

        Ok(crate::with_process_management(Arc::new(SmolvmWarmHandle {
            id: format!("smolvm:{machine}"),
            binary: binary.clone(),
            machine,
            request,
            egress: None,
            host_ports,
        })))
    }
}

/// Ephemeral-VM handle: one `smolvm machine run` per command.
struct SmolvmOneShotHandle {
    id: String,
    image: String,
    binary: PathBuf,
    boot_binary: Option<PathBuf>,
    request: SandboxRequest,
}

impl SmolvmOneShotHandle {
    fn build(&self, command: &SandboxCommand, cwd: &str) -> Result<Command> {
        let mut process = Command::new(&self.binary);
        process.arg("machine").arg("run");
        process.arg("--image").arg(&self.image);
        configure_spec_args(&mut process, &self.request.spec)?;
        configure_command_args(&mut process, command, cwd);
        // Arms smolvm's parent-death watchdog so the VM dies with a SIGKILLed CLI
        // rather than reparenting to init. Ephemeral runs only.
        if let Some(boot) = &self.boot_binary {
            process.env("SMOLVM_BOOT_BINARY", boot);
        }
        process.arg("--");
        process.args(&command.argv);
        process.kill_on_drop(true);
        Ok(process)
    }
}

#[async_trait]
impl ManagedSandboxHandle for SmolvmOneShotHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn effective_image(&self) -> Option<String> {
        Some(self.request.spec.image.clone())
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let cwd = resolve_cwd(command, &self.request.spec);
        let process = self.build(command, &cwd)?;
        run_command(process, &with_backstop_timeout(command), cwd).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        let cwd = resolve_cwd(command, &self.request.spec);
        let process = self.build(command, &cwd)?;
        spawn_sandbox_process(process, command).await
    }

    async fn stop(&self) -> Result<()> {
        // Ephemeral VMs are reclaimed when their command exits.
        Ok(())
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("one-shot smolvm sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!(
            "snapshot needs a persistent VM; construct the backend with SmolvmExecutionMode::Warm"
        )
    }
}

/// Persistent-machine handle: execs join a machine that stays booted.
struct SmolvmWarmHandle {
    id: String,
    binary: PathBuf,
    machine: String,
    request: SandboxRequest,
    egress: Option<Arc<SandboxEgress<SmolvmProxy>>>,
    host_ports: BTreeMap<u16, u16>,
}

struct SmolvmTcpHandle {
    id: String,
    host_ports: BTreeMap<u16, u16>,
}

#[async_trait]
impl ManagedSandboxHandle for SmolvmTcpHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports_tcp(&self) -> bool {
        !self.host_ports.is_empty()
    }

    async fn connect_tcp(&self, port: u16) -> Result<Option<BoxSandboxTcpStream>> {
        connect_published_tcp(&self.host_ports, port).await
    }

    async fn exec(&self, _command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        bail!("TCP access does not permit command execution")
    }

    async fn start_process(&self, _command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        bail!("TCP access does not permit starting processes")
    }

    async fn stop(&self) -> Result<()> {
        bail!("TCP access does not permit stopping the sandbox")
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("TCP access does not permit detaching the sandbox")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!("TCP access does not permit snapshotting the sandbox")
    }
}

async fn connect_published_tcp(
    host_ports: &BTreeMap<u16, u16>,
    port: u16,
) -> Result<Option<BoxSandboxTcpStream>> {
    if host_ports.is_empty() {
        return Ok(None);
    }
    let host_port = host_ports
        .get(&port)
        .context(format!("sandbox TCP port {port} is not published"))?;
    Ok(Some(Box::pin(
        TcpStream::connect((Ipv4Addr::LOCALHOST, *host_port)).await?,
    )))
}

impl SmolvmWarmHandle {
    fn build(&self, command: &SandboxCommand, cwd: &str, interactive: bool) -> Command {
        let mut process = Command::new(&self.binary);
        process
            .arg("machine")
            .arg("exec")
            .arg("--name")
            .arg(&self.machine);
        if interactive {
            process.arg("--interactive");
        }
        configure_command_args(&mut process, command, cwd);
        process.arg("--");
        process.args(&command.argv);
        process.kill_on_drop(true);
        process
    }
}

#[async_trait]
impl ManagedSandboxHandle for SmolvmWarmHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn effective_image(&self) -> Option<String> {
        Some(self.request.spec.image.clone())
    }

    fn provider_state(&self) -> Option<Value> {
        Some(json!({ "machine": self.machine }))
    }

    fn supports_tcp(&self) -> bool {
        !self.host_ports.is_empty()
    }

    async fn connect_tcp(&self, port: u16) -> Result<Option<BoxSandboxTcpStream>> {
        connect_published_tcp(&self.host_ports, port).await
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let command = SandboxEgress::prepare_command(self.egress.as_deref(), command)?;
        let command = command.as_ref();
        let cwd = resolve_cwd(command, &self.request.spec);
        let process = self.build(command, &cwd, false);
        run_command(process, &with_backstop_timeout(command), cwd).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        let command = SandboxEgress::prepare_command(self.egress.as_deref(), command)?;
        let command = command.as_ref();
        let cwd = resolve_cwd(command, &self.request.spec);
        let process = self.build(command, &cwd, true);
        spawn_sandbox_process(process, command).await
    }

    async fn is_running(&self) -> Result<Option<bool>> {
        if self.egress.is_none() {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct Status {
            state: String,
        }
        let mut status = Command::new(&self.binary);
        status.args(["machine", "status", "--name", &self.machine, "--json"]);
        let output = status
            .output()
            .await
            .context("spawn smolvm machine status")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if cli_says::no_such_machine(&stderr) {
                return Ok(Some(false));
            }
            bail!("smolvm machine status failed: {}", stderr.trim());
        }
        let status: Status = serde_json::from_slice(&output.stdout)?;
        Ok(Some(status.state == "running"))
    }

    async fn stop(&self) -> Result<()> {
        if let Some(egress) = &self.egress {
            egress.close();
        }
        let mut stop = Command::new(&self.binary);
        stop.arg("machine")
            .arg("stop")
            .arg("--name")
            .arg(&self.machine);
        run_checked(stop, "smolvm machine stop").await.map(|_| ())
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("smolvm sandboxes cannot be detached")
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        ensure!(
            self.egress.is_none(),
            "smolvm proxy egress does not support snapshots"
        );
        // `pack create --from-vm` re-pulls by manifest, so a VM built from a local
        // archive can never be packed: smolvm flattens those at boot.
        if is_local_image_ref(&self.request.spec.image) {
            bail!(
                "smolvm cannot snapshot a VM created from a local image ({}): \
                 `pack create --from-vm` needs a registry reference to re-pull. \
                 Use a registry image for sandboxes you intend to snapshot.",
                self.request.spec.image
            );
        }

        // `pack create --from-vm` reads a *stopped* VM's disks, so quiesce first.
        let mut stop = Command::new(&self.binary);
        stop.arg("machine")
            .arg("stop")
            .arg("--name")
            .arg(&self.machine);
        run_checked(stop, "smolvm machine stop").await?;

        let dir = snapshot_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create snapshot dir {}", dir.display()))?;
        // `-o` names the executable stub; smolvm writes `<stub>.smolmachine`
        // beside it and rejects being handed the sidecar path.
        let stub_path = dir.join(&self.machine);
        let pack_path = dir.join(format!("{}.smolmachine", self.machine));

        let mut pack = Command::new(&self.binary);
        pack.arg("pack")
            .arg("create")
            .arg("--from-vm")
            .arg(&self.machine)
            .arg("-o")
            .arg(&stub_path);
        run_checked(pack, "smolvm pack create --from-vm").await?;
        if !pack_path.exists() {
            bail!(
                "smolvm pack reported success but {} is missing",
                pack_path.display()
            );
        }

        let manifest = SmolvmSnapshotManifest {
            machine: self.machine.clone(),
            pack_path: pack_path.to_string_lossy().to_string(),
        };
        let bytes = serde_json::to_vec(&manifest).context("serialize smolvm snapshot manifest")?;

        // Leave the sandbox usable after snapshotting.
        let mut start = Command::new(&self.binary);
        start
            .arg("machine")
            .arg("start")
            .arg("--name")
            .arg(&self.machine);
        run_checked(start, "smolvm machine start").await?;

        Ok(SnapshotPayload {
            format: SnapshotFormat::SmolvmMachinePack,
            bytes: Bytes::from(bytes),
        })
    }
}

/// Bytes-by-reference snapshot manifest; the pack itself stays on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SmolvmSnapshotManifest {
    machine: String,
    pack_path: String,
}

fn snapshot_dir() -> PathBuf {
    std::env::temp_dir().join("exo-smolvm-snapshots")
}

fn resolve_cwd(command: &SandboxCommand, spec: &SandboxSpec) -> String {
    command
        .cwd
        .clone()
        .unwrap_or_else(|| spec.default_workdir.clone())
}

fn exact_limited_hosts(networking: &SandboxNetworkPolicy) -> Result<Option<Vec<String>>> {
    let SandboxNetworkPolicy::Limited { allowed_hosts } = networking else {
        return Ok(None);
    };
    let mut hosts: Vec<String> = crate::types::canonical_egress_hosts(allowed_hosts)?
        .into_iter()
        .collect();
    ensure!(
        !hosts.is_empty(),
        "smolvm limited networking requires at least one allowed host"
    );
    hosts.sort();
    Ok(Some(hosts))
}

/// Mark machines created with exact-host DNS rules so a warm machine made under
/// the older `--allow-host` rules cannot be reused with a stricter request.
fn exact_host_policy_fingerprint(networking: &SandboxNetworkPolicy) -> Result<Option<String>> {
    let Some(hosts) = exact_limited_hosts(networking)? else {
        return Ok(None);
    };
    let mut digest = Sha256::new();
    // The prefix distinguishes this exact-host policy from future label formats.
    digest.update(b"exact-v1\0");
    for host in hosts {
        digest.update(host.as_bytes());
        // Hostnames cannot contain NUL, so this separates adjacent entries.
        digest.update([0]);
    }
    Ok(Some(format!("exact-v1-{:x}", digest.finalize())))
}

/// Mounts and network policy, shared by the create/run paths.
fn configure_spec_args(process: &mut Command, spec: &SandboxSpec) -> Result<()> {
    let resources = spec.resources.unwrap_or_default();
    process.arg("--cpus").arg(resources.vcpu_count.to_string());
    process.arg("--mem").arg(resources.memory_mib.to_string());
    if let Some(storage) = resources.storage_gib {
        process.arg("--storage").arg(storage.to_string());
    }
    if let Some(overlay) = resources.overlay_gib {
        process.arg("--overlay").arg(overlay.to_string());
    }
    if spec.policy.networking_enabled() {
        process.args(["--net", "--net-backend", "virtio-net"]);
    } else if !spec.tcp_ports.is_empty() {
        process.arg("--outbound-localhost-only");
    }
    if let Some(hosts) = exact_limited_hosts(&spec.policy.networking)? {
        for host in hosts {
            process.arg(HOST_PATTERN_FLAG).arg(host);
        }
    }
    for mount in &spec.mounts {
        let mut value = format!("{}:{}", mount.host_path.display(), mount.guest_path);
        if mount.access == SandboxMountAccess::ReadOnly {
            value.push_str(":ro");
        }
        process.arg("--volume").arg(value);
    }
    Ok(())
}

/// Workdir, environment and timeout, shared by the run/exec paths.
///
/// The timeout goes down to smolvm because exo enforces deadlines by SIGKILLing
/// the CLI, which does not stop the VM it launched — every timed-out exec used to
/// strand a microVM. Enforced in-guest, the CLI exits and tears the VM down.
fn configure_command_args(process: &mut Command, command: &SandboxCommand, cwd: &str) {
    if !cwd.is_empty() {
        process.arg("--workdir").arg(cwd);
    }
    for (key, value) in &command.env {
        process.arg("--env").arg(format!("{key}={value}"));
    }
    if let Some(timeout) = command.timeout {
        process
            .arg("--timeout")
            .arg(format!("{}s", timeout.as_secs().max(1)));
    }
}

/// Pushes exo's backstop out so smolvm's in-guest timeout fires first.
const TIMEOUT_BACKSTOP_GRACE: Duration = Duration::from_secs(10);

fn with_backstop_timeout(command: &SandboxCommand) -> SandboxCommand {
    let mut backstop = command.clone();
    backstop.timeout = command.timeout.map(|t| t + TIMEOUT_BACKSTOP_GRACE);
    backstop
}

/// The binary to hand smolvm as `SMOLVM_BOOT_BINARY`, arming the boot child's
/// parent-death watchdog. Ephemeral runs only: a persistent machine is meant to
/// outlive the `machine start` that created it.
///
/// Prefers the sibling `smolvm-bin`, since a packaged `smolvm` may be a wrapper
/// script that cannot be exec'd as the boot binary.
///
/// An explicitly configured path short-circuits this in [`SmolvmSandboxBackend::boot_binary`].
async fn resolve_boot_binary(binary: &Path) -> Option<PathBuf> {
    let resolved = which_binary(binary).await?;
    let sibling = resolved.with_file_name("smolvm-bin");
    if is_file(&sibling).await {
        return Some(sibling);
    }
    Some(resolved)
}

/// Absolute path for a command that may be a bare name resolved through `PATH`.
async fn which_binary(binary: &Path) -> Option<PathBuf> {
    if binary.components().count() > 1 {
        return tokio::fs::canonicalize(binary).await.ok();
    }
    let path = std::env::var_os("PATH")?;
    // Sequential rather than concurrent: `PATH` order *is* the precedence rule,
    // and the first hit almost always wins on the first entry or two.
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if is_file(&candidate).await
            && let Ok(canonical) = tokio::fs::canonicalize(&candidate).await
        {
            return Some(canonical);
        }
    }
    None
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
}

/// smolvm has no named durable filesystem; refuse rather than hand back a sandbox
/// missing storage the caller asked for, as the Daytona backend does.
fn reject_unsupported_spec(spec: &SandboxSpec, image: &str) -> Result<()> {
    if !spec.durable_file_systems.is_empty() {
        let names: Vec<&str> = spec
            .durable_file_systems
            .iter()
            .map(|fs| fs.name.as_str())
            .collect();
        bail!(
            "the smolvm backend cannot provide durable file systems ({}); \
             use spec mounts, which map to smolvm volumes",
            names.join(", ")
        );
    }
    // smolvm resolves registry references over the machine's own network and
    // refuses this combination even for a cached image. Caught here so the caller
    // gets the two real remedies, not a failure deep in the CLI output.
    if spec.policy.networking == SandboxNetworkPolicy::Disabled && !is_local_image_ref(image) {
        bail!(
            "smolvm cannot use registry image '{}' in a network-disabled sandbox: \
             it resolves registry references over the machine's network, even for \
             cached images. Either set SandboxNetworkPolicy::Unrestricted, or supply the \
             image locally (a `docker save` tar path or an unpacked rootfs dir), \
             which keeps the sandbox fully network-isolated.",
            spec.image
        );
    }
    Ok(())
}

/// The one place this backend depends on smolvm's error *wording*, kept together
/// so a reworded message is a one-line fix rather than a scattered hunt.
mod cli_says {
    /// A create that lost a race: "already exists or is being created".
    pub fn already_exists(stderr: &str) -> bool {
        stderr.contains("already exists") || stderr.contains("is being created")
    }

    /// Already up, or coming up under a racing create.
    pub fn already_running(stderr: &str) -> bool {
        stderr.contains("already running") || stderr.contains("is being created")
    }

    /// A delete for a machine that is not there — the desired end state.
    pub fn no_such_machine(stderr: &str) -> bool {
        stderr.contains("not found") || stderr.contains("does not exist")
    }
}

/// The version reported by `smolvm --version`; `None` means "assume old".
///
/// Comparison is delegated to `semver` rather than hand-rolled, so precedence
/// follows the spec — notably that a prerelease sorts BELOW its release, which a
/// tuple comparison silently got wrong (`1.7.2-rc.1` used to satisfy a `>= 1.7.2`
/// gate even though it predates the fix that gate exists to require).
///
/// The input still needs a little normalizing before `semver` will take it:
/// `--version` prints `smolvm 1.7.5`, sometimes with a `v` prefix, and a
/// two-component `1.7` is not valid semver but is worth accepting as `1.7.0`.
fn parse_version(output: &str) -> Option<Version> {
    let token = output
        .split_whitespace()
        .map(|word| word.trim_start_matches('v'))
        .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    Version::parse(token)
        .ok()
        // `1.7` / `1` are not semver; retry with the missing components as zero
        // rather than reporting an unreadable version and disabling warm mode.
        .or_else(|| {
            let core = token.split(['-', '+']).next().unwrap_or(token);
            let mut parts = core.split('.').map(|p| p.parse::<u64>());
            let major = parts.next()?.ok()?;
            let minor = parts.next().transpose().ok()?.unwrap_or(0);
            let patch = parts.next().transpose().ok()?.unwrap_or(0);
            Some(Version::new(major, minor, patch))
        })
}

/// Whether an image reference names local disk rather than a registry: a tar, an
/// OCI archive or a rootfs dir, all flattened at boot with no manifest to re-pull.
fn is_local_image_ref(image: &str) -> bool {
    image == "-"
        || image.starts_with('/')
        || image.starts_with("./")
        || image.starts_with("../")
        || image.ends_with(".tar")
        || image.ends_with(".tar.gz")
        || image.ends_with(".tgz")
        || Path::new(image).exists()
}

/// Stable, filesystem-safe machine name for a sandbox key. FNV-1a rather than
/// `DefaultHasher`, whose output is not stable across processes or releases.
fn machine_name(key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("exo-{hash:016x}")
}

async fn probe_flag_at(binary: &Path, group: &str, subcommand: &str, flag: &str) -> bool {
    let Ok(output) = Command::new(binary)
        .args([group, subcommand, "--help"])
        .kill_on_drop(true)
        .output()
        .await
    else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .any(|word| word == flag)
}

async fn run_checked(mut process: Command, what: &str) -> Result<String> {
    let output = process
        .output()
        .await
        .with_context(|| format!("spawn {what}"))?;
    if !output.status.success() {
        bail!(
            "{what} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResourceScope;
    use crate::sandbox::SandboxLifecycleConfig;

    #[tokio::test]
    async fn published_tcp_ports_connect_and_reject_other_guest_ports() -> Result<()> {
        let first = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let second = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let mut request = test_request(Some(Duration::from_secs(60)));
        request.spec.tcp_ports = vec![20_000, 20_001];
        let handle = SmolvmWarmHandle {
            id: "smolvm:test".into(),
            binary: PathBuf::from("smolvm"),
            machine: "test".into(),
            request,
            egress: None,
            host_ports: BTreeMap::from([
                (20_000, first.local_addr()?.port()),
                (20_001, second.local_addr()?.port()),
            ]),
        };
        assert!(handle.supports_tcp());
        assert!(handle.connect_tcp(20_000).await?.is_some());
        first.accept().await?;
        assert!(handle.connect_tcp(20_001).await?.is_some());
        second.accept().await?;
        assert!(handle.connect_tcp(20_002).await.is_err());
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn tcp_access_inspects_existing_machine_without_acquiring_egress() -> Result<()> {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("smolvm");
        write_test_binary(
            &binary,
            &format!(
                "case \"$1 $2\" in\n\
                 '--version ') printf 'smolvm 1.19.0\\n';;\n\
                 'machine status') printf '%s\\n' '{{\"labels\":{{\"exo.sandbox.tcp-forward.20000\":\"{}\"}}}}';;\n\
                 *) exit 23;;\n\
                 esac",
                listener.local_addr()?.port()
            ),
        );
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(binary),
            ..Default::default()
        });
        let mut request = test_request(Some(Duration::from_secs(60)));
        request.spec.tcp_ports = vec![20_000];
        let handle = backend.acquire_tcp(request).await?;
        assert!(handle.supports_tcp());
        assert!(handle.connect_tcp(20_000).await?.is_some());
        listener.accept().await?;
        assert!(handle.connect_tcp(20_001).await.is_err());
        assert!(handle.stop().await.is_err());
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn warm_machine_publishes_requested_guest_ports() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("smolvm");
        let args_file = dir.path().join("create-args");
        write_test_binary(
            &binary,
            &format!(
                "case \"$1 $2 $3\" in\n\
                 '--version  ') printf 'smolvm 1.19.0\\n';;\n\
                 'machine create --help') printf '%s\\n' '--label';;\n\
                 'machine create '*) printf '%s\\n' \"$@\" > '{}';;\n\
                 'machine start '*) exit 0;;\n\
                 *) exit 23;;\n\
                 esac",
                args_file.display()
            ),
        );
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(binary),
            ..Default::default()
        });
        let mut request = test_request(Some(Duration::from_secs(60)));
        request.spec.tcp_ports = vec![20_000, 20_001];
        request.spec.resources = Some(serde_json::from_str(
            r#"{"vcpu_count":4,"memory_mib":16384,"storage_gib":64,"overlay_gib":64}"#,
        )?);
        let host_ports = backend
            .ensure_machine_started("test", &request.spec, "test", "alpine", None)
            .await?;
        let args = std::fs::read_to_string(args_file)?;
        assert!(args.contains("--storage\n64\n"));
        assert!(args.contains("--overlay\n64\n"));
        assert!(args.contains("--\n/bin/sh\n-c\ntrap 'exit 0' INT TERM;"));
        for (guest, host) in &host_ports {
            assert!(
                args.contains(&format!("--port\n{host}:{guest}\n")),
                "{args}"
            );
            assert!(
                args.contains(&format!(
                    "--label\n{TCP_FORWARD_LABEL_PREFIX}{guest}={host}\n"
                )),
                "{args}"
            );
        }
        assert_eq!(host_ports.len(), 2);
        assert!(args.contains("--outbound-localhost-only\n"), "{args}");
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn existing_machine_reuses_its_published_host_ports() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("smolvm");
        write_test_binary(
            &binary,
            "case \"$1 $2 $3\" in
             '--version  ') printf 'smolvm 1.19.0\\n';;
             'machine create --help') printf '%s\\n' '--label';;
             'machine create '*) echo 'already exists' >&2; exit 1;;
             'machine status '*) printf '%s\\n' '{\"labels\":{\"exo.sandbox.tcp-forward.20000\":\"43210\",\"exo.sandbox.tcp-forward.20001\":\"43211\"}}';;
             'machine start '*) exit 0;;
             *) exit 23;;
             esac",
        );
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(binary),
            ..Default::default()
        });
        let mut request = test_request(Some(Duration::from_secs(60)));
        request.spec.tcp_ports = vec![20_000, 20_001];
        assert_eq!(
            backend
                .ensure_machine_started("test", &request.spec, "test", "alpine", None)
                .await?,
            BTreeMap::from([(20_000, 43_210), (20_001, 43_211)])
        );
        request.spec.tcp_ports = vec![20_001];
        assert!(
            backend
                .ensure_machine_started("test", &request.spec, "test", "alpine", None)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn limited_machine_is_labeled_with_its_exact_host_policy() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("smolvm");
        let args_file = dir.path().join("create-args");
        write_test_binary(
            &binary,
            &format!(
                "case \"$1 $2 $3\" in\n\
                 '--version  ') printf 'smolvm 1.20.0\\n';;\n\
                 'machine create --help') printf '%s\\n' '--label --allow-host-pattern';;\n\
                 'machine create '*) printf '%s\\n' \"$@\" > '{}';;\n\
                 'machine start '*) exit 0;;\n\
                 *) exit 23;;\n\
                 esac",
                args_file.display()
            ),
        );
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(binary),
            ..Default::default()
        });
        let mut request = test_request(Some(Duration::from_secs(60)));
        request.spec.policy.networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["api.test".into()],
        };
        backend
            .ensure_machine_started("test", &request.spec, "test", "alpine", None)
            .await?;
        let args = std::fs::read_to_string(args_file)?;
        assert!(args.contains("--allow-host-pattern\napi.test\n"), "{args}");
        let policy = exact_host_policy_fingerprint(&request.spec.policy.networking)?.unwrap();
        assert!(
            args.contains(&format!("--label\n{EXACT_HOST_POLICY_LABEL}={policy}\n")),
            "{args}"
        );
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn limited_machine_rejects_existing_broader_host_policy() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("smolvm");
        let started = dir.path().join("started");
        write_test_binary(
            &binary,
            &format!(
                "case \"$1 $2 $3\" in\n\
                 '--version  ') printf 'smolvm 1.20.0\\n';;\n\
                 'machine create --help') printf '%s\\n' '--label --allow-host-pattern';;\n\
                 'machine start --help') exit 0;;\n\
                 'machine create '*) echo 'already exists' >&2; exit 1;;\n\
                 'machine status '*) printf '%s\\n' '{{\"labels\":{{}}}}';;\n\
                 'machine start '*) touch '{}';;\n\
                 *) exit 23;;\n\
                 esac",
                started.display()
            ),
        );
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(binary),
            ..Default::default()
        });
        let mut request = test_request(Some(Duration::from_secs(60)));
        request.spec.policy.networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["api.test".into()],
        };
        let error = backend
            .ensure_machine_started("test", &request.spec, "test", "alpine", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("older or different limited host policy"),
            "{error}"
        );
        assert!(!started.exists());
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn protected_sandboxes_require_the_native_hook_before_preparing_images() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("smolvm");
        write_test_binary(
            &binary,
            r#"case "$*" in
--version) printf 'smolvm 1.18.2\n';;
'machine create --help'|'machine start --help') exit 0;;
*) exit 23;;
esac"#,
        );
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(binary),
            image_cache: Some(dir.path().join("cache")),
            ..Default::default()
        });
        let mut request = test_request(Some(Duration::from_secs(60)));
        request.spec.policy = SandboxNetworkPolicy::Unrestricted.into();
        request
            .spec
            .policy
            .credentials
            .push(crate::EgressCredentialBinding {
                name: "key".into(),
                environment_variable: "API_KEY".into(),
                networking: crate::CredentialNetworkPolicy::Limited {
                    allowed_hosts: vec!["api.test".into()],
                },
                injection_location: crate::CredentialInjectionLocation { header: true },
            });
        request.spec.image = "/nonexistent/image.tar".into();
        let error = backend
            .acquire(request.clone())
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("--egress-interceptor"), "{error}");
        assert!(!dir.path().join("cache").exists());
        let mut limited = request.clone();
        limited.spec.policy.networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["api.test".into()],
        };
        let error = backend.acquire(limited).await.err().unwrap().to_string();
        assert!(error.contains("--egress-interceptor"), "{error}");
        request.lifecycle.idle_ttl = None;
        let error = backend.acquire(request).await.err().unwrap().to_string();
        assert!(error.contains("managed warm sandbox"), "{error}");
        Ok(())
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn limited_networking_reports_missing_flags_before_preparing_images() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("smolvm");
        write_test_binary(
            &binary,
            r#"case "$*" in
--version) printf 'smolvm 1.19.0\n';;
'machine start --help') printf '%s\n' '--egress-interceptor <ADDR>';;
'machine create --help') exit 0;;
*) exit 23;;
esac"#,
        );
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(binary),
            image_cache: Some(dir.path().join("cache")),
            ..Default::default()
        });
        let mut request = test_request(Some(Duration::from_secs(60)));
        request.spec.policy.networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["api.test".into()],
        };
        request.spec.image = "/nonexistent/image.tar".into();
        let error = backend.acquire(request).await.err().unwrap().to_string();
        assert!(error.contains(HOST_PATTERN_FLAG), "{error}");
        assert!(error.contains(LABEL_FLAG), "{error}");
        assert!(!dir.path().join("cache").exists());
        Ok(())
    }

    #[tokio::test]
    #[cfg(feature = "smolvm")]
    #[ignore = "provisions the pinned SmolVM release in the local SDK cache"]
    async fn provisioned_runtime_supports_native_egress() -> Result<()> {
        let backend = SmolvmSandboxBackend::new();
        let binary = backend.binary().await?;
        let capabilities = backend.capabilities().await?;
        assert!(
            capabilities.warm
                && capabilities.labels
                && capabilities.interceptor
                && capabilities.host_patterns
        );
        eprintln!("Compatible SmolVM: {}", binary.display());
        Ok(())
    }

    #[tokio::test]
    async fn missing_binary_reports_macos_install_instructions() {
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(PathBuf::from("/nonexistent/exo-test-smolvm")),
            ..Default::default()
        });
        let error = backend.binary().await.unwrap_err().to_string();
        assert!(
            error.contains("https://smolmachines.com/install.sh"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("--smolvm-binary"),
            "unexpected error: {error}"
        );
    }

    /// A configured boot binary is used as given. The point is what does *not*
    /// happen: no `PATH` walk, no `stat`, so a path that exists only on the host
    /// this config was written for still round-trips instead of being silently
    /// replaced by whatever resolution finds.
    #[tokio::test]
    async fn a_configured_boot_binary_is_used_verbatim() {
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            mode: SmolvmExecutionMode::OneShot,
            binary: Some(PathBuf::from("/nowhere/smolvm")),
            boot_binary: Some(PathBuf::from("/nowhere/smolvm-bin")),
            ..Default::default()
        });
        assert_eq!(
            backend.boot_binary().await.unwrap().as_deref(),
            Some(Path::new("/nowhere/smolvm-bin"))
        );
    }

    /// Resolution is memoized, which is what keeps it off the per-sandbox path:
    /// `acquire` asks for this every ephemeral run, and the answer costs a `PATH`
    /// walk plus a `canonicalize`.
    #[tokio::test]
    #[cfg(unix)]
    async fn boot_binary_resolution_is_cached_after_the_first_ask() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("smolvm");
        write_test_binary(&binary, "printf 'smolvm 1.17.0\\n'");
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            mode: SmolvmExecutionMode::OneShot,
            binary: Some(binary),
            boot_binary: None,
            ..Default::default()
        });
        assert!(
            !backend.boot_binary.initialized(),
            "construction must not resolve; that is the work being deferred"
        );
        // Not asserted against a fixed value: an inherited `SMOLVM_BOOT_BINARY`
        // legitimately changes the answer, and what is under test is that the
        // answer is computed once, not what it is.
        let first = backend.boot_binary().await.unwrap().clone();
        assert!(backend.boot_binary.initialized());
        assert_eq!(
            backend.boot_binary().await.unwrap(),
            &first,
            "the second ask must read the cell, not the filesystem"
        );
    }

    #[cfg(unix)]
    fn write_test_binary(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn idle_cleanup_stops_only_expired_machines_without_deleting_disks() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let binary = dir.path().join("smolvm");
        let stopped = dir.path().join("stopped");
        write_test_binary(
            &binary,
            &format!(
                "case \"$*\" in\n\
                 --version) printf 'smolvm 1.20.0\\n' ;;\n\
                 'machine stop --name '*) printf '%s\\n' \"$4\" >> '{}' ;;\n\
                 *) exit 23 ;;\n\
                 esac",
                stopped.display()
            ),
        );
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(binary),
            ..Default::default()
        });
        backend.warm_seen.lock().unwrap().extend([
            ("expired".into(), Instant::now() - Duration::from_secs(120)),
            ("active".into(), Instant::now()),
        ]);
        backend
            .reap_idle_machines(&test_request(Some(Duration::from_secs(60))))
            .await;
        assert_eq!(
            std::fs::read_to_string(stopped)?,
            format!("{}\n", machine_name("expired"))
        );
        assert!(backend.warm_seen.lock().unwrap().contains_key("active"));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_binary_errors_are_retried_without_using_another_engine() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("smolvm");
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(binary.clone()),
            ..Default::default()
        });
        assert!(
            backend
                .binary()
                .await
                .unwrap_err()
                .to_string()
                .contains(&binary.display().to_string())
        );
        write_test_binary(&binary, "echo broken-runtime >&2; exit 7");
        assert!(
            backend
                .binary()
                .await
                .unwrap_err()
                .to_string()
                .contains("broken-runtime")
        );
        write_test_binary(&binary, "printf 'smolvm 1.17.0\\n'");
        assert_eq!(backend.binary().await.unwrap(), &binary);
    }

    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(
            target_os = "linux",
            any(target_arch = "aarch64", target_arch = "x86_64")
        )
    ))]
    #[tokio::test]
    async fn default_runtime_uses_sdk_cache_or_path() {
        const CHILD_ROOT: &str = "EXO_SMOLVM_BOOTSTRAP_TEST_ROOT";
        let Some(root) = std::env::var_os(CHILD_ROOT).map(PathBuf::from) else {
            let dir = tempfile::tempdir().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sandbox_provider::smolvm::tests::default_runtime_uses_sdk_cache_or_path",
                    "--nocapture",
                ])
                .env(CHILD_ROOT, dir.path())
                .env("PATH", dir.path().join("bin"))
                .env_remove(SMOLVM_BIN_ENV)
                .env_remove(SMOLVM_BOOT_BIN_ENV)
                .env("SMOLMACHINES_CACHE_DIR", dir.path().join("cache"))
                .env("SMOLMACHINES_ENGINE_VERSION", "1.20.0")
                .env("SMOLMACHINES_NO_DOWNLOAD", "1")
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        };

        let backend = SmolvmSandboxBackend::new();
        assert!(!backend.warm_supported().await);
        let error = format!("{:#}", backend.binary().await.unwrap_err());
        #[cfg(feature = "smolvm")]
        {
            assert!(error.contains("downloads are disabled"), "{error}");
            let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
                ("macos", "aarch64") => "darwin-arm64",
                ("linux", "aarch64") => "linux-arm64",
                ("linux", "x86_64") => "linux-x86_64",
                _ => unreachable!(),
            };
            let cached = root.join("cache").join(format!("smolvm-1.20.0-{platform}"));
            let binary = cached.join("smolvm");
            write_test_binary(&binary, "printf 'smolvm 1.20.0\\n'");
            assert_eq!(backend.binary().await.unwrap(), &binary);
            let mut request = test_request(Some(Duration::from_secs(60)));
            request.spec.policy.networking = SandboxNetworkPolicy::Limited {
                allowed_hosts: vec!["api.test".into()],
            };
            let error = backend
                .require_capabilities(&request)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(INTERCEPTOR_FLAG), "{error}");
            assert!(error.contains(HOST_PATTERN_FLAG), "{error}");
            assert!(error.contains(LABEL_FLAG), "{error}");
            write_test_binary(&root.join("bin/smolvm"), "printf 'smolvm 1.16.2\\n'");
            write_test_binary(
                &binary,
                "case \"$*\" in --version) echo 'smolvm 1.20.0';; 'machine start --help') echo '--egress-interceptor <ADDR>';; 'machine create --help') echo '--allow-host-pattern <PATTERN>';; esac",
            );
            write_test_binary(&cached.join("smolvm-bin"), "exit 0");
            let backend = SmolvmSandboxBackend::new();
            assert_eq!(backend.binary().await.unwrap(), &binary);
            assert!(backend.warm_supported().await);
            assert_eq!(
                backend.boot_binary().await.unwrap().as_ref().unwrap(),
                &cached.join("smolvm-bin").canonicalize().unwrap()
            );
        }
        #[cfg(not(feature = "smolvm"))]
        assert!(
            error.contains("https://smolmachines.com/install.sh"),
            "{error}"
        );

        let installed = root.join("bin/smolvm");
        write_test_binary(
            &installed,
            "case \"$*\" in --version) echo 'smolvm 1.19.0';; 'machine start --help') echo '--egress-interceptor <ADDR>';; esac",
        );
        let installed = installed.canonicalize().unwrap();
        assert_eq!(
            SmolvmSandboxBackend::new().binary().await.unwrap(),
            &installed
        );
        #[cfg(not(feature = "smolvm"))]
        assert_eq!(backend.binary().await.unwrap(), &installed);
    }

    #[test]
    fn parses_the_version_line_smolvm_actually_prints() {
        assert_eq!(parse_version("smolvm 1.7.5\n"), Some(Version::new(1, 7, 5)));
        assert_eq!(
            parse_version("smolvm 1.7.5-rc.1"),
            Some(Version::parse("1.7.5-rc.1").unwrap())
        );
        assert_eq!(
            parse_version("smolvm v1.10.0"),
            Some(Version::new(1, 10, 0))
        );
        assert_eq!(parse_version("smolvm 1.7"), Some(Version::new(1, 7, 0)));
        assert_eq!(parse_version("not a version"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn warm_threshold_matches_the_releases_that_carry_the_fix() {
        let supports = |v: &str| parse_version(v).is_some_and(|got| got >= MIN_WARM_VERSION);
        assert!(!supports("smolvm 1.7.0"));
        assert!(!supports("smolvm 1.7.1"));
        assert!(supports("smolvm 1.7.2"));
        assert!(supports("smolvm 1.7.5"));
        assert!(supports("smolvm 2.0.0"));
        assert!(!supports("smolvm 1.6.13"));
        // Semver precedence, which the previous tuple comparison got wrong: a
        // prerelease sorts BELOW its release, so 1.7.2-rc.1 predates the fix the
        // gate requires and must not enable warm mode.
        assert!(!supports("smolvm 1.7.2-rc.1"));
        assert!(supports("smolvm 1.7.3-rc.1"));
    }

    fn test_request(idle_ttl: Option<Duration>) -> SandboxRequest {
        SandboxRequest {
            sandbox_id: "s".into(),
            scope: ResourceScope::Agent {
                agent_id: crate::Uuid7::now(),
            },
            spec: SandboxSpec {
                tcp_ports: vec![],
                image: "alpine".into(),
                resources: Default::default(),
                mounts: Vec::new(),
                durable_file_systems: Vec::new(),
                policy: SandboxNetworkPolicy::Disabled.into(),
                default_workdir: "/".into(),
            },
            lifecycle: SandboxLifecycleConfig { idle_ttl },
            provider_state: None,
        }
    }

    /// Never downgraded by version probing or by the request's lifecycle.
    #[tokio::test]
    async fn explicit_modes_are_honoured_without_probing() {
        let warm = SmolvmSandboxBackend::with_mode(SmolvmExecutionMode::Warm);
        assert_eq!(
            warm.resolve_mode(&test_request(None)).await,
            SmolvmExecutionMode::Warm
        );

        let one_shot = SmolvmSandboxBackend::with_mode(SmolvmExecutionMode::OneShot);
        assert_eq!(
            one_shot
                .resolve_mode(&test_request(Some(Duration::from_secs(60))))
                .await,
            SmolvmExecutionMode::OneShot
        );
    }

    #[tokio::test]
    async fn auto_without_idle_ttl_is_one_shot() {
        let backend = SmolvmSandboxBackend::new();
        assert_eq!(
            backend.resolve_mode(&test_request(None)).await,
            SmolvmExecutionMode::OneShot
        );
    }

    /// Must not panic or hang; the first real command reports the failure.
    #[tokio::test]
    async fn auto_falls_back_to_one_shot_when_smolvm_is_absent() {
        let backend = SmolvmSandboxBackend::from_config(SmolvmBackendConfig {
            binary: Some(PathBuf::from("/nonexistent/smolvm-does-not-exist")),
            ..Default::default()
        });
        assert!(!backend.warm_supported().await);
        assert_eq!(
            backend
                .resolve_mode(&test_request(Some(Duration::from_secs(60))))
                .await,
            SmolvmExecutionMode::OneShot
        );
    }

    #[test]
    fn durable_file_systems_are_rejected_not_ignored() {
        let mut spec = test_request(None).spec;
        spec.durable_file_systems = vec![crate::DurableFileSystem {
            name: "cache".into(),
            mount_path: "/cache".into(),
            mode: crate::FileSystemMountMode::ReadWrite,
        }];
        let err = reject_unsupported_spec(&spec, &spec.image)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cache"), "error should name the fs: {err}");
    }

    /// Must fail at acquire with guidance, not deep inside the CLI.
    #[test]
    fn registry_image_without_network_is_rejected() {
        let mut spec = test_request(None).spec;
        spec.image = "docker.io/library/ubuntu:24.04".into();
        spec.policy.networking = SandboxNetworkPolicy::Disabled;
        let err = reject_unsupported_spec(&spec, &spec.image)
            .unwrap_err()
            .to_string();
        assert!(err.contains("network-disabled"), "unexpected error: {err}");

        // Fine once the sandbox is allowed network...
        spec.policy.networking = SandboxNetworkPolicy::Unrestricted;
        assert!(reject_unsupported_spec(&spec, &spec.image).is_ok());

        // ...and a local archive is fine while staying isolated.
        spec.policy.networking = SandboxNetworkPolicy::Disabled;
        spec.image = "/tmp/alpine.tar".into();
        assert!(reject_unsupported_spec(&spec, &spec.image).is_ok());

        spec.image = "docker.io/library/ubuntu:24.04".into();
        assert!(reject_unsupported_spec(&spec, "/tmp/prepared-rootfs").is_ok());
    }

    #[test]
    fn local_image_refs_are_recognised() {
        assert!(is_local_image_ref("/tmp/alpine.tar"));
        assert!(is_local_image_ref("./rootfs"));
        assert!(is_local_image_ref("image.tar.gz"));
        assert!(is_local_image_ref("-"));
        assert!(!is_local_image_ref("alpine"));
        assert!(!is_local_image_ref("docker.io/library/ubuntu:24.04"));
    }

    /// exo's deadline must fire after smolvm's, or the CLI dies before cleanup.
    #[test]
    fn backstop_timeout_is_later_than_the_requested_one() {
        let mut command = SandboxCommand {
            argv: vec!["true".into()],
            env: Default::default(),
            display_argv: None,
            cwd: None,
            timeout: Some(Duration::from_secs(30)),
        };
        assert_eq!(
            with_backstop_timeout(&command).timeout,
            Some(Duration::from_secs(30) + TIMEOUT_BACKSTOP_GRACE)
        );
        command.timeout = None;
        assert_eq!(with_backstop_timeout(&command).timeout, None);
    }

    #[test]
    fn machine_name_uses_sandbox_id_without_owner_scope() {
        let mut request = test_request(None);
        let name = machine_name(&request.sandbox_id);
        request.scope = ResourceScope::Thread {
            agent_id: crate::Uuid7::now(),
            thread_id: crate::Uuid7::now(),
        };
        assert_eq!(name, machine_name(&request.sandbox_id));
        request.scope = ResourceScope::Global;
        assert_eq!(name, machine_name(&request.sandbox_id));
        request.sandbox_id = "sandbox-2".into();
        assert_ne!(name, machine_name(&request.sandbox_id));
        assert!(name.starts_with("exo-"));
        // These go on the CLI and into paths: keep them boring.
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn resource_shape_and_mounts_are_forwarded() {
        let spec = SandboxSpec {
            tcp_ports: vec![],
            image: "alpine".into(),
            resources: crate::SandboxResourceShape::new(3, 2048),
            mounts: vec![
                crate::sandbox::SandboxMount {
                    host_path: PathBuf::from("/host/rw"),
                    guest_path: "/guest/rw".into(),
                    access: SandboxMountAccess::ReadWrite,
                    internal: false,
                },
                crate::sandbox::SandboxMount {
                    host_path: PathBuf::from("/host/ro"),
                    guest_path: "/guest/ro".into(),
                    access: SandboxMountAccess::ReadOnly,
                    internal: false,
                },
            ],
            durable_file_systems: Vec::new(),
            policy: SandboxNetworkPolicy::Disabled.into(),
            default_workdir: "/work".into(),
        };

        let mut process = Command::new("smolvm");
        configure_spec_args(&mut process, &spec).unwrap();
        let rendered: Vec<String> = process
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        assert_eq!(&rendered[..4], ["--cpus", "3", "--mem", "2048"]);
        assert!(rendered.contains(&"/host/rw:/guest/rw".to_string()));
        assert!(rendered.contains(&"/host/ro:/guest/ro:ro".to_string()));
        // Disabled is smolvm's default, so no flag is emitted.
        assert!(!rendered.contains(&"--net".to_string()));
    }

    #[test]
    fn limited_hosts_are_forwarded_to_smolvm() {
        let mut spec = test_request(Some(Duration::from_secs(60))).spec;
        spec.resources = crate::SandboxResourceShape::new(3, 2048);
        spec.policy.networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["Z.example.com".into(), "api.example.com".into()],
        };
        let mut process = Command::new("smolvm");
        configure_spec_args(&mut process, &spec).unwrap();
        let rendered: Vec<String> = process
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            rendered,
            [
                "--cpus",
                "3",
                "--mem",
                "2048",
                "--net",
                "--net-backend",
                "virtio-net",
                "--allow-host-pattern",
                "api.example.com",
                "--allow-host-pattern",
                "z.example.com",
            ]
        );

        spec.policy.networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: Vec::new(),
        };
        let error = configure_spec_args(&mut Command::new("smolvm"), &spec)
            .unwrap_err()
            .to_string();
        assert!(error.contains("at least one allowed host"), "{error}");
    }

    #[test]
    fn exact_host_policy_fingerprint_tracks_the_canonical_host_set() -> Result<()> {
        let policy = |hosts: &[&str]| SandboxNetworkPolicy::Limited {
            allowed_hosts: hosts.iter().map(|host| (*host).into()).collect(),
        };
        let first = exact_host_policy_fingerprint(&policy(&["B.test", "a.test"]))?;
        assert_eq!(
            first,
            exact_host_policy_fingerprint(&policy(&["a.test", "b.test", "A.test"]))?
        );
        assert_ne!(first, exact_host_policy_fingerprint(&policy(&["a.test"]))?);
        Ok(())
    }
}
