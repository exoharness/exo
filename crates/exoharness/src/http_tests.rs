use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use tempfile::TempDir;

use crate::{
    BasicExoHarness, BasicExoHarnessConfig, ExoHarness, HttpExoHarness, SandboxBackendChoice,
    SecretBackendChoice, serve_exoharness_http_listener,
};

fn local_test_config(root: impl Into<std::path::PathBuf>) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.into(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_backend: SandboxBackendChoice::LocalProcess,
    }
}

struct HttpHarnessFixture {
    harness: Arc<dyn ExoHarness>,
    server: actix_web::rt::task::JoinHandle<crate::Result<()>>,
    _tempdir: TempDir,
}

impl Drop for HttpHarnessFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn http_harness() -> HttpHarnessFixture {
    let tempdir = TempDir::new().expect("tempdir");
    let basic = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("basic harness");
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("listener");
    let addr = listener.local_addr().expect("local addr");
    let server = actix_web::rt::spawn(serve_exoharness_http_listener(listener, Arc::new(basic)));
    let harness: Arc<dyn ExoHarness> =
        Arc::new(HttpExoHarness::new(format!("http://{addr}")).expect("http harness"));

    HttpHarnessFixture {
        harness,
        server,
        _tempdir: tempdir,
    }
}

#[actix_web::test]
async fn http_exoharness_supports_agent_and_conversation_crud() {
    let fixture = http_harness().await;
    crate::contract_tests::supports_agent_and_conversation_crud(Arc::clone(&fixture.harness)).await;
}

#[actix_web::test]
async fn http_exoharness_begin_turn_tracks_events_through_finish() {
    let fixture = http_harness().await;
    crate::contract_tests::begin_turn_tracks_events_through_finish(Arc::clone(&fixture.harness))
        .await;
}

#[actix_web::test]
async fn http_exoharness_turn_events_continue_after_artifact_writes() {
    let fixture = http_harness().await;
    crate::contract_tests::turn_events_continue_after_artifact_writes(Arc::clone(&fixture.harness))
        .await;
}

#[actix_web::test]
async fn http_exoharness_conversation_scope_overrides_and_forks() {
    let fixture = http_harness().await;
    crate::contract_tests::conversation_scope_overrides_agent_scope_and_fork_copies_bindings(
        Arc::clone(&fixture.harness),
    )
    .await;
}
