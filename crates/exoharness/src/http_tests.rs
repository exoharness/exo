use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use futures::io::AsyncReadExt;
use tempfile::TempDir;

use crate::test_support::local_test_config;
use crate::{
    BasicExoHarness, CreateSandboxRequest, ExoHarness, HttpExoHarness, RunInSandboxRequest,
    SandboxProcessEvent, SandboxProcessEventQuery, SandboxProcessStatus, SandboxProcessStdin,
    SandboxProvider, StartSandboxProcessRequest, WaitSandboxProcessRequest,
    WriteSandboxProcessInputRequest, serve_exoharness_http_listener,
};

struct HttpHarnessFixture {
    harness: Arc<dyn ExoHarness>,
    server: actix_web::rt::task::JoinHandle<crate::Result<()>>,
    _tempdir: TempDir,
}

impl Drop for HttpHarnessFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn http_harness() -> HttpHarnessFixture {
    let tempdir = TempDir::new().expect("tempdir");
    let basic = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("basic harness");
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("listener");
    let addr = listener.local_addr().expect("local addr");
    let server = actix_web::rt::spawn(serve_exoharness_http_listener(listener, Arc::new(basic)));
    let harness: Arc<dyn ExoHarness> =
        Arc::new(HttpExoHarness::new(format!("http://{addr}")).expect("http harness"));

    HttpHarnessFixture {
        harness,
        server,
        _tempdir: tempdir,
    }
}

#[actix_web::test]
async fn http_exoharness_supports_agent_and_conversation_crud() {
    let fixture = http_harness().await;
    crate::contract_tests::supports_agent_and_conversation_crud(Arc::clone(&fixture.harness)).await;
}

#[actix_web::test]
async fn http_exoharness_begin_turn_tracks_events_through_finish() {
    let fixture = http_harness().await;
    crate::contract_tests::begin_turn_tracks_events_through_finish(Arc::clone(&fixture.harness))
        .await;
}

#[actix_web::test]
async fn http_exoharness_turn_events_continue_after_artifact_writes() {
    let fixture = http_harness().await;
    crate::contract_tests::turn_events_continue_after_artifact_writes(Arc::clone(&fixture.harness))
        .await;
}

#[actix_web::test]
async fn http_exoharness_conversation_scope_overrides_and_forks() {
    let fixture = http_harness().await;
    crate::contract_tests::conversation_scope_overrides_agent_scope_and_fork_copies_bindings(
        Arc::clone(&fixture.harness),
    )
    .await;
}

#[actix_web::test]
#[ignore = "set EXO_CONTRACT_TEST_URL and optional EXO_CONTRACT_TEST_BEARER or EXO_CONTRACT_TEST_BEARER_ENV"]
async fn hosted_http_exoharness_core_contract() {
    let harness = hosted_harness_from_env();
    crate::contract_tests::supports_agent_and_conversation_crud(Arc::clone(&harness)).await;
    crate::contract_tests::begin_turn_tracks_events_through_finish(Arc::clone(&harness)).await;
    crate::contract_tests::turn_events_continue_after_artifact_writes(Arc::clone(&harness)).await;
}

#[actix_web::test]
async fn http_exoharness_runs_noninteractive_sandbox_commands() {
    let fixture = http_harness().await;
    let agent = fixture
        .harness
        .new_agent(crate::NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(crate::NewConversationRequest::default())
        .await
        .expect("conversation");
    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "local".to_string(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox");
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "printf hello".to_string(),
            ],
            env: Default::default(),
        })
        .await
        .expect("sandbox command");
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut output = Vec::new();
    stdout.read_to_end(&mut output).await.expect("stdout");
    assert_eq!(output, b"hello");
    assert_eq!(parts.wait.await.expect("exit"), 0);
}

#[actix_web::test]
async fn http_exoharness_supports_sandbox_process_events() {
    let fixture = http_harness().await;
    let agent = fixture
        .harness
        .new_agent(crate::NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(crate::NewConversationRequest::default())
        .await
        .expect("conversation");
    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "local".to_string(),
            default_workdir: Some("/".to_string()),
            file_system_mounts: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox");
    let process = conversation
        .start_sandbox_process(StartSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            command: vec!["/bin/sh".to_string(), "-lc".to_string(), "cat".to_string()],
            env: Default::default(),
            cwd: None,
            mode: Default::default(),
            stdin: SandboxProcessStdin::Open,
            output: Default::default(),
            lifecycle: Default::default(),
        })
        .await
        .expect("process");
    conversation
        .write_sandbox_process_input(WriteSandboxProcessInputRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
            data: b"hello http process".to_vec(),
        })
        .await
        .expect("stdin write");
    conversation
        .close_sandbox_process_input(crate::CloseSandboxProcessInputRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
        })
        .await
        .expect("stdin close");
    let status = conversation
        .wait_sandbox_process(WaitSandboxProcessRequest {
            sandbox_id: sandbox_id.clone(),
            process_id: process.id.clone(),
        })
        .await
        .expect("wait");
    assert_eq!(status, SandboxProcessStatus::Exited { exit_code: 0 });

    let events = conversation
        .get_sandbox_process_events(SandboxProcessEventQuery {
            sandbox_id,
            process_id: process.id,
            after: None,
            limit: None,
            follow: None,
        })
        .await
        .expect("events");
    assert!(events.events.iter().any(|event| matches!(
        event,
        SandboxProcessEvent::Stdout { data, .. }
            if String::from_utf8_lossy(data).contains("hello http process")
    )));
    assert!(matches!(
        events.events.last(),
        Some(SandboxProcessEvent::Exit { exit_code: 0, .. })
    ));
}

fn hosted_harness_from_env() -> Arc<dyn ExoHarness> {
    let url = std::env::var("EXO_CONTRACT_TEST_URL")
        .expect("EXO_CONTRACT_TEST_URL must point at an ExoHarness HTTP endpoint");
    let mut harness = HttpExoHarness::new(url).expect("hosted http harness");
    if let Ok(token) = std::env::var("EXO_CONTRACT_TEST_BEARER") {
        harness = harness.with_bearer_token(token);
    } else if let Ok(env_name) = std::env::var("EXO_CONTRACT_TEST_BEARER_ENV") {
        let token = std::env::var(&env_name).unwrap_or_else(|_| {
            panic!("EXO_CONTRACT_TEST_BEARER_ENV references unset environment variable {env_name}")
        });
        harness = harness.with_bearer_token(token);
    }
    Arc::new(harness)
}
