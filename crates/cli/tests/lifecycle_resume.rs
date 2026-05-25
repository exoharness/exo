//! Lifecycle tests for cross-process sandbox resume (PR #21).
//!
//! PR #21's `ensure_shell_sandbox` has a 3-tier fallback chain for
//! recovering a previous conversation's sandbox:
//!
//!   Tier 1: container exists (running or stopped) under the recorded
//!           `SandboxKey`. `try_resume` finds it via docker labels, starts
//!           it if needed, attaches. Same container, same `sandbox_id`.
//!   Tier 2: container is gone (idle-TTL expiry, manual rm, server-side
//!           reclaim) but a snapshot was taken for this sandbox. The
//!           harness restores the snapshot into a fresh container. New
//!           container id, same `sandbox_id`.
//!   Tier 3: container is gone and no snapshot was ever taken. Harness
//!           creates a fresh sandbox. New container id, NEW `sandbox_id`.
//!
//! Each test below targets one tier by simulating two `exo` processes:
//! drop the BasicExoHarness (Drop now stops containers instead of rm'ing
//! them, so they survive the "process boundary"), then construct a new
//! harness against the same `--root` directory. Whatever survives or
//! doesn't survive between the two harnesses tells us which tier fired.
//!
//! Docker only; the LocalProcess backend returns `None` from `try_resume`
//! by design so there's no lifecycle to exercise there.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use executor::{AgentConfig, AgentHarnessKind, BasicToolRuntime, ConversationConfig, ToolRuntime};
use exoharness::{
    AgentId, BasicExoHarness, BasicExoHarnessConfig, ConversationHandle, ConversationId, EventData,
    EventKind, EventQuery, EventQueryDirection, ExoHarness, NewAgentRequest,
    NewConversationRequest, RunInSandboxRequest, SandboxBackendChoice, SandboxId,
    SandboxProcessParts, SecretBackendChoice,
};
use futures::io::AsyncReadExt;
use tempfile::TempDir;

const SANDBOX_IMAGE: &str = "docker.io/library/ubuntu:24.04";

// ───────────────────── Tier 1 ─────────────────────
//
// First harness creates a container and writes a marker file. Drop the
// harness — Drop sends `docker stop`, leaving the container on disk in
// `Exited` state. Second harness opens the same conversation; its
// `try_resume` finds the stopped container via the `exo.sandbox.key`
// label, `docker start`s it, and attaches. The container's filesystem
// (and therefore the marker) is preserved by the stop+start cycle.

#[tokio::test]
#[ignore = "spawns real docker container; run with `cargo test -- --ignored`"]
async fn tier_1_stopped_container_is_resumed_same_id() {
    if !preflight() {
        return;
    }
    let root_dir = TempDir::new().expect("tempdir");

    // ── Harness #1: create sandbox + write marker ──
    let (agent_id, conv_id, sandbox_id, container_id) = {
        let harness = make_harness(root_dir.path()).await;
        let (agent_id, conv_id) = make_agent_and_conv(&harness, "tier-1-agent").await;
        let conv = open_conv(&harness, &agent_id, &conv_id).await;
        prepare(conv.as_ref()).await;

        let sandbox_id = latest_sandbox_id(conv.as_ref())
            .await
            .expect("ensure_shell_sandbox should have created a sandbox");
        let containers = list_containers_for_sandbox(&conv_id, &sandbox_id);
        assert_eq!(
            containers.len(),
            1,
            "round 1 should have exactly one container, got: {containers:?}"
        );
        let container_id = containers.into_iter().next().unwrap();

        let (rc, _, _) =
            exec_shell(conv.as_ref(), &sandbox_id, "echo 'tier-1-marker' > /tmp/x").await;
        assert_eq!(rc, 0, "writing marker should succeed");

        drop(conv);
        // harness drops at end of block; Drop stops the container.
        (agent_id, conv_id, sandbox_id, container_id)
    };

    // Container should be stopped (not removed) — PR #21's Drop behaviour.
    let state = docker_container_state(&container_id);
    assert_eq!(
        state, "exited",
        "container should be stopped (not rm'd) after harness drop; state={state:?}"
    );

    // ── Harness #2: resume ──
    {
        let harness = make_harness(root_dir.path()).await;
        let conv = open_conv(&harness, &agent_id, &conv_id).await;
        prepare(conv.as_ref()).await;

        // Same container, no new one.
        let containers = list_containers_for_sandbox(&conv_id, &sandbox_id);
        assert_eq!(
            containers,
            vec![container_id.clone()],
            "round 2 should reuse the same container ID, not create a new one"
        );

        // Marker persists across the stop/start cycle.
        let (rc, stdout, _) = exec_shell(conv.as_ref(), &sandbox_id, "cat /tmp/x").await;
        assert_eq!(rc, 0);
        assert_eq!(
            stdout.trim(),
            "tier-1-marker",
            "marker should persist across resume"
        );

        // The conversation log should still report exactly one
        // SandboxCreated event — Tier 1 never invokes create_sandbox.
        let created = count_events_of_type(conv.as_ref(), "sandbox_created").await;
        assert_eq!(created, 1, "tier 1 should not create a second sandbox");
    }

    rm_container(&container_id);
}

