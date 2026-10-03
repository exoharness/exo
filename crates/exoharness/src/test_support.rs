use std::path::PathBuf;

use crate::{
    BasicExoHarnessConfig, DaytonaBackendSpec, SandboxBackendRegistration, SandboxProvider,
    SecretBackendChoice,
};

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
