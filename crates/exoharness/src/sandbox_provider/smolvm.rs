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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use bytes::Bytes;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::OnceCell;

use crate::SandboxAttachment;
#[cfg(test)]
use crate::egress::UpstreamResolver;
use crate::egress::{
    EgressCredentialResolver, EgressRuntime, PublicUpstreamResolver, SandboxEgress,
};
use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxMountAccess, SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotFormat,
    SnapshotPayload, WARM_SANDBOX_KEY_LABEL, WARM_SANDBOX_OWNER_PID_LABEL, owner_pid_is_alive,
    run_command, spawn_sandbox_process,
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
static CONSUMABLE_SNAPSHOT_FORMATS: [SnapshotFormat; 1] = [SnapshotFormat::SmolvmMachinePack];

/// What the installed smolvm supports. Probed once per backend.
#[derive(Debug, Clone, Copy)]
struct Capabilities {
    /// An image-backed machine can be started.
    warm: bool,
    /// `machine create --label`; without it, reaping cannot cross processes.
    labels: bool,
    interceptor: bool,
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
    /// binary is downloaded and cached on first use.
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
    /// Last use of each warm machine this process created, for TTL reaping.
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
                        Some(installed) => installed,
                        None => {
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
                                PathBuf::from(DEFAULT_SMOLVM_BIN)
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
                         or configure its path with `exo sandbox provider create --sandbox smolvm \
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
                    interceptor: self
                        .probe_flag("machine", "start", "--egress-interceptor-ports")
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

    /// Whether the installed smolvm can label machines, which cross-process reaping needs.
    pub async fn labels_supported(&self) -> bool {
        self.capabilities().await.is_ok_and(|caps| caps.labels)
    }

    /// Whether a subcommand advertises `flag` in its own `--help`.
    async fn probe_flag(&self, group: &str, subcommand: &str, flag: &str) -> bool {
        let Ok(binary) = self.binary().await else {
            return false;
        };
        let Ok(output) = Command::new(binary)
            .args([group, subcommand, "--help"])
            .output()
            .await
        else {
            return false;
        };
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains(flag)
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
    ) -> Result<()> {
        let mut create = Command::new(self.binary().await?);
        create.arg("machine").arg("create").arg("--name").arg(name);
        create.arg("--image").arg(image);
        self.stamp_labels(&mut create, key).await;
        configure_spec_args(&mut create, spec);
        // Keepalive so the machine stays up between execs, as the Docker backend does.
        create.arg("--").arg("sleep").arg("infinity");
        let output = create
            .output()
            .await
            .context("spawn smolvm machine create")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !cli_says::already_exists(&stderr) {
                bail!("smolvm machine create failed: {}", stderr.trim());
            }
            if egress.is_some() {
                let mut stop = Command::new(self.binary().await?);
                stop.args(["machine", "stop", "--name", name]);
                run_checked(stop, "smolvm machine stop before replacing egress").await?;
            }
        }

        let mut start = Command::new(self.binary().await?);
        start.arg("machine").arg("start").arg("--name").arg(name);
        if let Some(egress) = egress {
            egress.proxy.configure(&mut start);
        }
        let output = start.output().await.context("spawn smolvm machine start")?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Already up is the caller's intent, not an error.
        if egress.is_none() && cli_says::already_running(&stderr) {
            return Ok(());
        }
        bail!(
            "smolvm machine start failed for '{name}': {}",
            stderr.trim()
        );
    }

    /// Drop warm machines this process created that are idle past `idle_ttl`.
    /// Idle age lives in memory, so [`Self::reap_abandoned_machines`] covers
    /// machines stranded by an earlier process.
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
                .terminate(&id, self.delete_machine_if_present(&name))
                .await
            {
                Ok(()) => tracing::info!(machine = %name, "reaped idle smolvm machine"),
                Err(error) => {
                    tracing::warn!(machine = %name, %error, "failed to reap idle smolvm machine")
                }
            }
        }
    }

    /// Record which sandbox a machine serves and which process owns it, under the
    /// same keys the Docker backend uses. A no-op without `--label`.
    async fn stamp_labels(&self, command: &mut Command, key: &str) {
        if !self.labels_supported().await {
            return;
        }
        command
            .arg("--label")
            .arg(format!("{WARM_SANDBOX_KEY_LABEL}={key}"))
            .arg("--label")
            .arg(format!(
                "{WARM_SANDBOX_OWNER_PID_LABEL}={}",
                std::process::id()
            ));
    }

    /// Reclaim labelled machines whose owning process is gone — a crash or a
    /// restart leaves nobody to expire them. A live owner is left alone: two
    /// harnesses may share a host, and reaping a peer's sandbox mid-turn is worse
    /// than leaking one.
    async fn reap_abandoned_machines(&self, current: &str) {
        let machines = match self.labelled_machines().await {
            Ok(machines) => machines,
            Err(error) => {
                tracing::debug!(%error, "could not list smolvm machines for reaping");
                return;
            }
        };
        for (name, owner) in machines {
            if name == current || owner_pid_is_alive(&owner) {
                continue;
            }
            match self.delete_machine_if_present(&name).await {
                Ok(()) => tracing::info!(machine = %name, owner, "reaped abandoned smolvm machine"),
                Err(error) => {
                    tracing::warn!(machine = %name, owner, %error, "failed to reap abandoned machine")
                }
            }
        }
    }

    /// `(name, owner pid)` for machines carrying this backend's labels. Reads
    /// `--json`: the table view truncates names and omits labels entirely.
    async fn labelled_machines(&self) -> Result<Vec<(String, String)>> {
        let output = Command::new(self.binary().await?)
            .args(["machine", "ls", "--json"])
            .output()
            .await
            .context("spawn smolvm machine ls --json")?;
        if !output.status.success() {
            bail!(
                "smolvm machine ls failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let parsed: Value =
            serde_json::from_slice(&output.stdout).context("parse smolvm machine ls --json")?;
        let items = parsed
            .as_array()
            .cloned()
            .or_else(|| parsed.get("machines")?.as_array().cloned())
            .unwrap_or_default();
        Ok(items
            .iter()
            .filter_map(|item| {
                let labels = item.get("labels")?;
                // The key label is what marks a machine as ours.
                labels.get(WARM_SANDBOX_KEY_LABEL)?;
                let name = item.get("name")?.as_str()?.to_string();
                let owner = labels
                    .get(WARM_SANDBOX_OWNER_PID_LABEL)?
                    .as_str()?
                    .to_string();
                Some((name, owner))
            })
            .collect())
    }

    /// Delete if present, tolerating "not found". Deliberately not a `machine ls`
    /// pre-check: that view truncates names at 15 chars and ours are 20, so the
    /// match could never hit — and asking outright has no check-then-act race.
    async fn delete_machine_if_present(&self, name: &str) -> Result<()> {
        let output = Command::new(self.binary().await?)
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
        ensure!(
            !matches!(
                request.spec.policy.networking,
                SandboxNetworkPolicy::Limited { .. }
            ),
            "smolvm does not support policy.networking.limited; its DNS filter permits subdomains, but Exo requires exact hosts"
        );
        let protected = request.spec.policy.requires_proxy();
        if protected {
            ensure!(
                request.lifecycle.idle_ttl.is_some() && self.mode != SmolvmExecutionMode::OneShot,
                "smolvm proxy egress requires a managed warm sandbox"
            );
            ensure!(
                self.capabilities().await?.interceptor,
                "smolvm proxy egress requires a binary with --egress-interceptor-ports support; configure --smolvm-binary"
            );
        }
        let binary = self.binary().await?;
        reject_unsupported_spec(&request.spec, &request.spec.image)?;
        if self.resolve_mode(&request).await != SmolvmExecutionMode::Warm {
            let image = self.prepare_image(&request.spec.image).await?;
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
                    self.ensure_machine_started(
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
        if self.labels_supported().await {
            self.reap_abandoned_machines(&machine).await;
        }
        Ok(crate::with_process_management(handle))
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
        reject_unsupported_spec(&request.spec, &request.spec.image)?;
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
        // A restored machine is ours too, or reaping would never see it.
        self.stamp_labels(&mut create, request.sandbox_id.as_str())
            .await;
        configure_spec_args(&mut create, &request.spec);
        run_checked(create, "smolvm machine create --from").await?;

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
    fn build(&self, command: &SandboxCommand, cwd: &str) -> Command {
        let mut process = Command::new(&self.binary);
        process.arg("machine").arg("run");
        process.arg("--image").arg(&self.image);
        configure_spec_args(&mut process, &self.request.spec);
        configure_command_args(&mut process, command, cwd);
        // Arms smolvm's parent-death watchdog so the VM dies with a SIGKILLed CLI
        // rather than reparenting to init. Ephemeral runs only.
        if let Some(boot) = &self.boot_binary {
            process.env("SMOLVM_BOOT_BINARY", boot);
        }
        process.arg("--");
        process.args(&command.argv);
        process.kill_on_drop(true);
        process
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
        let process = self.build(command, &cwd);
        run_command(process, &with_backstop_timeout(command), cwd).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        let cwd = resolve_cwd(command, &self.request.spec);
        let process = self.build(command, &cwd);
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

/// Mounts and network policy, shared by the create/run paths.
fn configure_spec_args(process: &mut Command, spec: &SandboxSpec) {
    let resources = spec.resources.unwrap_or_default();
    process.arg("--cpus").arg(resources.vcpu_count.to_string());
    process.arg("--mem").arg(resources.memory_mib.to_string());
    if spec.policy.networking_enabled() {
        process.args(["--net", "--net-backend", "virtio-net"]);
    }
    for mount in &spec.mounts {
        let mut value = format!("{}:{}", mount.host_path.display(), mount.guest_path);
        if mount.access == SandboxMountAccess::ReadOnly {
            value.push_str(":ro");
        }
        process.arg("--volume").arg(value);
    }
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
                model: None,
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
        assert!(error.contains("--egress-interceptor-ports"), "{error}");
        assert!(!dir.path().join("cache").exists());
        let mut limited = request.clone();
        limited.spec.policy.networking = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["api.test".into()],
        };
        let error = backend.acquire(limited).await.err().unwrap().to_string();
        assert!(error.contains("exact hosts"), "{error}");
        request.lifecycle.idle_ttl = None;
        let error = backend.acquire(request).await.err().unwrap().to_string();
        assert!(error.contains("managed warm sandbox"), "{error}");
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
                .env("SMOLMACHINES_ENGINE_VERSION", "1.17.0")
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
            let cached = root.join("cache").join(format!("smolvm-1.17.0-{platform}"));
            let binary = cached.join("smolvm");
            write_test_binary(&binary, "printf 'smolvm 1.17.0\\n'");
            write_test_binary(&cached.join("smolvm-bin"), "exit 0");
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
        write_test_binary(&installed, "printf 'smolvm 1.17.0\\n'");
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
        configure_spec_args(&mut process, &spec);
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
}
