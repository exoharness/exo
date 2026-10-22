//! Live teleport test: a sandbox running on Daytona is snapshotted and moved
//! to local Docker — the SAME sandbox id, resumed locally with its files
//! intact. Drives the harness API, i.e. the exact code path behind the REPL's
//! `/teleport docker` command:
//!
//!   1. create a sandbox on Daytona, write a marker file
//!   2. `snapshot_sandbox` (-> DaytonaRef payload in exoharness storage)
//!   3. `start_sandbox` with `provider: Some(Docker)` — the cross-provider
//!      override routes the DaytonaRef through the Daytona backend's export
//!      bridge (`docker pull` from Daytona's transient registry -> `docker
//!      save`), then the Docker backend restores from the tarball
//!   4. exec on the same sandbox id — now running on local Docker — and read
//!      the marker file back
//!
//! Ignored by default — requires a local Docker daemon and `DAYTONA_API_KEY`
//! (+ optional `DAYTONA_ORGANIZATION_ID` / `DAYTONA_TARGET`). Run with:
//!
//! ```bash
//! cargo test -p exo --test teleport_daytona_to_docker -- --ignored --nocapture
//! ```

use std::process::Command;

use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, ConversationHandle, CreateSandboxRequest,
    DaytonaBackendSpec, ExoHarness, NewAgentRequest, NewConversationRequest, PutSecretRequest,
    RunInSandboxRequest, SandboxBackendRegistration, SandboxId, SandboxProvider, Secret,
    SecretBackendChoice, StartSandboxRequest,
};
use futures::io::AsyncReadExt;
use tempfile::TempDir;

#[tokio::test]
#[ignore = "live: needs docker and DAYTONA_API_KEY"]
async fn teleport_daytona_sandbox_to_docker_keeps_files() {
    let Ok(api_key) = std::env::var("DAYTONA_API_KEY") else {
        eprintln!("skipping: DAYTONA_API_KEY not set");
        return;
    };
    if !docker_available() {
        eprintln!("skipping: docker not available");
        return;
    }

    let root = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(BasicExoHarnessConfig {
        root: root.path().to_path_buf(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::Daytona,
        sandbox_policy: None,
        sandbox_backends: vec![
            SandboxBackendRegistration::daytona(DaytonaBackendSpec::with_conventional_secrets()),
            SandboxBackendRegistration::docker(),
        ],
    })
    .await
    .expect("harness");

    // Daytona credentials live in the secret store (resolved lazily on first use).
    seed_secret(&harness, "DAYTONA_API_KEY", &api_key).await;
    if let Ok(v) = std::env::var("DAYTONA_ORGANIZATION_ID") {
        seed_secret(&harness, "DAYTONA_ORGANIZATION_ID", &v).await;
    }
    if let Ok(v) = std::env::var("DAYTONA_TARGET") {
        seed_secret(&harness, "DAYTONA_TARGET", &v).await;
    }

    let agent = harness
        .new_agent(NewAgentRequest {
            vaults: vec![],
            slug: "teleport-down".into(),
            name: "teleport-down".into(),
        })
        .await
        .expect("agent");
    let conv = agent
        .new_conversation(NewConversationRequest {
            environment: None,
            vaults: vec![],
            slug: Some("teleport-down-conv".into()),
            name: Some("teleport-down".into()),
        })
        .await
        .expect("conversation");

    // 1. Create a Daytona sandbox and write a marker file.
    let sandbox_id = conv
        .create_sandbox(CreateSandboxRequest {
            tcp_ports: vec![],
            name: None,
            provider: SandboxProvider::Daytona,
            image: String::new(),
            resources: Default::default(),
            default_workdir: Some("/".into()),
            file_system_mounts: None,
            durable_file_systems: None,
            policy: None,
            enable_networking: Some(true),
            idle_seconds: Some(300),
        })
        .await
        .expect("create daytona sandbox");
    let (rc, _, _) = exec_shell(
        conv.as_ref(),
        &sandbox_id,
        "echo teleport-marker > /exo-teleport.txt",
    )
    .await;
    assert_eq!(rc, 0, "writing the marker on daytona should succeed");

    // 2. Snapshot the Daytona sandbox (-> DaytonaRef in exoharness storage).
    let snapshot_id = conv
        .snapshot_sandbox(sandbox_id.clone(), exoharness::SnapshotKind::Filesystem)
        .await
        .expect("snapshot daytona sandbox");

    // 3. Restore under local Docker via the provider override (-> export bridge).
    conv.start_sandbox(StartSandboxRequest {
        id: sandbox_id.clone(),
        snapshot_id: Some(snapshot_id),
        idle_seconds: None,
        provider: Some(SandboxProvider::Docker),
    })
    .await
    .expect("start_sandbox on docker from the daytona snapshot");

    // 4. The file written on Daytona is present on the local Docker sandbox.
    let (rc, stdout, _) = exec_shell(
        conv.as_ref(),
        &sandbox_id,
        "cat /exo-teleport.txt && echo --- && hostname",
    )
    .await;
    eprintln!("after teleport, docker sees:\n{stdout}");
    assert_eq!(
        rc, 0,
        "exec on the teleported docker sandbox should succeed"
    );
    assert!(
        stdout.contains("teleport-marker"),
        "file should survive the daytona->docker teleport: {stdout:?}"
    );

    let _ = conv.stop_sandbox(sandbox_id).await;
}

async fn seed_secret(harness: &BasicExoHarness, name: &str, value: &str) {
    exoharness::vault::global_vault(harness)
        .await
        .expect("runtime vault")
        .put_secret(PutSecretRequest {
            policy: None,
            name: name.into(),
            secret: Secret::Key {
                value: value.into(),
            },
        })
        .await
        .expect("put secret");
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn exec_shell(
    conversation: &dyn ConversationHandle,
    sandbox_id: &SandboxId,
    cmd: &str,
) -> (i32, String, String) {
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id.clone(),
            command: vec!["/bin/bash".into(), "-c".into(), cmd.into()],
            env: Default::default(),
        })
        .await
        .unwrap_or_else(|error| panic!("run_in_sandbox({cmd:?}) failed: {error:#}"));

    let mut parts = process.into_parts();
    drop(parts.stdin);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let (stdout_result, stderr_result, wait_result) = tokio::join!(
        parts.stdout.read_to_end(&mut stdout_bytes),
        parts.stderr.read_to_end(&mut stderr_bytes),
        parts.wait,
    );
    stdout_result.expect("read stdout");
    stderr_result.expect("read stderr");
    let exit_code = wait_result.expect("wait on sandbox process");
    (
        exit_code,
        String::from_utf8_lossy(&stdout_bytes).into_owned(),
        String::from_utf8_lossy(&stderr_bytes).into_owned(),
    )
}
