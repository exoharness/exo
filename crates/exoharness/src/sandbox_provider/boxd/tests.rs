//! Tests for the boxd backend against an in-process fake of the boxd gRPC API.

use std::collections::HashMap;
use std::num::{NonZeroU8, NonZeroU32};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::api::boxd_api_server::{BoxdApi, BoxdApiServer};
use super::{BoxdConfig, BoxdSandboxBackend, api, machine_name_for_request};
use crate::sandbox::{
    ManagedSandboxBackend, SandboxCommand, SandboxRequest, SandboxSpec, SnapshotFormat,
    SnapshotPayload, WARM_SANDBOX_KEY_LABEL, WARM_SANDBOX_SPEC_HASH_LABEL, sandbox_spec_hash,
};
use crate::{
    ResourceScope, SandboxLifecycleConfig, SandboxMount, SandboxMountAccess, SandboxNetworkPolicy,
    SandboxResourceShape, SnapshotKind, Uuid7,
};

const API_KEY: &str = "test-api-key";
const TOKEN: &str = "session-token";

#[derive(Debug, Clone)]
struct FakeMachine {
    vm_id: String,
    name: String,
    status: String,
    isolated: bool,
    labels: HashMap<String, String>,
}

impl FakeMachine {
    fn to_response(&self) -> api::GetVmResponse {
        api::GetVmResponse {
            vm_id: self.vm_id.clone(),
            name: self.name.clone(),
            status: self.status.clone(),
            isolated: self.isolated,
            labels: self.labels.clone(),
        }
    }
}

#[derive(Default)]
struct FakeState {
    machines: Vec<FakeMachine>,
    calls: Vec<String>,
    creates: Vec<api::CreateVmRequest>,
    restores: Vec<api::CreateVmFromSnapshotRequest>,
    forks: Vec<api::ForkVmRequest>,
    commands: Vec<String>,
    /// Bearer tokens seen on each call.
    authorizations: Vec<String>,
    create_conflict: bool,
    /// Answer ListVms with every machine, as a server that ignores selectors would.
    ignore_selector: bool,
    /// Report every snapshot as failed.
    snapshot_fails: bool,
    /// Never answer GetVm.
    hang_get_vm: bool,
    /// How many `true` readiness probes to leave unanswered.
    hang_probes: usize,
    /// Machines whose hibernation is still being written out: reported as
    /// `suspended` once, then `hibernated`.
    hibernating: Vec<String>,
}

#[derive(Clone, Default)]
struct FakeBoxd {
    state: Arc<Mutex<FakeState>>,
}

impl FakeBoxd {
    fn record<T>(&self, call: &str, request: &Request<T>) -> std::sync::MutexGuard<'_, FakeState> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(call.to_string());
        state.authorizations.push(
            request
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string(),
        );
        state
    }

    fn with_machine(&self, machine: FakeMachine) -> Self {
        self.state.lock().unwrap().machines.push(machine);
        self.clone()
    }

    fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    fn set_status(state: &mut FakeState, vm_id: &str, status: &str) -> Result<(), Status> {
        let machine = state
            .machines
            .iter_mut()
            .find(|machine| machine.vm_id == vm_id || machine.name == vm_id)
            .ok_or_else(|| Status::not_found("vm not found"))?;
        machine.status = status.to_string();
        Ok(())
    }
}

fn add_machine(state: &mut FakeState, name: String, labels: HashMap<String, String>) -> String {
    let vm_id = format!("vm-{}", state.machines.len() + 1);
    state.machines.push(FakeMachine {
        vm_id: vm_id.clone(),
        name,
        status: "running".into(),
        isolated: true,
        labels,
    });
    vm_id
}

type ExecStream = Pin<Box<dyn Stream<Item = Result<api::ExecChunk, Status>> + Send>>;

#[async_trait]
impl BoxdApi for FakeBoxd {
    async fn create_vm(
        &self,
        request: Request<api::CreateVmRequest>,
    ) -> Result<Response<api::CreateVmResponse>, Status> {
        let mut state = self.record("CreateVm", &request);
        let message = request.into_inner();
        state.creates.push(message.clone());
        if state.create_conflict {
            add_machine(&mut state, message.name.clone(), message.labels.clone());
            return Err(Status::already_exists("name taken"));
        }
        let vm_id = add_machine(&mut state, message.name.clone(), message.labels);
        Ok(Response::new(api::CreateVmResponse {
            vm_id,
            name: message.name,
            status: "running".into(),
        }))
    }

