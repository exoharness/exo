//! Wiremock-driven tests for the Daytona sandbox backend. Each test points a
//! `DaytonaSandboxBackend` at an in-process fake of Daytona's REST API and
//! asserts on the wire contract — endpoints hit (control plane vs toolbox host),
//! request bodies/params, and how canned responses drive find-or-create.
//! Hermetic by design, so it catches code defects but not upstream drift.

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

/// Standard sandbox request used across tests. Conversation-keyed so the label
/// format matches what the find-by-label query expects to see.
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
            idle_ttl: Some(Duration::from_secs(300)),
        },
    }
}

/// Construct a backend pointed at a wiremock instance. Same wiremock host for
/// both control-plane and toolbox URLs — Daytona's real deployment uses
/// separate hosts, but tests differentiate by path prefix.
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

/// Mount a `GET /sandbox` (find-by-label) responder returning `items`.
async fn mount_find(server: &MockServer, items: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_response(items)))
        .mount(server)
        .await;
}

// ─────────────────────── acquire: find-or-create ───────────────────────

#[tokio::test]
async fn acquire_creates_when_no_warm_match() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
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

    let requests = server.received_requests().await.unwrap_or_default();
    let create = requests
        .iter()
        .find(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "POST")
        .expect("POST /sandbox (create) should have been called");
    let body: Value = serde_json::from_slice(&create.body).expect("body is JSON");

    // Labels carry the SandboxKey + spec hash; their absence would break
    // cross-process recovery by label.
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

    // Daytona's `snapshot` field refers to a pre-registered named snapshot, not
    // a docker image — a fresh create must never set it.
    assert!(
        body.get("snapshot").is_none() || body["snapshot"].is_null(),
        "fresh acquire must NOT set `snapshot`: {body:?}"
    );
}

#[tokio::test]
async fn acquire_reuses_running_match_without_create_or_start() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    // A labelled, already-running match: no create and no start should follow.
    mount_find(&server, vec![sandbox_json("sb-running", "started")]).await;

    let request = make_request("conv-3", "sandbox-3");
    backend
        .acquire(request)
        .await
        .expect("acquire should reuse the running sandbox");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests
            .iter()
            .any(|r| r.method.to_string().to_uppercase() == "POST"),
        "reusing a running sandbox must not POST (no create, no start): {:?}",
        requests.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn acquire_starts_stopped_match_without_creating() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, vec![sandbox_json("sb-stopped", "stopped")]).await;
    Mock::given(method("POST"))
        .and(path("/sandbox/sb-stopped/start"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let request = make_request("conv-4", "sandbox-4");
    backend
        .acquire(request)
        .await
        .expect("acquire should start the stopped sandbox");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/sandbox/sb-stopped/start"),
        "a stopped match must be started"
    );
    // It must reuse the existing sandbox, not create a fresh one.
    assert!(
        !requests
            .iter()
            .any(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "POST"),
        "starting a stopped match must NOT create a new sandbox"
    );
}

#[tokio::test]
async fn acquire_does_not_start_transient_match() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    // A sandbox mid-startup must be left alone: issuing /start on a "starting"
    // sandbox races with Daytona's own transition.
    mount_find(&server, vec![sandbox_json("sb-starting", "starting")]).await;

    backend
        .acquire(make_request("conv-12", "sandbox-12"))
        .await
        .expect("acquire should reuse the transitioning sandbox");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        !requests
            .iter()
            .any(|r| r.method.to_string().to_uppercase() == "POST"),
        "a transient (starting) sandbox must not be started or recreated: {:?}",
        requests.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn acquire_replaces_dead_match_with_fresh_create() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    // A terminal/error sandbox can't be reused — acquire must create a new one.
    mount_find(&server, vec![sandbox_json("sb-dead", "destroyed")]).await;
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-new", "started")))
        .expect(1)
        .mount(&server)
        .await;

    backend
        .acquire(make_request("conv-13", "sandbox-13"))
        .await
        .expect("acquire should create a fresh sandbox when the match is dead");

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "POST"),
        "a dead match must trigger a fresh create"
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.url.path() == "/sandbox/sb-dead/start"),
        "a dead match must not be started"
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
    let msg = format!("{error:#}").to_lowercase();
    assert!(
        msg.contains("mount") || msg.contains("daytona"),
        "error should mention mounts and/or Daytona: {msg}"
    );

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(
        requests.len(),
        0,
        "mount-rejected request must not reach the API"
    );
}

