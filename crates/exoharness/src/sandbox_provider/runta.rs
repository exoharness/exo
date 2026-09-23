//! Runta REST lifecycle and bidirectional WebSocket command execution.
//! Checkpoint data stays in Runta; Exo persists a `runta-ref` manifest.

use std::collections::{HashMap, HashSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use url::Url;
use uuid::Uuid;

use crate::sandbox::sandbox_spec_hash;
use crate::{
    ManagedSandboxBackend, ManagedSandboxHandle, SandboxAttachment, SandboxCommand,
    SandboxCommandOutput, SandboxNetworkPolicy, SandboxProcessParts, SandboxRequest,
    SnapshotFormat, SnapshotPayload,
};

pub const DEFAULT_RUNTA_API_URL: &str = "https://api.runta.com";
const READY_TIMEOUT: Duration = Duration::from_secs(300);
const PIPE_SIZE: usize = 64 * 1024;
static SNAPSHOT_FORMATS: [SnapshotFormat; 1] = [SnapshotFormat::RuntaRef];

#[derive(Clone)]
pub struct RuntaConfig {
    pub token: String,
    pub api_url: String,
}

#[derive(Clone)]
pub struct RuntaSandboxBackend {
    client: reqwest::Client,
    api_url: Url,
    authorization: HeaderValue,
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Deserialize)]
struct Runtime {
    id: String,
    status: String,
    desired_status: String,
    revision: u64,
}

#[derive(Deserialize)]
struct RuntimePage {
    data: Vec<ListedRuntime>,
    pagination: Pagination,
}

#[derive(Deserialize)]
struct ListedRuntime {
    display_name: String,
    #[serde(flatten)]
    runtime: Runtime,
}

#[derive(Deserialize)]
struct Pagination {
    next_cursor: Option<String>,
    has_more: bool,
}

#[derive(Serialize, Deserialize)]
struct RuntimeState {
    runtime_id: String,
    spec_hash: String,
}

#[derive(Serialize, Deserialize)]
struct CheckpointManifest {
    checkpoint_id: String,
}

#[derive(Deserialize)]
struct Checkpoint {
    id: String,
    state: String,
}

#[derive(Serialize)]
struct ImageSelection<'a> {
    id: &'a str,
}

#[derive(Serialize)]
struct Resources {
    requests: ResourceRequests,
}

#[derive(Serialize)]
struct ResourceRequests {
    vcpus: u8,
    memory_mib: u32,
}

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum IdlePolicy {
    Disabled,
    SuspendAndWakeup { suspend_after_secs: u32 },
}

#[derive(Serialize)]
struct CreateRuntime<'a> {
    name: &'a str,
    resources: Resources,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<ImageSelection<'a>>,
    idle_policy: IdlePolicy,
}

#[derive(Serialize)]
struct RestoreRuntime<'a> {
    name: &'a str,
    checkpoint_id: &'a str,
    idle_policy: IdlePolicy,
}

#[derive(Serialize)]
struct CreateCheckpoint {
    name: String,
    kind: &'static str,
}