    async fn destroy_vm(
        &self,
        request: Request<api::DestroyVmRequest>,
    ) -> Result<Response<api::DestroyVmResponse>, Status> {
        let mut state = self.record("DestroyVm", &request);
        let vm_id = request.into_inner().vm_id;
        let before = state.machines.len();
        state.machines.retain(|machine| machine.vm_id != vm_id);
        if state.machines.len() == before {
            return Err(Status::not_found("vm not found"));
        }
        Ok(Response::new(api::DestroyVmResponse {}))
    }

    async fn start_vm(
        &self,
        request: Request<api::StartVmRequest>,
    ) -> Result<Response<api::StartVmResponse>, Status> {
        let mut state = self.record("StartVm", &request);
        Self::set_status(&mut state, &request.into_inner().vm_id, "running")?;
        Ok(Response::new(api::StartVmResponse {}))
    }

    async fn get_vm(
        &self,
        request: Request<api::GetVmRequest>,
    ) -> Result<Response<api::GetVmResponse>, Status> {
        let hang = self.record("GetVm", &request).hang_get_vm;
        if hang {
            std::future::pending::<()>().await;
        }
        let mut state = self.state.lock().unwrap();
        let vm_id = request.into_inner().vm_id;
        if let Some(position) = state.hibernating.iter().position(|id| *id == vm_id) {
            state.hibernating.remove(position);
            return Ok(Response::new(api::GetVmResponse {
                vm_id,
                status: "suspended".into(),
                ..Default::default()
            }));
        }
        state
            .machines
            .iter()
            .find(|machine| machine.vm_id == vm_id || machine.name == vm_id)
            .map(|machine| Response::new(machine.to_response()))
            .ok_or_else(|| Status::not_found("vm not found"))
    }

    async fn list_vms(
        &self,
        request: Request<api::ListVmsRequest>,
    ) -> Result<Response<api::ListVmsResponse>, Status> {
        let state = self.record("ListVms", &request);
        let selector = request.into_inner().label_selector;
        let vms = state
            .machines
            .iter()
            .filter(|machine| {
                state.ignore_selector
                    || selector.iter().all(|pair| {
                        let (key, value) = pair.split_once('=').unwrap();
                        machine.labels.get(key).map(String::as_str) == Some(value)
                    })
            })
            .map(FakeMachine::to_response)
            .collect();
        Ok(Response::new(api::ListVmsResponse { vms }))
    }

    type ExecStream = ExecStream;

