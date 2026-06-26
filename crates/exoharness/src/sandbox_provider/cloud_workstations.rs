//! Google Cloud Workstations remote sandbox backend.
//!
//! Topology: exo (the harness + executor + event log) stays central/durable; a
//! Cloud Workstation is just a sandbox BACKEND that exo dispatches `exec` /
//! `start_process` into. `acquire` starts/resumes the workstation; `exec` runs a
//! command over the IAP-tunnelled SSH path that `gcloud workstations ssh
//! --command` already proves out; `stop` optionally suspends it.
//!
//! Unlike the e2b/daytona backends this one shells out to the `gcloud` CLI
//! rather than calling a REST API directly. `gcloud workstations {start,ssh,stop}`
//! is the battle-tested mechanism (it handles IAP auth, tunnelling, and host-key
//! management), so the backend stays thin: it shapes argv, runs the process with
//! a bounded timeout, and maps stdout/stderr/exit-code into a
//! [`SandboxCommandOutput`].
//!
//! Snapshotting is unsupported in v1 (like the local-process backend). The
//! documented future path is a PD-snapshot `SnapshotKind` variant backed by GCP
//! persistent-disk snapshots (`acquire_from_snapshot` -> restore-PD); see the
//! `bail!` in [`CloudWorkstationsSandboxHandle::snapshot`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures::future::BoxFuture;
use tokio::process::{Child, Command};
use tokio::time;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxRequest, SandboxSpec, SnapshotPayload,
};

/// Default GCP project for the remoco workstation fleet.
pub const DEFAULT_CLOUD_WORKSTATIONS_PROJECT: &str = "remoco-cloud";
/// Default workstation cluster.
pub const DEFAULT_CLOUD_WORKSTATIONS_CLUSTER: &str = "remoco";
/// Default workstation configuration (machine class).
pub const DEFAULT_CLOUD_WORKSTATIONS_CONFIG: &str = "wiley-xl";
/// Default region.
pub const DEFAULT_CLOUD_WORKSTATIONS_REGION: &str = "us-central1";
/// Default `gcloud` binary name (resolved on `PATH`).
pub const DEFAULT_GCLOUD_BIN: &str = "gcloud";
/// Default timeout for `gcloud workstations start`/`stop` admin calls.
const DEFAULT_ADMIN_TIMEOUT: Duration = Duration::from_secs(180);
/// Cap on captured stdout/stderr per stream (bytes) for buffered `exec`.
const DEFAULT_OUTPUT_CAP_BYTES: usize = 1024 * 1024;

/// Coordinates + tunables for a Cloud Workstations backend. Mirrors the
/// e2b/daytona `*Config` structs: a plain, fully-resolved value the backend
/// holds, built either from a `Binding::Sandbox` or from conventional defaults.
#[derive(Debug, Clone)]
pub struct CloudWorkstationsConfig {
    pub project: String,
    pub cluster: String,
    pub config: String,
    pub region: String,
    /// The workstation instance id (e.g. `wiley`). `acquire` targets this
    /// instance; the same workstation is reused across turns.
    pub workstation: String,
    /// `gcloud` binary path.
    pub gcloud_bin: PathBuf,
    /// If true, `stop` suspends the workstation (`gcloud workstations stop`).
    /// If false, `stop` is a no-op and the workstation keeps running (cheaper
    /// for back-to-back turns; the fleet's idle-timeout reclaims it).
    pub stop_on_release: bool,
}

impl Default for CloudWorkstationsConfig {
    fn default() -> Self {
        Self {
            project: DEFAULT_CLOUD_WORKSTATIONS_PROJECT.to_string(),
            cluster: DEFAULT_CLOUD_WORKSTATIONS_CLUSTER.to_string(),
            config: DEFAULT_CLOUD_WORKSTATIONS_CONFIG.to_string(),
            region: DEFAULT_CLOUD_WORKSTATIONS_REGION.to_string(),
            workstation: String::new(),
            gcloud_bin: PathBuf::from(DEFAULT_GCLOUD_BIN),
            stop_on_release: false,
        }
    }
}

impl CloudWorkstationsConfig {
    /// Shared `gcloud workstations <verb>` argument prefix: the resource coords
    /// every lifecycle/exec call needs.
    fn resource_args(&self, workstation: &str) -> Vec<String> {
        vec![
            workstation.to_string(),
            format!("--project={}", self.project),
            format!("--region={}", self.region),
            format!("--cluster={}", self.cluster),
            format!("--config={}", self.config),
        ]
    }

