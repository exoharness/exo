//! Cross-process E2B resume through `ensure_shell_sandbox` / `run_in_sandbox`.
//!
//! Regression coverage for:
//! - metadata list filter encoding (`try_resume` finds the VM)
//! - `state=running,paused` list query
//! - no second `sandbox_created` when the E2B VM is still reachable

use std::path::Path;
use std::sync::Arc;

use executor::{AgentConfig, AgentHarnessKind, BasicToolRuntime, ConversationConfig, ToolRuntime};
use exoharness::{
    AgentHandle, AgentId, BasicExoHarness, BasicExoHarnessConfig, ConversationHandle,
    ConversationId, E2bConfig, EventData, EventKind, EventQuery, EventQueryDirection, ExoHarness,
    NewAgentRequest, NewConversationRequest, RunInSandboxRequest, SandboxBackendChoice,
    SandboxId, SandboxProcessParts, SecretBackendChoice,
};
use futures::io::AsyncReadExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const E2B_SANDBOX_ID: &str = "sb-resume-test";

fn e2b_harness_config(root: &Path, server: &MockServer) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.to_path_buf(),
        secret_backend: SecretBackendChoice::Static([8u8; 32]),
        sandbox_backend: SandboxBackendChoice::E2b(E2bConfig {
            api_key: "test-api-key".into(),
            api_url: server.uri(),
            template_id: "base".into(),
            envd_port: 49_983,
            envd_base_url: Some(server.uri()),
            secure: false,
        }),
    }
}

fn sandbox_created_json() -> Value {
    json!({
        "sandboxID": E2B_SANDBOX_ID,
        "templateID": "base",
        "envdVersion": "0.1.0",
    })
}

fn listed_running_sandbox_json() -> Value {
    json!([{
        "sandboxID": E2B_SANDBOX_ID,
        "templateID": "base",
        "state": "running",
        "startedAt": "2026-06-01T12:00:00Z",
    }])
}

fn connect_enveloped_stream(messages: &[(u8, Value)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (flags, value) in messages {
        let payload = serde_json::to_vec(value).expect("json");
        out.push(*flags);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
    }
    out
}

fn connect_exit_ok() -> Vec<u8> {
    connect_enveloped_stream(&[(
        2,
        json!({"event": {"end": {"status": "exit status 0"}}}),
    )])
}

fn connect_stdout_and_exit(stdout: &str) -> Vec<u8> {
    connect_enveloped_stream(&[
        (
            0,
            json!({"event": {"data": {"stdout": stdout}}}),
        ),
        (
            2,
            json!({"event": {"end": {"status": "exit status 0"}}}),
        ),
    ])
}

async fn mount_e2b_mocks(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/sandboxes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(sandbox_created_json()))
        .expect(1)
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v2/sandboxes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(listed_running_sandbox_json()))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/process.Process/Start"))
        .respond_with(|req: &wiremock::Request| {
            let body = String::from_utf8_lossy(&req.body);
            let template = if body.contains("cat /tmp/x") {
                ResponseTemplate::new(200).set_body_raw(
                    connect_stdout_and_exit("tier-1-marker\n"),
                    "application/connect+json",
                )
            } else {
                ResponseTemplate::new(200).set_body_raw(connect_exit_ok(), "application/connect+json")
            };
            template
        })
        .mount(server)
        .await;
}

fn test_agent_config() -> AgentConfig {
    AgentConfig {
        instructions: Vec::new(),
        harness: AgentHarnessKind::Basic,
        typescript: None,
        enable_agent_tool_creation: false,
        sandbox_image: Some("base".into()),
        enable_networking: false,
        model: "e2b-resume-test".into(),
        max_output_tokens: None,
        max_tool_round_trips: None,
        braintrust: None,
    }
}

fn test_conv_config() -> ConversationConfig {
    ConversationConfig {
        enable_networking: false,
        shell_program: Some("bash".into()),
        mounts: Vec::new(),
    }
}

async fn prepare(conv: &dyn ConversationHandle) {
    BasicToolRuntime
        .prepare_conversation(conv, &test_agent_config(), &test_conv_config())
        .await
        .expect("prepare_conversation");
}

async fn exec_shell(conv: &dyn ConversationHandle, sandbox_id: &SandboxId, cmd: &str) -> (i32, String) {
    let process = conv
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id.clone(),
            command: vec!["bash".into(), "-lc".into(), cmd.into()],
            env: Default::default(),
        })
        .await
        .unwrap_or_else(|error| panic!("run_in_sandbox({cmd:?}) failed: {error:#}"));
    let mut parts: SandboxProcessParts = process.into_parts();
    drop(parts.stdin);
    let mut stdout = Vec::new();
    let (read_stdout, wait) = tokio::join!(parts.stdout.read_to_end(&mut stdout), parts.wait);
    read_stdout.expect("read stdout");
    let exit_code = wait.expect("wait");
    (
        exit_code,
        String::from_utf8_lossy(&stdout).into_owned(),
    )
}

