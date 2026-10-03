use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use exoharness::{AgentId, ConversationHandle};

pub(crate) fn needs_local_session(command: &crate::Commands) -> bool {
    matches!(
        command,
        crate::Commands::Agent {
            command: crate::AgentCommands::Run { .. },
            ..
        } | crate::Commands::Conversation {
            command: crate::ConversationCommands::Send { .. },
            ..
        } | crate::Commands::Conversation {
            command: crate::ConversationCommands::Sandbox {
                command: crate::ConversationSandboxCommands::Run { .. }
            },
            ..
        }
    )
}

fn open_root_lock(root: &Path) -> Result<File> {
    std::fs::create_dir_all(root)?;
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("service.lock"))?)
}

/// Local executions share the root, while a server owns it exclusively.
/// Acquire this before opening a thread or changing its configuration.
pub(crate) struct LocalRootLease {
    root: PathBuf,
    _lock: File,
}

impl LocalRootLease {
    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        let lock = open_root_lock(root)?;
        lock.try_lock_shared().context("exo serve owns this local state root; use an HTTP provider to run commands through the server")?;
        Ok(Self {
            root: root.to_owned(),
            _lock: lock,
        })
    }
}

pub(crate) fn lock_server_root(root: &Path) -> Result<File> {
    let lock = open_root_lock(root)?;
    lock.try_lock()
        .context("a local CLI session or another agent service is using this root")?;
    Ok(lock)
}

/// A local CLI owns a thread's VM lifetime. The OS releases this lock on a
/// crash, allowing the next session to stop leftover VMs before resuming.
pub(crate) struct LocalSession {
    pub thread: Arc<dyn ConversationHandle>,
    _lock: File,
    _root: Arc<LocalRootLease>,
}

impl LocalSession {
    pub(crate) async fn start(
        root: Arc<LocalRootLease>,
        agent: AgentId,
        thread: Arc<dyn ConversationHandle>,
    ) -> Result<Self> {
        let directory = root.root.join("sessions").join(agent.to_string());
        std::fs::create_dir_all(&directory)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join(format!("{}.lock", thread.record().id)))?;
        lock.try_lock()
            .context("this thread already has a local CLI session")?;
        if thread
            .list_sandboxes()
            .await?
            .iter()
            .any(|sandbox| sandbox.running && !sandbox.attached)
        {
            eprintln!("Stopping sandboxes left by the previous local session...");
            stop_session_sandboxes(Some(thread.as_ref())).await?;
        }
        Ok(Self {
            thread,
            _lock: lock,
            _root: root,
        })
    }
}