    /// `__echo__` commands print `ready`, echo stdin back and exit 0 at stdin EOF;
    /// `__no_exit__` prints and ends the stream without an exit code; anything
    /// else prints `ran` on stdout and `warn` on stderr, exiting 3 when the
    /// command line contains `fail`.
    async fn exec(
        &self,
        request: Request<Streaming<api::ExecChunk>>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        drop(self.record("Exec", &request));
        let mut inbound = request.into_inner();
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty exec stream"))?;
        let hang = {
            let mut state = self.state.lock().unwrap();
            state.commands.push(first.command.clone());
            let hang = first.command == "true" && state.hang_probes > 0;
            if hang {
                state.hang_probes -= 1;
            }
            hang
        };
        let (tx, rx) = mpsc::channel(16);
        if hang {
            // Hold the stream open without answering until the client gives up.
            tokio::spawn(async move {
                while let Ok(Some(_)) = inbound.message().await {}
                drop(tx);
            });
            return Ok(Response::new(Box::pin(ReceiverStream::new(rx))));
        }
        tokio::spawn(async move {
            let out = |data: &[u8], is_stderr: bool| api::ExecChunk {
                data: data.to_vec(),
                is_stderr,
                ..Default::default()
            };
            if first.command.contains("__echo__") {
                tx.send(Ok(out(b"ready\n", false))).await.unwrap();
                while let Ok(Some(chunk)) = inbound.message().await {
                    assert!(chunk.stdin, "stdin chunks must be marked stdin");
                    tx.send(Ok(out(&chunk.data, false))).await.unwrap();
                }
                tx.send(Ok(api::ExecChunk::default())).await.unwrap();
            } else if first.command.contains("__no_exit__") {
                tx.send(Ok(out(b"partial\n", false))).await.unwrap();
            } else {
                tx.send(Ok(out(b"ran\n", false))).await.unwrap();
                tx.send(Ok(out(b"warn\n", true))).await.unwrap();
                let exit_code = if first.command.contains("fail") { 3 } else { 0 };
                tx.send(Ok(api::ExecChunk {
                    exit_code,
                    ..Default::default()
                }))
                .await
                .unwrap();
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn fork_vm(
        &self,
        request: Request<api::ForkVmRequest>,
    ) -> Result<Response<api::ForkVmResponse>, Status> {
        let mut state = self.record("ForkVm", &request);
        let message = request.into_inner();
        state.forks.push(message.clone());
        let vm_id = add_machine(&mut state, message.name.clone(), message.labels);
        Ok(Response::new(api::ForkVmResponse {
            vm_id,
            name: message.name,
            status: "running".into(),
        }))
    }

    async fn resume_vm(
        &self,
        request: Request<api::ResumeVmRequest>,
    ) -> Result<Response<api::ResumeVmResponse>, Status> {
        let mut state = self.record("ResumeVm", &request);
        Self::set_status(&mut state, &request.into_inner().vm_id, "running")?;
        Ok(Response::new(api::ResumeVmResponse {}))
    }

    async fn hibernate_vm(
        &self,
        request: Request<api::HibernateVmRequest>,
    ) -> Result<Response<api::HibernateVmResponse>, Status> {
        let mut state = self.record("HibernateVm", &request);
        let vm_id = request.into_inner().vm_id;
        Self::set_status(&mut state, &vm_id, "hibernated")?;
        state.hibernating.push(vm_id);
        Ok(Response::new(api::HibernateVmResponse {}))
    }

    async fn wake_vm(
        &self,
        request: Request<api::WakeVmRequest>,
    ) -> Result<Response<api::WakeVmResponse>, Status> {
        let mut state = self.record("WakeVm", &request);
        Self::set_status(&mut state, &request.into_inner().vm_id, "running")?;
        Ok(Response::new(api::WakeVmResponse {}))
    }

    async fn create_snapshot(
        &self,
        request: Request<api::CreateSnapshotRequest>,
    ) -> Result<Response<api::CreateSnapshotResponse>, Status> {
        drop(self.record("CreateSnapshot", &request));
        let message = request.into_inner();
        Ok(Response::new(api::CreateSnapshotResponse {
            snapshot_id: "snap-1".into(),
            name: message.name,
            status: "pending".into(),
        }))
    }

    async fn get_snapshot(
        &self,
        request: Request<api::GetSnapshotRequest>,
    ) -> Result<Response<api::GetSnapshotResponse>, Status> {
        let fails = self.record("GetSnapshot", &request).snapshot_fails;
        let message = request.into_inner();
        Ok(Response::new(api::GetSnapshotResponse {
            snapshot: Some(api::SnapshotInfo {
                snapshot_id: message.name.clone(),
                name: message.name,
                version: 1,
                status: if fails { "failed" } else { "ready" }.into(),
                failure_reason: if fails { "disk full" } else { "" }.into(),
            }),
        }))
    }

    async fn delete_snapshot(
        &self,
        request: Request<api::DeleteSnapshotRequest>,
    ) -> Result<Response<api::DeleteSnapshotResponse>, Status> {
        drop(self.record("DeleteSnapshot", &request));
        Ok(Response::new(api::DeleteSnapshotResponse {}))
    }

    async fn create_vm_from_snapshot(
        &self,
        request: Request<api::CreateVmFromSnapshotRequest>,
    ) -> Result<Response<api::CreateVmResponse>, Status> {
        let mut state = self.record("CreateVmFromSnapshot", &request);
        let message = request.into_inner();
        state.restores.push(message.clone());
        let vm_id = add_machine(&mut state, message.name.clone(), message.labels);
        Ok(Response::new(api::CreateVmResponse {
            vm_id,
            name: message.name,
            status: "running".into(),
        }))
    }
}

struct Harness {
    fake: FakeBoxd,
    backend: BoxdSandboxBackend,
    tokens: MockServer,
}

async fn token_server() -> MockServer {
    let server = MockServer::start().await;
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/token"))
        .and(body_json(serde_json::json!({ "api_key": API_KEY })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": TOKEN,
            "expires_at": expires_at,
            "user_id": "usr_test",
        })))
        .expect(..=1)
        .mount(&server)
        .await;
    server
}

async fn serve(fake: FakeBoxd) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(BoxdApiServer::new(fake))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    format!("http://{address}")
}

fn config(api_url: String, token_url: String) -> BoxdConfig {
    BoxdConfig {
        api_key: API_KEY.into(),
        api_url,
        token_url,
        org: Some("acme".into()),
        name_prefix: "exo".into(),
        extra_labels: vec!["team=agents".into()],
    }
}

async fn harness(fake: FakeBoxd) -> Harness {
    harness_with_timeouts(fake, Duration::from_secs(60), Duration::from_secs(10)).await
}

async fn harness_with_timeouts(fake: FakeBoxd, call: Duration, probe: Duration) -> Harness {
    let tokens = token_server().await;
    let api_url = serve(fake.clone()).await;
    let backend = BoxdSandboxBackend::with_timeouts(
        config(api_url, format!("{}/api/v1/auth/token", tokens.uri())),
        call,
        probe,
    )
    .unwrap();
    Harness {
        fake,
        backend,
        tokens,
    }
}

fn make_request(sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        policy: SandboxNetworkPolicy::Unrestricted.into(),
        sandbox_id: sandbox_id.into(),
        scope: ResourceScope::Thread {
            agent_id: Uuid7::now(),
            thread_id: Uuid7::now(),
        },
        spec: SandboxSpec {
            tcp_ports: vec![],
            image: "default".into(),
            resources: None,
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            default_workdir: "/home/exo/workspace".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

fn existing_machine(request: &SandboxRequest, status: &str) -> FakeMachine {
    FakeMachine {
        vm_id: "vm-existing".into(),
        name: machine_name_for_request("exo", request),
        status: status.into(),
        isolated: true,
        labels: HashMap::from([
            (
                WARM_SANDBOX_KEY_LABEL.to_string(),
                request.sandbox_id.clone(),
            ),
            (
                WARM_SANDBOX_SPEC_HASH_LABEL.to_string(),
                sandbox_spec_hash(&request.spec),
            ),
        ]),
    }
}

fn command(argv: &[&str]) -> SandboxCommand {
    SandboxCommand {
        argv: argv.iter().map(|arg| arg.to_string()).collect(),
        env: HashMap::new(),
        display_argv: None,
        cwd: None,
        timeout: None,
    }
}

#[tokio::test]
async fn acquire_creates_an_isolated_labelled_machine_when_missing() {
    let h = harness(FakeBoxd::default()).await;
    let request = make_request("sandbox-a");
    h.backend.acquire(request.clone()).await.unwrap();

    let state = h.fake.state.lock().unwrap();
    assert_eq!(state.creates.len(), 1);
    let create = &state.creates[0];
    assert!(
        create.isolated,
        "sandboxes must always be isolated machines"
    );
    assert_eq!(create.org, "acme");
    assert_eq!(create.name, machine_name_for_request("exo", &request));
    assert!(
        create.config.is_none(),
        "no resources leaves the org default size"
    );
    assert_eq!(create.labels[WARM_SANDBOX_KEY_LABEL], "sandbox-a");
    assert_eq!(
        create.labels[WARM_SANDBOX_SPEC_HASH_LABEL],
        sandbox_spec_hash(&request.spec)
    );
    assert_eq!(create.labels["team"], "agents");
    assert!(
        state
            .commands
            .iter()
            .any(|line| line.contains("mkdir -p /home/exo/workspace")),
        "acquire creates the default workdir: {:?}",
        state.commands
    );
}

#[tokio::test]
async fn every_call_carries_the_exchanged_session_token() {
    let h = harness(FakeBoxd::default()).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    handle.exec(&command(&["true"])).await.unwrap();

    let authorizations = h.fake.state.lock().unwrap().authorizations.clone();
    assert!(!authorizations.is_empty());
    assert!(
        authorizations
            .iter()
            .all(|auth| auth == &format!("Bearer {TOKEN}")),
        "{authorizations:?}"
    );
    let exchanges = h.tokens.received_requests().await.unwrap();
    assert_eq!(exchanges.len(), 1, "the session token is cached");
}

#[tokio::test]
async fn rejected_api_key_names_the_setting() {
    let tokens = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&tokens)
        .await;
    let fake = FakeBoxd::default();
    let backend = BoxdSandboxBackend::new(config(
        serve(fake.clone()).await,
        format!("{}/api/v1/auth/token", tokens.uri()),
    ))
    .unwrap();
    let error = backend
        .acquire(make_request("sandbox-a"))
        .await
        .err()
        .expect("acquire must fail");
    assert!(format!("{error:#}").contains("BOXD_API_KEY"), "{error:#}");
    assert!(
        fake.calls().is_empty(),
        "nothing reaches the API without a token"
    );
}

#[tokio::test]
async fn empty_api_key_is_rejected_up_front() {
    let mut config = config("http://127.0.0.1:1".into(), "http://127.0.0.1:1".into());
    config.api_key.clear();
    let error = BoxdSandboxBackend::new(config).err().expect("empty key");
    assert!(error.to_string().contains("BOXD_API_KEY"), "{error}");
}

#[tokio::test]
async fn acquire_reuses_the_labelled_machine_without_create() {
    let request = make_request("sandbox-a");
    let fake = FakeBoxd::default().with_machine(existing_machine(&request, "running"));
    let h = harness(fake).await;
    h.backend.acquire(request).await.unwrap();
    let calls = h.fake.calls();
    assert!(!calls.contains(&"CreateVm".to_string()), "{calls:?}");
}

#[tokio::test]
async fn acquire_wakes_a_hibernated_machine() {
    let request = make_request("sandbox-a");
    let fake = FakeBoxd::default().with_machine(existing_machine(&request, "hibernated"));
    let h = harness(fake).await;
    h.backend.acquire(request).await.unwrap();
    assert!(h.fake.calls().contains(&"WakeVm".to_string()));
}

#[tokio::test]
async fn acquire_starts_a_stopped_machine() {
    let request = make_request("sandbox-a");
    let fake = FakeBoxd::default().with_machine(existing_machine(&request, "stopped"));
    let h = harness(fake).await;
    h.backend.acquire(request).await.unwrap();
    assert!(h.fake.calls().contains(&"StartVm".to_string()));
}

#[tokio::test]
async fn acquire_resumes_a_machine_in_standby() {
    let request = make_request("sandbox-a");
    let fake = FakeBoxd::default().with_machine(existing_machine(&request, "suspended"));
    let h = harness(fake).await;
    h.backend.acquire(request).await.unwrap();
    assert!(h.fake.calls().contains(&"ResumeVm".to_string()));
}

#[tokio::test]
async fn create_conflict_adopts_the_machine_a_lost_create_made() {
    let fake = FakeBoxd::default();
    fake.state.lock().unwrap().create_conflict = true;
    let h = harness(fake).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    handle.exec(&command(&["true"])).await.unwrap();
    assert_eq!(h.fake.state.lock().unwrap().machines.len(), 1);
}

#[tokio::test]
async fn acquire_rejects_host_mounts_before_any_call() {
    let h = harness(FakeBoxd::default()).await;
    let mut request = make_request("sandbox-a");
    request.spec.mounts.push(SandboxMount {
        host_path: PathBuf::from("/tmp"),
        guest_path: "/mnt".into(),
        access: SandboxMountAccess::ReadOnly,
        internal: false,
    });
    let error = h.backend.acquire(request).await.err().expect("mounts");
    assert!(format!("{error:#}").contains("bind-mounts"), "{error:#}");
    assert!(h.fake.calls().is_empty());
}

#[tokio::test]
async fn disabled_networking_fails_before_any_call() {
    let h = harness(FakeBoxd::default()).await;
    let mut request = make_request("sandbox-a");
    request.policy = SandboxNetworkPolicy::Disabled.into();
    let error = h.backend.acquire(request).await.err().expect("networking");
    assert!(format!("{error:#}").contains("networking"), "{error:#}");
    assert!(h.fake.calls().is_empty());
}

#[tokio::test]
async fn resources_round_up_to_a_boxd_size() {
    let h = harness(FakeBoxd::default()).await;
    let mut request = make_request("sandbox-a");
    request.spec.resources = Some(SandboxResourceShape {
        vcpu_count: NonZeroU8::new(2).unwrap(),
        memory_mib: NonZeroU32::new(4096).unwrap(),
    });
    h.backend.acquire(request).await.unwrap();
    let config = h.fake.state.lock().unwrap().creates[0].config.unwrap();
    assert_eq!(config.vcpu, 2);
    assert_eq!(config.memory_bytes, 8 * 1024 * 1024 * 1024);
}

#[tokio::test]
async fn resources_beyond_the_largest_size_are_rejected() {
    let h = harness(FakeBoxd::default()).await;
    let mut request = make_request("sandbox-a");
    request.spec.resources = Some(SandboxResourceShape {
        vcpu_count: NonZeroU8::new(16).unwrap(),
        memory_mib: NonZeroU32::new(4096).unwrap(),
    });
    let error = h.backend.acquire(request).await.err().expect("too big");
    assert!(format!("{error:#}").contains("machine size"), "{error:#}");
}

#[tokio::test]
async fn exec_quotes_argv_and_sends_cwd_and_env_with_the_command() {
    let h = harness(FakeBoxd::default()).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    let mut cmd = command(&["printf", "%s", "it's here"]);
    cmd.env.insert("GREETING".into(), "hello world".into());
    cmd.cwd = Some("/tmp/my dir".into());
    let output = handle.exec(&cmd).await.unwrap();

    assert!(output.ok);
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "ran\n");
    assert_eq!(output.stderr, "warn\n");
    assert_eq!(output.cwd, "/tmp/my dir");
    let state = h.fake.state.lock().unwrap();
    assert_eq!(
        state.commands.last().unwrap(),
        "cd '/tmp/my dir' && GREETING='hello world' printf '%s' 'it'\\''s here'"
    );
}

#[tokio::test]
async fn exec_runs_in_the_default_workdir_and_reports_the_exit_code() {
    let h = harness(FakeBoxd::default()).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    let output = handle.exec(&command(&["sh", "-c", "fail"])).await.unwrap();
    assert!(!output.ok);
    assert_eq!(output.exit_code, Some(3));
    assert_eq!(output.cwd, "/home/exo/workspace");
    assert!(
        h.fake
            .state
            .lock()
            .unwrap()
            .commands
            .last()
            .unwrap()
            .starts_with("cd /home/exo/workspace && sh -c fail")
    );
}

#[tokio::test]
async fn exec_rejects_env_names_the_shell_cannot_assign() {
    let h = harness(FakeBoxd::default()).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    let mut cmd = command(&["true"]);
    cmd.env.insert("BAD;NAME".into(), "x".into());
    let error = handle.exec(&cmd).await.expect_err("bad env name");
    assert!(error.to_string().contains("BAD;NAME"), "{error}");
}

#[tokio::test]
async fn start_process_streams_stdout_and_forwards_stdin_until_eof() {
    let h = harness(FakeBoxd::default()).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    let mut process = handle
        .start_process(&command(&["sh", "-c", "__echo__"]))
        .await
        .unwrap();

    let mut ready = [0u8; 6];
    tokio::time::timeout(
        Duration::from_secs(5),
        process.stdout.read_exact(&mut ready),
    )
    .await
    .expect("stdout streams before stdin is written")
    .unwrap();
    assert_eq!(&ready, b"ready\n");

    process.stdin.write_all(b"ping\n").await.unwrap();
    let mut echoed = [0u8; 5];
    tokio::time::timeout(
        Duration::from_secs(5),
        process.stdout.read_exact(&mut echoed),
    )
    .await
    .expect("stdin reaches the process while it runs")
    .unwrap();
    assert_eq!(&echoed, b"ping\n");

    process.stdin.close().await.unwrap();
    let exit_code = tokio::time::timeout(Duration::from_secs(5), process.wait)
        .await
        .expect("closing stdin ends the process")
        .unwrap();
    assert_eq!(exit_code, 0);
}

#[tokio::test]
async fn stop_returns_once_the_machine_is_hibernated_and_acquire_wakes_it() {
    let h = harness(FakeBoxd::default()).await;
    let request = make_request("sandbox-a");
    let handle = h.backend.acquire(request.clone()).await.unwrap();
    handle.stop().await.unwrap();
    {
        let state = h.fake.state.lock().unwrap();
        assert_eq!(state.machines[0].status, "hibernated");
        assert!(
            state.hibernating.is_empty(),
            "stop must wait until boxd reports the machine hibernated"
        );
    }

    h.backend.acquire(request.clone()).await.unwrap();
    let calls = h.fake.calls();
    assert!(calls.contains(&"WakeVm".to_string()), "{calls:?}");
    assert_eq!(
        calls.iter().filter(|call| *call == "CreateVm").count(),
        1,
        "{calls:?}"
    );

    h.backend.stop(request).await.unwrap();
    assert_eq!(
        h.fake.state.lock().unwrap().machines[0].status,
        "hibernated"
    );
}

#[tokio::test]
async fn terminate_destroys_every_machine_of_the_sandbox_and_is_idempotent() {
    let h = harness(FakeBoxd::default()).await;
    let request = make_request("sandbox-a");
    h.backend.acquire(request.clone()).await.unwrap();
    let mut other_spec = request.clone();
    other_spec.spec.default_workdir = "/srv".into();
    h.backend.acquire(other_spec).await.unwrap();
    h.backend.acquire(make_request("sandbox-b")).await.unwrap();
    assert_eq!(h.fake.state.lock().unwrap().machines.len(), 3);

    h.backend.terminate(request.clone()).await.unwrap();
    let remaining: Vec<_> = h
        .fake
        .state
        .lock()
        .unwrap()
        .machines
        .iter()
        .map(|machine| machine.labels[WARM_SANDBOX_KEY_LABEL].clone())
        .collect();
    assert_eq!(remaining, vec!["sandbox-b".to_string()]);

    h.backend.terminate(request).await.unwrap();
    h.backend
        .terminate_existing("sandbox-b", None)
        .await
        .unwrap();
    assert!(h.fake.state.lock().unwrap().machines.is_empty());
}

#[tokio::test]
async fn full_snapshot_round_trips_through_a_boxd_ref_manifest() {
    let h = harness(FakeBoxd::default()).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    let payload = handle.snapshot(SnapshotKind::Full).await.unwrap();
    assert_eq!(payload.format, SnapshotFormat::BoxdRef);
    let manifest: serde_json::Value = serde_json::from_slice(&payload.bytes).unwrap();
    assert_eq!(manifest["snapshot_id"], "snap-1");
    assert!(h.fake.calls().contains(&"GetSnapshot".to_string()));

    let restored = make_request("sandbox-restored");
    h.backend
        .acquire_from_snapshot(restored.clone(), payload.clone())
        .await
        .unwrap();
    {
        let state = h.fake.state.lock().unwrap();
        let restore = &state.restores[0];
        assert_eq!(restore.snapshot, "snap-1");
        assert!(restore.isolated);
        assert_eq!(restore.org, "acme");
        assert_eq!(restore.labels[WARM_SANDBOX_KEY_LABEL], "sandbox-restored");
    }

    h.backend.delete_snapshot(payload).await.unwrap();
    assert!(h.fake.calls().contains(&"DeleteSnapshot".to_string()));
}

#[tokio::test]
async fn filesystem_snapshots_are_rejected() {
    let h = harness(FakeBoxd::default()).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    let error = handle
        .snapshot(SnapshotKind::Filesystem)
        .await
        .expect_err("filesystem kind");
    assert!(error.to_string().contains("snapshot kind"), "{error}");
    assert!(!h.fake.calls().contains(&"CreateSnapshot".to_string()));
}

#[tokio::test]
async fn acquire_from_snapshot_rejects_other_formats() {
    let h = harness(FakeBoxd::default()).await;
    let error = h
        .backend
        .acquire_from_snapshot(
            make_request("sandbox-a"),
            SnapshotPayload {
                format: SnapshotFormat::SpritesRef,
                bytes: Bytes::from_static(b"{}"),
            },
        )
        .await
        .err()
        .expect("wrong format");
    assert!(format!("{error:#}").contains("boxd-ref"), "{error:#}");
}

#[tokio::test]
async fn fork_copies_the_source_machine_into_an_isolated_target() {
    let h = harness(FakeBoxd::default()).await;
    let source = make_request("sandbox-a");
    h.backend.acquire(source.clone()).await.unwrap();
    let target = make_request("sandbox-b");
    h.backend
        .fork_sandbox(source, target.clone())
        .await
        .unwrap();
    let state = h.fake.state.lock().unwrap();
    let fork = &state.forks[0];
    assert_eq!(fork.source_vm_id, "vm-1");
    assert!(fork.isolated);
    assert_eq!(fork.name, machine_name_for_request("exo", &target));
    assert_eq!(fork.labels[WARM_SANDBOX_KEY_LABEL], "sandbox-b");
}

#[test]
fn config_debug_hides_the_api_key() {
    let config = config("https://boxd.sh:9443".into(), "https://app.boxd.sh".into());
    assert!(!format!("{config:?}").contains(API_KEY));
}

#[tokio::test]
async fn machines_of_other_sandboxes_are_never_used_or_destroyed() {
    let other = make_request("someone-else");
    let fake = FakeBoxd::default().with_machine(existing_machine(&other, "running"));
    fake.state.lock().unwrap().ignore_selector = true;
    let h = harness(fake).await;

    let request = make_request("sandbox-a");
    h.backend.acquire(request.clone()).await.unwrap();
    assert_eq!(
        h.fake.state.lock().unwrap().creates.len(),
        1,
        "a machine labelled for another sandbox is not reused"
    );

    h.backend.terminate(request).await.unwrap();
    let remaining: Vec<_> = h
        .fake
        .state
        .lock()
        .unwrap()
        .machines
        .iter()
        .map(|machine| machine.vm_id.clone())
        .collect();
    assert_eq!(remaining, vec!["vm-existing".to_string()]);
}

#[tokio::test]
async fn exec_without_an_exit_code_is_an_error_not_success() {
    let h = harness(FakeBoxd::default()).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    let error = handle
        .exec(&command(&["sh", "-c", "__no_exit__"]))
        .await
        .expect_err("a stream that ends without an exit code");
    assert!(
        error.to_string().contains("without an exit code"),
        "{error}"
    );

    let process = handle
        .start_process(&command(&["sh", "-c", "__no_exit__"]))
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), process.wait)
        .await
        .unwrap()
        .expect_err("wait must not report success");
    assert!(
        error.to_string().contains("without an exit code"),
        "{error}"
    );
}