impl RuntaSandboxBackend {
    pub fn new(config: RuntaConfig) -> Result<Self> {
        let api_url = Url::parse(&format!("{}/", config.api_url.trim_end_matches('/')))
            .context("parsing Runta API URL")?;
        ensure!(
            matches!(api_url.scheme(), "http" | "https"),
            "Runta API URL must use HTTP or HTTPS"
        );
        ensure!(
            api_url.username().is_empty()
                && api_url.password().is_none()
                && api_url.query().is_none()
                && api_url.fragment().is_none(),
            "Runta API URL must not include credentials, query, or fragment"
        );
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", config.token))
            .context("RUNTA_TOKEN is not a valid HTTP header")?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization.clone());
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            client,
            api_url,
            authorization,
        })
    }

    fn endpoint(&self, segments: &[&str]) -> Url {
        let mut url = self.api_url.clone();
        url.path_segments_mut()
            .expect("validated HTTP URL")
            .pop_if_empty()
            .extend(segments);
        url
    }

    async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
        Ok(response
            .error_for_status()
            .context("Runta API request failed")?
            .json::<Envelope<T>>()
            .await
            .context("decoding Runta response")?
            .data)
    }

    async fn runtime(&self, id: &str) -> Result<Option<Runtime>> {
        let response = self
            .client
            .get(self.endpoint(&["v2", "runtimes", id]))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Self::decode(response).await.map(Some)
    }

    async fn runtime_by_name(&self, name: &str) -> Result<Option<Runtime>> {
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        loop {
            let mut request = self
                .client
                .get(self.endpoint(&["v2", "runtimes"]))
                .query(&[("limit", "100")]);
            if let Some(after) = &cursor {
                request = request.query(&[("after", after)]);
            }
            let page: RuntimePage = request
                .send()
                .await?
                .error_for_status()
                .context("listing Runta runtimes")?
                .json()
                .await
                .context("decoding Runta runtime list")?;
            if let Some(found) = page
                .data
                .into_iter()
                .find(|runtime| runtime.display_name == name)
            {
                return Ok(Some(found.runtime));
            }
            if !page.pagination.has_more {
                return Ok(None);
            }
            let next = page
                .pagination
                .next_cursor
                .filter(|cursor| !cursor.is_empty())
                .ok_or_else(|| anyhow!("Runta runtime list is missing its next cursor"))?;
            ensure!(
                seen.insert(next.clone()),
                "Runta runtime list repeated a pagination cursor"
            );
            cursor = Some(next);
        }
    }

    async fn required_runtime(&self, id: &str) -> Result<Runtime> {
        self.runtime(id)
            .await?
            .ok_or_else(|| anyhow!("Runta runtime {id} no longer exists"))
    }

    async fn transition(&self, runtime: &Runtime, action: &str) -> Result<Runtime> {
        Self::decode(
            self.client
                .post(self.endpoint(&["v2", "runtimes", &runtime.id, action]))
                .query(&[("expected_revision", runtime.revision)])
                .send()
                .await?,
        )
        .await
    }

    async fn ready(&self, mut runtime: Runtime) -> Result<Runtime> {
        tokio::time::timeout(READY_TIMEOUT, async {
            // Mutations are asynchronous; issue at most one start/resume, then poll.
            if runtime.desired_status != "running" {
                let action = if runtime.status == "paused" {
                    "resume"
                } else {
                    "start"
                };
                runtime = self.transition(&runtime, action).await?;
            }
            loop {
                match runtime.status.as_str() {
                    "running" if runtime.desired_status == "running" => return Ok(runtime),
                    "error" | "crashed" | "deleting" => {
                        bail!("Runta runtime {} entered {}", runtime.id, runtime.status)
                    }
                    _ => {}
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                runtime = self.required_runtime(&runtime.id).await?;
            }
        })
        .await
        .context("timed out waiting for Runta runtime readiness")?
    }

    fn handle(&self, request: SandboxRequest, runtime: Runtime) -> Arc<dyn ManagedSandboxHandle> {
        crate::with_process_management(Arc::new(RuntaSandboxHandle {
            id: format!("runta:{}", request.sandbox_id),
            runtime_id: runtime.id,
            request,
            backend: self.clone(),
        }))
    }
}

fn validate(request: &SandboxRequest) -> Result<()> {
    request.spec.policy.validate_basic("runta")?;
    ensure!(
        request.spec.policy.networking == SandboxNetworkPolicy::Unrestricted,
        "Runta does not support policy.networking.disabled"
    );
    ensure!(
        request.spec.mounts.is_empty(),
        "Runta does not support host bind-mounts"
    );
    ensure!(
        request.spec.durable_file_systems.is_empty(),
        "Runta does not support durable filesystem mounts"
    );
    Ok(())
}

fn idle_policy(request: &SandboxRequest) -> IdlePolicy {
    match request.lifecycle.idle_ttl {
        None => IdlePolicy::Disabled,
        Some(ttl) => IdlePolicy::SuspendAndWakeup {
            suspend_after_secs: ttl.as_secs().clamp(1, i32::MAX as u64) as u32,
        },
    }
}

fn runtime_name(request: &SandboxRequest) -> String {
    let mut hasher = DefaultHasher::new();
    request.sandbox_id.hash(&mut hasher);
    sandbox_spec_hash(&request.spec).hash(&mut hasher);
    format!("exo-{:016x}", hasher.finish())
}

