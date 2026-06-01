//! Daytona remote-container sandbox backend.
//!
//! Speaks Daytona's REST API directly via `reqwest`. State persistence is
//! handled by Daytona itself: `stop` preserves the sandbox filesystem and the
//! next `acquire` finds the same sandbox by label and `start`s it. Explicit
//! `/snapshot` and `/rewind` go through [`SnapshotKind::DaytonaSnapshot`]
//! payloads whose bytes are a small JSON manifest pointing at a named snapshot
//! in Daytona's registry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::io::Cursor;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::sandbox::{
    DEFAULT_SANDBOX_IMAGE, ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand,
    SandboxCommandOutput, SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotKind,
    SnapshotPayload, WARM_SANDBOX_KEY_LABEL, WARM_SANDBOX_SPEC_HASH_LABEL, sandbox_spec_hash,
};

pub const DEFAULT_DAYTONA_API_URL: &str = "https://app.daytona.io/api";

/// Per-sandbox operations (exec, fs) go through Daytona's toolbox proxy rather
/// than the control-plane API. The two have different base hosts.
pub const DEFAULT_DAYTONA_TOOLBOX_URL: &str = "https://proxy.app.daytona.io";

#[derive(Debug, Clone)]
pub struct DaytonaConfig {
    pub api_key: String,
    pub api_url: String,
    pub toolbox_url: String,
    /// Optional region target (`eu` / `us`). Passed through to Daytona on
    /// sandbox creation when set.
    pub target: Option<String>,
    /// Organization ID to scope requests to. Sent as the
    /// `X-Daytona-Organization-ID` header. API keys can belong to multiple
    /// organizations; without this, Daytona may default to a "personal" org
    /// that isn't the one with credit / the one the user expects.
    pub organization_id: Option<String>,
}

impl DaytonaConfig {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("DAYTONA_API_KEY").map_err(|_| {
            anyhow!("DAYTONA_API_KEY is not set; required for the Daytona sandbox backend")
        })?;
        let api_url = std::env::var("DAYTONA_API_URL")
            .unwrap_or_else(|_| DEFAULT_DAYTONA_API_URL.to_string());
        let toolbox_url = std::env::var("DAYTONA_TOOLBOX_URL")
            .unwrap_or_else(|_| DEFAULT_DAYTONA_TOOLBOX_URL.to_string());
        let target = std::env::var("DAYTONA_TARGET").ok();
        let organization_id = std::env::var("DAYTONA_ORGANIZATION_ID").ok();
        Ok(Self {
            api_key,
            api_url,
            toolbox_url,
            target,
            organization_id,
        })
    }
}

