use std::collections::HashSet;

use anyhow::Result;
use exoharness::{ConversationHandle, EventData, EventKind, EventQuery, EventQueryDirection};

/// Local sessions release their managed sandboxes after the harness has exited.
/// HTTP clients leave lifecycle ownership with the server and pass no thread.
pub(crate) async fn stop_session_sandboxes(thread: Option<&dyn ConversationHandle>) -> Result<()> {
    let Some(thread) = thread else {
        return Ok(());
    };
    let events = thread
        .get_events(Some(EventQuery {
            types: Some(vec![
                EventKind::SANDBOX_CREATED,
                EventKind::SANDBOX_ATTACHED,
            ]),
            direction: Some(EventQueryDirection::Asc),
            ..Default::default()
        }))
        .await?
        .events;
    let mut managed = HashSet::new();
    for event in events {
        match event.data {
            EventData::SandboxCreated { sandbox_id, .. } => {
                managed.insert(sandbox_id);
            }
            EventData::SandboxAttached { sandbox_id, .. } => {
                managed.remove(&sandbox_id);
            }
            _ => {}
        }
    }
    let mut failure = None;
    for sandbox in thread.list_sandboxes().await? {
        if sandbox.running
            && managed.contains(&sandbox.id)
            && let Err(error) = thread.stop_sandbox(sandbox.id).await
        {
            failure = Some(error);
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exoharness::{
        BasicExoHarness, BasicExoHarnessConfig, CreateSandboxRequest, ExoHarness, NewAgentRequest,
        SandboxBackendRegistration, SandboxProvider, SecretBackendChoice,
    };

    #[tokio::test]
    async fn session_exit_stops_only_its_managed_sandboxes() -> Result<()> {
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
                slug: "session".into(),
                name: "Session".into(),
                vaults: vec![],
            })
            .await?;
        let session = agent.new_thread(Default::default()).await?;
        let other = agent.new_thread(Default::default()).await?;
        let request = CreateSandboxRequest {
            provider: SandboxProvider::LocalProcess,
            image: "unused".into(),
            tcp_ports: vec![],
            name: None,
            resources: None,
            default_workdir: None,
            file_system_mounts: None,
            durable_file_systems: None,
            policy: None,
            enable_networking: Some(true),
            idle_seconds: Some(300),
        };
        session.create_sandbox(request.clone()).await?;
        other.create_sandbox(request).await?;
        stop_session_sandboxes(None).await?;
        assert!(session.list_sandboxes().await?[0].running);
        stop_session_sandboxes(Some(session.as_ref())).await?;
        assert!(!session.list_sandboxes().await?[0].running);
        assert!(other.list_sandboxes().await?[0].running);
        Ok(())
    }
}
