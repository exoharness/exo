use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_lambdamicrovms::Client;
use aws_sdk_lambdamicrovms::types::{IdlePolicy, MicrovmState, PortSpecification};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::sandbox::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxCommand, SandboxCommandOutput,
    SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotPayload, sandbox_spec_hash,
    validate_durable_file_systems,
};
use crate::sandbox_provider::process_bridge;

const DEFAULT_MAX_IDLE_DURATION_SECONDS: i32 = 900;
const DEFAULT_SUSPENDED_DURATION_SECONDS: i32 = 28_800;
const DEFAULT_MAXIMUM_DURATION_SECONDS: i32 = 28_800;
const DEFAULT_AUTH_TOKEN_EXPIRATION_MINUTES: i32 = 30;
const DEFAULT_RUNTIME_PORT: i32 = 8080;
const WAIT_RETRY_DELAY: Duration = Duration::from_secs(2);
const WAIT_MAX_ATTEMPTS: usize = 60;

pub fn default_aws_lambda_microvm_image() -> String {
    String::new()
}

pub fn default_aws_lambda_microvm_port() -> i32 {
    DEFAULT_RUNTIME_PORT
}

#[derive(Debug, Clone)]
pub struct AwsLambdaMicrovmConfig {
    pub image_identifier: String,
    pub region: String,
    pub image_version: Option<String>,
    pub endpoint_url: Option<String>,
    pub credentials: Option<AwsLambdaMicrovmCredentials>,
    pub ingress_network_connector_arns: Vec<String>,
    pub egress_network_connector_arns: Vec<String>,
    pub execution_role_arn: Option<String>,
    pub max_idle_duration_seconds: Option<i32>,
    pub suspended_duration_seconds: Option<i32>,
    pub auto_resume_enabled: Option<bool>,
    pub maximum_duration_seconds: Option<i32>,
    pub auth_token_expiration_minutes: Option<i32>,
    pub runtime_port: i32,
}

#[derive(Debug, Clone)]
pub struct AwsLambdaMicrovmCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

pub struct AwsLambdaMicrovmSandboxBackend {
    client: Client,
    http: reqwest::Client,
    image_identifier: String,
    image_version: Option<String>,
    ingress_network_connector_arns: Vec<String>,
    egress_network_connector_arns: Vec<String>,
    execution_role_arn: Option<String>,
    idle_policy: IdlePolicy,
    maximum_duration_seconds: i32,
    auth_token_expiration_minutes: i32,
    runtime_port: i32,
    sessions: Mutex<HashMap<String, AwsLambdaMicrovmSession>>,
}

