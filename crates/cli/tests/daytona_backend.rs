//! Wiremock-driven tests for the Daytona sandbox backend (PR #22).
//!
//! Daytona is a remote SaaS, so exercising the backend against the real
//! service in CI would require credentials, network egress, real sandbox
//! provisioning (which costs money and takes seconds per test), and
//! cleanup that's brittle if a test panics partway through. Instead, each
//! test stands up an in-process `wiremock` HTTP server that pretends to
//! be Daytona's REST API, points a `DaytonaSandboxBackend` at it, and
//! asserts on:
//!
//!   - which endpoints were hit (verifying URL routing — control plane
//!     vs the separate toolbox host),
//!   - what request bodies/query params were sent (verifying the
//!     translation from `SandboxRequest` etc. into Daytona's wire
//!     format), and
//!   - how the backend interprets the canned responses (state machine
//!     transitions, payload encoding).
//!
//! This catches code-defects in the backend without catching upstream
//! Daytona drift. Drift detection is the job of the manual live test
//! plan documented in the PR description.

use std::collections::HashMap;

use bytes::Bytes;
use exoharness::{
    DaytonaConfig, DaytonaSandboxBackend, ManagedSandboxBackend, SandboxKey,
    SandboxLifecycleConfig, SandboxMount, SandboxMountAccess, SandboxNetworkPolicy, SandboxRequest,
    SandboxSpec, SnapshotKind, SnapshotPayload,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ─────────────────────── Test fixtures / helpers ───────────────────────

/// Standard sandbox request used across tests. Conversation-keyed so the
/// label format matches what try_resume's filter expects to see.
fn make_request(conversation_id: &str, sandbox_id: &str) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: conversation_id.into(),
            sandbox_id: sandbox_id.into(),
        },
        spec: SandboxSpec {
            image: "docker.io/library/ubuntu:24.04".into(),
            mounts: Vec::new(),
            network: SandboxNetworkPolicy::Enabled,
            default_workdir: "/".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            // idle_ttl required for try_resume to even attempt a lookup
            // (try_resume short-circuits to None for non-warm sandboxes).
            idle_ttl: Some(Duration::from_secs(300)),
        },
    }
}

/// Construct a backend pointed at a wiremock instance. Same wiremock host
/// for both control-plane and toolbox URLs — Daytona's real deployment
/// uses separate hosts, but tests differentiate by path prefix.
fn backend_for_mock(server: &MockServer) -> DaytonaSandboxBackend {
    DaytonaSandboxBackend::new(DaytonaConfig {
        api_key: "test-api-key".into(),
        api_url: server.uri(),
        toolbox_url: server.uri(),
        target: None,
        organization_id: Some("test-org".into()),
    })
    .expect("DaytonaSandboxBackend::new")
}

fn sandbox_json(id: &str, state: &str) -> Value {
    json!({
        "id": id,
        "state": state,
        "createdAt": "2026-05-25T08:00:00Z"
    })
}

fn list_response(items: Vec<Value>) -> Value {
    json!({ "items": items })
}

// ─────────────────────── acquire ───────────────────────

#[tokio::test]
async fn acquire_posts_to_sandbox_endpoint_with_labels() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-fresh", "started")))
        .expect(1)
        .mount(&server)
        .await;

    let request = make_request("conv-1", "sandbox-1");
    let _handle = backend
        .acquire(request)
        .await
        .expect("acquire should succeed against a 200-returning mock");

    // Inspect what the backend actually sent on the wire.
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).expect("body is JSON");

    // Labels carry the SandboxKey + spec hash. We only assert the key is
    // present (spec hash is computed internally); the absence of these
    // labels would break try_resume across processes.
    let labels = body
        .get("labels")
        .and_then(Value::as_object)
        .expect("labels present");
    assert!(
        labels.contains_key("exo.sandbox.key"),
        "labels should include exo.sandbox.key: {labels:?}"
    );
    assert!(
        labels.contains_key("exo.sandbox.spec-hash"),
        "labels should include exo.sandbox.spec-hash: {labels:?}"
    );

    // Daytona's `snapshot` field refers to a pre-registered named
    // snapshot, not a docker image — `acquire` (fresh-create) should
    // never set it, so the platform falls back to its default image.
    assert!(
        body.get("snapshot").is_none() || body["snapshot"].is_null(),
        "fresh acquire must NOT set `snapshot`: {body:?}"
    );
}

#[tokio::test]
async fn acquire_rejects_host_mounts() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    let mut request = make_request("conv-2", "sandbox-2");
    request.spec.mounts.push(SandboxMount {
        host_path: PathBuf::from("/tmp/foo"),
        guest_path: "/workspace".into(),
        access: SandboxMountAccess::ReadWrite,
        internal: false,
    });

    let error = match backend.acquire(request).await {
        Ok(_) => panic!("acquire should reject requests with host mounts"),
        Err(e) => e,
    };
    let msg = format!("{error:#}");
    assert!(
        msg.to_lowercase().contains("mount") || msg.to_lowercase().contains("daytona"),
        "error should mention mounts and/or Daytona: {msg}"
    );

    // No HTTP call should have happened — request was rejected up front.
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests.len(),
        0,
        "mount-rejected request must not reach the API"
    );
}

