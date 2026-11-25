//! boxd remote sandbox backend.
//!
//! Each exo sandbox is one isolated [boxd](https://boxd.sh?ref=exo) machine.
//! The backend talks to the boxd public gRPC API (`boxd.api.v1.BoxdApi`,
//! documented at <https://docs.boxd.sh/reference/grpc-api>): lifecycle calls
//! are unary RPCs, and commands run over the bidirectional `Exec` stream, so
//! `start_process` streams stdout and stderr while stdin stays open.
//!
//! Cross-process resume finds the machine by its exo labels (same role as
//! Docker labels / E2B metadata); the machine name is derived from the sandbox
//! ID + spec hash so a create retried after a lost response reuses the first
//! machine. `stop` hibernates the machine (memory, processes and disk are
//! kept) and the next `acquire` wakes it. Snapshots are bytes-by-reference via
//! [`SnapshotFormat::BoxdRef`] manifests naming a boxd snapshot.
//!
//! An API key is exchanged for a short-lived session token, which is cached
//! and renewed before it expires.
//!
//! `boxd/api.rs` is generated from `boxd/api.proto`, a subset of the published
//! proto at <https://github.com/boxd-sh/sdk/blob/main/proto/api.proto>, with
//! `tonic-prost-build` 0.14.0 and then `cargo fmt`:
//!
//! ```text
//! tonic_prost_build::configure()
//!     .build_transport(false)
//!     .server_mod_attribute(".", "#[cfg(test)]")
//!     .out_dir(out)
//!     .compile_protos(&["api.proto"], &["."])
//! ```

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Status};

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxRequest, SnapshotFormat, SnapshotPayload, WARM_SANDBOX_KEY_LABEL,
    WARM_SANDBOX_SPEC_HASH_LABEL, sandbox_spec_hash,
};
use crate::{SandboxAttachment, SandboxResult};

#[allow(clippy::all, clippy::pedantic)]
mod api;
#[cfg(test)]
mod tests;

use api::boxd_api_client::BoxdApiClient;

pub const DEFAULT_BOXD_API_URL: &str = "https://boxd.sh:9443";
pub const DEFAULT_BOXD_TOKEN_URL: &str = "https://app.boxd.sh/api/v1/auth/token";
pub const DEFAULT_BOXD_NAME_PREFIX: &str = "exo";

const PROCESS_PIPE_BUFFER_SIZE: usize = 64 * 1024;
const STDIN_CHANNEL_CAPACITY: usize = 16;
/// Renew the session token when less than this much of its lifetime is left.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
/// How long a machine may take to become usable after create, wake or start.
const READY_TIMEOUT: Duration = Duration::from_secs(180);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How long a new snapshot may take to become restorable.
const SNAPSHOT_READY_TIMEOUT: Duration = Duration::from_secs(300);
const SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Bound on connecting and on each lifecycle call. `Exec` streams have none:
/// commands run as long as they need, bounded by their own timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound on one readiness probe, so a stream that never answers can't stall acquire.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);
/// boxd machine sizes as (vCPU, GiB of memory), smallest first.
const MACHINE_SIZES: [(u8, u32); 4] = [(1, 4), (2, 8), (4, 16), (8, 32)];
static CONSUMABLE_SNAPSHOT_FORMATS: [SnapshotFormat; 1] = [SnapshotFormat::BoxdRef];

#[derive(Clone)]
pub struct BoxdConfig {
    pub api_key: String,
    /// gRPC endpoint. `https://nyc.boxd.sh:9443` for machines in New York.
    pub api_url: String,
    /// Where the API key is exchanged for a session token.
    pub token_url: String,
    /// Org (name or id) the machines live in. `None` leaves it to the API key's default.
    pub org: Option<String>,
    /// Machine names are `<prefix>-<hash>`.
    pub name_prefix: String,
    /// Binding-level `key=value` labels; exo resume labels are always merged in on create.
    pub extra_labels: Vec<String>,
}

impl std::fmt::Debug for BoxdConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxdConfig")
            .field("api_key", &"<redacted>")
            .field("api_url", &self.api_url)
            .field("token_url", &self.token_url)
            .field("org", &self.org)
            .field("name_prefix", &self.name_prefix)
            .field("extra_labels", &self.extra_labels)
            .finish()
    }
}

/// JSON persisted for [`SnapshotFormat::BoxdRef`]. Memory and disk live in the
/// boxd snapshot; we only store its id.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BoxdSnapshotManifest {
    snapshot_id: String,
    snapshot_name: String,
}

pub struct BoxdSandboxBackend {
    client: BoxdClient,
    org: Option<String>,
    name_prefix: String,
    extra_labels: HashMap<String, String>,
}

impl BoxdSandboxBackend {
    pub fn new(config: BoxdConfig) -> Result<Self> {
        Self::with_timeouts(config, CALL_TIMEOUT, PROBE_TIMEOUT)
    }

