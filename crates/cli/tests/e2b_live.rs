//! Live E2B smoke tests (require `E2B_API_KEY`). Run with:
//!
//! ```bash
//! export E2B_API_KEY=...
//! export E2B_TEMPLATE_ID=base   # optional, defaults to "base"
//! cargo test --package exo --test e2b_live -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::time::Duration;

use exoharness::{
    E2bConfig, E2bSandboxBackend, ManagedSandboxBackend, SandboxKey, SandboxLifecycleConfig,
    SandboxNetworkPolicy, SandboxRequest, SandboxSpec,
};

fn live_config() -> Option<E2bConfig> {
    if std::env::var("E2B_API_KEY").is_err() {
        eprintln!("skipping E2B live test: E2B_API_KEY not set");
        return None;
    }
    E2bConfig::from_env().ok()
}

fn live_request(label: &str) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: "live-conv".into(),
            sandbox_id: label.into(),
        },
        spec: SandboxSpec {
            image: String::new(),
            mounts: Vec::new(),
            network: SandboxNetworkPolicy::Enabled,
            default_workdir: "/home/user".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
    }
}

#[tokio::test]
#[ignore = "requires E2B_API_KEY and bills E2B; run with cargo test --test e2b_live -- --ignored"]
async fn live_acquire_exec_and_pause() {
    let Some(config) = live_config() else {
        return;
    };
    let backend = E2bSandboxBackend::new(config).expect("backend");
    let handle = backend
        .acquire(live_request("live-sandbox-1"))
        .await
        .expect("acquire");
    let output = handle
        .exec(&exoharness::SandboxCommand {
            argv: vec!["/bin/sh".into(), "-lc".into(), "echo exo-e2b-live".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("exec");
    assert!(
        output.stdout.trim().contains("exo-e2b-live"),
        "stdout: {:?}",
        output.stdout
    );
    handle.stop().await.expect("pause");
}

#[tokio::test]
#[ignore = "requires E2B_API_KEY and bills E2B; run with cargo test --test e2b_live -- --ignored"]
async fn live_try_resume_finds_sandbox_after_process_boundary() {
    let Some(config) = live_config() else {
        return;
    };
    let backend = E2bSandboxBackend::new(config).expect("backend");
    let request = live_request("live-resume-boundary");
    let handle = backend.acquire(request.clone()).await.expect("acquire");
    handle
        .exec(&exoharness::SandboxCommand {
            argv: vec![
                "/bin/sh".into(),
                "-lc".into(),
                "echo resume-marker > /tmp/exo-e2b-resume".into(),
            ],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("write marker");
    drop(handle);

    let resumed = backend
        .try_resume(request)
        .await
        .expect("try_resume")
        .expect("E2B sandbox should be listed by metadata after acquire");
    let output = resumed
        .exec(&exoharness::SandboxCommand {
            argv: vec![
                "/bin/sh".into(),
                "-lc".into(),
                "cat /tmp/exo-e2b-resume".into(),
            ],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("read marker");
    assert!(
        output.stdout.trim().contains("resume-marker"),
        "stdout: {:?}",
        output.stdout
    );
    resumed.stop().await.expect("pause");
}