/// JSON payload persisted alongside a `SnapshotKind::DaytonaSnapshot`. The
/// canonical filesystem bytes live in Daytona — we only persist enough
/// metadata to ask Daytona to create a sandbox from the same snapshot later.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaytonaSnapshotManifest {
    /// Name of the snapshot in Daytona's snapshot registry.
    snapshot_name: String,
    /// Image the snapshotted sandbox was originally created from. Used as a
    /// fallback if the snapshot itself doesn't carry enough info to recreate.
    base_image: String,
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
            .context("DAYTONA_API_KEY contains characters that aren't valid in an HTTP header")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(org_id) = config.organization_id.as_deref() {
            let value = HeaderValue::from_str(org_id).context(
                "DAYTONA_ORGANIZATION_ID contains characters that aren't valid in an HTTP header",
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
        // Daytona expects `labels` as a single query parameter whose value is a
        // JSON-encoded object. Both label keys go into the same object so a
        // spec change yields a fresh sandbox (matching the CLI backend's
        // "evict on spec change" semantics).
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
        // If Daytona returned more than one match (e.g. concurrent creates),
        // keep the most recent and let the rest age out via auto-stop /
        // auto-archive. The prototype doesn't try to GC duplicates.
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

        let auto_stop_minutes = request
            .lifecycle
            .idle_ttl
            .map(idle_ttl_to_minutes)
            // 0 disables auto-stop in Daytona; we don't want that — pick a
            // conservative default that matches Daytona's own (15 min).
            .unwrap_or(15);

        // `snapshot` on Daytona refers to a named, pre-registered snapshot in
        // the user's account — not an arbitrary docker image ref. For fresh
        // creates we omit it so Daytona falls back to its default base image;
        // only the snapshot-restore path passes a name through. Honouring
        // `spec.image` against Daytona would require registering it as a
        // Daytona snapshot first; that's a follow-up.
        let body = DaytonaCreateRequest {
            snapshot: snapshot_name.map(str::to_string),
            target: self.target.clone(),
            labels,
            env: HashMap::new(),
            auto_stop_interval: auto_stop_minutes,
            // Daytona's default auto-archive is one week; leave it alone.
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
}

#[async_trait]
impl ManagedSandboxBackend for DaytonaSandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_host_mounts(&request)?;
        let spec_hash = sandbox_spec_hash(&request.spec);
        // `acquire` is "create fresh." The harness calls `try_resume` first
        // when it wants to reuse an existing sandbox; once we're in `acquire`
        // the contract is to mint a new one. Daytona's label-based lookup of
        // existing sandboxes lives in `try_resume`.
        let sandbox = self.create_sandbox(&request, &spec_hash, None).await?;
        Ok(Arc::new(DaytonaSandboxHandle {
            id: format!("daytona:{}", request.key),
            sandbox_id: sandbox.id,
            request,
            backend: self.handle_backend()?,
        }))
    }

    async fn try_resume(
        &self,
        request: SandboxRequest,
    ) -> Result<Option<Arc<dyn ManagedSandboxHandle>>> {
        reject_host_mounts(&request)?;
        let spec_hash = sandbox_spec_hash(&request.spec);
        let key_label = request.key.to_string();

        let Some(existing) = self.find_sandbox_by_labels(&key_label, &spec_hash).await? else {
            return Ok(None);
        };
        // Daytona preserves filesystem state across stop/start, so a
        // labelled sandbox we find here always carries the previous
        // session's writes. Start it if the auto-stop timer has fired.
        if !existing.is_running() {
            self.start_sandbox(&existing.id).await?;
        }
        Ok(Some(Arc::new(DaytonaSandboxHandle {
            id: format!("daytona:{}", request.key),
            sandbox_id: existing.id,
            request,
            backend: self.handle_backend()?,
        })))
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_host_mounts(&request)?;
        if !matches!(payload.kind, SnapshotKind::DaytonaSnapshot) {
            bail!(
                "Daytona sandbox backend can only restore from SnapshotKind::DaytonaSnapshot, \
                 got {:?}",
                payload.kind
            );
        }
        let manifest: DaytonaSnapshotManifest =
            serde_json::from_slice(&payload.bytes).context("decoding DaytonaSnapshot manifest")?;
        let spec_hash = sandbox_spec_hash(&request.spec);
        let sandbox = self
            .create_sandbox(&request, &spec_hash, Some(&manifest.snapshot_name))
            .await?;
        Ok(Arc::new(DaytonaSandboxHandle {
            id: format!("daytona-restored:{}", request.key),
            sandbox_id: sandbox.id,
            request,
            backend: self.handle_backend()?,
        }))
    }
}

impl DaytonaSandboxBackend {
    /// Clone the backend's HTTP state into a value the handle can hold. We
    /// can't hand out `Arc<Self>` from `&self` without making the backend's
    /// internals `Arc`-shared at construction; instead the handle holds its
    /// own clone of the (cheap) client + URL + target.
    fn handle_backend(&self) -> Result<DaytonaBackendHandle> {
        Ok(DaytonaBackendHandle {
            client: self.client.clone(),
            api_url: self.api_url.clone(),
            toolbox_url: self.toolbox_url.clone(),
            target: self.target.clone(),
        })
    }
}