    fn with_timeouts(
        config: BoxdConfig,
        call_timeout: Duration,
        probe_timeout: Duration,
    ) -> Result<Self> {
        ensure!(
            !config.api_key.is_empty(),
            "the boxd API key (BOXD_API_KEY by default) is empty"
        );
        ensure!(
            !config.name_prefix.is_empty(),
            "boxd name_prefix must not be empty"
        );
        let extra_labels = parse_labels(&config.extra_labels)?;
        let client = BoxdClient::new(&config, call_timeout, probe_timeout)?;
        Ok(Self {
            client,
            org: config.org.filter(|org| !org.is_empty()),
            name_prefix: config.name_prefix,
            extra_labels,
        })
    }

    fn org(&self) -> String {
        self.org.clone().unwrap_or_default()
    }

    fn labels_for_request(&self, request: &SandboxRequest) -> HashMap<String, String> {
        let mut labels = self.extra_labels.clone();
        labels.insert(
            WARM_SANDBOX_KEY_LABEL.to_string(),
            request.sandbox_id.clone(),
        );
        labels.insert(
            WARM_SANDBOX_SPEC_HASH_LABEL.to_string(),
            sandbox_spec_hash(&request.spec),
        );
        labels
    }

    /// The machine holding this sandbox, found by its exo labels.
    async fn find_machine(&self, request: &SandboxRequest) -> Result<Option<api::GetVmResponse>> {
        let selector = vec![
            format!("{WARM_SANDBOX_KEY_LABEL}={}", request.sandbox_id),
            format!(
                "{WARM_SANDBOX_SPEC_HASH_LABEL}={}",
                sandbox_spec_hash(&request.spec)
            ),
        ];
        let response = self
            .client
            .api()
            .list_vms(
                self.client
                    .request(api::ListVmsRequest {
                        org: self.org(),
                        label_selector: selector,
                    })
                    .await?,
            )
            .await
            .map_err(|status| status_error("list machines", status))?;
        let spec_hash = sandbox_spec_hash(&request.spec);
        Ok(response.into_inner().vms.into_iter().find(|vm| {
            has_label(vm, WARM_SANDBOX_KEY_LABEL, &request.sandbox_id)
                && has_label(vm, WARM_SANDBOX_SPEC_HASH_LABEL, &spec_hash)
        }))
    }

    /// Every machine carrying this sandbox key, whatever its spec hash.
    async fn machines_for_sandbox(&self, sandbox_id: &str) -> Result<Vec<api::GetVmResponse>> {
        let response = self
            .client
            .api()
            .list_vms(
                self.client
                    .request(api::ListVmsRequest {
                        org: self.org(),
                        label_selector: vec![format!("{WARM_SANDBOX_KEY_LABEL}={sandbox_id}")],
                    })
                    .await?,
            )
            .await
            .map_err(|status| status_error("list machines", status))?;
        // Check the labels again here: only this sandbox's machines may be destroyed.
        Ok(response
            .into_inner()
            .vms
            .into_iter()
            .filter(|vm| has_label(vm, WARM_SANDBOX_KEY_LABEL, sandbox_id))
            .collect())
    }

    async fn create_machine(&self, request: &SandboxRequest) -> Result<String> {
        let name = machine_name_for_request(&self.name_prefix, request);
        let config = machine_config(request)?;
        let result = self
            .client
            .api()
            .create_vm(
                self.client
                    .request(api::CreateVmRequest {
                        name: name.clone(),
                        config,
                        org: self.org(),
                        isolated: true,
                        labels: self.labels_for_request(request),
                    })
                    .await?,
            )
            .await;
        match result {
            Ok(response) => Ok(response.into_inner().vm_id),
            // A retried create whose first response was lost: adopt the machine
            // it made, which carries the same deterministic name and labels.
            Err(status) if status.code() == Code::AlreadyExists => {
                let machine =
                    self.client.get_vm(&name).await?.with_context(|| {
                        format!("boxd machine {name} exists but cannot be read")
                    })?;
                ensure!(
                    machine.labels.get(WARM_SANDBOX_KEY_LABEL) == Some(&request.sandbox_id),
                    "boxd machine name {name} is taken by another machine"
                );
                Ok(machine.vm_id)
            }
            Err(status) => Err(status_error("create machine", status)),
        }
    }

    fn handle(&self, id_prefix: &str, request: SandboxRequest, vm_id: String) -> BoxdSandboxHandle {
        BoxdSandboxHandle {
            id: format!("{id_prefix}:{}", request.sandbox_id.as_str()),
            vm_id,
            request,
            client: self.client.clone(),
            org: self.org(),
        }
    }

