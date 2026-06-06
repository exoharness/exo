//! Sprites.dev remote sandbox backend.
//!
//! Uses the Sprites platform REST API (`api.sprites.dev`) for lifecycle, HTTP
//! exec, and checkpoint/restore snapshots. Cross-process resume uses a
//! deterministic sprite name derived from [`SandboxKey`] + spec hash (same role
//! as Docker labels / E2B metadata). Snapshots are bytes-by-reference via
//! [`SnapshotKind::SpritesSnapshot`] manifests pointing at a checkpoint id.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::io::Cursor;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxRequest, SandboxSpec, SnapshotKind, SnapshotPayload, WARM_SANDBOX_KEY_LABEL,
    WARM_SANDBOX_SPEC_HASH_LABEL, sandbox_spec_hash,
};

pub const DEFAULT_SPRITES_API_URL: &str = "https://api.sprites.dev";

#[derive(Debug, Clone)]
pub struct SpritesConfig {
    pub token: String,
    pub api_url: String,
    /// Sprite HTTP URL auth: `sprite` or `public`. `None` lets Sprites default to `sprite`.
    pub url_auth: Option<String>,
    /// Organization slug for multi-org tokens.
    pub organization: Option<String>,
    /// Binding-level labels; exo resume labels are always merged in on create.
    pub extra_labels: Vec<String>,
}

/// JSON persisted for [`SnapshotKind::SpritesSnapshot`]. Filesystem state lives on
/// the sprite; we only store the checkpoint id and sprite name.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SpritesSnapshotManifest {
    checkpoint_id: String,
    sprite_name: String,
}

pub struct SpritesSandboxBackend {
    client: reqwest::Client,
    api_url: String,
    url_auth: Option<String>,
    organization: Option<String>,
    extra_labels: Vec<String>,
}

impl SpritesSandboxBackend {
    pub fn new(config: SpritesConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", config.token)).context(
            "SPRITES_TOKEN contains characters that aren't valid in an HTTP header",
        )?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("building Sprites HTTP client")?;
        if let Some(url_auth) = config.url_auth.as_deref() {
            validate_url_auth(url_auth)?;
        }
        Ok(Self {
            client,
            api_url: config.api_url.trim_end_matches('/').to_string(),
            url_auth: config.url_auth,
            organization: config.organization,
            extra_labels: config.extra_labels,
        })
    }

    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }

    async fn get_sprite(&self, name: &str) -> Result<Option<()>> {
        let response = self
            .client
            .get(self.api_endpoint(&format!("/v1/sprites/{name}")))
            .send()
            .await
            .with_context(|| format!("fetching Sprites sprite {name}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("Sprites get-sprite failed ({status}): {text}");
        }
        Ok(Some(()))
    }

    async fn create_sprite(&self, name: &str, request: &SandboxRequest) -> Result<()> {
        let spec_hash = sandbox_spec_hash(&request.spec);
        let labels = sprite_labels_for_request(request, &spec_hash, &self.extra_labels);
        let body = SpritesCreateRequest {
            name: name.to_string(),
            organization: self.organization.clone(),
            url_settings: self
                .url_auth
                .as_ref()
                .map(|auth| SpritesUrlSettings { auth: auth.clone() }),
            labels,
        };
        let response = self
            .client
            .post(self.api_endpoint("/v1/sprites"))
            .json(&body)
            .send()
            .await
            .context("creating Sprites sprite")?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::CONFLICT
            || (status == reqwest::StatusCode::BAD_REQUEST && text.contains("exists"))
        {
            return Ok(());
        }
        bail!("Sprites create-sprite failed ({status}): {text}");
    }

    async fn ensure_sprite(&self, name: &str, request: &SandboxRequest) -> Result<()> {
        if self.get_sprite(name).await?.is_some() {
            return Ok(());
        }
        self.create_sprite(name, request).await
    }

    fn handle_backend(&self) -> SpritesBackendHandle {
        SpritesBackendHandle {
            client: self.client.clone(),
            api_url: self.api_url.clone(),
        }
    }
}

#[async_trait]
impl ManagedSandboxBackend for SpritesSandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_host_mounts(&request)?;
        let sprite_name = sprite_name_for_request(&request);
        self.ensure_sprite(&sprite_name, &request).await?;
        Ok(Arc::new(SpritesSandboxHandle {
            id: format!("sprites:{}", request.key),
            sprite_name,
            request,
            backend: self.handle_backend(),
        }))
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_host_mounts(&request)?;
        if !matches!(payload.kind, SnapshotKind::SpritesSnapshot) {
            bail!(
                "Sprites sandbox backend can only restore from SnapshotKind::SpritesSnapshot, \
                 got {:?}",
                payload.kind
            );
        }
        let manifest: SpritesSnapshotManifest =
            serde_json::from_slice(&payload.bytes).context("decoding SpritesSnapshot manifest")?;
        let sprite_name = sprite_name_for_request(&request);
        if manifest.sprite_name != sprite_name {
            bail!(
                "Sprites snapshot belongs to sprite {}, but this sandbox key maps to {}",
                manifest.sprite_name,
                sprite_name
            );
        }
        self.ensure_sprite(&sprite_name, &request).await?;
        restore_checkpoint_via_backend(
            &self.handle_backend(),
            &sprite_name,
            &manifest.checkpoint_id,
        )
        .await?;
        Ok(Arc::new(SpritesSandboxHandle {
            id: format!("sprites-restored:{}", request.key),
            sprite_name,
            request,
            backend: self.handle_backend(),
        }))
    }
}