#[tokio::test]
async fn acquire_filters_by_label_as_single_json_query_param() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    Mock::given(method("GET"))
        .and(path("/sandbox"))
        .and(query_param_present("labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list_response(Vec::new())))
        .expect(1)
        .mount(&server)
        .await;
    // No warm match → acquire proceeds to create; mock it so acquire succeeds.
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sandbox_json("sb-q", "started")))
        .mount(&server)
        .await;

    let request = make_request("conv-6", "sandbox-6");
    backend.acquire(request).await.unwrap();

    let requests = server.received_requests().await.unwrap_or_default();
    let find = requests
        .iter()
        .find(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "GET")
        .expect("GET /sandbox (find) should have been called");
    let label_params: Vec<_> = find
        .url
        .query_pairs()
        .filter(|(k, _)| k == "labels")
        .collect();
    assert_eq!(
        label_params.len(),
        1,
        "labels must be one query param, not repeated: {}",
        find.url
    );
    let parsed: Value =
        serde_json::from_str(&label_params[0].1).expect("labels query value must be JSON-encoded");
    assert!(
        parsed.get("exo.sandbox.key").is_some(),
        "labels JSON should include exo.sandbox.key: {parsed:?}"
    );
}

/// Assert that a given query param key is present in the URL (without asserting
/// on its value).
fn query_param_present(name: &'static str) -> impl wiremock::Match {
    struct Present(&'static str);
    impl wiremock::Match for Present {
        fn matches(&self, request: &wiremock::Request) -> bool {
            request.url.query_pairs().any(|(k, _)| k == self.0)
        }
    }
    Present(name)
}

// ─────────────────────── stop ───────────────────────

#[tokio::test]
async fn stop_calls_stop_endpoint_not_delete() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
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
    // No DELETE: stopped sandboxes must survive so the next process resumes them.
    assert!(
        !requests
            .iter()
            .any(|r| r.method.to_string().to_uppercase() == "DELETE"),
        "stop must not DELETE the sandbox"
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

    mount_find(&server, Vec::new()).await;
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

    let handle = backend
        .acquire(make_request("conv-8", "sandbox-8"))
        .await
        .unwrap();
    let payload = handle
        .snapshot()
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

    // acquire_from_snapshot creates directly (no find step).
    Mock::given(method("POST"))
        .and(path("/sandbox"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sandbox_json("sb-restored", "started")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let manifest = json!({ "snapshot_name": "exo-snap-canonical-fixture" });
    let payload = SnapshotPayload {
        kind: SnapshotKind::DaytonaSnapshot,
        bytes: Bytes::from(serde_json::to_vec(&manifest).unwrap()),
    };

    let request = make_request("conv-9", "sandbox-9");
    backend
        .acquire_from_snapshot(request, payload)
        .await
        .expect("acquire_from_snapshot should succeed against a 200 mock");

    let requests = server.received_requests().await.unwrap_or_default();
    let create = requests
        .iter()
        .find(|r| r.url.path() == "/sandbox" && r.method.to_string().to_uppercase() == "POST")
        .expect("POST /sandbox should have been called");
    let body: Value = serde_json::from_slice(&create.body).unwrap();
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

    let requests = server.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 0, "kind-mismatch must not reach the API");
}

// ─────────────────────── exec (toolbox URL) ───────────────────────

#[tokio::test]
async fn exec_uses_toolbox_url_not_api_url() {
    let server = MockServer::start().await;
    let backend = backend_for_mock(&server);

    mount_find(&server, Vec::new()).await;
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

    let handle = backend
        .acquire(make_request("conv-11", "sandbox-11"))
        .await
        .unwrap();
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

    // The toolbox path must be used: in production it's a different DNS name,
    // so the code must route exec through `toolbox_endpoint`, not `api_endpoint`.
    let requests = server.received_requests().await.unwrap_or_default();
    assert!(
        requests
            .iter()
            .any(|r| r.url.path() == "/toolbox/sb-exec/process/execute"),
        "exec must hit /toolbox/<id>/process/execute"
    );
}

/// Match any POST body that has a `command` field — keeps the test from
/// over-asserting on the precise shell-rendering while still proving the body
/// shape lines up with Daytona's expected schema.
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