    /// The workstation id this request targets. The request's `spec.image`
    /// field is overloaded to carry a per-request workstation override (mirrors
    /// how e2b overloads `spec.image` as the template id); falls back to the
    /// config default when empty.
    fn workstation_for(&self, spec: &SandboxSpec) -> Result<String> {
        let id = if spec.image.trim().is_empty() {
            self.workstation.clone()
        } else {
            spec.image.trim().to_string()
        };
        if id.is_empty() {
            bail!(
                "cloud-workstations backend requires a workstation id; set it on the sandbox \
                 binding (workstation=<id>) or pass it as the sandbox image"
            );
        }
        Ok(id)
    }
}

pub struct CloudWorkstationsSandboxBackend {
    config: CloudWorkstationsConfig,
}

impl CloudWorkstationsSandboxBackend {
    pub fn new(config: CloudWorkstationsConfig) -> Result<Self> {
        Ok(Self { config })
    }

    /// Ensure the workstation is started/resumed. `gcloud workstations start`
    /// is idempotent: it returns success for an already-running workstation, so
    /// `acquire` can call it unconditionally.
    async fn ensure_started(&self, workstation: &str) -> Result<()> {
        let mut args = vec!["workstations".to_string(), "start".to_string()];
        args.extend(self.config.resource_args(workstation));
        let output = run_gcloud(&self.config.gcloud_bin, &args, DEFAULT_ADMIN_TIMEOUT).await?;
        if !output.ok {
            let stderr = output.stderr.trim();
            // An already-running workstation is success for our purposes.
            if is_already_running(stderr) {
                return Ok(());
            }
            bail!("failed to start workstation {workstation}: {stderr}");
        }
        Ok(())
    }
}

#[async_trait]
impl ManagedSandboxBackend for CloudWorkstationsSandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_host_mounts(&request)?;
        let workstation = self.config.workstation_for(&request.spec)?;
        self.ensure_started(&workstation).await?;
        Ok(Arc::new(CloudWorkstationsSandboxHandle {
            id: format!("cloud-workstations:{}", request.key),
            workstation,
            config: self.config.clone(),
            request,
        }))
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        // v1: no snapshot. The future path is a PD-snapshot SnapshotKind variant
        // (acquire_from_snapshot -> restore a persistent-disk snapshot, then
        // start a workstation off the restored disk). See the matching note in
        // `CloudWorkstationsSandboxHandle::snapshot`.
        bail!(
            "restore-from-snapshot is not supported by the cloud-workstations backend (v1); \
             a GCP persistent-disk SnapshotKind variant is the documented future path"
        )
    }
}

struct CloudWorkstationsSandboxHandle {
    id: String,
    workstation: String,
    config: CloudWorkstationsConfig,
    request: SandboxRequest,
}

impl CloudWorkstationsSandboxHandle {
    /// Build the `gcloud workstations ssh ... --command=<remote>` argv for a
    /// sandbox command. The remote command is the env-prefixed, cwd-prefixed,
    /// shell-quoted argv joined into one `--command` string (ssh runs a single
    /// remote shell, so env/cwd have to be baked into that string).
    fn ssh_args(&self, command: &SandboxCommand, cwd: &str) -> Vec<String> {
        let mut args = vec!["workstations".to_string(), "ssh".to_string()];
        args.extend(self.config.resource_args(&self.workstation));
        args.push(format!("--command={}", self.remote_command(command, cwd)));
        args
    }

    fn remote_command(&self, command: &SandboxCommand, cwd: &str) -> String {
        remote_command_string(&command.env, cwd, &command.argv)
    }
}

#[async_trait]
impl ManagedSandboxHandle for CloudWorkstationsSandboxHandle {
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
        let args = self.ssh_args(command, &cwd);
        let mut output = run_gcloud(
            &self.config.gcloud_bin,
            &args,
            command.timeout.unwrap_or(DEFAULT_ADMIN_TIMEOUT),
        )
        .await?;
        // Report the user's command + cwd, not the gcloud wrapper argv.
        output.command = command
            .display_argv
            .clone()
            .unwrap_or_else(|| command.argv.clone());
        output.cwd = cwd;
        Ok(output)
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        if command.argv.is_empty() {
            bail!("sandbox command requires at least one argv entry");
        }
        let cwd = command
            .cwd
            .clone()
            .unwrap_or_else(|| self.request.spec.default_workdir.clone());
        let args = self.ssh_args(command, &cwd);
        spawn_gcloud_process(&self.config.gcloud_bin, &args, command).await
    }

    async fn stop(&self) -> Result<()> {
        if !self.config.stop_on_release {
            return Ok(());
        }
        let mut args = vec!["workstations".to_string(), "stop".to_string()];
        args.extend(self.config.resource_args(&self.workstation));
        let output = run_gcloud(&self.config.gcloud_bin, &args, DEFAULT_ADMIN_TIMEOUT).await?;
        if !output.ok {
            let stderr = output.stderr.trim();
            if is_already_stopped(stderr) {
                return Ok(());
            }
            bail!("failed to stop workstation {}: {stderr}", self.workstation);
        }
        Ok(())
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        // A Cloud Workstation's persistent state is its home-disk PD. A real
        // snapshot would be a GCP PD-snapshot tagged with a new SnapshotKind
        // (e.g. GcpPdSnapshot) and restored via acquire_from_snapshot ->
        // start-workstation-off-restored-disk. That is an upstream contribution
        // tracked separately; until it lands we fail explicitly like the
        // local-process backend.
        bail!(
            "snapshot is not supported by the cloud-workstations backend (v1); \
             a GCP persistent-disk SnapshotKind variant is the documented future path"
        )
    }
}