    async fn restore_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        request.policy.validate_basic("boxd")?;
        reject_unsupported_request(&request)?;
        if payload.format != SnapshotFormat::BoxdRef {
            bail!(
                "boxd sandbox backend can only restore from {}, got {}",
                SnapshotFormat::BoxdRef,
                payload.format
            );
        }
        let manifest: BoxdSnapshotManifest =
            serde_json::from_slice(&payload.bytes).context("decoding boxd snapshot manifest")?;
        if let Some(existing) = self.find_machine(&request).await? {
            bail!(
                "boxd sandbox {} already has machine {}; terminate it before restoring a snapshot",
                request.sandbox_id,
                existing.name
            );
        }
        let response = self
            .client
            .api()
            .create_vm_from_snapshot(
                self.client
                    .request(api::CreateVmFromSnapshotRequest {
                        snapshot: manifest.snapshot_id.clone(),
                        name: machine_name_for_request(&self.name_prefix, &request),
                        org: self.org(),
                        isolated: true,
                        labels: self.labels_for_request(&request),
                    })
                    .await?,
            )
            .await
            .map_err(|status| {
                status_error(
                    &format!("restore snapshot {}", manifest.snapshot_name),
                    status,
                )
            })?;
        let vm_id = response.into_inner().vm_id;
        self.client.wait_until_ready(&vm_id).await?;
        Ok(Arc::new(self.handle("boxd-restored", request, vm_id)))
    }
}

#[async_trait]
impl ManagedSandboxBackend for BoxdSandboxBackend {
    fn capabilities(&self) -> crate::SandboxBackendCapabilities {
        crate::SandboxBackendCapabilities::default()
    }

    fn is_local(&self) -> bool {
        false
    }

    /// A stopped sandbox is a hibernated machine; deletion destroys it.
    fn retains_disk_when_stopped(&self) -> bool {
        true
    }

    /// Machines are found by their labels, so stopping needs no saved state.
    fn stop_requires_provider_state(&self) -> bool {
        false
    }

    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &CONSUMABLE_SNAPSHOT_FORMATS
    }

    async fn acquire(
        &self,
        request: SandboxRequest,
    ) -> SandboxResult<Arc<dyn ManagedSandboxHandle>> {
        request.policy.validate_basic("boxd")?;
        reject_unsupported_request(&request)?;
        let vm_id = match self.find_machine(&request).await? {
            Some(machine) => machine.vm_id,
            None => self.create_machine(&request).await?,
        };
        self.client.wait_until_ready(&vm_id).await?;
        let handle = self.handle("boxd", request, vm_id);
        handle.ensure_workdir().await?;
        Ok(Arc::new(handle))
    }

    async fn stop(&self, request: SandboxRequest) -> Result<()> {
        match self.find_machine(&request).await? {
            Some(machine) => self.client.hibernate(&machine.vm_id).await,
            None => Ok(()),
        }
    }

    async fn terminate(&self, request: SandboxRequest) -> Result<()> {
        self.terminate_existing(&request.sandbox_id, Some(&request))
            .await
    }

    async fn terminate_existing(
        &self,
        sandbox_id: &str,
        _request: Option<&SandboxRequest>,
    ) -> Result<()> {
        for machine in self.machines_for_sandbox(sandbox_id).await? {
            self.client.destroy(&machine.vm_id).await?;
        }
        Ok(())
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("boxd sandbox backend does not support external attachments")
    }

    async fn suspend(
        &self,
        _request: SandboxRequest,
        _kind: crate::SnapshotKind,
    ) -> SandboxResult<SnapshotPayload> {
        Err(crate::SandboxFailure::InvalidRequest {
            message: "boxd sandbox backend does not support suspension".into(),
        }
        .into())
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> SandboxResult<Arc<dyn ManagedSandboxHandle>> {
        self.restore_snapshot(request, payload)
            .await
            .map_err(crate::SandboxError::from)
    }

    async fn delete_snapshot(&self, payload: SnapshotPayload) -> Result<()> {
        delete_snapshot(&self.client, &self.org(), payload).await
    }

    async fn fork_sandbox(
        &self,
        source: SandboxRequest,
        target: SandboxRequest,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        source.policy.validate_basic("boxd fork source")?;
        target.policy.validate_basic("boxd fork target")?;
        reject_unsupported_request(&target)?;
        ensure!(
            source.sandbox_id != target.sandbox_id,
            "boxd fork source and target must be different sandboxes"
        );
        let source_machine = self.find_machine(&source).await?.with_context(|| {
            format!("boxd sandbox {} has no machine to fork", source.sandbox_id)
        })?;
        if let Some(existing) = self.find_machine(&target).await? {
            bail!(
                "boxd sandbox {} already has machine {}",
                target.sandbox_id,
                existing.name
            );
        }
        self.client.wait_until_ready(&source_machine.vm_id).await?;
        let response = self
            .client
            .api()
            .fork_vm(
                self.client
                    .request(api::ForkVmRequest {
                        source_vm_id: source_machine.vm_id.clone(),
                        name: machine_name_for_request(&self.name_prefix, &target),
                        isolated: true,
                        labels: self.labels_for_request(&target),
                    })
                    .await?,
            )
            .await
            .map_err(|status| status_error("fork machine", status))?;
        let vm_id = response.into_inner().vm_id;
        self.client.wait_until_ready(&vm_id).await?;
        let handle = self.handle("boxd", target, vm_id);
        handle.ensure_workdir().await?;
        Ok(Arc::new(handle))
    }
}