// ─────────────────────── try_resume ───────────────────────

#[tokio::test]
async fn try_resume_finds_running_sandbox_without_starting() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    // GET /sandbox returns one labelled match in `started` state — no
    // /start call should follow.
    Mock::given(method("GET"))
        .and(path("/sandbox"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(list_response(vec![sandbox_json("sb-running", "started")])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let request = make_request("conv-3", "sandbox-3");
    let handle = backend
        .try_resume(request)
        .await
        .expect("try_resume should not error");
    assert!(
        handle.is_some(),
        "try_resume should find the running sandbox"
    );

    // Crucially: no POST /sandbox/sb-running/start should have happened.
    let requests = server.received_requests().await.unwrap_or_default();
    let start_calls = requests
        .iter()
        .filter(|r| r.url.path().contains("/start"))
        .count();
    assert_eq!(start_calls, 0, "must not start an already-running sandbox");
}

#[tokio::test]
async fn try_resume_starts_stopped_sandbox() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("GET"))
        .and(path("/sandbox"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(list_response(vec![sandbox_json("sb-stopped", "stopped")])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandbox/sb-stopped/start"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let request = make_request("conv-4", "sandbox-4");
    let handle = backend.try_resume(request).await.expect("try_resume ok");
    assert!(handle.is_some(), "try_resume should produce a handle");

    // Both endpoints should have fired.
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests.iter().any(|r| r.url.path().ends_with("/sandbox")),
        "list /sandbox must be called"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/sandbox/sb-stopped/start"),
        "start endpoint must be called for a stopped sandbox"
    );
}

#[tokio::test]
async fn try_resume_returns_none_when_no_match() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("GET"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_response(Vec::new())))
        .expect(1)
        .mount(&server)
        .await;

    let request = make_request("conv-5", "sandbox-5");
    let handle = backend.try_resume(request).await.expect("try_resume ok");
    assert!(
        handle.is_none(),
        "try_resume must return None when the label query is empty"
    );
}

#[tokio::test]
async fn try_resume_filters_by_label_as_single_json_query_param() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    // The matcher requires `labels` to be present as a single query
    // param (not repeated). Wiremock's `query_param` matches the *first*
    // value — if multiple were sent we'd want to fail; the absence-of-
    // assertion is covered below by parsing the URL after the fact.
    Mock::given(method("GET"))
        .and(path("/sandbox"))
        .and(query_param_present("labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_response(Vec::new())))
        .expect(1)
        .mount(&server)
        .await;

    let request = make_request("conv-6", "sandbox-6");
    backend.try_resume(request).await.unwrap();

    let requests = server.received_requests().await.unwrap_or_default();
    let url = &requests[0].url;
    let label_params: Vec<_> = url.query_pairs().filter(|(k, _)| k == "labels").collect();
    assert_eq!(
        label_params.len(),
        1,
        "labels must be one query param, not repeated: {url}"
    );
    // Confirm the value is JSON, not k=v-style.
    let value = &label_params[0].1;
    let parsed: Value =
        serde_json::from_str(value).expect("labels query value must be JSON-encoded");
    assert!(
        parsed.get("exo.sandbox.key").is_some(),
        "labels JSON should include exo.sandbox.key: {parsed:?}"
    );
}

/// Helper: assert that a given query param key is present in the URL
/// (without asserting on its value).
fn query_param_present(name: &'static str) -> impl wiremock::Match {
    struct Present(&'static str);
    impl wiremock::Match for Present {
        fn matches(&self, request: &wiremock::Request) -> bool {
            request.url.query_pairs().any(|(k, _)| k == self.0)
        }
    }
    Present(name)
}

// ─────────────────────── stop / Drop ───────────────────────

#[tokio::test]
async fn stop_calls_stop_endpoint_not_delete() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-stop", "started")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandbox/sb-stop/stop"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let handle = backend
        .acquire(make_request("conv-7", "sandbox-7"))
        .await
        .unwrap();
    handle.stop().await.expect("stop should succeed");

    let requests = server.received_requests().await.unwrap_or_default();
    // No DELETE should have been issued — the resume contract requires
    // stopped sandboxes to survive so the next process can resume them.
    let deletes: Vec<_> = requests
        .iter()
        .filter(|r| r.method.to_string().to_uppercase() == "DELETE")
        .collect();
    assert!(
        deletes.is_empty(),
        "stop must not DELETE the sandbox: {deletes:?}"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/sandbox/sb-stop/stop"),
        "POST /sandbox/<id>/stop should have been called"
    );
}

// ─────────────────────── snapshot ───────────────────────

#[tokio::test]
async fn snapshot_returns_daytona_snapshot_payload_with_manifest() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-snap", "started")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sandbox/sb-snap/snapshot"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let request = make_request("conv-8", "sandbox-8");
    let handle = backend.acquire(request).await.unwrap();
    let payload = handle
        .snapshot()
        // Snapshot mode is filesystem-only on this backend; we go through
        // SnapshotMode::Filesystem to hit the implemented path. The
        // current trait shape on this PR doesn't take a mode arg yet —
        // if the snapshot API signature ever grows it, this call site
        // updates trivially.
        .await
        .expect("snapshot should succeed against the mock");

    assert!(
        matches!(payload.kind, SnapshotKind::DaytonaSnapshot),
        "kind should be DaytonaSnapshot, got {:?}",
        payload.kind
    );

    let manifest: Value =
        serde_json::from_slice(&payload.bytes).expect("payload should be a JSON manifest");
    assert!(
        manifest
            .get("snapshot_name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.starts_with("exo-snap-")),
        "manifest should carry a snapshot_name with the exo-snap- prefix: {manifest:?}"
    );
    assert!(
        manifest.get("base_image").is_some(),
        "manifest should carry the base_image: {manifest:?}"
    );

    // Verify request body sent the snapshot name to Daytona.
    let requests = server.received_requests().await.unwrap_or_default();
    let snap_call = requests
        .iter()
        .find(|r| r.url.path() == "/sandbox/sb-snap/snapshot")
        .expect("snapshot endpoint should have been called");
    let body: Value = serde_json::from_slice(&snap_call.body).unwrap();
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .expect("snapshot request body must contain `name`");
    assert!(name.starts_with("exo-snap-"));
}

#[tokio::test]
async fn acquire_from_snapshot_passes_snapshot_name_in_create_body() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sandbox_json("sb-restored", "started")),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Construct a DaytonaSnapshot payload by hand. The manifest schema is
    // private to daytona.rs; we serialize the same shape directly.
    let manifest = json!({
        "snapshot_name": "exo-snap-canonical-fixture",
        "base_image": "ubuntu:24.04",
    });
    let payload = SnapshotPayload {
        kind: SnapshotKind::DaytonaSnapshot,
        bytes: Bytes::from(serde_json::to_vec(&manifest).unwrap()),
    };

    let request = make_request("conv-9", "sandbox-9");
    let _handle = backend
        .acquire_from_snapshot(request, payload)
        .await
        .expect("acquire_from_snapshot should succeed against a 200 mock");

    let requests = server.received_requests().await.unwrap_or_default();
    let create_call = requests
        .iter()
        .find(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "POST")
        .expect("POST /sandbox should have been called");
    let body: Value = serde_json::from_slice(&create_call.body).unwrap();
    assert_eq!(
        body.get("snapshot").and_then(Value::as_str),
        Some("exo-snap-canonical-fixture"),
        "create-from-snapshot must pass the manifest's snapshot_name to Daytona: {body:?}"
    );
}