/// The cloud-workstations backend has no host filesystem to bind-mount; reject
/// conversation mounts the same way the e2b backend does.
fn reject_host_mounts(request: &SandboxRequest) -> Result<()> {
    if request.spec.mounts.is_empty() {
        return Ok(());
    }
    bail!(
        "cloud-workstations sandbox backend does not support host bind-mounts; \
         remove conversation mounts or use a local sandbox provider"
    )
}

/// Shape the single remote-shell command string for `gcloud ... ssh --command`.
/// Env assignments and a `cd` are prefixed, then the shell-quoted argv. Splitting
/// this out keeps it unit-testable without a live workstation.
fn remote_command_string(env: &HashMap<String, String>, cwd: &str, argv: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !cwd.trim().is_empty() {
        parts.push(format!("cd {} &&", super::shell_quote(cwd)));
    }
    // Deterministic env ordering so the shaped command is stable/testable.
    let mut env_keys: Vec<&String> = env.keys().collect();
    env_keys.sort();
    for key in env_keys {
        let value = &env[key];
        parts.push(format!("{}={}", key, super::shell_quote(value)));
    }
    let quoted_argv: Vec<String> = argv.iter().map(|a| super::shell_quote(a)).collect();
    parts.push(quoted_argv.join(" "));
    parts.join(" ")
}

fn is_already_running(stderr: &str) -> bool {
    let lc = stderr.to_lowercase();
    lc.contains("already running")
        || lc.contains("already started")
        || lc.contains("state_running")
        || lc.contains("is running")
}

fn is_already_stopped(stderr: &str) -> bool {
    let lc = stderr.to_lowercase();
    lc.contains("already stopped")
        || lc.contains("not running")
        || lc.contains("state_stopped")
        || lc.contains("is stopped")
}

/// Run a `gcloud` invocation, capture bounded stdout/stderr + exit code with a
/// timeout. Returns a [`SandboxCommandOutput`] with the gcloud argv recorded;
/// callers overwrite `command`/`cwd` with the user-facing values.
async fn run_gcloud(
    gcloud_bin: &PathBuf,
    args: &[String],
    timeout: Duration,
) -> Result<SandboxCommandOutput> {
    let mut process = Command::new(gcloud_bin);
    process.args(args);
    process.kill_on_drop(true);

    let output = match time::timeout(timeout, process.output()).await {
        Ok(result) => result.with_context(|| {
            format!(
                "failed to invoke {} {}",
                gcloud_bin.display(),
                args.join(" ")
            )
        })?,
        Err(_) => {
            return Err(anyhow!(
                "gcloud command timed out after {}s: gcloud {}",
                timeout.as_secs(),
                args.join(" ")
            ));
        }
    };

    Ok(SandboxCommandOutput {
        ok: output.status.success(),
        exit_code: output.status.code(),
        stdout: cap_output(&output.stdout),
        stderr: cap_output(&output.stderr),
        command: args.to_vec(),
        cwd: String::new(),
    })
}

