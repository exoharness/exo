//! Integration test exercising the real `exo` binary against:
//!   - a real sandbox provider (docker / apple-container), and
//!   - a wiremock-backed fake OpenAI Responses endpoint.
//!
//! `#[ignore]`'d so `cargo test` skips it by default; the integration workflow
//! runs `cargo test --workspace -- --ignored` and selects the provider via the
//! `EXO_TEST_SANDBOX_BACKEND` env var (defaults to `docker`), matching the
//! matrix cells in `.github/workflows/integration.yml`. The secret backend is
//! always `file`, with the master key materialised inside a per-test tempdir
//! via `XDG_CONFIG_HOME`.

use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SandboxProvider {
    LocalProcess,
    Docker,
    AppleContainer,
}

impl SandboxProvider {
    fn from_env() -> Self {
        let raw = std::env::var("EXO_TEST_SANDBOX_BACKEND").unwrap_or_else(|_| "docker".into());
        match raw.as_str() {
            "local-process" => Self::LocalProcess,
            "docker" => Self::Docker,
            "apple-container" => Self::AppleContainer,
            other => panic!("unknown EXO_TEST_SANDBOX_BACKEND={other}"),
        }
    }

    fn cli_arg(self) -> &'static str {
        match self {
            Self::LocalProcess => "local-process",
            Self::Docker => "docker",
            Self::AppleContainer => "apple-container",
        }
    }

    fn runtime_available(self) -> bool {
        match self {
            Self::LocalProcess => true,
            Self::Docker => Command::new("docker")
                .arg("info")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
            Self::AppleContainer => Command::new("container")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
        }
    }
}

fn exo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_exo"))
}

fn run_exo(args: &[&str], root: &str, xdg: &str) -> std::process::Output {
    let output = Command::new(exo_bin())
        .args(["--root", root])
        .args(["--secret-backend", "file"])
        .args(args)
        .env("XDG_CONFIG_HOME", xdg)
        .env("OPENAI_API_KEY", "sk-test-key")
        .output()
        .expect("failed to spawn exo");
    if !output.status.success() {
        panic!(
            "exo {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    output
}

fn canned_response_body() -> Value {
    json!({
        "id": "resp_test_abc123",
        "object": "response",
        "status": "completed",
        "created_at": 1_700_000_000_u64,
        "model": "gpt-test",
        "output": [
            {
                "type": "message",
                "id": "msg_test_xyz",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": "Hello from the mock OpenAI server.",
                        "annotations": []
                    }
                ]
            }
        ],
        "usage": {
            "input_tokens": 5,
            "output_tokens": 7,
            "total_tokens": 12
        }
    })
}

/// Runs the `secret set` → `model register` → `agent create` →
/// `conversation create` provisioning flow shared by every test below.
fn provision_agent(root: &str, xdg: &str, mock_uri: &str, provider: SandboxProvider) {
    run_exo(
        &["secret", "set", "test-key", "--env", "OPENAI_API_KEY"],
        root,
        xdg,
    );
    run_exo(
        &[
            "model",
            "register",
            "gpt-test",
            "--secret",
            "test-key",
            "--base-url",
            mock_uri,
        ],
        root,
        xdg,
    );
    run_exo(
        &[
            "agent",
            "create",
            "--slug",
            "test-agent",
            "--model",
            "gpt-test",
            "--sandbox-provider",
            provider.cli_arg(),
            "Integration Test Agent",
        ],
        root,
        xdg,
    );
    run_exo(
        &["conversation", "create", "test-agent", "first"],
        root,
        xdg,
    );
}

/// First model turn: ask the executor to run a shell command in the sandbox.
fn canned_tool_call_body() -> Value {
    json!({
        "id": "resp_test_tool_call",
        "object": "response",
        "status": "completed",
        "created_at": 1_700_000_000_u64,
        "model": "gpt-test",
        "output": [
            {
                "type": "function_call",
                "id": "fc_test_1",
                "call_id": "call_test_1",
                "name": "shell",
                "arguments": "{\"command\":\"echo tool-roundtrip-ok\"}",
                "status": "completed"
            }
        ],
        "usage": {
            "input_tokens": 5,
            "output_tokens": 7,
            "total_tokens": 12
        }
    })
}

/// Second model turn: final assistant message after seeing the tool result.
fn canned_tool_followup_body() -> Value {
    json!({
        "id": "resp_test_tool_followup",
        "object": "response",
        "status": "completed",
        "created_at": 1_700_000_001_u64,
        "model": "gpt-test",
        "output": [
            {
                "type": "message",
                "id": "msg_test_followup",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": "The shell tool reported success.",
                        "annotations": []
                    }
                ]
            }
        ],
        "usage": {
            "input_tokens": 20,
            "output_tokens": 8,
            "total_tokens": 28
        }
    })
}

