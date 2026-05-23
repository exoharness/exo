//! Integration test exercising the real `exo` binary against:
//!   - a real Docker container (sandbox), and
//!   - a wiremock-backed fake OpenAI Responses endpoint.
//!
//! `#[ignore]`'d so `cargo test` skips it by default; the integration workflow
//! runs `cargo test --workspace -- --ignored` on push to main.
//!
//! Linux-only because the production sandbox backend on macOS is the Apple
//! `container` CLI, which is not assumed to be present on macOS CI runners
//! today. Verifying the Apple arm in CI is a separate follow-up.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn exo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_exo"))
}

fn run_exo(args: &[&str], root: &str, xdg: &str) -> std::process::Output {
    let output = Command::new(exo_bin())
        .args(["--root", root])
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

/// Canned OpenAI Responses API completion body. Format derived from the
/// public API surface; if lingua's parsing tightens, this is the place to
/// update.
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
#[ignore = "spawns real exo binary + real Docker container + wiremock; run with cargo test -- --ignored"]
async fn chat_send_round_trips_through_real_docker_and_mocked_openai() {
    // Pre-flight: skip cleanly if Docker isn't available on the runner.
    let docker_available = Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !docker_available {
        eprintln!("docker not available, skipping integration test");
        return;
    }

    let root_dir = TempDir::new().expect("tempdir for --root");
    let xdg_dir = TempDir::new().expect("tempdir for XDG_CONFIG_HOME");
    let root = root_dir.path().to_string_lossy().into_owned();
    let xdg = xdg_dir.path().to_string_lossy().into_owned();

    // Stand up the fake OpenAI Responses endpoint. lingua targets `/responses`
    // relative to the supplied base_url (it does not auto-prepend `/v1`).
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(canned_response_body()))
        .mount(&mock_server)
        .await;

    // Wire up secret + model + agent + conversation via the real binary.
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

    // Send a chat; this is the bit that hits the mock server through the real
    // executor + the real Docker sandbox.
    let output = run_exo(
        &["chat", "send", "test-agent", "first", "hello there"],
        &root,
        &xdg,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Hello from the mock OpenAI server."),
        "expected mocked assistant text in stdout; got: {stdout}"
    );

    // The mock server should have received exactly one /responses request.
    let recorded = mock_server.received_requests().await.unwrap_or_default();
    let responses_calls = recorded
        .iter()
        .filter(|r| r.url.path() == "/responses")
        .count();
    assert_eq!(
        responses_calls, 1,
        "expected exactly one POST /responses; got {responses_calls} (all paths: {:?})",
        recorded
            .iter()
            .map(|r| r.url.path().to_string())
            .collect::<Vec<_>>()
    );

    // The conversation's event log should contain the assistant message we
    // returned from the mock.
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

    // Sandbox lifecycle: the harness should have created at least one
    // labeled Docker container while running, and `docker rm -f` should
    // have cleaned it up by the time the binary exited.
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
