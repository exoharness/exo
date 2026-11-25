//! Live tests for the boxd sandbox backend against the real boxd API.
//!
//! `BOXD_API_KEY=... cargo test -p exo --test boxd_sandbox_live -- --ignored --nocapture --test-threads=1`
//!
//! Optional: `BOXD_ORG` (org name or id), `BOXD_API_URL` (default
//! `https://boxd.sh:9443`), `BOXD_NAME_PREFIX` (machine name prefix, default
//! `exo`) and `BOXD_LABELS` (comma-separated `key=value` labels for the test
//! machines). Every test destroys the machines it created.

use std::collections::HashMap;
use std::env;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use exoharness::{
    BoxdConfig, BoxdSandboxBackend, ManagedSandboxBackend, ManagedSandboxHandle, ResourceScope,
    SandboxCommand, SandboxLifecycleConfig, SandboxNetworkPolicy, SandboxRequest, SandboxSpec,
    SnapshotKind,
};
use futures::FutureExt;

const WORKDIR: &str = "/home/exo/workspace";

fn boxd_config_from_env() -> Option<BoxdConfig> {
    let api_key = match env::var("BOXD_API_KEY") {
        Ok(value) if !value.is_empty() => value,
        _ => {
            eprintln!("skipping real boxd test: BOXD_API_KEY is not set");
            return None;
        }
    };
    Some(BoxdConfig {
        api_key,
        api_url: env::var("BOXD_API_URL")
            .unwrap_or_else(|_| exoharness::DEFAULT_BOXD_API_URL.into()),
        token_url: exoharness::DEFAULT_BOXD_TOKEN_URL.into(),
        org: env::var("BOXD_ORG").ok(),
        name_prefix: env::var("BOXD_NAME_PREFIX")
            .unwrap_or_else(|_| exoharness::DEFAULT_BOXD_NAME_PREFIX.into()),
        extra_labels: env::var("BOXD_LABELS")
            .map(|labels| labels.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
    })
}

fn make_request(sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        policy: SandboxNetworkPolicy::Unrestricted.into(),
        sandbox_id: sandbox_id.into(),
        scope: ResourceScope::Thread {
            agent_id: exoharness::Uuid7::now(),
            thread_id: exoharness::Uuid7::now(),
        },
        spec: SandboxSpec {
            tcp_ports: vec![],
            image: "default".into(),
            resources: Default::default(),
            mounts: Vec::new(),
            durable_file_systems: Vec::new(),
            default_workdir: WORKDIR.into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        provider_state: None,
    }
}

fn unique_sandbox_id(name: &str) -> String {
    format!("{name}-{}", exoharness::Uuid7::now())
}

fn shell(script: &str) -> SandboxCommand {
    SandboxCommand {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: HashMap::new(),
        display_argv: None,
        cwd: None,
        timeout: Some(Duration::from_secs(60)),
    }
}

async fn acquire_timed(
    backend: &BoxdSandboxBackend,
    request: SandboxRequest,
    what: &str,
) -> Arc<dyn ManagedSandboxHandle> {
    let started = Instant::now();
    let handle = backend.acquire(request).await.expect(what);
    println!("{what}: {:.1}s", started.elapsed().as_secs_f64());
    handle
}

#[tokio::test]
#[ignore = "requires BOXD_API_KEY"]
async fn boxd_exec_reports_exit_codes_env_streams_and_timeouts() {
    let Some(config) = boxd_config_from_env() else {
        return;
    };
    let backend = BoxdSandboxBackend::new(config).expect("BoxdSandboxBackend::new");
    let request = make_request(&unique_sandbox_id("boxd-live-exec"));
    let handle = acquire_timed(&backend, request.clone(), "create and ready").await;

    let result = AssertUnwindSafe(async {
        let output = handle.exec(&shell("exit 42")).await.expect("exec exit 42");
        assert_eq!(output.exit_code, Some(42));
        assert!(!output.ok);

        let mut with_env = shell("printf '%s|' \"$EXO_LIVE\"; pwd; echo oops >&2");
        with_env
            .env
            .insert("EXO_LIVE".into(), "it's a \"value\" $HOME".into());
        let output = handle.exec(&with_env).await.expect("exec with env");
        assert_eq!(output.stdout, format!("it's a \"value\" $HOME|{WORKDIR}\n"));
        assert_eq!(output.stderr, "oops\n");

        let mut in_tmp = shell("pwd");
        in_tmp.cwd = Some("/tmp".into());
        assert_eq!(
            handle.exec(&in_tmp).await.expect("exec cwd").stdout,
            "/tmp\n"
        );

        let mut slow = shell("sleep 30");
        slow.timeout = Some(Duration::from_secs(2));
        let started = Instant::now();
        let error = handle.exec(&slow).await.expect_err("timeout");
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(10));

        let output = handle
            .exec(&shell("nohup sleep 300 >/dev/null 2>&1 & echo started"))
            .await
            .expect("start background process");
        assert_eq!(output.stdout, "started\n");
        let output = handle
            .exec(&shell("pgrep -x sleep >/dev/null && echo alive"))
            .await
            .expect("background process survives the exec");
        assert_eq!(output.stdout, "alive\n");

        let output = handle
            .exec(&shell("id -un; test -w \"$PWD\" && echo writable"))
            .await
            .expect("whoami");
        println!(
            "exec user and workdir: {}",
            output.stdout.replace('\n', " ")
        );
    })
    .catch_unwind()
    .await;
    backend.terminate(request).await.expect("terminate");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "requires BOXD_API_KEY"]