#[derive(Clone)]
struct SpritesBackendHandle {
    client: reqwest::Client,
    api_url: String,
}

impl SpritesBackendHandle {
    fn api_endpoint(&self, path: &str) -> String {
        format!("{}{}", self.api_url, path)
    }
}

struct SpritesSandboxHandle {
    id: String,
    sprite_name: String,
    request: SandboxRequest,
    backend: SpritesBackendHandle,
}

#[async_trait]
impl ManagedSandboxHandle for SpritesSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        exec_in_sprite(
            &self.backend,
            &self.sprite_name,
            &self.request.spec,
            command,
        )
        .await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        let output = exec_in_sprite(
            &self.backend,
            &self.sprite_name,
            &self.request.spec,
            command,
        )
        .await?;
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
        // Sprites hibernate when idle; do not DELETE — the next session resumes the
        // same sprite by deterministic name via `try_resume`.
        Ok(())
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        save_checkpoint_via_backend(&self.backend, &self.sprite_name).await
    }
}

async fn exec_in_sprite(
    backend: &SpritesBackendHandle,
    sprite_name: &str,
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

    let mut query: Vec<(&str, String)> = Vec::new();
    for arg in &command.argv {
        query.push(("cmd", arg.clone()));
    }
    query.push(("stdin", "false".to_string()));
    if !cwd.is_empty() {
        query.push(("dir", cwd.clone()));
    }
    for (key, value) in &command.env {
        query.push(("env", format!("{key}={value}")));
    }

    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/v1/sprites/{sprite_name}/exec")))
        .query(&query)
        .send()
        .await
        .with_context(|| format!("exec in Sprites sprite {sprite_name}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("reading Sprites exec response body")?;
    if !status.is_success() {
        bail!("Sprites exec failed ({status}): {body}");
    }

    let (exit_code, stdout, stderr) = parse_exec_response(&body)?;
    Ok(SandboxCommandOutput {
        ok: exit_code == 0,
        exit_code: Some(exit_code),
        stdout,
        stderr,
        command: command
            .display_argv
            .clone()
            .unwrap_or_else(|| command.argv.clone()),
        cwd,
    })
}

fn parse_exec_response(body: &str) -> Result<(i32, String, String)> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok((0, String::new(), String::new()));
    }

    // Some deployments return a single JSON object; others stream NDJSON or raw stdout.
    if trimmed.starts_with('{') && !trimmed.contains('\n') {
        if let Ok(parsed) = serde_json::from_str::<SpritesHttpExecResponse>(trimmed) {
            return Ok(parsed.into_parts());
        }
    }

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = None;
    let mut parsed_stream_event = false;
    let mut raw_lines = String::new();

    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<SpritesStreamEvent>(line) else {
            raw_lines.push_str(line);
            raw_lines.push('\n');
            continue;
        };
        parsed_stream_event = true;
        match event.event_type.as_str() {
            "stdout" => {
                if let Some(data) = event.data {
                    stdout.push_str(&data);
                }
            }
            "stderr" => {
                if let Some(data) = event.data {
                    stderr.push_str(&data);
                }
            }
            "complete" | "exit" => {
                if let Some(code) = event.exit_code {
                    exit_code = Some(code);
                }
            }
            "error" => {
                let message = event
                    .error
                    .or(event.message)
                    .unwrap_or_else(|| "Sprites exec stream error".into());
                bail!("{message}");
            }
            _ => {}
        }
    }

    if parsed_stream_event {
        return Ok((exit_code.unwrap_or(0), stdout, stderr));
    }

    if !raw_lines.is_empty() {
        return Ok((exit_code.unwrap_or(0), raw_lines, stderr));
    }

    // Plain-text body (common for simple HTTP exec, e.g. `curl ... | python`).
    Ok((exit_code.unwrap_or(0), trimmed.to_string(), stderr))
}

