use std::path::PathBuf;

use crate::{
    BasicExoHarnessConfig, DaytonaBackendSpec, SandboxBackendRegistration, SandboxProvider,
    SecretBackendChoice,
};

#[cfg(all(test, unix))]
pub(crate) fn write_test_executable(path: &std::path::Path, script: &str) -> crate::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Write in a child so parallel test spawns cannot inherit a writable
    // executable descriptor, which briefly makes Linux exec return ETXTBSY.
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", "printf '%s' \"$2\" > \"$1\"", "write-test-executable"])
        .arg(path)
        .arg(script)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "writing test executable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

pub fn local_test_config(root: impl Into<PathBuf>) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.into(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    }
}

pub async fn new_test_agent(
    harness: &dyn crate::ExoHarness,
    slug: &str,
) -> crate::Result<std::sync::Arc<dyn crate::AgentHandle>> {
    harness
        .new_agent(crate::NewAgentRequest {
            slug: slug.into(),
            name: slug.into(),
            vaults: vec![],
        })
        .await
}

pub fn sandbox_request() -> crate::CreateSandboxRequest {
    serde_json::from_str(r#"{"provider":"local_process","image":"unused","enable_networking":true,"idle_seconds":300}"#)
        .expect("test sandbox request")
}

/// Like [`local_test_config`] but also advertises Daytona, so tests can exercise
/// lazy secret resolution. Daytona credentials are still read from the secret
/// store on first use.
pub fn local_test_config_with_daytona(root: impl Into<PathBuf>) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.into(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![
            SandboxBackendRegistration::local_process(),
            SandboxBackendRegistration::daytona(DaytonaBackendSpec::default()),
        ],
    }
}

pub fn corrupt_thread_history(
    root: &std::path::Path,
    agent: crate::AgentId,
    thread: crate::ThreadId,
) -> crate::Result<(PathBuf, Vec<u8>)> {
    let directory = root
        .join("agents")
        .join(agent.to_string())
        .join("conversations")
        .join(thread.to_string())
        .join("events");
    let path = std::fs::read_dir(directory)?
        .next()
        .ok_or_else(|| anyhow::anyhow!("thread has no event files"))??
        .path();
    let original = std::fs::read(&path)?;
    std::fs::write(&path, b"invalid event JSON")?;
    Ok((path, original))
}

pub fn new_test_turn_record() -> crate::TurnRecord {
    crate::TurnRecord {
        id: crate::Uuid7::now(),
        session_id: crate::Uuid7::now(),
    }
}

pub async fn begin_test_turn(
    thread: &dyn crate::ThreadHandle,
) -> crate::Result<std::sync::Arc<dyn crate::TurnHandle>> {
    thread
        .begin_turn(crate::BeginTurnRequest {
            turn: new_test_turn_record(),
            new_session: true,
            input: Vec::new(),
            initial_events: Vec::new(),
        })
        .await
}
