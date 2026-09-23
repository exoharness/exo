//! Runta lifecycle contract tests; no cloud credentials or runtimes required.
use bytes::Bytes;
use exoharness::{
    ManagedSandboxBackend, RuntaConfig, RuntaSandboxBackend, SandboxLifecycleConfig,
    SandboxNetworkPolicy, SandboxRequest, SandboxSpec, SnapshotFormat, SnapshotPayload,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request() -> SandboxRequest {
    SandboxRequest {
        sandbox_id: "sandbox-1".into(),
        scope: None,
        spec: SandboxSpec {
            image: String::new(),
            resources: Default::default(),
            mounts: vec![],
            durable_file_systems: vec![],
            policy: SandboxNetworkPolicy::Unrestricted.into(),
            default_workdir: "/tmp".into(),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(90)),
        },
        provider_state: None,
    }
}
fn backend(server: &MockServer) -> RuntaSandboxBackend {
    RuntaSandboxBackend::new(RuntaConfig {
        token: "test-token".into(),
        api_url: server.uri(),
    })
    .unwrap()
}
fn runtime(id: &str, status: &str, revision: u64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"data": {
        "id": id, "status": status, "desired_status": status, "revision": revision
    }}))
}
fn runtime_name() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let req = request();
    let mut spec = DefaultHasher::new();
    req.spec.hash(&mut spec);
    let mut name = DefaultHasher::new();
    req.sandbox_id.hash(&mut name);
    format!("{:016x}", spec.finish()).hash(&mut name);
    format!("exo-{:016x}", name.finish())
}

fn empty_list() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({"data":[],"pagination":{"has_more":false,"next_cursor":null}}))
}

async fn existing(server: &MockServer, status: &str) {
    Mock::given(method("GET")).and(path("/v2/runtimes"))
        .and(header("Authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{
            "id":"runtime-1", "display_name":runtime_name(), "status":status, "desired_status":status, "revision":7
        }],"pagination":{"has_more":false,"next_cursor":null}})))
        .mount(server).await;
    Mock::given(method("GET"))
        .and(path("/v2/runtimes/runtime-1"))
        .respond_with(runtime("runtime-1", status, 7))
        .mount(server)
        .await;
}

#[tokio::test]
async fn creates_with_resources_image_and_idle_policy() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/runtimes"))
        .respond_with(empty_list())
        .expect(1)
        .mount(&server)
        .await;
    let mut req = request();
    req.spec.image = "my-runtime-image".into();
    Mock::given(method("POST")).and(path("/v2/runtimes"))
        .and(header("Authorization", "Bearer test-token"))
        .and(body_partial_json(json!({"image":{"id":"my-runtime-image"},
            "resources":{"requests":{"vcpus": req.spec.resources.vcpu_count.get(), "memory_mib":req.spec.resources.memory_mib.get()}},
            "idle_policy":{"mode":"suspend_and_wakeup","suspend_after_secs":90}})))
        .respond_with(runtime("runtime-1", "running", 1)).expect(1).mount(&server).await;
    let handle = backend(&server).acquire(req).await.unwrap();
    assert_eq!(handle.id(), "runta:sandbox-1");
    assert!(handle.provider_state().is_some());
}

#[derive(Deserialize)]
struct CreateBody {
    image: Option<serde::de::IgnoredAny>,
    name: String,
}

#[tokio::test]
async fn default_image_is_omitted_and_creation_is_idempotent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/runtimes"))
        .respond_with(empty_list())
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/runtimes"))
        .respond_with(runtime("runtime-1", "running", 1))
        .mount(&server)
        .await;
    backend(&server).acquire(request()).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let create = requests.iter().find(|r| r.method == "POST").unwrap();
    let body: CreateBody = serde_json::from_slice(&create.body).unwrap();
    assert!(body.image.is_none());
    assert_eq!(
        create.headers.get("Idempotency-Key").unwrap(),
        body.name.as_str()
    );
}

#[tokio::test]
async fn reuses_paused_runtime_and_stops_without_deleting() {
    let server = MockServer::start().await;
    existing(&server, "paused").await;
    Mock::given(method("POST"))
        .and(path("/v2/runtimes/runtime-1/resume"))
        .and(query_param("expected_revision", "7"))
        .respond_with(runtime("runtime-1", "running", 8))
        .expect(1)
        .mount(&server)
        .await;
    let handle = backend(&server).acquire(request()).await.unwrap();
    server.reset().await;
    existing(&server, "running").await;
    Mock::given(method("POST"))
        .and(path("/v2/runtimes/runtime-1/pause"))
        .and(query_param("expected_revision", "7"))
        .respond_with(runtime("runtime-1", "paused", 8))
        .expect(1)
        .mount(&server)
        .await;
    handle.stop().await.unwrap();
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method != "DELETE")
    );
}