// ───────────────────── Tier 2 ─────────────────────
//
// First harness creates the container, writes a marker, takes a snapshot.
// Drop the harness. Then `docker rm -f` the container to simulate the
// idle-TTL or manual-cleanup case. Second harness's `try_resume` finds
// nothing (label query empty); `ensure_shell_sandbox` falls through to
// Tier 2, walks `SandboxSnapshotted` events for the recorded sandbox,
// finds the snapshot, and calls `start_sandbox` which calls
// `acquire_from_snapshot` (PR #20 path) to bring up a fresh container.
// The new container is a brand-new docker id, but the `sandbox_id` (the
// exoharness logical identifier) is reused, and the marker comes back
// because the snapshot included it.

#[tokio::test]
#[ignore = "spawns real docker container; run with `cargo test -- --ignored`"]
async fn tier_2_gone_container_with_snapshot_restores() {
    if !preflight() {
        return;
    }
    let root_dir = TempDir::new().expect("tempdir");

    // ── Harness #1: provision + snapshot + drop ──
    let (agent_id, conv_id, sandbox_id, container_id_round1) = {
        let harness = make_harness(root_dir.path()).await;
        let (agent_id, conv_id) = make_agent_and_conv(&harness, "tier-2-agent").await;
        let conv = open_conv(&harness, &agent_id, &conv_id).await;
        prepare(conv.as_ref()).await;

        let sandbox_id = latest_sandbox_id(conv.as_ref()).await.unwrap();
        let containers = list_containers_for_sandbox(&conv_id, &sandbox_id);
        assert_eq!(containers.len(), 1);
        let container_id = containers.into_iter().next().unwrap();

        exec_shell(conv.as_ref(), &sandbox_id, "echo 'tier-2-marker' > /tmp/x").await;

        // Snapshot the sandbox while the container is still live. PR #20's
        // snapshot_sandbox needs the handle in the harness's
        // running_sandboxes map, which prepare_conversation populated above.
        conv.snapshot_sandbox(sandbox_id.clone())
            .await
            .expect("snapshot_sandbox should succeed while container is live");

        drop(conv);
        (agent_id, conv_id, sandbox_id, container_id)
    };

    // Simulate the container being reaped (idle-TTL expired, manual rm,
    // host pruning, whatever). Tier 1 should miss in round 2.
    rm_container(&container_id_round1);
    assert!(
        list_containers_for_sandbox(&conv_id, &sandbox_id).is_empty(),
        "container should be gone after rm",
    );

    // ── Harness #2: restore-from-snapshot ──
    {
        let harness = make_harness(root_dir.path()).await;
        let conv = open_conv(&harness, &agent_id, &conv_id).await;
        prepare(conv.as_ref()).await;

        // A new container exists, but it's a DIFFERENT docker id from the
        // one we removed — the harness booted a new container from the
        // snapshotted image, it didn't somehow resurrect the original.
        let containers = list_containers_for_sandbox(&conv_id, &sandbox_id);
        assert_eq!(
            containers.len(),
            1,
            "tier 2 should leave exactly one container"
        );
        assert_ne!(
            containers[0], container_id_round1,
            "tier 2 should create a NEW container (restored from snapshot)"
        );

        // The marker from before the snapshot is back, proving the
        // snapshot's filesystem layer was actually applied.
        let (rc, stdout, _) = exec_shell(conv.as_ref(), &sandbox_id, "cat /tmp/x").await;
        assert_eq!(rc, 0);
        assert_eq!(
            stdout.trim(),
            "tier-2-marker",
            "snapshot's filesystem state should be present after tier-2 restore"
        );

        // Still exactly one logical sandbox: tier 2 reuses the existing
        // `sandbox_id`, it doesn't create a new one.
        assert_eq!(
            count_events_of_type(conv.as_ref(), "sandbox_created").await,
            1,
            "tier 2 must not create a new sandbox"
        );
        assert!(
            count_events_of_type(conv.as_ref(), "sandbox_started").await >= 1,
            "tier 2 should emit a SandboxStarted event when restoring"
        );

        let containers = list_containers_for_sandbox(&conv_id, &sandbox_id);
        for c in containers {
            rm_container(&c);
        }
    }
}

