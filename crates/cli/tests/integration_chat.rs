//! Integration test exercising the real `exo` binary against:
//!   - a real sandbox provider (docker / apple-container), and
//!   - a wiremock-backed fake OpenAI Responses endpoint.
//!
//! `#[ignore]`'d so `cargo test` skips it by default; the integration workflow
//! runs `cargo test --workspace -- --ignored` and selects the provider via the
//! `EXO_TEST_SANDBOX_PROVIDER` env var (defaults to `docker`). The secret
//! backend is always `file`, with the master key materialised inside a
//! per-test tempdir via `XDG_CONFIG_HOME`.

use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SandboxProvider {
    Docker,
    AppleContainer,
}

impl SandboxProvider {
    fn from_env() -> Self {
        let raw = std::env::var("EXO_TEST_SANDBOX_PROVIDER").unwrap_or_else(|_| "docker".into());
        match raw.as_str() {
            "docker" => Self::Docker,
            "apple-container" => Self::AppleContainer,
            other => panic!("unknown EXO_TEST_SANDBOX_PROVIDER={other}"),
        }
    }

    fn cli_arg(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::AppleContainer => "apple-container",
        }
    }

    fn runtime_available(self) -> bool {
        match self {
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

    run_exo(
        &["secret", "set", "test-key", "--env", "OPENAI_API_KEY"],
        &root,
        &xdg,
    );
    run_exo(
        &[
            "model",
            "register",
            "gpt-test",
            "--secret",
            "test-key",
            "--base-url",
            &mock_server.uri(),
        ],
        &root,
        &xdg,
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
        &root,
        &xdg,
    );
    run_exo(
        &["conversation", "create", "test-agent", "first"],
        &root,
        &xdg,
    );

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
        // Drop stops (not rm's) the container so the next process can resume it.
        let leftover = list_exo_containers_for_conversation(&conv_dir);
        assert_eq!(
            leftover.len(),
            1,
            "expected exactly one stopped exo container after binary exit (resume target); found: {leftover:?}"
        );

        // Test cleanup: don't leak the resume target onto the docker host.
        for id in &leftover {
            let _ = Command::new("docker").args(["rm", "-f", id]).output();
        }
    }
}

/// Docker container IDs labelled for the conversation at `conv_dir`.
fn list_exo_containers_for_conversation(conv_dir: &std::path::Path) -> Vec<String> {
    let conv_id = conv_dir
        .file_name()
        .and_then(|s| s.to_str())
        .expect("conv dir name");
    let key_prefix = format!("conversation:{conv_id}:");
    let output = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=exo.sandbox.key",
            "--format",
            "{{json .}}",
        ])
        .output()
        .expect("docker ps");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let row: Value = serde_json::from_str(line).expect("docker ps row is json");
            let id = row.get("ID")?.as_str()?.to_string();
            let labels = row.get("Labels")?.as_str()?;
            let key = labels
                .split(',')
                .filter_map(|kv| kv.split_once('='))
                .find_map(|(k, v)| (k == "exo.sandbox.key").then_some(v))?;
            key.starts_with(&key_prefix).then_some(id)
        })
        .collect()
}

/// Two separate `conversation send` invocations on the same conversation must
/// hit the same docker container — the cross-process resume path.
#[tokio::test]
#[ignore = "spawns real exo binary + real sandbox + wiremock; run with cargo test -- --ignored"]
async fn cross_process_send_resumes_the_same_sandbox_container() {
    let provider = SandboxProvider::from_env();
    if provider != SandboxProvider::Docker {
        eprintln!("cross-process resume test only meaningful on docker; skipping");
        return;
    }
    if !provider.runtime_available() {
        eprintln!("docker not available on this runner; skipping");
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

    run_exo(
        &["secret", "set", "test-key", "--env", "OPENAI_API_KEY"],
        &root,
        &xdg,
    );
    run_exo(
        &[
            "model",
            "register",
            "gpt-test",
            "--secret",
            "test-key",
            "--base-url",
            &mock_server.uri(),
        ],
        &root,
        &xdg,
    );
    run_exo(
        &[
            "agent",
            "create",
            "--slug",
            "test-agent",
            "--model",
            "gpt-test",
            "Integration Test Agent",
        ],
        &root,
        &xdg,
    );
    run_exo(
        &["conversation", "create", "test-agent", "first"],
        &root,
        &xdg,
    );

    // First send provisions the warm sandbox.
    run_exo(
        &[
            "conversation",
            "send",
            "test-agent",
            "first",
            "first message",
        ],
        &root,
        &xdg,
    );

    // Second send: fresh exo process; should `try_resume` the same container.
    run_exo(
        &[
            "conversation",
            "send",
            "test-agent",
            "first",
            "second message",
        ],
        &root,
        &xdg,
    );

    let conv_dir = root_dir
        .path()
        .join("exoharness/agents")
        .read_dir()
        .expect("agents dir")
        .next()
        .expect("agent")
        .unwrap()
        .path()
        .join("conversations")
        .read_dir()
        .expect("conversations dir")
        .next()
        .expect("conversation")
        .unwrap()
        .path();

    let containers = list_exo_containers_for_conversation(&conv_dir);
    assert_eq!(
        containers.len(),
        1,
        "expected exactly one docker container after two cross-process sends \
         (resume should reuse, not create new); found: {containers:?}"
    );

    // Cleanup.
    for id in &containers {
        let _ = Command::new("docker").args(["rm", "-f", id]).output();
    }
}