async fn latest_sandbox_id(conv: &dyn ConversationHandle) -> SandboxId {
    let result = conv
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(1),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .expect("get_events");
    let event = result.events.first().expect("sandbox_created present");
    match &event.data {
        EventData::SandboxCreated { sandbox_id, .. } => sandbox_id.clone(),
        other => panic!("unexpected event: {other:?}"),
    }
}

async fn count_sandbox_created(conv: &dyn ConversationHandle) -> usize {
    let mut cursor = None;
    let mut count = 0usize;
    loop {
        let result = conv
            .get_events(Some(EventQuery {
                cursor,
                direction: Some(EventQueryDirection::Asc),
                limit: Some(100),
                session_id: None,
                turn_id: None,
                types: Some(vec![EventKind::SANDBOX_CREATED]),
            }))
            .await
            .expect("get_events");
        let done = result.events.is_empty();
        count += result.events.len();
        if done || result.cursor.is_none() {
            break;
        }
        cursor = result.cursor;
    }
    count
}

async fn setup_agent_and_conversation(
    harness: &BasicExoHarness,
) -> (AgentId, ConversationId, Arc<dyn ConversationHandle>) {
    let agent: Arc<dyn AgentHandle> = harness
        .new_agent(NewAgentRequest {
            slug: "e2b-resume-agent".into(),
            name: "e2b-resume-agent".into(),
        })
        .await
        .expect("new_agent");
    let agent_id = agent.record().id;
    let conv: Arc<dyn ConversationHandle> = agent
        .new_conversation(NewConversationRequest {
            slug: Some("e2b-resume-conv".into()),
            name: Some("e2b-resume-conv".into()),
        })
        .await
        .expect("new_conversation");
    let conv_id = conv.record().id;
    (agent_id, conv_id, conv)
}

async fn open_conversation(
    harness: &BasicExoHarness,
    agent_id: &AgentId,
    conv_id: &ConversationId,
) -> Arc<dyn ConversationHandle> {
    let agent: Arc<dyn AgentHandle> = harness
        .get_agent(agent_id)
        .await
        .expect("get_agent")
        .expect("agent");
    agent
        .get_conversation(conv_id)
        .await
        .expect("get_conversation")
        .expect("conversation")
}

#[tokio::test]
async fn e2b_cross_process_resume_keeps_single_sandbox_created_event() {
    let server = MockServer::start().await;
    mount_e2b_mocks(&server).await;
    let root = TempDir::new().expect("tempdir");

    let (agent_id, conv_id, sandbox_id) = {
        let harness = BasicExoHarness::new(e2b_harness_config(root.path(), &server))
            .await
            .expect("harness");
        let (agent_id, conv_id, conv) = setup_agent_and_conversation(&harness).await;
        prepare(conv.as_ref()).await;
        let sandbox_id: String = latest_sandbox_id(conv.as_ref()).await;
        let (rc, stdout) = exec_shell(
            conv.as_ref(),
            &sandbox_id,
            "echo exo-marker > /tmp/x",
        )
        .await;
        assert_eq!(rc, 0, "write marker failed: stdout={stdout:?}");
        (agent_id, conv_id, sandbox_id)
    };

    {
        let harness = BasicExoHarness::new(e2b_harness_config(root.path(), &server))
            .await
            .expect("harness");
        let conv = open_conversation(&harness, &agent_id, &conv_id).await;
        prepare(conv.as_ref()).await;

        let containers_created = count_sandbox_created(conv.as_ref()).await;
        assert_eq!(
            containers_created, 1,
            "resume must not call create_sandbox again"
        );

        let (rc, stdout) = exec_shell(conv.as_ref(), &sandbox_id, "cat /tmp/x").await;
        assert_eq!(rc, 0);
        assert_eq!(stdout.trim(), "tier-1-marker");

        let list_requests: Vec<_> = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method == "GET" && r.url.path() == "/v2/sandboxes")
            .collect();
        assert!(
            !list_requests.is_empty(),
            "second process should list sandboxes via try_resume"
        );
        let query = list_requests[0]
            .url
            .query()
            .expect("list sandboxes query");
        assert!(
            !query.contains("%253A"),
            "metadata must not be double-encoded: {query}"
        );
        assert!(
            query.contains("state=running%2Cpaused") || query.contains("state=running,paused"),
            "expected state filter: {query}"
        );
    }
}