#[tokio::test]
async fn a_hung_readiness_probe_is_retried_instead_of_stalling_acquire() {
    let fake = FakeBoxd::default();
    fake.state.lock().unwrap().hang_probes = 1;
    let h = harness_with_timeouts(fake, Duration::from_secs(60), Duration::from_millis(200)).await;
    tokio::time::timeout(
        Duration::from_secs(10),
        h.backend.acquire(make_request("sandbox-a")),
    )
    .await
    .expect("acquire must not wait on a probe that never answers")
    .unwrap();
}

#[tokio::test]
async fn a_hung_lifecycle_call_times_out() {
    let fake = FakeBoxd::default();
    let h = harness_with_timeouts(fake, Duration::from_millis(300), Duration::from_secs(10)).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    h.fake.state.lock().unwrap().hang_get_vm = true;
    let result = tokio::time::timeout(Duration::from_secs(10), handle.is_running()).await;
    let error = result
        .expect("the call is bounded by the call timeout")
        .expect_err("a call that never answers fails");
    assert!(format!("{error:#}").contains("get machine"), "{error:#}");
}

#[tokio::test]
async fn a_failed_snapshot_is_deleted_and_reported() {
    let h = harness(FakeBoxd::default()).await;
    let handle = h.backend.acquire(make_request("sandbox-a")).await.unwrap();
    h.fake.state.lock().unwrap().snapshot_fails = true;
    let error = handle
        .snapshot(SnapshotKind::Full)
        .await
        .expect_err("snapshot fails");
    assert!(error.to_string().contains("disk full"), "{error}");
    assert!(
        h.fake.calls().contains(&"DeleteSnapshot".to_string()),
        "the unfinished snapshot is removed"
    );
}