impl AwsLambdaMicrovmSandboxBackend {
    pub async fn new(config: AwsLambdaMicrovmConfig) -> Result<Self> {
        if config.image_identifier.trim().is_empty() {
            bail!("Lambda MicroVM image identifier must not be empty");
        }
        if config.region.trim().is_empty() {
            bail!("Lambda MicroVM region must not be empty");
        }
        validate_seconds(
            "max_idle_duration_seconds",
            config
                .max_idle_duration_seconds
                .unwrap_or(DEFAULT_MAX_IDLE_DURATION_SECONDS),
        )?;
        validate_seconds(
            "suspended_duration_seconds",
            config
                .suspended_duration_seconds
                .unwrap_or(DEFAULT_SUSPENDED_DURATION_SECONDS),
        )?;
        validate_seconds(
            "maximum_duration_seconds",
            config
                .maximum_duration_seconds
                .unwrap_or(DEFAULT_MAXIMUM_DURATION_SECONDS),
        )?;
        let auth_token_expiration_minutes = config
            .auth_token_expiration_minutes
            .unwrap_or(DEFAULT_AUTH_TOKEN_EXPIRATION_MINUTES);
        if !(1..=60).contains(&auth_token_expiration_minutes) {
            bail!("auth_token_expiration_minutes must be between 1 and 60");
        }
        if !(1..=65_535).contains(&config.runtime_port) {
            bail!("Lambda MicroVM runtime_port must be between 1 and 65535");
        }

        let region = config.region;
        let mut sdk_config_loader =
            aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region.clone()));
        if let Some(credentials) = config.credentials {
            sdk_config_loader = sdk_config_loader.credentials_provider(Credentials::new(
                credentials.access_key_id,
                credentials.secret_access_key,
                credentials.session_token,
                None,
                "aws-lambda-microvm",
            ));
        }
        let sdk_config = sdk_config_loader.load().await;
        let mut service_config_builder = aws_sdk_lambdamicrovms::config::Builder::from(&sdk_config);
        if let Some(endpoint_url) = config.endpoint_url {
            service_config_builder = service_config_builder.endpoint_url(endpoint_url);
        } else {
            service_config_builder.set_endpoint_url(None);
        }

        let ingress_network_connector_arns =
            default_ingress_connectors(region.as_str(), config.ingress_network_connector_arns);
        let egress_network_connector_arns =
            default_egress_connectors(region.as_str(), config.egress_network_connector_arns);
        let idle_policy = IdlePolicy::builder()
            .max_idle_duration_seconds(
                config
                    .max_idle_duration_seconds
                    .unwrap_or(DEFAULT_MAX_IDLE_DURATION_SECONDS),
            )
            .suspended_duration_seconds(
                config
                    .suspended_duration_seconds
                    .unwrap_or(DEFAULT_SUSPENDED_DURATION_SECONDS),
            )
            .auto_resume_enabled(config.auto_resume_enabled.unwrap_or(true))
            .build()
            .context("building Lambda MicroVM idle policy")?;

        Ok(Self {
            client: Client::from_conf(service_config_builder.build()),
            http: reqwest::Client::new(),
            image_identifier: config.image_identifier,
            image_version: config.image_version,
            ingress_network_connector_arns,
            egress_network_connector_arns,
            execution_role_arn: config.execution_role_arn,
            idle_policy,
            maximum_duration_seconds: config
                .maximum_duration_seconds
                .unwrap_or(DEFAULT_MAXIMUM_DURATION_SECONDS),
            auth_token_expiration_minutes,
            runtime_port: config.runtime_port,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub async fn terminate_all(&self) -> Result<()> {
        let sessions: Vec<_> = self.sessions.lock().await.values().cloned().collect();
        for session in sessions {
            self.client
                .terminate_microvm()
                .microvm_identifier(session.microvm_id)
                .send()
                .await
                .context("terminating Lambda MicroVM sandbox")?;
        }
        self.sessions.lock().await.clear();
        Ok(())
    }
}

#[async_trait]
impl ManagedSandboxBackend for AwsLambdaMicrovmSandboxBackend {
    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        reject_unsupported_request(&request)?;
        let spec_hash = sandbox_spec_hash(&request.spec);
        let session_key = lambda_microvm_session_key(&request, &spec_hash);

        let cached_session = {
            let sessions = self.sessions.lock().await;
            sessions.get(&session_key).cloned()
        };
        if let Some(session) = cached_session {
            match self.ensure_session_running(&session).await {
                Ok(running) => {
                    drop(
                        self.sessions
                            .lock()
                            .await
                            .insert(session_key.clone(), running.clone()),
                    );
                    return Ok(self.handle(session_key, request, running));
                }
                Err(error) => {
                    self.sessions.lock().await.remove(&session_key);
                    tracing::debug!(
                        error = %error,
                        microvm_id = %session.microvm_id,
                        "dropping unusable Lambda MicroVM session"
                    );
                }
            }
        }

        if let Some(provider_state) = request.provider_state.clone() {
            let state = serde_json::from_value::<AwsLambdaMicrovmProviderState>(provider_state)
                .context("decoding Lambda MicroVM provider state")?;
            let session = AwsLambdaMicrovmSession {
                microvm_id: state.microvm_id,
                endpoint: state.endpoint,
            };
            match self.ensure_session_running(&session).await {
                Ok(running) => {
                    drop(
                        self.sessions
                            .lock()
                            .await
                            .insert(session_key.clone(), running.clone()),
                    );
                    return Ok(self.handle(session_key, request, running));
                }
                Err(error) => {
                    tracing::debug!(
                        error = %error,
                        microvm_id = %session.microvm_id,
                        "persisted Lambda MicroVM session is unusable; launching replacement"
                    );
                }
            }
        }

        let session = self.run_session(&request, &spec_hash).await?;
        drop(
            self.sessions
                .lock()
                .await
                .insert(session_key.clone(), session.clone()),
        );
        Ok(self.handle(session_key, request, session))
    }

    async fn acquire_from_snapshot(
        &self,
        _request: SandboxRequest,
        _payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("restoring a Lambda MicroVM sandbox from a snapshot is not implemented yet");
    }
}