#[tokio::test]
async fn acquire_from_snapshot_rejects_wrong_kind() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    let payload = SnapshotPayload {
        kind: SnapshotKind::DockerImageTar,
        bytes: Bytes::from_static(b"\x00not-a-real-tar"),
    };
    let request = make_request("conv-10", "sandbox-10");
    let error = match backend.acquire_from_snapshot(request, payload).await {
        Ok(_) => panic!("Daytona backend must reject a DockerImageTar payload"),
        Err(e) => e,
    };
    let msg = format!("{error:#}").to_lowercase();
    assert!(
        msg.contains("daytona") || msg.contains("docker") || msg.contains("kind"),
        "error should explain the kind mismatch: {msg}"
    );

    // No API call should have happened — we rejected the payload before
    // hitting the network.
    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 0, "kind-mismatch must not reach the API");
}

// ─────────────────────── exec (toolbox URL) ───────────────────────

#[tokio::test]
async fn exec_uses_toolbox_url_not_api_url() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-exec", "started")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/toolbox/sb-exec/process/execute"))
        .and(body_json_includes_command())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "exitCode": 0,
            "result": "hello from mock",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let request = make_request("conv-11", "sandbox-11");
    let handle = backend.acquire(request).await.unwrap();
    let output = handle
        .exec(&exoharness::SandboxCommand {
            argv: vec!["/bin/echo".into(), "hello".into()],
            env: HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: None,
        })
        .await
        .expect("exec should succeed");

    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "hello from mock");

    // Most important assertion: the toolbox path was used. The same
    // hostname is fine in test (one wiremock), but in production the
    // toolbox lives on a different DNS name; routing in the code MUST
    // call `toolbox_endpoint` here, not `api_endpoint`.
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/toolbox/sb-exec/process/execute"),
        "exec must hit /toolbox/<id>/process/execute"
    );
}

/// Match any POST body that has a `command` field — keeps the test from
/// over-asserting on the precise shell-rendering, while still proving
/// the body shape lines up with Daytona's expected schema.
fn body_json_includes_command() -> impl wiremock::Match {
    struct Has;
    impl wiremock::Match for Has {
        fn matches(&self, request: &wiremock::Request) -> bool {
            serde_json::from_slice::<Value>(&request.body)
                .ok()
                .and_then(|v| v.get("command").and_then(Value::as_str).map(str::to_string))
                .is_some()
        }
    }
    Has
}