#[tokio::test]
async fn starts_suspended_runtime() {
    let server = MockServer::start().await;
    existing(&server, "suspended").await;
    Mock::given(method("POST"))
        .and(path("/v2/runtimes/runtime-1/start"))
        .and(query_param("expected_revision", "7"))
        .respond_with(runtime("runtime-1", "running", 8))
        .expect(1)
        .mount(&server)
        .await;
    backend(&server).acquire(request()).await.unwrap();
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    checkpoint_id: String,
}

#[tokio::test]
async fn snapshots_wait_for_readiness_and_restore_tracks_new_runtime() {
    let server = MockServer::start().await;
    existing(&server, "running").await;
    let provider = backend(&server);
    let handle = provider.acquire(request()).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/v2/runtimes/runtime-1/checkpoints"))
        .and(body_partial_json(json!({"kind":"full"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{"id":"cp-1","state":"creating"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/checkpoints/cp-1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data":{"id":"cp-1","state":"ready"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let payload = handle.snapshot().await.unwrap();
    assert_eq!(payload.format, SnapshotFormat::RuntaRef);
    let manifest: Manifest = serde_json::from_slice(&payload.bytes).unwrap();
    assert_eq!(manifest.checkpoint_id, "cp-1");
    Mock::given(method("POST"))
        .and(path("/v2/runtimes"))
        .and(body_partial_json(json!({"checkpoint_id":"cp-1"})))
        .respond_with(runtime("restored-1", "running", 1))
        .expect(1)
        .mount(&server)
        .await;
    let restored = provider
        .acquire_from_snapshot(request(), payload)
        .await
        .unwrap();
    let mut req = request();
    req.provider_state = restored.provider_state();
    Mock::given(method("GET"))
        .and(path("/v2/runtimes/restored-1"))
        .respond_with(runtime("restored-1", "running", 1))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    provider.acquire(req).await.unwrap();
}

#[tokio::test]
async fn rejects_unsupported_policy_and_snapshot_before_network_access() {
    let server = MockServer::start().await;
    let provider = backend(&server);
    let mut req = request();
    req.spec.policy = SandboxNetworkPolicy::Disabled.into();
    assert!(
        provider
            .acquire(req)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("networking.disabled")
    );
    assert!(
        provider
            .acquire_from_snapshot(
                request(),
                SnapshotPayload {
                    format: SnapshotFormat::E2bRef,
                    bytes: Bytes::new(),
                }
            )
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("runta-ref")
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn authentication_failure_does_not_create_a_runtime() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    assert!(backend(&server).acquire(request()).await.is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn missing_persisted_runtime_is_an_error() {
    let server = MockServer::start().await;
    existing(&server, "running").await;
    let provider = backend(&server);
    let handle = provider.acquire(request()).await.unwrap();
    let mut req = request();
    req.provider_state = handle.provider_state();
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v2/runtimes/runtime-1"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    assert!(
        provider
            .acquire(req)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("no longer exists")
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn creation_waits_until_runtime_is_running() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/runtimes"))
        .respond_with(empty_list())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/runtimes"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data":{
            "id":"runtime-1", "status":"creating", "desired_status":"running", "revision":1
        }})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/runtimes/runtime-1"))
        .respond_with(runtime("runtime-1", "running", 2))
        .expect(1)
        .mount(&server)
        .await;
    backend(&server).acquire(request()).await.unwrap();
}

#[tokio::test]
async fn failed_checkpoint_is_not_saved_as_a_snapshot() {
    let server = MockServer::start().await;
    existing(&server, "running").await;
    let handle = backend(&server).acquire(request()).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/v2/runtimes/runtime-1/checkpoints"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{"id":"cp-1","state":"failed"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert!(
        handle
            .snapshot()
            .await
            .unwrap_err()
            .to_string()
            .contains("entered failed")
    );
}

#[tokio::test]
async fn name_lookup_follows_pagination_and_uses_runtime_uuid() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/v2/runtimes"))
        .and(query_param("after", "page-two"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{
            "id":"runtime-1", "display_name":runtime_name(), "status":"paused", "desired_status":"paused", "revision":7
        }],"pagination":{"has_more":false,"next_cursor":null}})))
        .with_priority(1).expect(1).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/v2/runtimes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data":[],"pagination":{"has_more":true,"next_cursor":"page-two"}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v2/runtimes/runtime-1/resume"))
        .respond_with(runtime("runtime-1", "running", 8))
        .expect(1)
        .mount(&server)
        .await;
    backend(&server).acquire(request()).await.unwrap();
}
