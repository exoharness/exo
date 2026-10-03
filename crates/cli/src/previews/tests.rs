use super::*;
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, ExoHarness, NewAgentRequest,
    SandboxBackendRegistration, SandboxProvider, SecretBackendChoice,
};

#[test]
fn hostnames_are_stable_valid_and_distinguish_threads_with_the_same_name() -> Result<()> {
    let hostname = thread_hostname("Braintrust Dev", "Hello / world", "first", "exo.localhost")?;
    assert_eq!(
        hostname,
        thread_hostname("Braintrust Dev", "Hello / world", "first", "exo.localhost")?
    );
    assert_ne!(
        hostname,
        thread_hostname("Braintrust Dev", "Hello / world", "second", "exo.localhost")?
    );
    assert!(hostname.starts_with("hello-world-"));
    assert!(hostname.ends_with(".braintrust-dev.exo.localhost"));
    let long = thread_hostname(&"a".repeat(128), &"b".repeat(128), "id", "exo.localhost")?;
    assert!(long.split('.').all(|label| label.len() <= 63));
    Ok(())
}

#[tokio::test]
async fn routing_uses_current_ports_when_thread_history_cannot_be_read() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let harness = BasicExoHarness::new(BasicExoHarnessConfig {
        root: temp.path().to_owned(),
        secret_backend: SecretBackendChoice::Static([7; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    })
    .await?;
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".into(),
            name: "Agent".into(),
            vaults: vec![],
        })
        .await?;
    let thread = agent.new_thread(Default::default()).await?;
    let mut request = exoharness::CreateSandboxRequest {
        name: None,
        provider: SandboxProvider::LocalProcess,
        image: "unused".into(),
        resources: None,
        default_workdir: None,
        file_system_mounts: None,
        durable_file_systems: None,
        policy: None,
        enable_networking: Some(true),
        idle_seconds: Some(300),
        tcp_ports: vec![13000],
    };
    let deleted = thread.create_sandbox(request.clone()).await?;
    thread.terminate_sandbox(deleted).await?;
    let app = thread.create_sandbox(request.clone()).await?;
    let stopped = thread.create_sandbox(request.clone()).await?;
    thread.stop_sandbox(stopped).await?;
    request.tcp_ports = vec![8000];
    let api = thread.create_sandbox(request).await?;

    let events = temp
        .path()
        .join("agents")
        .join(agent.record().id.to_string())
        .join("conversations")
        .join(thread.record().id.to_string())
        .join("events");
    std::fs::write(events.join("unreadable.json"), b"invalid event JSON")?;
    assert!(thread.get_events(None).await.is_err());

    assert_eq!(
        crate::port_forward::published_sandbox(thread.as_ref(), 13000).await?,
        app
    );
    assert_eq!(
        crate::port_forward::published_sandbox(thread.as_ref(), 8000).await?,
        api
    );
    assert!(
        crate::port_forward::published_sandbox(thread.as_ref(), 9000)
            .await
            .is_err()
    );
    Ok(())
}
