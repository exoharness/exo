//! Daytona remote-container sandbox backend, speaking its REST API via
//! `reqwest`. API reference: <https://www.daytona.io/docs/en/tools/api/>.
//!
//! Daytona persists state itself: `stop` keeps the filesystem and the next
//! `acquire` finds the sandbox by label and `start`s it. Snapshot/restore is
//! not implemented yet.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::io::Cursor;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotPayload, WARM_SANDBOX_KEY_LABEL,
    WARM_SANDBOX_SPEC_HASH_LABEL, sandbox_spec_hash,
};

pub const DEFAULT_DAYTONA_API_URL: &str = "https://app.daytona.io/api";

/// Per-sandbox operations (exec, fs) go through Daytona's toolbox proxy rather
/// than the control-plane API. The two have different base hosts.
pub const DEFAULT_DAYTONA_TOOLBOX_URL: &str = "https://proxy.app.daytona.io";

const START_POLL_INTERVAL: Duration = Duration::from_millis(500);
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// Resolved Daytona connection parameters: assembled from a
/// [`crate::DaytonaBackendSpec`] plus secrets read on first use.
#[derive(Debug, Clone)]
pub struct DaytonaConfig {
    pub api_key: String,
    pub api_url: String,
    pub toolbox_url: String,
    /// Region target (`eu` / `us`), passed through on create when set.
    pub target: Option<String>,
    /// Scopes requests via `X-Daytona-Organization-ID`; without it Daytona may
    /// default to a "personal" org that lacks credit.
    pub organization_id: Option<String>,
}

pub struct DaytonaSandboxBackend {
    client: reqwest::Client,
    api_url: String,
    toolbox_url: String,
    target: Option<String>,
}

impl DaytonaSandboxBackend {
    pub fn new(config: DaytonaConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .context("Daytona API key contains characters that aren't valid in an HTTP header")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(org_id) = config.organization_id.as_deref() {
            let value = HeaderValue::from_str(org_id).context(
                "Daytona organization id contains characters that aren't valid in an HTTP header",
            )?;
            headers.insert("X-Daytona-Organization-ID", value);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("building Daytona HTTP client")?;
        Ok(Self {
            client,
            api_url: config.api_url.trim_end_matches('/').to_string(),
            toolbox_url: config.toolbox_url.trim_end_matches('/').to_string(),
            target: config.target,
        })
    }

    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    async fn find_sandbox_by_labels(
        &self,
        key_label: &str,
        spec_hash: &str,
    ) -> Result<Option<DaytonaSandbox>> {
        // `labels` is one query param holding a JSON object (per `GET /sandbox`);
        // both keys share it so a spec change yields a fresh sandbox.
        let labels_filter = serde_json::json!({
            WARM_SANDBOX_KEY_LABEL: key_label,
            WARM_SANDBOX_SPEC_HASH_LABEL: spec_hash,
        });
        let response = self
            .client
            .get(self.api_endpoint("/sandbox"))
            .query(&[("labels", labels_filter.to_string())])
            .send()
            .await
            .context("listing Daytona sandboxes")?
            .error_for_status()
            .context("Daytona list sandboxes returned an error status")?;
        let list: DaytonaSandboxList = response
            .json()
            .await
            .context("decoding Daytona sandbox list response")?;
        let mut sandboxes = list.items;
        // On multiple matches keep the most recent; the rest age out via Daytona.
        sandboxes.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(sandboxes.into_iter().next())
    }

    async fn create_sandbox(
        &self,
        request: &SandboxRequest,
        spec_hash: &str,
        snapshot_name: Option<&str>,
    ) -> Result<DaytonaSandbox> {
        let mut labels = HashMap::new();
        labels.insert(WARM_SANDBOX_KEY_LABEL.to_string(), request.key.to_string());
        labels.insert(
            WARM_SANDBOX_SPEC_HASH_LABEL.to_string(),
            spec_hash.to_string(),
        );

        // 0 would disable auto-stop; default to Daytona's 15 min instead.
        let auto_stop_minutes = request
            .lifecycle
            .idle_ttl
            .map(idle_ttl_to_minutes)
            .unwrap_or(15);

        // Restore uses the saved snapshot; a fresh create falls back to the
        // requested image as a snapshot name (Daytona's only base selector).
        let snapshot = match snapshot_name {
            Some(name) => Some(name.to_string()),
            None => {
                let image = request.spec.image.trim();
                (!image.is_empty()).then(|| image.to_string())
            }
        };
        let body = DaytonaCreateRequest {
            snapshot,
            target: self.target.clone(),
            labels,
            env: HashMap::new(),
            auto_stop_interval: auto_stop_minutes,
            network_block_all: matches!(request.spec.network, SandboxNetworkPolicy::Disabled),
        };

        let response = self
            .client
            .post(self.api_endpoint("/sandbox"))
            .json(&body)
            .send()
            .await
            .context("creating Daytona sandbox")?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("Daytona create-sandbox failed ({status}): {text}");
        }
        let sandbox: DaytonaSandbox = response
            .json()
            .await
            .context("decoding Daytona create-sandbox response")?;
        Ok(sandbox)
    }

