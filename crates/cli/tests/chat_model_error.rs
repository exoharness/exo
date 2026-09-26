//! Regression test: a model-call failure mid-turn must not kill the chat.
//!
//! Exercises the real `exo chat` binary with piped stdin against a
//! wiremock-backed fake OpenAI endpoint that always returns 400. Each sent
//! line fails its turn; the chat must print `turn failed: ...` and keep
//! reading input instead of exiting non-zero after the first error.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn exo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_exo"))
}

fn run_exo(args: &[&str], root: &str, xdg: &str) -> std::process::Output {
    let output = Command::new(exo_bin())
        .args(["--root", root])
        .args(["--secret-backend", "file"])
        .arg("--master-key-path")
        .arg(PathBuf::from(root).join("master.key"))
        .arg("--pricing-path")
        .arg(PathBuf::from(root).join("prices.json"))
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

#[tokio::test]
async fn chat_survives_model_call_failure() {
    let root_dir = TempDir::new().expect("tempdir for --root");
    let xdg_dir = TempDir::new().expect("tempdir for XDG_CONFIG_HOME");
    let root = root_dir.path().to_string_lossy().into_owned();
    let xdg = xdg_dir.path().to_string_lossy().into_owned();

    std::fs::write(root_dir.path().join("prices.json"), "{}").unwrap();
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "message": "Invalid test request.",
                "type": "invalid_request_error"
            }
        })))
        .mount(&mock_server)
        .await;

    run_exo(
        &["secret", "create", "test-key", "--env", "OPENAI_API_KEY"],
        &root,
        &xdg,
    );
    run_exo(
        &[
            "model",
            "create",
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
            "--provider",
            "local-process",
            "Chat Error Test Agent",
        ],
        &root,
        &xdg,
    );

    let mut chat = Command::new(exo_bin())
        .args(["--root", &root])
        .args(["--secret-backend", "file"])
        .arg("--master-key-path")
        .arg(PathBuf::from(&root).join("master.key"))
        .arg("--pricing-path")
        .arg(PathBuf::from(&root).join("prices.json"))
        .args(["chat", "--agent", "test-agent"])
        .env("XDG_CONFIG_HOME", &xdg)
        .env("OPENAI_API_KEY", "sk-test-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn exo chat");

    // Two turns: before the fix the process died on the first model error and
    // never saw the second line.
    chat.stdin
        .take()
        .expect("chat stdin piped")
        .write_all(b"hello once\nhello twice\n")
        .expect("write to chat stdin");

    let output = chat.wait_with_output().expect("wait for chat");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "chat should exit cleanly after failed turns; status={:?} stdout={stdout} stderr={stderr}",
        output.status
    );
    let failed_turns = stdout.matches("turn failed:").count();
    assert_eq!(
        failed_turns, 2,
        "expected both turns to fail without killing the chat; stdout={stdout} stderr={stderr}"
    );

    let recorded = mock_server.received_requests().await.unwrap_or_default();
    let responses_calls = recorded
        .iter()
        .filter(|r| r.url.path() == "/responses")
        .count();
    assert!(
        responses_calls >= 2,
        "expected a model call per chat line; got {responses_calls}"
    );
}