#[async_trait]
impl ManagedSandboxBackend for RuntaSandboxBackend {
    fn is_local(&self) -> bool {
        false
    }
    fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
        &SNAPSHOT_FORMATS
    }

    async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
        validate(&request)?;
        let name = runtime_name(&request);
        let state = request
            .provider_state
            .clone()
            .map(serde_json::from_value::<RuntimeState>)
            .transpose()
            .context("decoding Runta provider state")?;
        let existing = match state {
            Some(state) if state.spec_hash == sandbox_spec_hash(&request.spec) => {
                // A missing persisted runtime must not silently become an empty filesystem.
                Some(self.required_runtime(&state.runtime_id).await?)
            }
            _ => self.runtime_by_name(&name).await?,
        };
        let runtime = match existing {
            Some(runtime) => runtime,
            None => {
                Self::decode(
                    self.client
                        .post(self.endpoint(&["v2", "runtimes"]))
                        .header("Idempotency-Key", &name)
                        .json(&CreateRuntime {
                            name: &name,
                            resources: Resources {
                                requests: ResourceRequests {
                                    vcpus: request.spec.resources.vcpu_count.get(),
                                    memory_mib: request.spec.resources.memory_mib.get(),
                                },
                            },
                            image: (!request.spec.image.is_empty()).then_some(ImageSelection {
                                id: &request.spec.image,
                            }),
                            idle_policy: idle_policy(&request),
                        })
                        .send()
                        .await?,
                )
                .await?
            }
        };
        Ok(self.handle(request, self.ready(runtime).await?))
    }

    async fn attach(
        &self,
        _request: SandboxRequest,
        _attachment: SandboxAttachment,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        bail!("Runta does not support external attachments")
    }

    async fn acquire_from_snapshot(
        &self,
        request: SandboxRequest,
        payload: SnapshotPayload,
    ) -> Result<Arc<dyn ManagedSandboxHandle>> {
        validate(&request)?;
        ensure!(
            payload.format == SnapshotFormat::RuntaRef,
            "Runta can only restore runta-ref snapshots, got {}",
            payload.format
        );
        let manifest: CheckpointManifest =
            serde_json::from_slice(&payload.bytes).context("decoding Runta checkpoint manifest")?;
        let name = format!("exo-{}", Uuid::new_v4().simple());
        let runtime = Self::decode(
            self.client
                .post(self.endpoint(&["v2", "runtimes"]))
                .header("Idempotency-Key", &name)
                .json(&RestoreRuntime {
                    name: &name,
                    checkpoint_id: &manifest.checkpoint_id,
                    idle_policy: idle_policy(&request),
                })
                .send()
                .await?,
        )
        .await?;
        Ok(self.handle(request, self.ready(runtime).await?))
    }
}

struct RuntaSandboxHandle {
    id: String,
    runtime_id: String,
    request: SandboxRequest,
    backend: RuntaSandboxBackend,
}