/// Spawn a streaming `gcloud ... ssh` process and hand back its piped stdio.
async fn spawn_gcloud_process(
    gcloud_bin: &PathBuf,
    args: &[String],
    command: &SandboxCommand,
) -> Result<crate::SandboxProcessParts> {
    let mut process = Command::new(gcloud_bin);
    process.args(args);
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process.kill_on_drop(true);

    let mut child = process.spawn().with_context(|| {
        format!(
            "failed to start gcloud sandbox command: {}",
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
        .ok_or_else(|| anyhow!("gcloud process did not expose stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("gcloud process did not expose stderr"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("gcloud process did not expose stdin"))?;

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

fn cap_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= DEFAULT_OUTPUT_CAP_BYTES {
        return text.into_owned();
    }
    let mut capped: String = text.chars().take(DEFAULT_OUTPUT_CAP_BYTES).collect();
    capped.push_str("\n…[output truncated]");
    capped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{SandboxKey, SandboxLifecycleConfig, SandboxNetworkPolicy};

    fn spec(image: &str, workdir: &str) -> SandboxSpec {
        SandboxSpec {
            image: image.to_string(),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            network: SandboxNetworkPolicy::Enabled,
            default_workdir: workdir.to_string(),
        }
    }

    fn request(image: &str) -> SandboxRequest {
        SandboxRequest {
            key: SandboxKey::ConversationSandbox {
                conversation_id: "conv".to_string(),
                sandbox_id: "sbx".to_string(),
            },
            spec: spec(image, "/home/user"),
            lifecycle: SandboxLifecycleConfig::default(),
        }
    }

    #[test]
    fn defaults_match_remoco_coords() {
        let config = CloudWorkstationsConfig::default();
        assert_eq!(config.project, "remoco-cloud");
        assert_eq!(config.cluster, "remoco");
        assert_eq!(config.config, "wiley-xl");
        assert_eq!(config.region, "us-central1");
        assert!(!config.stop_on_release);
    }

    #[test]
    fn resource_args_carry_all_coords() {
        let config = CloudWorkstationsConfig {
            workstation: "wiley".to_string(),
            ..Default::default()
        };
        let args = config.resource_args("wiley");
        assert_eq!(args[0], "wiley");
        assert!(args.contains(&"--project=remoco-cloud".to_string()));
        assert!(args.contains(&"--region=us-central1".to_string()));
        assert!(args.contains(&"--cluster=remoco".to_string()));
        assert!(args.contains(&"--config=wiley-xl".to_string()));
    }

    #[test]
    fn workstation_id_prefers_spec_image_override() {
        let config = CloudWorkstationsConfig {
            workstation: "default-ws".to_string(),
            ..Default::default()
        };
        // Per-request override via spec.image.
        assert_eq!(
            config.workstation_for(&spec("other-ws", "/x")).unwrap(),
            "other-ws"
        );
        // Empty image falls back to the config default.
        assert_eq!(
            config.workstation_for(&spec("", "/x")).unwrap(),
            "default-ws"
        );
    }

    #[test]
    fn workstation_id_required() {
        let config = CloudWorkstationsConfig::default(); // empty workstation
        let err = config.workstation_for(&spec("", "/x")).unwrap_err();
        assert!(err.to_string().contains("requires a workstation id"));
    }

    #[test]
    fn remote_command_prefixes_cd_and_env() {
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let argv = vec!["echo".to_string(), "hello world".to_string()];
        let remote = remote_command_string(&env, "/home/user", &argv);
        assert_eq!(remote, "cd /home/user && FOO=bar echo 'hello world'");
    }

    #[test]
    fn remote_command_quotes_dangerous_args() {
        let env = HashMap::new();
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "rm -rf /; echo $HOME".to_string(),
        ];
        let remote = remote_command_string(&env, "", &argv);
        // No cwd prefix when cwd is empty; the injection payload is fully quoted.
        assert_eq!(remote, "sh -c 'rm -rf /; echo $HOME'");
    }

    #[test]
    fn ssh_args_shape_full_invocation() {
        let backend = CloudWorkstationsSandboxBackend::new(CloudWorkstationsConfig {
            workstation: "wiley".to_string(),
            ..Default::default()
        })
        .unwrap();
        let handle = CloudWorkstationsSandboxHandle {
            id: "cloud-workstations:test".to_string(),
            workstation: "wiley".to_string(),
            config: backend.config.clone(),
            request: request(""),
        };
        let command = SandboxCommand {
            argv: vec!["echo".to_string(), "hello".to_string()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        };
        let args = handle.ssh_args(&command, "/home/user");
        assert_eq!(args[0], "workstations");
        assert_eq!(args[1], "ssh");
        assert!(args.contains(&"wiley".to_string()));
        assert!(args.contains(&"--cluster=remoco".to_string()));
        let command_flag = args
            .iter()
            .find(|a| a.starts_with("--command="))
            .expect("ssh args must carry a --command flag");
        assert_eq!(command_flag, "--command=cd /home/user && echo hello");
    }

    #[test]
    fn already_running_and_stopped_detection() {
        assert!(is_already_running("Workstation is already running"));
        assert!(is_already_running("state: STATE_RUNNING"));
        assert!(!is_already_running("some other error"));
        assert!(is_already_stopped("Workstation already stopped"));
        assert!(is_already_stopped("workstation is not running"));
        assert!(!is_already_stopped("permission denied"));
    }

    #[test]
    fn output_cap_truncates_large_streams() {
        let big = vec![b'a'; DEFAULT_OUTPUT_CAP_BYTES + 100];
        let capped = cap_output(&big);
        assert!(capped.contains("output truncated"));
        assert!(capped.len() < big.len());
    }
}