impl AwsLambdaMicrovmSandboxBackend {
    fn handle(
        &self,
        session_key: String,
        request: SandboxRequest,
        session: AwsLambdaMicrovmSession,
    ) -> Arc<dyn ManagedSandboxHandle> {
        Arc::new(AwsLambdaMicrovmSandboxHandle {
            id: format!("aws-lambda-microvm:{}", session.microvm_id),
            session_key,
            request,
            session,
            backend: AwsLambdaMicrovmBackendHandle {
                client: self.client.clone(),
                http: self.http.clone(),
                runtime_port: self.runtime_port,
                auth_token_expiration_minutes: self.auth_token_expiration_minutes,
            },
        })
    }

    async fn run_session(
        &self,
        request: &SandboxRequest,
        spec_hash: &str,
    ) -> Result<AwsLambdaMicrovmSession> {
        let run = self
            .client
            .run_microvm()
            .image_identifier(self.image_identifier.clone())
            .set_image_version(self.image_version.clone())
            .set_ingress_network_connectors(Some(self.ingress_network_connector_arns.clone()))
            .set_egress_network_connectors(Some(self.egress_network_connector_arns.clone()))
            .set_execution_role_arn(self.execution_role_arn.clone())
            .idle_policy(self.idle_policy.clone())
            .maximum_duration_in_seconds(self.maximum_duration_seconds)
            .client_token(lambda_microvm_client_token(request, spec_hash));
        let output = run.send().await.with_context(|| {
            format!(
                "running Lambda MicroVM image {}",
                self.image_identifier.as_str()
            )
        })?;
        let session = AwsLambdaMicrovmSession {
            microvm_id: output.microvm_id,
            endpoint: normalize_microvm_endpoint(&output.endpoint),
        };
        self.ensure_session_running(&session).await
    }

    async fn ensure_session_running(
        &self,
        session: &AwsLambdaMicrovmSession,
    ) -> Result<AwsLambdaMicrovmSession> {
        let current = self
            .client
            .get_microvm()
            .microvm_identifier(session.microvm_id.clone())
            .send()
            .await
            .with_context(|| format!("getting Lambda MicroVM {}", session.microvm_id))?;
        match current.state {
            MicrovmState::Running => Ok(AwsLambdaMicrovmSession {
                microvm_id: current.microvm_id,
                endpoint: normalize_microvm_endpoint(&current.endpoint),
            }),
            MicrovmState::Suspended => {
                self.client
                    .resume_microvm()
                    .microvm_identifier(session.microvm_id.clone())
                    .send()
                    .await
                    .with_context(|| format!("resuming Lambda MicroVM {}", session.microvm_id))?;
                self.wait_for_state(&session.microvm_id, MicrovmState::Running)
                    .await
            }
            MicrovmState::Suspending => {
                self.wait_for_state(&session.microvm_id, MicrovmState::Suspended)
                    .await?;
                self.client
                    .resume_microvm()
                    .microvm_identifier(session.microvm_id.clone())
                    .send()
                    .await
                    .with_context(|| format!("resuming Lambda MicroVM {}", session.microvm_id))?;
                self.wait_for_state(&session.microvm_id, MicrovmState::Running)
                    .await
            }
            MicrovmState::Pending => {
                self.wait_for_state(&session.microvm_id, MicrovmState::Running)
                    .await
            }
            MicrovmState::Terminated | MicrovmState::Terminating => {
                bail!(
                    "Lambda MicroVM {} is {}; launching a replacement",
                    session.microvm_id,
                    current.state
                );
            }
            other => {
                bail!(
                    "Lambda MicroVM {} is in unsupported state {}",
                    session.microvm_id,
                    other
                );
            }
        }
    }