#[async_trait]
impl ManagedSandboxHandle for RuntaSandboxHandle {
    fn id(&self) -> &str {
        &self.id
    }
    fn provider_state(&self) -> Option<serde_json::Value> {
        Some(
            serde_json::to_value(RuntimeState {
                runtime_id: self.runtime_id.clone(),
                spec_hash: sandbox_spec_hash(&self.request.spec),
            })
            .expect("Runta state contains only strings"),
        )
    }
    async fn is_running(&self) -> Result<Option<bool>> {
        Ok(Some(
            self.backend
                .runtime(&self.runtime_id)
                .await?
                .is_some_and(|r| r.status == "running"),
        ))
    }
    async fn stop(&self) -> Result<()> {
        let runtime = self.backend.required_runtime(&self.runtime_id).await?;
        // Pause preserves memory and disk; deleting would lose the agent's environment.
        if runtime.desired_status != "paused"
            && runtime.status != "suspended"
            && runtime.status != "shutdown"
        {
            self.backend.transition(&runtime, "pause").await?;
        }
        Ok(())
    }
    async fn detach(&self) -> Result<SandboxAttachment> {
        bail!("Runta sandboxes cannot be detached")
    }
    async fn snapshot(&self) -> Result<SnapshotPayload> {
        let mut checkpoint: Checkpoint = RuntaSandboxBackend::decode(
            self.backend
                .client
                .post(
                    self.backend
                        .endpoint(&["v2", "runtimes", &self.runtime_id, "checkpoints"]),
                )
                .json(&CreateCheckpoint {
                    name: format!("exo-{}", Uuid::new_v4().simple()),
                    kind: "full",
                })
                .send()
                .await?,
        )
        .await?;
        tokio::time::timeout(READY_TIMEOUT, async {
            loop {
                match checkpoint.state.as_str() {
                    "ready" => return Ok(()),
                    "creating" => {}
                    state => bail!("Runta checkpoint {} entered {state}", checkpoint.id),
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                checkpoint = RuntaSandboxBackend::decode(
                    self.backend
                        .client
                        .get(
                            self.backend
                                .endpoint(&["v2", "checkpoints", &checkpoint.id]),
                        )
                        .send()
                        .await?,
                )
                .await?;
            }
        })
        .await
        .context("timed out waiting for Runta checkpoint")??;
        Ok(SnapshotPayload {
            format: SnapshotFormat::RuntaRef,
            bytes: Bytes::from(serde_json::to_vec(&CheckpointManifest {
                checkpoint_id: checkpoint.id,
            })?),
        })
    }
    async fn exec(&self, command: &SandboxCommand) -> Result<SandboxCommandOutput> {
        use futures::io::AsyncReadExt;
        let SandboxProcessParts {
            mut stdout,
            mut stderr,
            stdin,
            wait,
        } = self.start_process(command).await?;
        drop(stdin);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (code, _, _) = futures::try_join!(
            wait,
            async { Ok::<_, anyhow::Error>(stdout.read_to_end(&mut out).await?) },
            async { Ok::<_, anyhow::Error>(stderr.read_to_end(&mut err).await?) }
        )?;
        Ok(SandboxCommandOutput {
            ok: code == 0,
            exit_code: Some(code),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            command: command.argv.clone(),
            cwd: command
                .cwd
                .clone()
                .unwrap_or_else(|| self.request.spec.default_workdir.clone()),
        })
    }
    async fn start_process(&self, command: &SandboxCommand) -> Result<SandboxProcessParts> {
        ensure!(
            !command.argv.is_empty(),
            "sandbox command requires at least one argv entry"
        );
        let cwd = command
            .cwd
            .as_deref()
            .unwrap_or(&self.request.spec.default_workdir);
        let (program, args) = command_with_cwd(command, cwd);
        let mut url =
            self.backend
                .endpoint(&["v2", "runtimes", &self.runtime_id, "exec", "stream"]);
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|()| anyhow!("invalid Runta WebSocket URL"))?;
        let mut upgrade = url.as_str().into_client_request()?;
        upgrade
            .headers_mut()
            .insert(AUTHORIZATION, self.backend.authorization.clone());
        let (mut ws, _) = tokio::time::timeout(Duration::from_secs(60), connect_async(upgrade))
            .await
            .context("Runta exec connection timed out")??;
        ws.send(Message::Text(
            serde_json::to_string(&ClientFrame::Start {
                command: program,
                args,
                env: command.env.clone(),
                tty: false,
            })?
            .into(),
        ))
        .await?;
        let (stdout, stdout_writer) = tokio::io::duplex(PIPE_SIZE);
        let (stderr, stderr_writer) = tokio::io::duplex(PIPE_SIZE);
        let (stdin_reader, stdin) = tokio::io::duplex(PIPE_SIZE);
        let (mut tx, rx) = oneshot::channel();
        let timeout = command.timeout;
        tokio::spawn(async move {
            let deadline = async {
                match timeout {
                    Some(duration) => tokio::time::sleep(duration).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let result = tokio::select! {
                result = run_process(&mut ws, stdin_reader, stdout_writer, stderr_writer) => result,
                () = deadline => Err(anyhow!("Runta command timed out")),
                () = tx.closed() => Err(anyhow!("Runta command cancelled")),
            };
            if result.is_err() {
                // Bound cancellation even when the peer no longer reads the socket.
                let signal = Message::Text(
                    serde_json::to_string(&ClientFrame::Signal { signal: "TERM" })
                        .expect("signal is a literal string")
                        .into(),
                );
                if let Err(error) = tokio::time::timeout(Duration::from_secs(5), async {
                    ws.send(signal).await?;
                    ws.close(None).await
                })
                .await
                .context("Runta cancellation timed out")
                .and_then(|result| result.map_err(Into::into))
                {
                    tracing::debug!(%error, "failed to cancel Runta exec stream");
                }
            }
            if tx.send(result).is_err() {
                tracing::debug!("Runta process waiter dropped");
            }
        });
        Ok(SandboxProcessParts {
            stdout: Box::pin(stdout.compat()),
            stderr: Box::pin(stderr.compat()),
            stdin: Box::pin(stdin.compat_write()),
            wait: Box::pin(async move {
                rx.await
                    .context("Runta process stopped without reporting exit")?
            }),
        })
    }
}

fn command_with_cwd(command: &SandboxCommand, cwd: &str) -> (String, Vec<String>) {
    if cwd.is_empty() {
        return (command.argv[0].clone(), command.argv[1..].to_vec());
    }
    // Positional shell arguments preserve arbitrary argv bytes and avoid interpolation.
    let mut args = vec![
        "-c".into(),
        "cd -- \"$1\" && shift && exec \"$@\"".into(),
        "exo".into(),
        cwd.into(),
    ];
    args.extend(command.argv.clone());
    ("/bin/sh".into(), args)
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
    Start {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        tty: bool,
    },
    Stdin {
        data_base64: String,
    },
    CloseStdin,
    Signal {
        signal: &'static str,
    },
    Heartbeat,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame {
    Stdout { data_base64: String },
    Stderr { data_base64: String },
    Exit { code: i32 },
    Error { message: String },
    Heartbeat,
}

type ExecSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn run_process(
    ws: &mut ExecSocket,
    mut stdin: tokio::io::DuplexStream,
    mut stdout: tokio::io::DuplexStream,
    mut stderr: tokio::io::DuplexStream,
) -> Result<i32> {
    let mut buffer = vec![0; PIPE_SIZE];
    let mut stdin_closed = false;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = heartbeat.tick() => { ws.send(Message::Text(serde_json::to_string(&ClientFrame::Heartbeat)?.into())).await?; }
            read = stdin.read(&mut buffer), if !stdin_closed => {
                let size = read?;
                let frame = if size == 0 { stdin_closed = true; ClientFrame::CloseStdin }
                    else { ClientFrame::Stdin { data_base64: BASE64.encode(&buffer[..size]) } };
                ws.send(Message::Text(serde_json::to_string(&frame)?.into())).await?;
            }
            message = ws.next() => {
                match message.transpose()?.ok_or_else(|| anyhow!("Runta exec stream closed without an exit event"))? {
                    Message::Text(text) => match serde_json::from_str::<ServerFrame>(&text).context("decoding Runta exec frame")? {
                        ServerFrame::Stdout { data_base64 } => stdout.write_all(&BASE64.decode(data_base64)?).await?,
                        ServerFrame::Stderr { data_base64 } => stderr.write_all(&BASE64.decode(data_base64)?).await?,
                        ServerFrame::Exit { code } => return Ok(code),
                        ServerFrame::Error { message } => bail!("Runta exec failed: {message}"),
                        ServerFrame::Heartbeat => {},
                    },
                    Message::Ping(data) => ws.send(Message::Pong(data)).await?,
                    Message::Close(_) => bail!("Runta exec stream closed without an exit event"),
                    Message::Binary(_) => bail!("unexpected binary Runta exec frame"),
                    _ => {},
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    fn command() -> SandboxCommand {
        SandboxCommand {
            argv: vec![
                "printf".into(),
                "%s".into(),
                "$(touch /tmp/should-not-exist)".into(),
            ],
            env: HashMap::from([("EXO_TEST".into(), "some value".into())]),
            display_argv: None,
            cwd: Some("/tmp/space ' quote".into()),
            timeout: None,
        }
    }

    fn handle(address: std::net::SocketAddr) -> RuntaSandboxHandle {
        RuntaSandboxHandle {
            id: "runta:test".into(),
            runtime_id: "runtime-1".into(),
            backend: RuntaSandboxBackend::new(RuntaConfig {
                token: "test-token".into(),
                api_url: format!("http://{address}"),
            })
            .unwrap(),
            request: SandboxRequest {
                sandbox_id: "sandbox-1".into(),
                scope: None,
                provider_state: None,
                lifecycle: Default::default(),
                spec: crate::SandboxSpec {
                    image: String::new(),
                    resources: Default::default(),
                    mounts: vec![],
                    durable_file_systems: vec![],
                    policy: SandboxNetworkPolicy::Unrestricted.into(),
                    default_workdir: "/tmp".into(),
                },
            },
        }
    }

    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum ReceivedFrame {
        Start {
            command: String,
            args: Vec<String>,
            env: HashMap<String, String>,
            tty: bool,
        },
        Stdin {
            data_base64: String,
        },
        CloseStdin,
        Heartbeat,
        Signal {
            signal: String,
        },
    }

    #[tokio::test]
    async fn streams_output_before_exit_and_delivers_binary_stdin_and_eof() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let handle = handle(listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            // Tungstenite requires this callback to return its unboxed HTTP error response.
            #[allow(clippy::result_large_err)]
            let mut ws = accept_hdr_async(socket, |request: &Request, response: Response| {
                assert_eq!(request.uri().path(), "/v2/runtimes/runtime-1/exec/stream");
                assert_eq!(request.headers()["authorization"], "Bearer test-token");
                Ok(response)
            })
            .await?;
            let text = ws.next().await.unwrap()?.into_text()?;
            match serde_json::from_str::<ReceivedFrame>(&text)? {
                ReceivedFrame::Start {
                    command: program,
                    args,
                    env,
                    tty,
                } => {
                    assert_eq!(
                        (program, args),
                        command_with_cwd(&command(), command().cwd.as_deref().unwrap())
                    );
                    assert_eq!(env, command().env);
                    assert!(!tty);
                }
                _ => panic!("expected start frame"),
            }
            ws.send(Message::Text(
                r#"{"type":"stdout","data_base64":"cmVhZHkK"}"#.into(),
            ))
            .await?;
            let mut input = Vec::new();
            loop {
                let text = ws.next().await.unwrap()?.into_text()?;
                match serde_json::from_str::<ReceivedFrame>(&text)? {
                    ReceivedFrame::Stdin { data_base64 } => {
                        input.extend(BASE64.decode(data_base64)?)
                    }
                    ReceivedFrame::CloseStdin => break,
                    ReceivedFrame::Heartbeat => {}
                    _ => panic!("unexpected input frame"),
                }
            }
            assert_eq!(input, b"hello\0\xff");
            ws.send(Message::Text(
                r#"{"type":"stderr","data_base64":"ZXJyb3IK"}"#.into(),
            ))
            .await?;
            ws.send(Message::Text(r#"{"type":"exit","code":7}"#.into()))
                .await?;
            Ok::<_, anyhow::Error>(())
        });
        let SandboxProcessParts {
            mut stdout,
            mut stderr,
            mut stdin,
            wait,
        } = handle.start_process(&command()).await?;
        let mut ready = [0; 6];
        tokio::time::timeout(Duration::from_secs(5), stdout.read_exact(&mut ready)).await??;
        assert_eq!(&ready, b"ready\n");
        stdin.write_all(b"hello\0\xff").await?;
        stdin.close().await?;
        let mut error = String::new();
        stderr.read_to_string(&mut error).await?;
        assert_eq!(error, "error\n");
        assert_eq!(wait.await?, 7);
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn stream_closed_without_exit_is_not_success() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let handle = handle(listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            let mut ws = tokio_tungstenite::accept_async(socket).await?;
            ws.next().await.unwrap()?;
            ws.close(None).await?;
            Ok::<_, anyhow::Error>(())
        });
        assert!(handle.exec(&command()).await.is_err());
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn timeout_signals_the_remote_process() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let handle = handle(listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            let mut ws = tokio_tungstenite::accept_async(socket).await?;
            loop {
                let message = ws.next().await.unwrap()?;
                if let Message::Text(text) = message
                    && let ReceivedFrame::Signal { signal } = serde_json::from_str(&text)?
                {
                    assert_eq!(signal, "TERM");
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        });
        let mut command = command();
        command.timeout = Some(Duration::from_millis(50));
        let error = tokio::time::timeout(Duration::from_secs(10), handle.exec(&command))
            .await?
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        server.await??;
        Ok(())
    }
    #[tokio::test]
    async fn dropping_waiter_signals_the_remote_process() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let handle = handle(listener.local_addr()?);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            let mut ws = tokio_tungstenite::accept_async(socket).await?;
            loop {
                let message = ws.next().await.unwrap()?;
                if let Message::Text(text) = message
                    && let ReceivedFrame::Signal { signal } = serde_json::from_str(&text)?
                {
                    assert_eq!(signal, "TERM");
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        });
        let parts = handle.start_process(&command()).await?;
        drop(parts.wait);
        tokio::time::timeout(Duration::from_secs(10), server).await???;
        Ok(())
    }
}
