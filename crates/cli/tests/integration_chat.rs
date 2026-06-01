//! Integration test exercising the real `exo` binary against:
//!   - a real sandbox backend (docker / apple-container / local-process), and
//!   - a wiremock-backed fake OpenAI Responses endpoint.
//!
//! `#[ignore]`'d so `cargo test` skips it by default; the integration workflow
//! runs `cargo test --workspace -- --ignored` and selects the backend via the
//! `EXO_TEST_SANDBOX_BACKEND` env var (defaults to `docker`). The secret
//! backend is always `file`, with the master key materialised inside a
//! per-test tempdir via `XDG_CONFIG_HOME`.

use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SandboxBackend {
    Docker,
    AppleContainer,
    LocalProcess,
}

impl SandboxBackend {
    fn from_env() -> Self {
        let raw = std::env::var("EXO_TEST_SANDBOX_BACKEND").unwrap_or_else(|_| "docker".into());
        match raw.as_str() {
            "docker" => Self::Docker,
            "apple-container" => Self::AppleContainer,
            "local-process" => Self::LocalProcess,
            other => panic!("unknown EXO_TEST_SANDBOX_BACKEND={other}"),
        }
    }

    fn cli_arg(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::AppleContainer => "apple-container",
            Self::LocalProcess => "local-process",
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
            Self::LocalProcess => true,
        }
    }
}

fn exo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_exo"))
}

fn run_exo(args: &[&str], root: &str, xdg: &str, backend: SandboxBackend) -> std::process::Output {
    let output = Command::new(exo_bin())
        .args(["--root", root])
        .args(["--secret-backend", "file"])
        .args(["--sandbox-backend", backend.cli_arg()])
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
    let backend = SandboxBackend::from_env();
    if !backend.runtime_available() {
        eprintln!(
            "sandbox backend {:?} not available on this runner, skipping",
            backend
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
        backend,
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
        backend,
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
        backend,
    );
    run_exo(
        &["conversation", "create", "test-agent", "first"],
        &root,
        &xdg,
        backend,
    );

    let output = run_exo(
        &["conversation", "send", "test-agent", "first", "hello there"],
        &root,
        &xdg,
        backend,
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

    if backend == SandboxBackend::Docker {
        // After process exit, the warm container is stopped (not deleted)
        // so the next exo invocation can `try_resume` it. The cross-process
        // idle-TTL reaper will collect it later. So we expect *exactly one*
        // Exited container labelled with this conversation's SandboxKey,
        // not zero.
        let leftover = list_exo_containers_for_conversation(&conv_dir);
        assert_eq!(
            leftover.len(),
            1,
            "expected exactly one stopped exo container after binary exit (resume target); found: {leftover:?}"
        );

        // Cleanup: tear down the resume target so this test doesn't leak
        // state into later runs / other tests.
        for id in &leftover {
            let _ = Command::new("docker").args(["rm", "-f", id]).output();
        }
    }
}

/// Returns the IDs of every docker container labelled for the conversation
/// whose state dir is at `conv_dir`. Walks the conversation's persisted
/// sandbox records to derive the SandboxKey, then filters docker ps.
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

/// Resume scenario: two separate `conversation send` invocations against the same
/// conversation must hit the *same* underlying docker container. Validates
/// that PR-#21's Tier 1 (try_resume) wires through end-to-end on Docker.
#[tokio::test]
#[ignore = "spawns real exo binary + real sandbox + wiremock; run with cargo test -- --ignored"]
async fn cross_process_send_resumes_the_same_sandbox_container() {
    let backend = SandboxBackend::from_env();
    // Only Docker has a meaningful resume path; AppleContainer is similar
    // but we don't exercise it in CI, and LocalProcess returns None from
    // try_resume by design.
    if backend != SandboxBackend::Docker {
        eprintln!("cross-process resume test only meaningful on docker; skipping");
        return;
    }
    if !backend.runtime_available() {
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
        backend,
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
        backend,
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
        backend,
    );
    run_exo(
        &["conversation", "create", "test-agent", "first"],
        &root,
        &xdg,
        backend,
    );

    // First send: provisions the warm sandbox.
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
        backend,
    );

    // Second send: in a fresh exo process. ensure_shell_sandbox should hit
    // Tier 1 — try_resume against the labelled container — instead of
    // creating a new one.
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
        backend,
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