// ───────────────────── Tier 3 ─────────────────────
//
// First harness creates the container, writes a marker, **does not**
// take a snapshot. Drop the harness, `docker rm -f` the container.
// Second harness's `try_resume` misses (Tier 1), the snapshot lookup
// returns nothing (Tier 2), so `ensure_shell_sandbox` falls through to
// `create_sandbox` (Tier 3). Result: a brand-new sandbox with a new
// logical `sandbox_id`, fresh container, and no trace of the previous
// state.

#[tokio::test]
#[ignore = "spawns real docker container; run with `cargo test -- --ignored`"]
async fn tier_3_gone_container_without_snapshot_creates_fresh() {
    if !preflight() {
        return;
    }
    let root_dir = TempDir::new().expect("tempdir");

    // ── Harness #1: create, write marker, drop. NO snapshot. ──
    let (agent_id, conv_id, sandbox_id_round1, container_id_round1) = {
        let harness = make_harness(root_dir.path()).await;
        let (agent_id, conv_id) = make_agent_and_conv(&harness, "tier-3-agent").await;
        let conv = open_conv(&harness, &agent_id, &conv_id).await;
        prepare(conv.as_ref()).await;

        let sandbox_id = latest_sandbox_id(conv.as_ref()).await.unwrap();
        let containers = list_containers_for_sandbox(&conv_id, &sandbox_id);
        assert_eq!(containers.len(), 1);
        let container_id = containers.into_iter().next().unwrap();

        exec_shell(conv.as_ref(), &sandbox_id, "echo 'tier-3-marker' > /tmp/x").await;

        drop(conv);
        (agent_id, conv_id, sandbox_id, container_id)
    };

    rm_container(&container_id_round1);

    // ── Harness #2: should create a fresh sandbox ──
    {
        let harness = make_harness(root_dir.path()).await;
        let conv = open_conv(&harness, &agent_id, &conv_id).await;
        prepare(conv.as_ref()).await;

        let sandbox_id_round2 = latest_sandbox_id(conv.as_ref()).await.unwrap();
        assert_ne!(
            sandbox_id_round2, sandbox_id_round1,
            "tier 3 should create a new sandbox with a new id"
        );

        // Event log should now show two `sandbox_created`s.
        assert_eq!(
            count_events_of_type(conv.as_ref(), "sandbox_created").await,
            2,
            "tier 3 should record a second SandboxCreated event"
        );

        // The marker from round 1 is gone — the new container is fresh.
        let (rc, _, _) = exec_shell(conv.as_ref(), &sandbox_id_round2, "test -f /tmp/x").await;
        assert_ne!(rc, 0, "marker should NOT exist in the fresh container");

        // Cleanup the new container.
        let containers = list_containers_for_sandbox(&conv_id, &sandbox_id_round2);
        for c in containers {
            rm_container(&c);
        }
    }
}

// ───────────────────── helpers ─────────────────────

fn preflight() -> bool {
    let docker_ok = Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !docker_ok {
        eprintln!("docker not available, skipping lifecycle test");
        return false;
    }
    let backend = std::env::var("EXO_TEST_SANDBOX_BACKEND").unwrap_or_else(|_| "docker".into());
    if backend != "docker" {
        eprintln!("lifecycle test is docker-only, skipping (EXO_TEST_SANDBOX_BACKEND={backend})");
        return false;
    }
    true
}

async fn make_harness(root: &Path) -> BasicExoHarness {
    BasicExoHarness::new(BasicExoHarnessConfig {
        root: root.to_path_buf(),
        secret_backend: SecretBackendChoice::Static([9u8; 32]),
        sandbox_backend: SandboxBackendChoice::Docker,
    })
    .await
    .expect("BasicExoHarness::new")
}

