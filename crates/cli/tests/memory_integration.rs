//! Integration test for the exoclaw agent-memory feature, end to end against a
//! REAL model.
//!
//! Unlike `integration_chat.rs` (which mocks the OpenAI Responses endpoint with
//! wiremock), this test drives a real model so it actually exercises the model
//! *deciding* to call the `remember` tool and later recalling the fact from
//! injected memory. It therefore needs a real OpenAI key.
//!
//! Gating (mirrors the daytona/vercel real-service tests):
//!   - `#[ignore]` so plain `cargo test` skips it; run with `--ignored`.
//!   - Skips with a message unless a usable `OPENAI_API_KEY` is in the env
//!     (so CI without a real key is a no-op, and local runs with the key
//!     exported — the way these are normally run — exercise the full path).
//!
//! What it proves that the vitest unit tests (which use an in-memory fake
//! handle) cannot: `remember` writes through the REAL exoharness artifact store,
//! and a DIFFERENT conversation reads the fact back via the real
//! read-artifact-by-path bridge + memory injection.

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

fn exo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_exo"))
}

/// Repo root. `exo` resolves the TypeScript harness runner relative to its
/// working directory, so the spawned binary must run from here (not the crate
/// dir that `cargo test` defaults to).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root should resolve")
}

/// Absolute path to the exoclaw harness module (the harness that registers the
/// memory tools), resolved from this crate's manifest dir.
fn exoclaw_harness() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/exoclaw/harness.ts")
        .canonicalize()
        .expect("exoclaw harness.ts should exist")
}

/// A real OpenAI key, or `None` if the test should be skipped. Treats the
/// `sk-test-*` placeholder used by the mocked integration test as "absent" so
/// this never accidentally fires real requests with a fake key.
fn real_openai_key() -> Option<String> {
    let key = std::env::var("OPENAI_API_KEY").ok()?;
    let key = key.trim().to_string();
    if key.is_empty() || key.starts_with("sk-test") {
        return None;
    }
    Some(key)
}

fn run_exo(args: &[&str], root: &str, xdg: &str, openai_key: &str) -> std::process::Output {
    let output = Command::new(exo_bin())
        .current_dir(repo_root())
        .args(["--root", root])
        .args(["--secret-backend", "file"])
        .args(args)
        .env("XDG_CONFIG_HOME", xdg)
        .env("OPENAI_API_KEY", openai_key)
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

/// True if any artifact at the given path exists anywhere under the store root
/// (the agent-scoped memory store is `memory/exoclaw-memory.json`).
fn memory_artifact_written(root: &std::path::Path) -> bool {
    fn walk(dir: &std::path::Path, needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if walk(&path, needle) {
                    return true;
                }
            } else if let Ok(raw) = std::fs::read_to_string(&path) {
                // Artifact metadata records its logical path; the memory store's
                // path appears in the metadata json for the written artifact.
                if raw.contains(needle) {
                    return true;
                }
            }
        }
        false
    }
    walk(root, "memory/exoclaw-memory.json")
}

#[tokio::test]
#[ignore = "drives a REAL model; needs OPENAI_API_KEY. run with cargo test -- --ignored"]
async fn remember_then_recall_across_conversations_with_real_model() {
    let Some(openai_key) = real_openai_key() else {
        println!("OPENAI_API_KEY not set (or is a test placeholder); skipping memory integration");
        return;
    };
    let model = std::env::var("EXO_TEST_MODEL").unwrap_or_else(|_| "gpt-4o".into());
    let harness = exoclaw_harness();
    let harness = harness.to_string_lossy();

    let root_dir = TempDir::new().expect("tempdir for --root");
    let xdg_dir = TempDir::new().expect("tempdir for XDG_CONFIG_HOME");
    let root = root_dir.path().to_string_lossy().into_owned();
    let xdg = xdg_dir.path().to_string_lossy().into_owned();
    let go = |args: &[&str]| run_exo(args, &root, &xdg, &openai_key);

    // Wire the model from the real key, on the exoclaw harness, local-process
    // sandbox (memory needs no real container).
    go(&["secret", "set", "openai-key", "--env", "OPENAI_API_KEY"]);
    go(&["model", "register", &model, "--secret", "openai-key"]);
    go(&[
        "agent",
        "create",
        "--slug",
        "memtest",
        "--model",
        &model,
        "--harness",
        &harness,
        "--sandbox-provider",
        "local-process",
        "Memory Integration Agent",
    ]);
    go(&["conversation", "create", "memtest", "c-remember"]);
    go(&["conversation", "create", "memtest", "c-recall"]);

    // Turn 1: ask the agent to remember a distinctive fact.
    go(&[
        "conversation",
        "send",
        "memtest",
        "c-remember",
        "Please remember this for the future: my favorite programming language is Rust.",
    ]);

    // Deterministic check of the WRITE path: the memory store artifact now
    // exists in the real store, independent of how the model phrased its reply.
    assert!(
        memory_artifact_written(root_dir.path()),
        "expected memory artifact memory/exoclaw-memory.json to be written under {}",
        root_dir.path().display()
    );

    // Turn 2: a DIFFERENT conversation must recall the fact via injected memory.
    let output = go(&[
        "conversation",
        "send",
        "memtest",
        "c-recall",
        "Based only on what you have saved in memory, what is my favorite programming language? \
         Answer with just the language name.",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
    assert!(
        stdout.contains("rust"),
        "expected the recalled fact (\"rust\") in the second conversation's reply; got: {stdout}"
    );
}