async fn boxd_start_process_contracts() {
    let Some(config) = boxd_config_from_env() else {
        return;
    };
    let backend = BoxdSandboxBackend::new(config).expect("BoxdSandboxBackend::new");
    let request = make_request(&unique_sandbox_id("boxd-live-process"));

    let handle = acquire_timed(&backend, request.clone(), "create and ready").await;
    let interactive =
        exoharness::contract_tests::sandbox_handle_start_process_supports_interactive_stdio_and_env(
            handle,
        )
        .await;

    // The contract stops (hibernates) the machine; acquiring again wakes it.
    let handle = acquire_timed(&backend, request.clone(), "wake after stop").await;
    let protocol = exoharness::contract_tests::sandbox_handle_start_process_supports_long_running_request_response_protocol(handle).await;

    backend.terminate(request).await.expect("terminate");
    interactive.expect("interactive stdio and env contract");
    protocol.expect("long-running request/response contract");
}

#[tokio::test]
#[ignore = "requires BOXD_API_KEY"]
async fn boxd_workdir_and_protocol_survive_stop_and_reacquire() {
    let Some(config) = boxd_config_from_env() else {
        return;
    };
    let backend: Arc<dyn ManagedSandboxBackend> =
        Arc::new(BoxdSandboxBackend::new(config).expect("BoxdSandboxBackend::new"));
    let request = make_request(&unique_sandbox_id("boxd-live-stop"));
    let started = Instant::now();
    let result = exoharness::contract_tests::sandbox_backend_long_running_process_and_workdir_survive_stop_and_reacquire(
        Arc::clone(&backend),
        request.clone(),
    )
    .await;
    println!(
        "create, protocol, hibernate, wake, protocol: {:.1}s",
        started.elapsed().as_secs_f64()
    );
    backend.terminate(request).await.expect("terminate");
    result.expect("workdir and protocol survive stop and reacquire");
}

#[tokio::test]
#[ignore = "requires BOXD_API_KEY"]
async fn boxd_snapshot_restore_and_fork_keep_files_and_processes() {
    let Some(config) = boxd_config_from_env() else {
        return;
    };
    let backend = BoxdSandboxBackend::new(config).expect("BoxdSandboxBackend::new");
    let source = make_request(&unique_sandbox_id("boxd-live-source"));
    let restored = make_request(&unique_sandbox_id("boxd-live-restored"));
    let forked = make_request(&unique_sandbox_id("boxd-live-forked"));

    let result = AssertUnwindSafe(async {
        let handle = acquire_timed(&backend, source.clone(), "create and ready").await;
        handle
            .exec(&shell(
                "echo kept > marker.txt && (nohup sleep 600 >/dev/null 2>&1 &)",
            ))
            .await
            .expect("write marker");

        let started = Instant::now();
        let payload = handle
            .snapshot(SnapshotKind::Full)
            .await
            .expect("full snapshot");
        println!("snapshot ready: {:.1}s", started.elapsed().as_secs_f64());

        let started = Instant::now();
        let copy = backend
            .acquire_from_snapshot(restored.clone(), payload.clone())
            .await
            .expect("restore snapshot");
        println!("restore ready: {:.1}s", started.elapsed().as_secs_f64());
        let output = copy
            .exec(&shell(
                "cat marker.txt; pgrep -x sleep >/dev/null && echo running",
            ))
            .await
            .expect("read restored marker");
        assert_eq!(output.stdout, "kept\nrunning\n");

        let started = Instant::now();
        let fork = backend
            .fork_sandbox(source.clone(), forked.clone())
            .await
            .expect("fork");
        println!("fork ready: {:.1}s", started.elapsed().as_secs_f64());
        let output = fork
            .exec(&shell("cat marker.txt"))
            .await
            .expect("read forked marker");
        assert_eq!(output.stdout, "kept\n");

        backend
            .delete_snapshot(payload)
            .await
            .expect("delete snapshot");
    })
    .catch_unwind()
    .await;
    for request in [source, restored, forked] {
        backend.terminate(request).await.expect("terminate");
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "requires BOXD_API_KEY"]
async fn boxd_rejects_a_wrong_api_key_with_a_clear_error() {
    let Some(mut config) = boxd_config_from_env() else {
        return;
    };
    config.api_key = "not-a-boxd-key".into();
    let backend = BoxdSandboxBackend::new(config).expect("BoxdSandboxBackend::new");
    let error = backend
        .acquire(make_request(&unique_sandbox_id("boxd-live-wrong-key")))
        .await
        .err()
        .expect("wrong key must fail");
    println!("wrong key: {error:#}");
    assert!(format!("{error:#}").contains("BOXD_API_KEY"), "{error:#}");
}