async fn make_agent_and_conv(
    harness: &BasicExoHarness,
    agent_slug: &str,
) -> (AgentId, ConversationId) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: agent_slug.into(),
            name: agent_slug.into(),
        })
        .await
        .expect("new_agent");
    let agent_id = agent.record().id;
    let conv = agent
        .new_conversation(NewConversationRequest {
            slug: Some("conv".into()),
            name: Some("conversation".into()),
        })
        .await
        .expect("new_conversation");
    let conv_id = conv.record().id;
    (agent_id, conv_id)
}

async fn open_conv(
    harness: &BasicExoHarness,
    agent_id: &AgentId,
    conv_id: &ConversationId,
) -> Arc<dyn ConversationHandle> {
    let agent = harness
        .get_agent(agent_id)
        .await
        .expect("get_agent")
        .expect("agent should exist on disk");
    agent
        .get_conversation(conv_id)
        .await
        .expect("get_conversation")
        .expect("conversation should exist on disk")
}

fn test_agent_config() -> AgentConfig {
    AgentConfig {
        instructions: Vec::new(),
        harness: AgentHarnessKind::Basic,
        typescript: None,
        enable_agent_tool_creation: false,
        sandbox_image: Some(SANDBOX_IMAGE.into()),
        enable_networking: false,
        model: "lifecycle-test-model".into(),
        max_output_tokens: None,
        max_tool_round_trips: None,
        braintrust: None,
    }
}

fn test_conv_config() -> ConversationConfig {
    ConversationConfig {
        enable_networking: false,
        // shell_program: Some(_) is the trigger for ensure_shell_sandbox.
        shell_program: Some("bash".into()),
        mounts: Vec::new(),
    }
}

/// Fire `BasicToolRuntime::prepare_conversation`, which is the public-ish
/// entry point that internally calls `ensure_shell_sandbox` — the
/// function that runs the 3-tier lifecycle fallback we're testing.
async fn prepare(conv: &dyn ConversationHandle) {
    BasicToolRuntime
        .prepare_conversation(conv, &test_agent_config(), &test_conv_config())
        .await
        .expect("prepare_conversation");
}

async fn exec_shell(
    conv: &dyn ConversationHandle,
    sandbox_id: &SandboxId,
    cmd: &str,
) -> (i32, String, String) {
    let process = conv
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id.clone(),
            command: vec!["/bin/bash".into(), "-c".into(), cmd.into()],
            env: Default::default(),
        })
        .await
        .unwrap_or_else(|error| panic!("run_in_sandbox({cmd:?}) failed: {error:#}"));
    let mut parts: SandboxProcessParts = process.into_parts();
    drop(parts.stdin);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let (a, b, c) = tokio::join!(
        parts.stdout.read_to_end(&mut stdout),
        parts.stderr.read_to_end(&mut stderr),
        parts.wait,
    );
    a.expect("read stdout");
    b.expect("read stderr");
    let exit_code = c.expect("wait");
    (
        exit_code,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

async fn latest_sandbox_id(conv: &dyn ConversationHandle) -> Option<SandboxId> {
    let result = conv
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(50),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await
        .ok()?;
    for event in result.events {
        if let EventData::SandboxCreated { sandbox_id, .. } = event.data {
            return Some(sandbox_id);
        }
    }
    None
}

async fn count_events_of_type(conv: &dyn ConversationHandle, ty: &str) -> usize {
    let mut cursor = None;
    let mut count = 0;
    loop {
        let result = conv
            .get_events(Some(EventQuery {
                cursor,
                direction: Some(EventQueryDirection::Asc),
                limit: Some(100),
                session_id: None,
                turn_id: None,
                types: Some(vec![EventKind::custom(ty.to_string())]),
            }))
            .await
            .expect("get_events");
        let events_empty = result.events.is_empty();
        count += result.events.len();
        if events_empty || result.cursor.is_none() {
            break;
        }
        cursor = result.cursor;
    }
    count
}

fn list_containers_for_sandbox(conv_id: &ConversationId, sandbox_id: &SandboxId) -> Vec<String> {
    let label = format!("exo.sandbox.key=conversation:{conv_id}:{sandbox_id}");
    let output = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={label}"),
            "--format",
            "{{.ID}}",
        ])
        .output()
        .expect("docker ps");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .collect()
}

fn docker_container_state(id: &str) -> String {
    let output = Command::new("docker")
        .args(["inspect", "--format", "{{.State.Status}}", id])
        .output()
        .expect("docker inspect");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn rm_container(id: &str) {
    let _ = Command::new("docker").args(["rm", "-f", id]).output();
}