    async fn start_sandbox(&self, id: &str) -> Result<()> {
        let response = self
            .client
            .post(self.api_endpoint(&format!("/sandbox/{id}/start")))
            .send()
            .await
            .with_context(|| format!("starting Daytona sandbox {id}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("Daytona start-sandbox failed ({status}): {text}");
        }
        Ok(())
    }

    async fn get_sandbox(&self, id: &str) -> Result<DaytonaSandbox> {
        self.client
            .get(self.api_endpoint(&format!("/sandbox/{id}")))
            .send()
            .await
            .with_context(|| format!("fetching Daytona sandbox {id}"))?
            .error_for_status()
            .with_context(|| format!("Daytona get-sandbox {id} returned an error status"))?
            .json()
            .await
            .with_context(|| format!("decoding Daytona sandbox {id}"))
    }

    /// Poll until the sandbox is `Started`, so the first exec doesn't race a
    /// still-booting sandbox (create and start are both async on Daytona's side).
    async fn wait_until_started(&self, id: &str) -> Result<()> {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match self.get_sandbox(id).await?.state {
                DaytonaSandboxState::Started => return Ok(()),
                DaytonaSandboxState::Error
                | DaytonaSandboxState::Destroying
                | DaytonaSandboxState::Destroyed => {
                    bail!("Daytona sandbox {id} entered a terminal state before starting")
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                bail!(
                    "Daytona sandbox {id} did not reach `started` within {}s",
                    START_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(START_POLL_INTERVAL).await;
        }
    }
}

#[async_trait]
impl ManagedSandboxBackend for DaytonaSandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_host_mounts(&request)?;
        let spec_hash = sandbox_spec_hash(&request.spec);

        // Reuse a matching sandbox if one exists (also how we recover across exo
        // restarts); Daytona keeps its filesystem across stop.
        let sandbox = match self
            .find_sandbox_by_labels(&request.key.to_string(), &spec_hash)
            .await?
        {
            // Replace a terminal/error sandbox; start only durably-stopped ones
            // (starting a running/mid-transition sandbox would race).
            Some(existing) if existing.is_reusable() => {
                if existing.needs_start() {
                    self.start_sandbox(&existing.id).await?;
                }
                existing
            }
            _ => self.create_sandbox(&request, &spec_hash, None).await?,
        };

        self.wait_until_started(&sandbox.id).await?;
        Ok(Arc::new(DaytonaSandboxHandle {
            id: format!("daytona:{}", request.key),
            sandbox_id: sandbox.id,
            request,
            backend: self.handle_backend(),
        }))
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        // Restoring needs snapshot-readiness handling that isn't in place yet.
        bail!("restoring a Daytona sandbox from a snapshot is not implemented yet");
    }
}

impl DaytonaSandboxBackend {
    /// Clone the HTTP state into a value the handle owns, so handle operations
    /// don't re-borrow the backend.
    fn handle_backend(&self) -> DaytonaBackendHandle {
        DaytonaBackendHandle {
            client: self.client.clone(),
            api_url: self.api_url.clone(),
            toolbox_url: self.toolbox_url.clone(),
        }
    }
}

#[derive(Clone)]
struct DaytonaBackendHandle {
    client: reqwest::Client,
    api_url: String,
    toolbox_url: String,
}

impl DaytonaBackendHandle {
    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    fn toolbox_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.toolbox_url, path)
    }
}

struct DaytonaSandboxHandle {
    id: String,
    sandbox_id: String,
    request: SandboxRequest,
    backend: DaytonaBackendHandle,
}

#[async_trait]
impl ManagedSandboxHandle for DaytonaSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        exec_in_sandbox(&self.backend, &self.sandbox_id, &self.request.spec, command).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        // Daytona's exec endpoint is request/response, not streaming: run the
        // command, then hand back already-populated stdout/stderr/wait. stdin
        // goes to a sink — this endpoint doesn't accept piped input.
        let output =
            exec_in_sandbox(&self.backend, &self.sandbox_id, &self.request.spec, command).await?;
        let Some(exit_code) = output.exit_code else {
            bail!("Daytona exec returned no exit code; refusing to report success");
        };
        let stdout = Cursor::new(output.stdout.into_bytes());
        let stderr = Cursor::new(output.stderr.into_bytes());
        let stdin = futures::io::sink();
        let wait: BoxFuture<'static, crate::Result<i32>> = Box::pin(async move { Ok(exit_code) });
        Ok(crate::SandboxProcessParts {
            stdout: Box::pin(stdout),
            stderr: Box::pin(stderr),
            stdin: Box::pin(stdin),
            wait,
        })
    }

    async fn stop(&self) -> Result<()> {
        // Stop, don't delete: Daytona preserves filesystem state across stop and
        // the next acquire for the same key finds and starts this sandbox again.
        stop_via_backend(&self.backend, &self.sandbox_id).await
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        // Capturing a snapshot reliably needs readiness handling that isn't in
        // place yet.
        bail!("Daytona sandbox snapshots are not implemented yet");
    }
}