    async fn wait_for_state(
        &self,
        microvm_id: &str,
        desired: MicrovmState,
    ) -> Result<AwsLambdaMicrovmSession> {
        wait_for_microvm_state(&self.client, microvm_id, desired).await
    }
}

#[derive(Clone)]
struct AwsLambdaMicrovmBackendHandle {
    client: Client,
    http: reqwest::Client,
    runtime_port: i32,
    auth_token_expiration_minutes: i32,
}

#[derive(Clone)]
struct AwsLambdaMicrovmSession {
    microvm_id: String,
    endpoint: String,
}

#[derive(Serialize, Deserialize)]
struct AwsLambdaMicrovmProviderState {
    microvm_id: String,
    endpoint: String,
}

struct AwsLambdaMicrovmSandboxHandle {
    id: String,
    session_key: String,
    request: SandboxRequest,
    session: AwsLambdaMicrovmSession,
    backend: AwsLambdaMicrovmBackendHandle,
}

#[async_trait]
impl ManagedSandboxHandle for AwsLambdaMicrovmSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn provider_state(&self) -> Option<serde_json::Value> {
        serde_json::to_value(AwsLambdaMicrovmProviderState {
            microvm_id: self.session.microvm_id.clone(),
            endpoint: self.session.endpoint.clone(),
        })
        .ok()
    }

    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        exec_in_microvm(&self.backend, &self.session, &self.request.spec, command).await
    }

    async fn start_process(&self, command: &SandboxCommand) -> Result<crate::SandboxProcessParts> {
        start_process_in_microvm(&self.backend, &self.session, &self.request.spec, command).await
    }

    async fn stop(&self) -> Result<()> {
        self.backend
            .client
            .suspend_microvm()
            .microvm_identifier(self.session.microvm_id.clone())
            .send()
            .await
            .with_context(|| {
                format!(
                    "suspending Lambda MicroVM sandbox {} for session {}",
                    self.session.microvm_id, self.session_key
                )
            })?;
        wait_for_microvm_state(
            &self.backend.client,
            &self.session.microvm_id,
            MicrovmState::Suspended,
        )
        .await
        .with_context(|| {
            format!(
                "waiting for Lambda MicroVM sandbox {} to suspend",
                self.session.microvm_id
            )
        })?;
        Ok(())
    }

    async fn snapshot(&self) -> Result<SnapshotPayload> {
        bail!("Lambda MicroVM sandbox snapshots are not implemented yet");
    }
}

async fn exec_in_microvm(
    backend: &AwsLambdaMicrovmBackendHandle,
    session: &AwsLambdaMicrovmSession,
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
    let body = MicrovmExecRequest {
        request_type: "exec",
        argv: command.argv.clone(),
        env: command.env.clone(),
        cwd: cwd.clone(),
        timeout_ms: command.timeout.map(duration_to_millis),
    };
    let response: MicrovmExecResponse = invoke_microvm_json(backend, session, &body).await?;
    if let Some(error) = response.error {
        bail!("Lambda MicroVM exec failed: {error}");
    }
    let exit_code = response
        .exit_code
        .context("Lambda MicroVM exec response did not include exit_code")?;
    let ok = response.ok.unwrap_or(exit_code == 0);
    Ok(SandboxCommandOutput {
        ok,
        exit_code: Some(exit_code),
        stdout: response.stdout.unwrap_or_default(),
        stderr: response.stderr.unwrap_or_default(),
        command: command.argv.clone(),
        cwd: response.cwd.unwrap_or(cwd),
    })
}