async fn save_checkpoint_via_backend(
    backend: &SpritesBackendHandle,
    sprite_name: &str,
) -> Result<SnapshotPayload> {
    let body = serde_json::json!({ "comment": "exo snapshot" });
    let response = backend
        .client
        .post(backend.api_endpoint(&format!("/v1/sprites/{sprite_name}/checkpoint")))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("creating Sprites checkpoint for {sprite_name}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("reading Sprites checkpoint response")?;
    if !status.is_success() {
        bail!("Sprites checkpoint failed ({status}): {text}");
    }
    let checkpoint_id = parse_checkpoint_id_from_stream(&text)?;
    let manifest = SpritesSnapshotManifest {
        checkpoint_id,
        sprite_name: sprite_name.to_string(),
    };
    let bytes =
        serde_json::to_vec(&manifest).context("serializing Sprites snapshot manifest")?;
    Ok(SnapshotPayload {
        kind: SnapshotKind::SpritesSnapshot,
        bytes: Bytes::from(bytes),
    })
}

async fn restore_checkpoint_via_backend(
    backend: &SpritesBackendHandle,
    sprite_name: &str,
    checkpoint_id: &str,
) -> Result<()> {
    let response = backend
        .client
        .post(backend.api_endpoint(&format!(
            "/v1/sprites/{sprite_name}/checkpoints/{checkpoint_id}/restore"
        )))
        .send()
        .await
        .with_context(|| {
            format!("restoring Sprites checkpoint {checkpoint_id} on {sprite_name}")
        })?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("reading Sprites restore response")?;
    if !status.is_success() {
        bail!("Sprites checkpoint restore failed ({status}): {text}");
    }
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: SpritesStreamEvent =
            serde_json::from_str(line).context("decoding Sprites restore NDJSON line")?;
        if event.event_type == "error" {
            let message = event
                .error
                .or(event.message)
                .unwrap_or_else(|| "Sprites restore stream error".into());
            bail!("{message}");
        }
    }
    Ok(())
}

fn parse_checkpoint_id_from_stream(body: &str) -> Result<String> {
    let mut checkpoint_id = None;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: SpritesStreamEvent =
            serde_json::from_str(line).context("decoding Sprites checkpoint NDJSON line")?;
        if event.event_type == "error" {
            let message = event
                .error
                .or(event.message)
                .unwrap_or_else(|| "Sprites checkpoint stream error".into());
            bail!("{message}");
        }
        if let Some(data) = event.data {
            if let Some(id) = data.strip_prefix("  ID: ") {
                checkpoint_id = Some(id.trim().to_string());
            }
            if let Some(rest) = data.strip_prefix("Checkpoint ") {
                if let Some(id) = rest.split_whitespace().next() {
                    checkpoint_id = Some(id.to_string());
                }
            }
        }
    }
    checkpoint_id.ok_or_else(|| anyhow!("Sprites checkpoint stream did not report a checkpoint id"))
}

/// Deterministic sprite name for a sandbox key + spec. Sprites names must be unique
/// per organization; hashing the exo key and spec hash gives stable cross-process resume.
fn sprite_name_for_request(request: &SandboxRequest) -> String {
    let spec_hash = sandbox_spec_hash(&request.spec);
    let mut hasher = DefaultHasher::new();
    request.key.hash(&mut hasher);
    spec_hash.hash(&mut hasher);
    format!("exo-{:016x}", hasher.finish())
}

fn reject_host_mounts(request: &SandboxRequest) -> Result<()> {
    if request.spec.mounts.is_empty() {
        return Ok(());
    }
    bail!(
        "Sprites sandbox backend does not support host bind-mounts; \
         remove conversation mounts or switch to --sandbox-backend docker. \
         A remote-workspace provisioner is planned as a follow-up."
    )
}

fn validate_url_auth(url_auth: &str) -> Result<()> {
    match url_auth {
        "sprite" | "public" => Ok(()),
        other => bail!("Sprites url_auth must be `sprite` or `public`, got {other:?}"),
    }
}

fn sprite_label(key: &str, value: &str) -> String {
    format!("{key}={value}")
}

fn sprite_labels_for_request(
    request: &SandboxRequest,
    spec_hash: &str,
    extra_labels: &[String],
) -> Vec<String> {
    let mut labels = Vec::with_capacity(extra_labels.len() + 2);
    labels.push(sprite_label(
        WARM_SANDBOX_KEY_LABEL,
        &request.key.to_string(),
    ));
    labels.push(sprite_label(
        WARM_SANDBOX_SPEC_HASH_LABEL,
        spec_hash,
    ));
    labels.extend(extra_labels.iter().cloned());
    labels
}

#[derive(Debug, Serialize)]
struct SpritesUrlSettings {
    auth: String,
}

#[derive(Debug, Serialize)]
struct SpritesCreateRequest {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    organization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url_settings: Option<SpritesUrlSettings>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SpritesHttpExecResponse {
    #[serde(default, alias = "exitCode")]
    exit_code: Option<i32>,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    output: String,
}

impl SpritesHttpExecResponse {
    fn into_parts(self) -> (i32, String, String) {
        let exit_code = self.exit_code.unwrap_or(0);
        let stdout = if self.stdout.is_empty() {
            self.output
        } else {
            self.stdout
        };
        (exit_code, stdout, self.stderr)
    }
}

#[derive(Debug, Deserialize)]
struct SpritesStreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, alias = "exitCode")]
    exit_code: Option<i32>,
}