async fn exec_in_sandbox(
    backend: &DaytonaBackendHandle,
    id: &str,
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
    let body = DaytonaExecRequest {
        command: render_shell_command(&command.argv),
        cwd: Some(cwd.clone()),
        env: command.env.clone(),
        timeout: command.timeout.map(|t| t.as_secs()),
    };
    let response = backend
        .client
        .post(backend.toolbox_endpoint(&format!("/toolbox/{id}/process/execute")))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("exec in Daytona sandbox {id}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona exec failed ({status}): {text}");
    }
    let response: DaytonaExecResponse = response
        .json()
        .await
        .context("decoding Daytona exec response")?;
    Ok(SandboxCommandOutput {
        ok: response.exit_code == 0,
        exit_code: Some(response.exit_code),
        stdout: response.result.unwrap_or_default(),
        stderr: String::new(),
        command: command
            .display_argv
            .clone()
            .unwrap_or_else(|| command.argv.clone()),
        cwd,
    })
}

async fn stop_via_backend(backend: &DaytonaBackendHandle, id: &str) -> Result<()> {
    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/sandbox/{id}/stop")))
        .send()
        .await
        .with_context(|| format!("stopping Daytona sandbox {id}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona stop-sandbox failed ({status}): {text}");
    }
    Ok(())
}

fn reject_host_mounts(request: &SandboxRequest) -> Result<()> {
    if request.spec.mounts.is_empty() {
        return Ok(());
    }
    bail!(
        "Daytona sandbox backend does not support host bind-mounts; \
         remove conversation mounts or use a local sandbox provider"
    )
}

fn idle_ttl_to_minutes(ttl: Duration) -> u32 {
    // Daytona auto-stop is in minutes, 0 meaning "disabled". Round up so we
    // never expire earlier than asked, and floor at 1 so the smallest TTL still
    // produces an active timer.
    let minutes = ttl.as_secs().div_ceil(60);
    minutes.clamp(1, u32::MAX as u64) as u32
}

fn render_shell_command(argv: &[String]) -> String {
    // Daytona's execute endpoint takes a single shell-interpreted string. Quote
    // each argv element so spaces / metacharacters survive.
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | ',')
        })
    {
        return arg.to_string();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    quoted
}

#[derive(Debug, Serialize)]
struct DaytonaCreateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    labels: HashMap<String, String>,
    env: HashMap<String, String>,
    #[serde(rename = "autoStopInterval")]
    auto_stop_interval: u32,
    #[serde(rename = "networkBlockAll")]
    network_block_all: bool,
}

#[derive(Debug, Serialize)]
struct DaytonaExecRequest {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    env: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DaytonaExecResponse {
    #[serde(rename = "exitCode", alias = "exit_code")]
    exit_code: i32,
    #[serde(default)]
    result: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DaytonaSandboxList {
    #[serde(default)]
    items: Vec<DaytonaSandbox>,
}

#[derive(Debug, Deserialize)]
struct DaytonaSandbox {
    id: String,
    state: DaytonaSandboxState,
    #[serde(default, rename = "createdAt", alias = "created_at")]
    created_at: Option<String>,
}

/// Daytona sandbox lifecycle states (see the API reference). Unrecognized
/// values fold into `Unknown`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DaytonaSandboxState {
    Creating,
    Starting,
    #[serde(alias = "running")]
    Started,
    Stopping,
    Stopped,
    Archiving,
    Archived,
    Destroying,
    Destroyed,
    Error,
    #[serde(other)]
    Unknown,
}

impl DaytonaSandbox {
    /// Reusable unless terminal/error (then `acquire` creates a fresh one).
    fn is_reusable(&self) -> bool {
        !matches!(
            self.state,
            DaytonaSandboxState::Error
                | DaytonaSandboxState::Destroying
                | DaytonaSandboxState::Destroyed
        )
    }

    /// Needs a `start` before use. Running/mid-transition states are left alone;
    /// `Unknown` errs toward starting (a no-op if already running).
    fn needs_start(&self) -> bool {
        matches!(
            self.state,
            DaytonaSandboxState::Stopped
                | DaytonaSandboxState::Archived
                | DaytonaSandboxState::Unknown
        )
    }
}