async fn start_process_in_microvm(
    backend: &AwsLambdaMicrovmBackendHandle,
    session: &AwsLambdaMicrovmSession,
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
    let response: MicrovmStartProcessResponse = invoke_microvm_json(
        backend,
        session,
        &MicrovmStartProcessRequest {
            request_type: "start_process",
            argv: command.argv.clone(),
            env: command.env.clone(),
            cwd,
        },
    )
    .await?;
    if let Some(error) = response.error {
        bail!("Lambda MicroVM start_process failed: {error}");
    }
    let process_id = response
        .process_id
        .context("Lambda MicroVM start_process response did not include process_id")?;
    let client = AwsLambdaMicrovmProcessBridgeClient {
        backend: backend.clone(),
        session: session.clone(),
        process_id,
    };
    Ok(process_bridge::process_parts(Arc::new(client)))
}

struct AwsLambdaMicrovmProcessBridgeClient {
    backend: AwsLambdaMicrovmBackendHandle,
    session: AwsLambdaMicrovmSession,
    process_id: String,
}

#[async_trait]
impl process_bridge::Client for AwsLambdaMicrovmProcessBridgeClient {
    async fn request(&self, request: process_bridge::Request) -> Result<process_bridge::Response> {
        invoke_microvm_json(
            &self.backend,
            &self.session,
            &MicrovmProcessBridgeRequest {
                request_type: "process_bridge",
                process_id: self.process_id.clone(),
                request,
            },
        )
        .await
    }
}

async fn invoke_microvm_json<Request, Response>(
    backend: &AwsLambdaMicrovmBackendHandle,
    session: &AwsLambdaMicrovmSession,
    body: &Request,
) -> Result<Response>
where
    Request: Serialize,
    Response: for<'de> Deserialize<'de>,
{
    let token = backend
        .client
        .create_microvm_auth_token()
        .microvm_identifier(session.microvm_id.clone())
        .expiration_in_minutes(backend.auth_token_expiration_minutes)
        .allowed_ports(PortSpecification::Port(backend.runtime_port))
        .send()
        .await
        .with_context(|| {
            format!(
                "creating endpoint auth token for Lambda MicroVM {}",
                session.microvm_id
            )
        })?
        .auth_token()
        .get("X-aws-proxy-auth")
        .cloned()
        .context("Lambda MicroVM auth token response missing X-aws-proxy-auth")?;
    let response = backend
        .http
        .post(format!(
            "{}/invocations",
            session.endpoint.trim_end_matches('/')
        ))
        .header("X-aws-proxy-auth", token)
        .header("X-aws-proxy-port", backend.runtime_port.to_string())
        .json(body)
        .send()
        .await
        .with_context(|| format!("invoking Lambda MicroVM {}", session.microvm_id))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .context("reading Lambda MicroVM response body")?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        bail!("Lambda MicroVM returned status {status}: {text}");
    }
    serde_json::from_slice(&bytes).with_context(|| {
        let text = String::from_utf8_lossy(&bytes);
        format!("decoding Lambda MicroVM JSON response: {text}")
    })
}