/// gRPC channel plus the cached session token, shared by the backend and its handles.
#[derive(Clone)]
struct BoxdClient {
    /// Lifecycle calls, each bounded by `CALL_TIMEOUT`.
    channel: Channel,
    /// `Exec` streams, which last as long as the command.
    exec_channel: Channel,
    probe_timeout: Duration,
    http: reqwest::Client,
    token_url: String,
    api_key: Arc<str>,
    token: Arc<Mutex<Option<SessionToken>>>,
}

#[derive(Clone)]
struct SessionToken {
    value: String,
    renew_at: Instant,
}

#[derive(Serialize)]
struct TokenExchangeRequest<'a> {
    api_key: &'a str,
}

#[derive(Deserialize)]
struct TokenExchangeResponse {
    token: String,
    /// Unix seconds.
    expires_at: u64,
}

impl BoxdClient {
    fn new(config: &BoxdConfig, call_timeout: Duration, probe_timeout: Duration) -> Result<Self> {
        let api_url = config.api_url.trim_end_matches('/').to_string();
        let mut endpoint = Endpoint::from_shared(api_url.clone())
            .with_context(|| format!("invalid boxd api_url {api_url:?}"))?
            .connect_timeout(CONNECT_TIMEOUT)
            .http2_keep_alive_interval(HTTP2_KEEPALIVE_INTERVAL)
            .keep_alive_timeout(HTTP2_KEEPALIVE_TIMEOUT)
            .keep_alive_while_idle(true);
        if api_url.starts_with("https://") {
            endpoint = endpoint
                .tls_config(ClientTlsConfig::new().with_webpki_roots())
                .context("configuring TLS for the boxd API")?;
        } else if !api_url.starts_with("http://") {
            bail!("boxd api_url must be an http(s) URL, got {api_url}");
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(call_timeout)
            .build()
            .context("building boxd HTTP client")?;
        Ok(Self {
            exec_channel: endpoint.connect_lazy(),
            channel: endpoint.timeout(call_timeout).connect_lazy(),
            probe_timeout,
            http,
            token_url: config.token_url.clone(),
            api_key: Arc::from(config.api_key.as_str()),
            token: Arc::new(Mutex::new(None)),
        })
    }

    fn api(&self) -> BoxdApiClient<Channel> {
        BoxdApiClient::new(self.channel.clone())
    }

    async fn session_token(&self) -> Result<String> {
        if let Some(token) = self
            .token
            .lock()
            .map_err(|_| anyhow!("boxd token cache lock poisoned"))?
            .as_ref()
            .filter(|token| Instant::now() < token.renew_at)
        {
            return Ok(token.value.clone());
        }
        let response = self
            .http
            .post(&self.token_url)
            .json(&TokenExchangeRequest {
                api_key: &self.api_key,
            })
            .send()
            .await
            .context("exchanging the boxd API key for a session token")?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            bail!(
                "boxd rejected the API key ({status}); check the boxd secret (BOXD_API_KEY by default)"
            );
        }
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("boxd token exchange failed ({status}): {text}");
        }
        let exchanged: TokenExchangeResponse = response
            .json()
            .await
            .context("decoding boxd token exchange response")?;
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs();
        let lifetime = Duration::from_secs(exchanged.expires_at.saturating_sub(now_unix));
        let token = SessionToken {
            value: exchanged.token,
            renew_at: Instant::now() + lifetime.saturating_sub(TOKEN_REFRESH_MARGIN),
        };
        let value = token.value.clone();
        *self
            .token
            .lock()
            .map_err(|_| anyhow!("boxd token cache lock poisoned"))? = Some(token);
        Ok(value)
    }

    async fn request<T>(&self, message: T) -> Result<tonic::Request<T>> {
        let token = self.session_token().await?;
        let mut request = tonic::Request::new(message);
        let mut value: MetadataValue<_> = format!("Bearer {token}")
            .parse()
            .context("boxd session token is not a valid header value")?;
        value.set_sensitive(true);
        request.metadata_mut().insert("authorization", value);
        Ok(request)
    }

    /// `None` when no machine has this id or name.
    async fn get_vm(&self, vm_id: &str) -> Result<Option<api::GetVmResponse>> {
        let result = self
            .api()
            .get_vm(
                self.request(api::GetVmRequest {
                    vm_id: vm_id.to_string(),
                })
                .await?,
            )
            .await;
        match result {
            Ok(response) => Ok(Some(response.into_inner())),
            Err(status) if status.code() == Code::NotFound => Ok(None),
            Err(status) => Err(status_error(&format!("get machine {vm_id}"), status)),
        }
    }

    async fn hibernate(&self, vm_id: &str) -> Result<()> {
        let result = self
            .api()
            .hibernate_vm(
                self.request(api::HibernateVmRequest {
                    vm_id: vm_id.to_string(),
                })
                .await?,
            )
            .await;
        match result {
            Ok(_) => {}
            Err(status) if status.code() == Code::FailedPrecondition => {
                // Already hibernated or stopped, or a concurrent stop is
                // still writing memory out (`suspended`): wait for it below.
                match self.get_vm(vm_id).await?.map(|vm| vm.status) {
                    None => return Ok(()),
                    Some(current) if is_stopped(&current) || current == "suspended" => {}
                    Some(_) => {
                        return Err(status_error(&format!("hibernate machine {vm_id}"), status));
                    }
                }
            }
            Err(status) if status.code() == Code::NotFound => return Ok(()),
            Err(status) => return Err(status_error(&format!("hibernate machine {vm_id}"), status)),
        }
        // The call returns while memory is still being written out; wait for
        // it so a stop is finished when it returns.
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let Some(machine) = self.get_vm(vm_id).await? else {
                return Ok(());
            };
            if is_stopped(&machine.status) {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "boxd machine {vm_id} did not hibernate within {}s (status {})",
                READY_TIMEOUT.as_secs(),
                machine.status
            );
            tokio::time::sleep(READY_POLL_INTERVAL).await;
        }
    }

    /// Idempotent: a machine that is already gone counts as destroyed.
    async fn destroy(&self, vm_id: &str) -> Result<()> {
        let result = self
            .api()
            .destroy_vm(
                self.request(api::DestroyVmRequest {
                    vm_id: vm_id.to_string(),
                })
                .await?,
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(status) if status.code() == Code::NotFound => Ok(()),
            Err(status) => Err(status_error(&format!("destroy machine {vm_id}"), status)),
        }
    }

    /// Bring the machine to `running` (waking, resuming or starting it) and
    /// wait until a command runs in it.
    async fn wait_until_ready(&self, vm_id: &str) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut requested = None::<&str>;
        loop {
            let machine = self
                .get_vm(vm_id)
                .await?
                .with_context(|| format!("boxd machine {vm_id} no longer exists"))?;
            let action = match machine.status.as_str() {
                "running" => break,
                "hibernated" => Some("wake"),
                "suspended" => Some("resume"),
                "stopped" => Some("start"),
                "failed" | "destroyed" => {
                    bail!(
                        "boxd machine {vm_id} is {} and cannot be used",
                        machine.status
                    )
                }
                _ => None,
            };
            if let Some(action) = action
                && requested != Some(action)
            {
                self.change_power_state(vm_id, action).await?;
                requested = Some(action);
            }
            ensure!(
                Instant::now() < deadline,
                "boxd machine {vm_id} did not reach running within {}s (status {})",
                READY_TIMEOUT.as_secs(),
                machine.status
            );
            tokio::time::sleep(READY_POLL_INTERVAL).await;
        }
        loop {
            let probe = tokio::time::timeout(
                self.probe_timeout
                    .min(deadline.saturating_duration_since(Instant::now())),
                collect_exec(self, vm_id, "true".to_string()),
            )
            .await
            .unwrap_or_else(|_| Err(anyhow!("readiness probe timed out")));
            match probe {
                Ok(output) if output.exit_code == 0 => return Ok(()),
                Ok(output) => bail!(
                    "boxd machine {vm_id} readiness probe exited with {}",
                    output.exit_code
                ),
                Err(error) if Instant::now() < deadline => {
                    tracing::debug!(%vm_id, error = %format!("{error:#}"), "boxd machine not ready yet");
                    tokio::time::sleep(READY_POLL_INTERVAL).await;
                }
                Err(error) => {
                    return Err(error.context(format!(
                        "boxd machine {vm_id} is running but did not run a command within {}s",
                        READY_TIMEOUT.as_secs()
                    )));
                }
            }
        }
    }

    async fn change_power_state(&self, vm_id: &str, action: &str) -> Result<()> {
        let mut api = self.api();
        let vm_id_owned = vm_id.to_string();
        let result = match action {
            "wake" => api
                .wake_vm(
                    self.request(api::WakeVmRequest { vm_id: vm_id_owned })
                        .await?,
                )
                .await
                .map(|_| ()),
            "resume" => api
                .resume_vm(
                    self.request(api::ResumeVmRequest { vm_id: vm_id_owned })
                        .await?,
                )
                .await
                .map(|_| ()),
            "start" => api
                .start_vm(
                    self.request(api::StartVmRequest { vm_id: vm_id_owned })
                        .await?,
                )
                .await
                .map(|_| ()),
            other => bail!("unknown boxd power action {other}"),
        };
        match result {
            Ok(()) => Ok(()),
            // The machine moved on by itself (for example woke on its own); the
            // status poll that follows decides what to do next.
            Err(status) if status.code() == Code::FailedPrecondition => Ok(()),
            Err(status) => Err(status_error(&format!("{action} machine {vm_id}"), status)),
        }
    }

    /// Open an `Exec` stream. `stdin` carries the chunks after the first; the
    /// remote process sees stdin EOF when it ends.
    async fn open_exec(
        &self,
        vm_id: &str,
        command_line: String,
        stdin: mpsc::Receiver<api::ExecChunk>,
    ) -> Result<tonic::Streaming<api::ExecChunk>> {
        let first = api::ExecChunk {
            vm_id: vm_id.to_string(),
            command: command_line,
            ..Default::default()
        };
        let requests =
            futures::stream::once(async move { first }).chain(ReceiverStream::new(stdin));
        let response = BoxdApiClient::new(self.exec_channel.clone())
            .exec(self.request(requests).await?)
            .await
            .map_err(|status| status_error(&format!("exec in machine {vm_id}"), status))?;
        Ok(response.into_inner())
    }
}

