use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use exoharness::{AgentId, ConversationHandle};

pub(crate) fn needs_local_root_lock(command: &crate::Commands) -> bool {
    use crate::{
        AgentCommands, AgentMountCommands, Commands, ConversationCommands,
        ConversationMountCommands, ConversationSandboxCommands,
        environment::{EnvironmentCommands, ProviderCommands},
        vaults::VaultCommands,
    };

    // Queries and forwarding can coexist with a server. All state changes must
    // go through that server while it owns the root.
    match command {
        Commands::Agent { command, .. } => !matches!(
            command,
            AgentCommands::List
                | AgentCommands::Get { .. }
                | AgentCommands::Mount {
                    command: AgentMountCommands::List { .. }
                }
        ),
        Commands::Conversation { command, .. } => !matches!(
            command,
            ConversationCommands::Ports { .. }
                | ConversationCommands::List { .. }
                | ConversationCommands::Get { .. }
                | ConversationCommands::Events { .. }
                | ConversationCommands::Mount {
                    command: ConversationMountCommands::List { .. }
                }
                | ConversationCommands::Sandbox {
                    command: ConversationSandboxCommands::Forward { .. }
                }
        ),
        Commands::Environment { command, .. } => !matches!(
            command,
            EnvironmentCommands::List
                | EnvironmentCommands::Get { .. }
                | EnvironmentCommands::Provider {
                    command: ProviderCommands::List
                }
        ),
        Commands::Vault { command, .. } => !matches!(
            command,
            VaultCommands::List { .. } | VaultCommands::Get { .. }
        ),
        Commands::Serve { .. } | Commands::Provider { .. } | Commands::FirecrackerBridge => false,
    }
}

fn open_root_lock(root: &Path) -> Result<File> {
    std::fs::create_dir_all(root)?;
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("service.lock"))?)
}

/// Local executions and state changes share the root; a server owns it exclusively.
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
        .context("a local CLI command or another agent service is using this root")?;
    Ok(lock)
}

/// A local CLI owns a thread's VM lifetime. The OS releases this lock on a
/// crash, allowing the next session to stop leftover VMs before resuming.
pub(crate) struct LocalSession {
    thread: Arc<dyn ConversationHandle>,
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
            stop_owned_sandboxes(thread.as_ref()).await?;
        }
        Ok(Self {
            thread,
            _lock: lock,
            _root: root,
        })
    }

    /// Release managed sandboxes after the harness has finished shutting down.
    pub(crate) async fn finish(self) -> Result<()> {
        stop_owned_sandboxes(self.thread.as_ref()).await
    }
}

/// Local sessions release their managed sandboxes after the harness has exited.
/// Attached sandboxes keep their external owner.
async fn stop_owned_sandboxes(thread: &dyn ConversationHandle) -> Result<()> {
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
        AttachSandboxRequest, BasicExoHarness, BasicExoHarnessConfig, ExoHarness,
        LocalProcessSandboxBackend, ManagedSandboxBackend, ManagedSandboxHandle, SandboxAttachment,
        SandboxBackendRegistration, SandboxProvider, SandboxRequest, SnapshotFormat,
        SnapshotPayload,
    };

    fn config(root: &Path) -> BasicExoHarnessConfig {
        let mut config = exoharness::test_support::local_test_config(root);
        config
            .sandbox_backends
            .push(SandboxBackendRegistration::from_backend(
                SandboxProvider::Docker,
                Arc::new(AttachedBackend),
            ));
        config
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
        let agent = exoharness::test_support::new_test_agent(&harness, "session").await?;
        let session = agent.new_thread(Default::default()).await?;
        let other = agent.new_thread(Default::default()).await?;
        let local_session = LocalSession::start(
            Arc::new(LocalRootLease::acquire(temp.path())?),
            agent.record().id,
            session.clone(),
        )
        .await?;
        let managed = session
            .create_sandbox(exoharness::test_support::sandbox_request())
            .await?;
        let attached = session
            .attach_sandbox(AttachSandboxRequest {
                attachment: SandboxAttachment::DockerContainer {
                    container_id: "external".into(),
                },
                default_workdir: None,
            })
            .await?;
        other
            .create_sandbox(exoharness::test_support::sandbox_request())
            .await?;
        let events = temp
            .path()
            .join("agents")
            .join(agent.record().id.to_string())
            .join("conversations")
            .join(session.record().id.to_string())
            .join("events");
        std::fs::write(events.join("unreadable.json"), b"invalid event JSON")?;
        assert!(session.get_events(None).await.is_err());
        local_session.finish().await?;
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
        let agent = exoharness::test_support::new_test_agent(&harness, "recovery").await?;
        let thread = agent.new_thread(Default::default()).await?;
        let root = Arc::new(LocalRootLease::acquire(temp.path())?);
        let session = LocalSession::start(root.clone(), agent.record().id, thread.clone()).await?;
        thread
            .create_sandbox(exoharness::test_support::sandbox_request())
            .await?;
        // Simulate process death: release its OS lock without clean shutdown.
        drop(session);
        let reopened = BasicExoHarness::new(config(temp.path())).await?;
        let agent = reopened.get_agent(&agent.record().id).await?.unwrap();
        let thread = agent.get_thread(&thread.record().id).await?.unwrap();
        let recovered =
            LocalSession::start(root.clone(), agent.record().id, thread.clone()).await?;
        assert!(!thread.list_sandboxes().await?[0].running);
        let active = thread
            .create_sandbox(exoharness::test_support::sandbox_request())
            .await?;
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
        recovered.finish().await?;
        Ok(())
    }

    #[tokio::test]
    async fn server_and_local_sessions_cannot_claim_each_others_sandboxes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let harness = BasicExoHarness::new(config(temp.path())).await?;
        let agent = exoharness::test_support::new_test_agent(&harness, "ownership").await?;
        let thread = agent.new_thread(Default::default()).await?;
        thread
            .create_sandbox(exoharness::test_support::sandbox_request())
            .await?;
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