#[derive(Clone)]
struct DaytonaBackendHandle {
    client: reqwest::Client,
    api_url: String,
    toolbox_url: String,
    #[allow(dead_code)] // available for future per-handle operations that re-target.
    target: Option<String>,
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
        // Delegate to a free function so the backend struct doesn't need to
        // be re-borrowed; the handle owns everything it needs.
        exec_in_sandbox(&self.backend, &self.sandbox_id, &self.request.spec, command).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        // Daytona's exec endpoint is request/response, not streaming. Run the
        // command synchronously, then hand the caller a SandboxProcessParts
        // whose stdout/stderr/wait are already populated. stdin goes to a
        // sink — Daytona's exec doesn't accept piped input on this endpoint.
        let output =
            exec_in_sandbox(&self.backend, &self.sandbox_id, &self.request.spec, command).await?;
        let exit_code = output.exit_code.unwrap_or(0);
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
        // Stop, do NOT delete. Daytona preserves filesystem state across stop;
        // the next acquire() for the same SandboxKey will find this sandbox by
        // label and start() it back up. Auto-archive / auto-delete on the
        // Daytona side eventually GCs sandboxes nobody comes back for.
        stop_via_backend(&self.backend, &self.sandbox_id).await
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        save_as_snapshot_via_backend(
            &self.backend,
            &self.sandbox_id,
            resolve_image(&self.request.spec),
        )
        .await
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

async fn save_as_snapshot_via_backend(
    backend: &DaytonaBackendHandle,
    id: &str,
    base_image: String,
) -> Result<SnapshotPayload> {
    let snapshot_name = format!("exo-snap-{}", Uuid::new_v4().simple());
    let body = DaytonaSnapshotRequest {
        name: snapshot_name.clone(),
    };
    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/sandbox/{id}/snapshot")))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("snapshotting Daytona sandbox {id}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("Daytona snapshot failed ({status}): {text}");
    }
    let manifest = DaytonaSnapshotManifest {
        snapshot_name,
        base_image,
    };
    let bytes = serde_json::to_vec(&manifest).context("serializing Daytona snapshot manifest")?;
    Ok(SnapshotPayload {
        kind: SnapshotKind::DaytonaSnapshot,
        bytes: Bytes::from(bytes),
    })
}

fn reject_host_mounts(request: &SandboxRequest) -> Result<()> {
    if request.spec.mounts.is_empty() {
        return Ok(());
    }
    bail!(
        "Daytona sandbox backend does not support host bind-mounts; \
         remove conversation mounts or switch to --sandbox-backend docker. \
         A remote-workspace provisioner (git clone / Daytona Volume) is planned as a follow-up."
    )
}

fn resolve_image(spec: &SandboxSpec) -> String {
    if spec.image.trim().is_empty() {
        DEFAULT_SANDBOX_IMAGE.to_string()
    } else {
        spec.image.clone()
    }
}

fn idle_ttl_to_minutes(ttl: Duration) -> u32 {
    // Daytona auto-stop is in minutes with 0 meaning "disabled". Round up
    // so we never expire earlier than the caller asked for, and floor at 1
    // so the smallest meaningful TTL still produces an active timer.
    let secs = ttl.as_secs();
    let minutes = secs.div_ceil(60);
    minutes.clamp(1, u32::MAX as u64) as u32
}

fn render_shell_command(argv: &[String]) -> String {
    // Daytona's execute endpoint takes a single command string interpreted by
    // a shell. Quote each argv element so spaces / metacharacters survive.
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

#[derive(Debug, Serialize)]
struct DaytonaSnapshotRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
struct DaytonaSandboxList {
    #[serde(default)]
    items: Vec<DaytonaSandbox>,
}

#[derive(Debug, Deserialize)]
struct DaytonaSandbox {
    id: String,
    state: String,
    #[serde(default, rename = "createdAt", alias = "created_at")]
    created_at: Option<String>,
}

impl DaytonaSandbox {
    fn is_running(&self) -> bool {
        // Daytona's state strings vary by API version; treat anything that
        // looks like an active state as running, and anything else as
        // requiring a `start` call. The conservative call is "if I'm not sure
        // it's running, ask Daytona to start it" — `start` is a no-op on an
        // already-running sandbox.
        matches!(self.state.as_str(), "started" | "running")
    }
}