struct ExecResult {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run one command with stdin closed and collect its output.
async fn collect_exec(
    client: &BoxdClient,
    vm_id: &str,
    command_line: String,
) -> Result<ExecResult> {
    let (stdin_tx, stdin_rx) = mpsc::channel(1);
    drop(stdin_tx);
    let mut stream = client.open_exec(vm_id, command_line, stdin_rx).await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|status| status_error(&format!("exec in machine {vm_id}"), status))?;
        exit_code = final_exit_code(&chunk);
        if chunk.is_stderr {
            stderr.extend_from_slice(&chunk.data);
        } else {
            stdout.extend_from_slice(&chunk.data);
        }
    }
    Ok(ExecResult {
        exit_code: exit_code.with_context(|| missing_exit_code(vm_id))?,
        stdout,
        stderr,
    })
}

/// The exit code a chunk reports when it is the stream's last: the final chunk
/// carries the exit code and no output.
fn final_exit_code(chunk: &api::ExecChunk) -> Option<i32> {
    chunk.data.is_empty().then_some(chunk.exit_code)
}

fn missing_exit_code(vm_id: &str) -> String {
    format!("boxd exec stream in machine {vm_id} ended without an exit code")
}

struct BoxdSandboxHandle {
    id: String,
    vm_id: String,
    request: SandboxRequest,
    client: BoxdClient,
    org: String,
}