#[tokio::test]
#[ignore = "spawns real exo binary + real sandbox + wiremock; run with cargo test -- --ignored"]
async fn conversation_send_round_trips_through_real_sandbox_and_mocked_openai() {
    let provider = SandboxProvider::from_env();
    if !provider.runtime_available() {
        println!(
            "sandbox provider {:?} not available on this runner, skipping",
            provider
        );
        return;
    }

    let root_dir = TempDir::new().expect("tempdir for --root");
    let xdg_dir = TempDir::new().expect("tempdir for XDG_CONFIG_HOME");
    let root = root_dir.path().to_string_lossy().into_owned();
    let xdg = xdg_dir.path().to_string_lossy().into_owned();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(canned_response_body()))
        .mount(&mock_server)
        .await;

    provision_agent(&root, &xdg, &mock_server.uri(), provider);

    let output = run_exo(
        &["conversation", "send", "test-agent", "first", "hello there"],
        &root,
        &xdg,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Hello from the mock OpenAI server."),
        "expected mocked assistant text in stdout; got: {stdout}"
    );

    let recorded = mock_server.received_requests().await.unwrap_or_default();
    let responses_calls = recorded
        .iter()
        .filter(|r| r.url.path() == "/responses")
        .count();
    assert_eq!(
        responses_calls,
        1,
        "expected exactly one POST /responses; got {responses_calls} (all paths: {:?})",
        recorded
            .iter()
            .map(|r| r.url.path().to_string())
            .collect::<Vec<_>>()
    );

    let conv_root = root_dir
        .path()
        .join("exoharness/agents")
        .read_dir()
        .expect("agents dir exists")
        .next()
        .expect("at least one agent")
        .unwrap()
        .path()
        .join("conversations");
    let conv_dir = conv_root
        .read_dir()
        .expect("conversations dir exists")
        .next()
        .expect("at least one conversation")
        .unwrap()
        .path();
    let events_dir = conv_dir.join("events");
    let mut found_assistant_text = false;
    for entry in events_dir.read_dir().expect("events dir exists").flatten() {
        let raw = std::fs::read(entry.path()).expect("event file readable");
        let event: Value = serde_json::from_slice(&raw).expect("event is valid json");
        let Some(messages) = event
            .pointer("/data/messages")
            .and_then(Value::as_array)
            .cloned()
        else {
            continue;
        };
        for message in messages {
            if message.get("role").and_then(Value::as_str) == Some("assistant") {
                let text = serde_json::to_string(&message).unwrap_or_default();
                if text.contains("Hello from the mock OpenAI server.") {
                    found_assistant_text = true;
                }
            }
        }
    }
    assert!(
        found_assistant_text,
        "expected mocked assistant text in persisted events under {}",
        events_dir.display()
    );

    if provider == SandboxProvider::Docker {
        let leftover_containers = Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                "label=exo.sandbox.owner-pid",
                "--filter",
                "status=exited",
            ])
            .output()
            .expect("docker ps");
        let stdout = String::from_utf8_lossy(&leftover_containers.stdout);
        let stale = stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>();
        assert!(
            stale.is_empty(),
            "expected zero leftover Exited exo containers after binary exit; found: {stale:?}"
        );
    }
}

/// Exercises the full tool-call loop through the real binary: the mocked model
/// requests the `shell` tool, the executor runs it inside the real sandbox,
/// feeds the output back, and the mocked model answers with a final message.
#[tokio::test]
#[ignore = "spawns real exo binary + real sandbox + wiremock; run with cargo test -- --ignored"]
async fn conversation_send_executes_tool_call_round_trip() {
    let provider = SandboxProvider::from_env();
    if !provider.runtime_available() {
        println!(
            "sandbox provider {:?} not available on this runner, skipping",
            provider
        );
        return;
    }

    let root_dir = TempDir::new().expect("tempdir for --root");
    let xdg_dir = TempDir::new().expect("tempdir for XDG_CONFIG_HOME");
    let root = root_dir.path().to_string_lossy().into_owned();
    let xdg = xdg_dir.path().to_string_lossy().into_owned();

    let mock_server = MockServer::start().await;
    // First POST /responses returns a tool call; every later one the final
    // message. Wiremock consults mocks in mount order, and an exhausted
    // `up_to_n_times` mock stops matching.
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(canned_tool_call_body()))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(canned_tool_followup_body()))
        .mount(&mock_server)
        .await;

    provision_agent(&root, &xdg, &mock_server.uri(), provider);

    let output = run_exo(
        &[
            "conversation",
            "send",
            "test-agent",
            "first",
            "run the check",
        ],
        &root,
        &xdg,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("The shell tool reported success."),
        "expected final assistant text in stdout; got: {stdout}"
    );

    let recorded = mock_server.received_requests().await.unwrap_or_default();
    let responses_calls: Vec<_> = recorded
        .iter()
        .filter(|r| r.url.path() == "/responses")
        .collect();
    assert_eq!(
        responses_calls.len(),
        2,
        "expected exactly two POST /responses (tool round + follow-up); got {}",
        responses_calls.len()
    );

    // The follow-up request must carry the sandbox's actual tool output back
    // to the model.
    let followup_body = String::from_utf8_lossy(&responses_calls[1].body);
    assert!(
        followup_body.contains("tool-roundtrip-ok"),
        "expected shell output in the follow-up model request; got: {followup_body}"
    );

    // The tool exchange must also be durably persisted in the event log.
    let mut events_blob = String::new();
    for entry in walk_event_files(root_dir.path()) {
        events_blob.push_str(&String::from_utf8_lossy(
            &std::fs::read(&entry).expect("event file readable"),
        ));
    }
    assert!(
        events_blob.contains("tool-roundtrip-ok"),
        "expected persisted tool result in conversation events"
    );
    assert!(
        events_blob.contains("The shell tool reported success."),
        "expected persisted final assistant message in conversation events"
    );
}

/// Collects every event file under the (single) agent's (single) conversation.
fn walk_event_files(root: &std::path::Path) -> Vec<PathBuf> {
    let conv_root = root
        .join("exoharness/agents")
        .read_dir()
        .expect("agents dir exists")
        .next()
        .expect("at least one agent")
        .unwrap()
        .path()
        .join("conversations");
    let events_dir = conv_root
        .read_dir()
        .expect("conversations dir exists")
        .next()
        .expect("at least one conversation")
        .unwrap()
        .path()
        .join("events");
    events_dir
        .read_dir()
        .expect("events dir exists")
        .flatten()
        .map(|e| e.path())
        .collect()
}