/// Local sessions release their managed sandboxes after the harness has exited.
/// HTTP clients leave lifecycle ownership with the server and pass no thread.
pub(crate) async fn stop_session_sandboxes(thread: Option<&dyn ConversationHandle>) -> Result<()> {
    let Some(thread) = thread else {
        return Ok(());
    };
    let mut failure = None;
    for sandbox in thread.list_sandboxes().await? {
        if sandbox.running
            && !sandbox.attached
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
        AttachSandboxRequest, BasicExoHarness, BasicExoHarnessConfig, CreateSandboxRequest,
        ExoHarness, LocalProcessSandboxBackend, ManagedSandboxBackend, ManagedSandboxHandle,
        NewAgentRequest, SandboxAttachment, SandboxBackendRegistration, SandboxProvider,
        SandboxRequest, SecretBackendChoice, SnapshotFormat, SnapshotPayload,
    };

    fn config(root: &Path) -> BasicExoHarnessConfig {
        BasicExoHarnessConfig {
            root: root.to_owned(),
            secret_backend: SecretBackendChoice::Static([7; 32]),
            sandbox_default: SandboxProvider::LocalProcess,
            sandbox_policy: None,
            sandbox_backends: vec![
                SandboxBackendRegistration::local_process(),
                SandboxBackendRegistration::from_backend(
                    SandboxProvider::Docker,
                    Arc::new(AttachedBackend),
                ),
            ],
        }
    }

    fn request() -> CreateSandboxRequest {
        CreateSandboxRequest {
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
        }
    }

    struct AttachedBackend;

    #[async_trait::async_trait]
    impl ManagedSandboxBackend for AttachedBackend {
        fn is_local(&self) -> bool {
            true
        }
        fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
            &[]
        }
        async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
            LocalProcessSandboxBackend.acquire(request).await
        }
        async fn attach(
            &self,
            request: SandboxRequest,
            _attachment: SandboxAttachment,
        ) -> Result<Arc<dyn ManagedSandboxHandle>> {
            self.acquire(request).await
        }
        async fn acquire_from_snapshot(
            &self,
            _request: SandboxRequest,
            _payload: SnapshotPayload,
        ) -> Result<Arc<dyn ManagedSandboxHandle>> {
            anyhow::bail!("unsupported")
        }
    }

    #[tokio::test]
    async fn session_exit_stops_only_its_managed_sandboxes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let harness = BasicExoHarness::new(config(temp.path())).await?;
        let agent = harness
            .new_agent(NewAgentRequest {
                slug: "session".into(),
                name: "Session".into(),
                vaults: vec![],
            })
            .await?;
        let session = agent.new_thread(Default::default()).await?;
        let other = agent.new_thread(Default::default()).await?;
        let managed = session.create_sandbox(request()).await?;
        let attached = session
            .attach_sandbox(AttachSandboxRequest {
                attachment: SandboxAttachment::DockerContainer {
                    container_id: "external".into(),
                },
                default_workdir: None,
            })
            .await?;
        other.create_sandbox(request()).await?;
        stop_session_sandboxes(None).await?;
        let events = temp
            .path()
            .join("agents")
            .join(agent.record().id.to_string())
            .join("conversations")
            .join(session.record().id.to_string())
            .join("events");
        std::fs::write(events.join("unreadable.json"), b"invalid event JSON")?;
        assert!(session.get_events(None).await.is_err());
        stop_session_sandboxes(Some(session.as_ref())).await?;
        let sandboxes = session.list_sandboxes().await?;
        assert!(!sandboxes.iter().find(|s| s.id == managed).unwrap().running);
        let external = sandboxes.iter().find(|s| s.id == attached).unwrap();
        assert!(external.running && external.attached);
        assert!(other.list_sandboxes().await?[0].running);
        Ok(())
    }

    #[tokio::test]
    async fn reopening_recovers_abandoned_sandboxes_and_excludes_live_sessions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let harness = BasicExoHarness::new(config(temp.path())).await?;
        let agent = harness
            .new_agent(NewAgentRequest {
                slug: "recovery".into(),
                name: "Recovery".into(),
                vaults: vec![],
            })
            .await?;
        let thread = agent.new_thread(Default::default()).await?;
        let root = Arc::new(LocalRootLease::acquire(temp.path())?);
        let session = LocalSession::start(root.clone(), agent.record().id, thread.clone()).await?;
        thread.create_sandbox(request()).await?;
        // Simulate process death: release its OS lock without clean shutdown.
        drop(session);
        let reopened = BasicExoHarness::new(config(temp.path())).await?;
        let agent = reopened.get_agent(&agent.record().id).await?.unwrap();
        let thread = agent.get_thread(&thread.record().id).await?.unwrap();
        let recovered =
            LocalSession::start(root.clone(), agent.record().id, thread.clone()).await?;
        assert!(!thread.list_sandboxes().await?[0].running);
        let active = thread.create_sandbox(request()).await?;
        assert!(
            LocalSession::start(root, agent.record().id, thread.clone())
                .await
                .is_err()
        );
        assert!(
            thread
                .list_sandboxes()
                .await?
                .iter()
                .find(|s| s.id == active)
                .unwrap()
                .running
        );
        stop_session_sandboxes(Some(recovered.thread.as_ref())).await?;
        Ok(())
    }

    #[tokio::test]
    async fn server_and_local_sessions_cannot_claim_each_others_sandboxes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let harness = BasicExoHarness::new(config(temp.path())).await?;
        let agent = harness
            .new_agent(NewAgentRequest {
                slug: "ownership".into(),
                name: "Ownership".into(),
                vaults: vec![],
            })
            .await?;
        let thread = agent.new_thread(Default::default()).await?;
        thread.create_sandbox(request()).await?;
        let server = lock_server_root(temp.path())?;
        assert!(LocalRootLease::acquire(temp.path()).is_err());
        assert!(thread.list_sandboxes().await?[0].running);
        drop(server);
        let root = Arc::new(LocalRootLease::acquire(temp.path())?);
        let session = LocalSession::start(root.clone(), agent.record().id, thread).await?;
        assert!(lock_server_root(temp.path()).is_err());
        drop(root);
        assert!(lock_server_root(temp.path()).is_err());
        drop(session);
        let _server = lock_server_root(temp.path())?;
        Ok(())
    }
}