impl BoxdSandboxHandle {
    fn cwd(&self, command: &SandboxCommand) -> String {
        command
            .cwd
            .clone()
            .unwrap_or_else(|| self.request.spec.default_workdir.clone())
    }

    /// Create the default workdir, which boxd's base image doesn't have.
    async fn ensure_workdir(&self) -> Result<()> {
        let workdir = &self.request.spec.default_workdir;
        if workdir.is_empty() {
            return Ok(());
        }
        let dir = super::shell_quote(workdir);
        let script = format!(
            "mkdir -p {dir} 2>/dev/null || {{ sudo -n mkdir -p {dir} && sudo -n chown \"$(id -u):$(id -g)\" {dir}; }}"
        );
        let output = collect_exec(&self.client, &self.vm_id, script).await?;
        ensure!(
            output.exit_code == 0,
            "creating workdir {workdir} in boxd machine {} failed: {}",
            self.vm_id,
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
}

#[async_trait]
impl ManagedSandboxHandle for BoxdSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    async fn is_running(&self) -> Result<Option<bool>> {
        Ok(Some(
            self.client
                .get_vm(&self.vm_id)
                .await?
                .is_some_and(|vm| vm.status == "running"),
        ))
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        let cwd = self.cwd(command);
        let line = command_line(command, &cwd)?;
        let output = match command.timeout {
            // Dropping the stream cancels the call, which stops the remote command.
            Some(timeout) => {
                tokio::time::timeout(timeout, collect_exec(&self.client, &self.vm_id, line))
                    .await
                    .map_err(|_| {
                        anyhow!(
                            "sandbox command timed out after {}s: {}",
                            timeout.as_secs(),
                            command
                                .display_argv
                                .as_ref()
                                .unwrap_or(&command.argv)
                                .join(" ")
                        )
                    })??
            }
            None => collect_exec(&self.client, &self.vm_id, line).await?,
        };
        Ok(SandboxCommandOutput {
            ok: output.exit_code == 0,
            exit_code: Some(output.exit_code),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            command: command
                .display_argv
                .clone()
                .unwrap_or_else(|| command.argv.clone()),
            cwd,
        })
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        let line = command_line(command, &self.cwd(command))?;
        let (stdin_tx, stdin_rx) = mpsc::channel(STDIN_CHANNEL_CAPACITY);
        let stream = self.client.open_exec(&self.vm_id, line, stdin_rx).await?;

        let (stdout_reader, stdout_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
        let (stderr_reader, stderr_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
        let (stdin_reader, stdin_writer) = tokio::io::duplex(PROCESS_PIPE_BUFFER_SIZE);
        let (wait_tx, wait_rx) = oneshot::channel();

        tokio::spawn(forward_stdin(stdin_reader, stdin_tx));
        let vm_id = self.vm_id.clone();
        tokio::spawn(async move {
            let result = pump_exec_output(stream, stdout_writer, stderr_writer, &vm_id).await;
            if wait_tx.send(result).is_err() {
                tracing::debug!(%vm_id, "boxd process waiter dropped before completion");
            }
        });

        let wait: BoxFuture<'static, crate::Result<i32>> = Box::pin(async move {
            match wait_rx.await {
                Ok(result) => result,
                Err(_) => Err(anyhow!("boxd exec stream stopped before reporting exit")),
            }
        });

        Ok(crate::SandboxProcessParts {
            control: None,
            stdout: Box::pin(stdout_reader.compat()),
            stderr: Box::pin(stderr_reader.compat()),
            stdin: Box::pin(stdin_writer.compat_write()),
            wait,
        })
    }

    async fn stop(&self) -> Result<()> {
        self.client.hibernate(&self.vm_id).await
    }

    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("boxd sandboxes cannot be detached")
    }