async fn wait_for_microvm_state(
    client: &Client,
    microvm_id: &str,
    desired: MicrovmState,
) -> Result<AwsLambdaMicrovmSession> {
    for _attempt in 0..WAIT_MAX_ATTEMPTS {
        let current = client
            .get_microvm()
            .microvm_identifier(microvm_id.to_string())
            .send()
            .await
            .with_context(|| format!("getting Lambda MicroVM {microvm_id}"))?;
        if current.state == desired {
            return Ok(AwsLambdaMicrovmSession {
                microvm_id: current.microvm_id,
                endpoint: normalize_microvm_endpoint(&current.endpoint),
            });
        }
        if matches!(
            current.state,
            MicrovmState::Terminated | MicrovmState::Terminating
        ) {
            bail!(
                "Lambda MicroVM {microvm_id} reached terminal state {} before {desired}",
                current.state
            );
        }
        sleep(WAIT_RETRY_DELAY).await;
    }
    bail!("timed out waiting for Lambda MicroVM {microvm_id} to become {desired}");
}

fn reject_unsupported_request(request: &SandboxRequest) -> Result<()> {
    if !request.spec.mounts.is_empty() {
        bail!(
            "Lambda MicroVM sandbox backend does not support host bind-mounts; remove conversation mounts or use a local sandbox provider"
        );
    }
    validate_durable_file_systems(&request.spec.durable_file_systems)?;
    match request.spec.durable_file_systems.as_slice() {
        [] => {}
        [file_system] => {
            if matches!(file_system.mode, crate::FileSystemMountMode::ReadOnly) {
                bail!(
                    "Lambda MicroVM sandbox backend does not support read-only durable file systems"
                );
            }
            if file_system.mount_path != request.spec.default_workdir {
                bail!(
                    "Lambda MicroVM durable file system mount path {:?} must match the sandbox default workdir {:?}",
                    file_system.mount_path,
                    request.spec.default_workdir
                );
            }
        }
        _ => {
            bail!("Lambda MicroVM sandbox backend supports at most one durable file system");
        }
    }
    if matches!(request.spec.network, SandboxNetworkPolicy::Disabled) {
        bail!("Lambda MicroVM sandbox backend cannot enforce disabled networking");
    }
    Ok(())
}

fn lambda_microvm_session_key(request: &SandboxRequest, spec_hash: &str) -> String {
    format!("{}\n{spec_hash}", request.key)
}

fn lambda_microvm_client_token(request: &SandboxRequest, spec_hash: &str) -> String {
    format!(
        "exo-{}",
        stable_fnv1a_hex(&lambda_microvm_session_key(request, spec_hash))
    )
}

fn stable_fnv1a_hex(input: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn normalize_microvm_endpoint(endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.to_string()
    } else {
        format!("https://{endpoint}")
    }
}

fn default_ingress_connectors(region: &str, configured: Vec<String>) -> Vec<String> {
    if configured.is_empty() {
        vec![format!(
            "arn:aws:lambda:{region}:aws:network-connector:aws-network-connector:ALL_INGRESS"
        )]
    } else {
        configured
    }
}

fn default_egress_connectors(region: &str, configured: Vec<String>) -> Vec<String> {
    if configured.is_empty() {
        vec![format!(
            "arn:aws:lambda:{region}:aws:network-connector:aws-network-connector:INTERNET_EGRESS"
        )]
    } else {
        configured
    }
}

fn validate_seconds(name: &str, value: i32) -> Result<()> {
    if !(1..=28_800).contains(&value) {
        bail!("{name} must be between 1 and 28800 seconds");
    }
    Ok(())
}

fn duration_to_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, Serialize)]
struct MicrovmExecRequest {
    #[serde(rename = "type")]
    request_type: &'static str,
    argv: Vec<String>,
    env: HashMap<String, String>,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct MicrovmStartProcessRequest {
    #[serde(rename = "type")]
    request_type: &'static str,
    argv: Vec<String>,
    env: HashMap<String, String>,
    cwd: String,
}

#[derive(Debug, Serialize)]
struct MicrovmProcessBridgeRequest {
    #[serde(rename = "type")]
    request_type: &'static str,
    process_id: String,
    request: process_bridge::Request,
}

#[derive(Debug, Deserialize)]
struct MicrovmExecResponse {
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    stderr: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MicrovmStartProcessResponse {
    #[serde(default)]
    process_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
}