    async fn snapshot(&self, kind: crate::SnapshotKind) -> SandboxResult<SnapshotPayload> {
        save_snapshot(&self.client, &self.org, &self.vm_id, &self.request, kind)
            .await
            .map_err(crate::SandboxError::from)
    }

    async fn delete_snapshot(&self, payload: SnapshotPayload) -> Result<()> {
        delete_snapshot(&self.client, &self.org, payload).await
    }
}

/// Copy the process's stdin pipe into `Exec` chunks; ending the channel is EOF.
async fn forward_stdin(
    mut stdin_reader: tokio::io::DuplexStream,
    stdin_tx: mpsc::Sender<api::ExecChunk>,
) {
    let mut buffer = vec![0u8; PROCESS_PIPE_BUFFER_SIZE];
    loop {
        match stdin_reader.read(&mut buffer).await {
            Ok(0) => return,
            Ok(read) => {
                let chunk = api::ExecChunk {
                    data: buffer[..read].to_vec(),
                    stdin: true,
                    ..Default::default()
                };
                if stdin_tx.send(chunk).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                tracing::debug!(error = %error, "reading boxd process stdin pipe failed");
                return;
            }
        }
    }
}

async fn pump_exec_output(
    mut stream: tonic::Streaming<api::ExecChunk>,
    mut stdout_writer: tokio::io::DuplexStream,
    mut stderr_writer: tokio::io::DuplexStream,
    vm_id: &str,
) -> crate::Result<i32> {
    let mut exit_code = None;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|status| status_error(&format!("exec in machine {vm_id}"), status))?;
        if !chunk.data.is_empty() {
            let writer = if chunk.is_stderr {
                &mut stderr_writer
            } else {
                &mut stdout_writer
            };
            writer
                .write_all(&chunk.data)
                .await
                .context("writing boxd process output pipe")?;
        }
        exit_code = final_exit_code(&chunk);
    }
    exit_code.with_context(|| missing_exit_code(vm_id))
}

async fn save_snapshot(
    client: &BoxdClient,
    org: &str,
    vm_id: &str,
    request: &SandboxRequest,
    kind: crate::SnapshotKind,
) -> Result<SnapshotPayload> {
    // A boxd snapshot always holds memory and running processes as well as
    // the disk, so it only stands in for a full capture.
    ensure!(
        kind == crate::SnapshotKind::Full,
        "boxd does not support the requested snapshot kind"
    );
    let name = format!(
        "exo-{:016x}-{}",
        sandbox_hash(request),
        uuid::Uuid::new_v4().simple()
    );
    let created = client
        .api()
        .create_snapshot(
            client
                .request(api::CreateSnapshotRequest {
                    vm: vm_id.to_string(),
                    name: name.clone(),
                })
                .await?,
        )
        .await
        .map_err(|status| status_error(&format!("snapshot machine {vm_id}"), status))?
        .into_inner();
    if let Err(error) = wait_for_snapshot(client, org, &created.snapshot_id).await {
        // Nobody will hold this snapshot's id, so remove it rather than leak it.
        if let Err(cleanup) = delete_snapshot_by_id(client, org, &created.snapshot_id).await {
            tracing::warn!(
                snapshot_id = %created.snapshot_id,
                error = %format!("{cleanup:#}"),
                "deleting an unfinished boxd snapshot failed"
            );
        }
        return Err(error);
    }
    let manifest = BoxdSnapshotManifest {
        snapshot_id: created.snapshot_id,
        snapshot_name: name,
    };
    let bytes = serde_json::to_vec(&manifest).context("serializing boxd snapshot manifest")?;
    Ok(SnapshotPayload {
        format: SnapshotFormat::BoxdRef,
        bytes: Bytes::from(bytes),
    })
}

async fn wait_for_snapshot(client: &BoxdClient, org: &str, snapshot_id: &str) -> Result<()> {
    let deadline = Instant::now() + SNAPSHOT_READY_TIMEOUT;
    loop {
        let info = client
            .api()
            .get_snapshot(
                client
                    .request(api::GetSnapshotRequest {
                        name: snapshot_id.to_string(),
                        org: org.to_string(),
                    })
                    .await?,
            )
            .await
            .map_err(|status| status_error(&format!("get snapshot {snapshot_id}"), status))?
            .into_inner()
            .snapshot
            .with_context(|| format!("boxd returned no details for snapshot {snapshot_id}"))?;
        match info.status.as_str() {
            "ready" => return Ok(()),
            "failed" => bail!(
                "boxd snapshot {snapshot_id} failed: {}",
                info.failure_reason
            ),
            _ => {}
        }
        ensure!(
            Instant::now() < deadline,
            "boxd snapshot {snapshot_id} was not ready within {}s (status {})",
            SNAPSHOT_READY_TIMEOUT.as_secs(),
            info.status
        );
        tokio::time::sleep(SNAPSHOT_POLL_INTERVAL).await;
    }
}

async fn delete_snapshot(client: &BoxdClient, org: &str, payload: SnapshotPayload) -> Result<()> {
    ensure!(
        payload.format == SnapshotFormat::BoxdRef,
        "boxd sandbox backend can only delete {} snapshots, got {}",
        SnapshotFormat::BoxdRef,
        payload.format
    );
    let manifest: BoxdSnapshotManifest =
        serde_json::from_slice(&payload.bytes).context("decoding boxd snapshot manifest")?;
    delete_snapshot_by_id(client, org, &manifest.snapshot_id).await
}

/// Idempotent: a snapshot that is already gone counts as deleted.
async fn delete_snapshot_by_id(client: &BoxdClient, org: &str, snapshot_id: &str) -> Result<()> {
    let result = client
        .api()
        .delete_snapshot(
            client
                .request(api::DeleteSnapshotRequest {
                    name: snapshot_id.to_string(),
                    org: org.to_string(),
                })
                .await?,
        )
        .await;
    match result {
        Ok(_) => Ok(()),
        Err(status) if status.code() == Code::NotFound => Ok(()),
        Err(status) => Err(status_error(
            &format!("delete snapshot {snapshot_id}"),
            status,
        )),
    }
}

/// The shell line `Exec` runs: `cd <cwd> && NAME=value ... argv...`.
fn command_line(command: &SandboxCommand, cwd: &str) -> Result<String> {
    if command.argv.is_empty() {
        bail!("sandbox command requires at least one argv entry");
    }
    let mut line = String::new();
    if !cwd.is_empty() {
        line.push_str("cd ");
        line.push_str(&super::shell_quote(cwd));
        line.push_str(" && ");
    }
    let mut env: Vec<_> = command.env.iter().collect();
    env.sort();
    for (key, value) in env {
        ensure!(
            is_env_name(key),
            "boxd sandbox env name {key:?} is not a valid shell variable name"
        );
        line.push_str(key);
        line.push('=');
        line.push_str(&super::shell_quote(value));
        line.push(' ');
    }
    let argv: Vec<String> = command
        .argv
        .iter()
        .map(|arg| super::shell_quote(arg))
        .collect();
    line.push_str(&argv.join(" "));
    Ok(line)
}

fn has_label(vm: &api::GetVmResponse, key: &str, value: &str) -> bool {
    vm.labels.get(key).map(String::as_str) == Some(value)
}

fn is_stopped(status: &str) -> bool {
    status == "hibernated" || status == "stopped"
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn sandbox_hash(request: &SandboxRequest) -> u64 {
    let spec_hash = sandbox_spec_hash(&request.spec);
    let mut hasher = DefaultHasher::new();
    request.sandbox_id.as_str().hash(&mut hasher);
    spec_hash.hash(&mut hasher);
    hasher.finish()
}

/// Deterministic machine name for a sandbox key + spec.
fn machine_name_for_request(prefix: &str, request: &SandboxRequest) -> String {
    format!("{prefix}-{:016x}", sandbox_hash(request))
}

/// The smallest boxd size that holds the requested resources; `None` leaves
/// the size to the org's default.
fn machine_config(request: &SandboxRequest) -> Result<Option<api::VmConfig>> {
    let Some(resources) = request.spec.resources else {
        return Ok(None);
    };
    let vcpu = resources.vcpu_count.get();
    let memory_mib = resources.memory_mib.get();
    let (size_vcpu, size_gib) = MACHINE_SIZES
        .into_iter()
        .find(|(size_vcpu, size_gib)| *size_vcpu >= vcpu && size_gib * 1024 >= memory_mib)
        .with_context(|| {
            format!("boxd has no machine size with {vcpu} vCPUs and {memory_mib} MiB of memory")
        })?;
    Ok(Some(api::VmConfig {
        vcpu: u32::from(size_vcpu),
        memory_bytes: u64::from(size_gib) * 1024 * 1024 * 1024,
    }))
}

fn reject_unsupported_request(request: &SandboxRequest) -> Result<()> {
    if request.policy.networking == crate::SandboxNetworkPolicy::Disabled {
        bail!("boxd does not support policy.networking.disabled");
    }
    if request.spec.mounts.is_empty() {
        return Ok(());
    }
    bail!(
        "boxd sandbox backend does not support host bind-mounts; \
         remove conversation mounts or use a local sandbox provider"
    )
}

fn parse_labels(labels: &[String]) -> Result<HashMap<String, String>> {
    labels
        .iter()
        .map(|label| {
            let (key, value) = label
                .split_once('=')
                .with_context(|| format!("boxd label {label:?} must be key=value"))?;
            ensure!(!key.is_empty(), "boxd label {label:?} has an empty key");
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

fn status_error(operation: &str, status: Status) -> anyhow::Error {
    match status.code() {
        Code::Unauthenticated => anyhow!(
            "boxd {operation} failed: not authenticated ({}); check the boxd API key",
            status.message()
        ),
        Code::PermissionDenied => anyhow!(
            "boxd {operation} failed: permission denied ({})",
            status.message()
        ),
        code => anyhow!("boxd {operation} failed ({code:?}): {}", status.message()),
    }
}
