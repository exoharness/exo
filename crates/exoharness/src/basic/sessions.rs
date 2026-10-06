use std::{collections::HashMap, fs::File, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use tokio::sync::OnceCell;

use super::sandboxes::StoredSandbox;
use super::{BasicExoHarness, BasicScopedSandboxHandle};
use crate::{AgentId, ResourceScope, SandboxId};

/// Process ownership is scoped to the sandbox's thread (or agent scope).
/// The OS releases each lease on a crash; a new owner then recovers its VMs.
// TurnCoordinator owns execution ordering. These leases also protect thread
// mutations and retained VMs between turns, so their lifetime stays separate.
pub(super) struct LocalSessions {
    root: PathBuf,
    state: std::sync::Mutex<SessionState>,
}

#[derive(Default)]
struct SessionState {
    leases: HashMap<ResourceScope, Arc<SessionLease>>,
    closed: bool,
}

pub(super) struct SessionLease {
    pub recovered: OnceCell<()>,
    _lock: File,
    path: PathBuf,
}

impl LocalSessions {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            state: Default::default(),
        }
    }

    // A short namespace guard prevents agent deletion racing thread creation
    // or a new ownership claim. It is never held for a session's lifetime.
    pub async fn agent_change(&self, agent: AgentId) -> Result<File> {
        let lock = self.open(agent, "lifecycle.lock")?;
        tokio::task::spawn_blocking(move || {
            lock.lock().context("locking agent lifecycle")?;
            Ok(lock)
        })
        .await?
    }

    fn open(&self, agent: AgentId, name: &str) -> Result<File> {
        let directory = self.root.join("sessions").join(agent.to_string());
        std::fs::create_dir_all(&directory)?;
        Ok(std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join(name))?)
    }

    pub fn claim(&self, scope: ResourceScope) -> Result<Arc<SessionLease>> {
        let mut state = self.state.lock().expect("local sessions poisoned");
        anyhow::ensure!(!state.closed, "local runtime has shut down");
        if let Some(lease) = state.leases.get(&scope) {
            return Ok(lease.clone());
        }
        let lease = self.open_lease(scope)?;
        state.leases.insert(scope, lease.clone());
        Ok(lease)
    }

    /// Acquire every affected scope before publishing any new ownership. A
    /// failed agent deletion must not stop a subset of its unowned VMs on exit.
    pub fn claim_all(&self, scopes: &[ResourceScope]) -> Result<()> {
        let mut state = self.state.lock().expect("local sessions poisoned");
        anyhow::ensure!(!state.closed, "local runtime has shut down");
        let mut acquired = HashMap::new();
        for &scope in scopes {
            if !state.leases.contains_key(&scope) && !acquired.contains_key(&scope) {
                acquired.insert(scope, self.open_lease(scope)?);
            }
        }
        state.leases.extend(acquired);
        Ok(())
    }

    fn open_lease(&self, scope: ResourceScope) -> Result<Arc<SessionLease>> {
        let (agent, name) = match scope {
            ResourceScope::Agent { agent_id } => (agent_id, "agent.lock".into()),
            ResourceScope::Thread {
                agent_id,
                thread_id,
            } => (agent_id, format!("{thread_id}.lock")),
            ResourceScope::Global => anyhow::bail!("global sandboxes have no local session owner"),
        };
        let lock = self.open(agent, &name)?;
        lock.try_lock().with_context(|| match scope {
            ResourceScope::Thread { thread_id, .. } => format!("thread {thread_id} is owned by another local process; use its HTTP provider if it is served by exo serve"),
            _ => format!("agent {agent} sandboxes are owned by another local process"),
        })?;
        Ok(Arc::new(SessionLease {
            recovered: OnceCell::new(),
            _lock: lock,
            path: self
                .root
                .join("sessions")
                .join(agent.to_string())
                .join(name),
        }))
    }

    pub fn owns(&self, scope: ResourceScope) -> bool {
        self.state
            .lock()
            .expect("local sessions poisoned")
            .leases
            .contains_key(&scope)
    }

    pub fn claimed(&self, scope: ResourceScope) -> Result<Arc<SessionLease>> {
        self.state
            .lock()
            .expect("local sessions poisoned")
            .leases
            .get(&scope)
            .cloned()
            .context("sandbox scope has not been claimed")
    }

    pub fn release(&self, scope: ResourceScope) -> Result<()> {
        let lease = self
            .state
            .lock()
            .expect("local sessions poisoned")
            .leases
            .remove(&scope);
        if let Some(lease) = lease {
            std::fs::remove_file(&lease.path).context("removing deleted session lock")?;
        }
        Ok(())
    }

    pub fn release_agent(&self, agent: AgentId) -> Result<()> {
        let scopes: Vec<_> = self
            .state
            .lock()
            .expect("local sessions poisoned")
            .leases
            .keys()
            .copied()
            .filter(|scope| scope.agent_id() == Some(agent))
            .collect();
        for scope in scopes {
            self.release(scope)?;
        }
        Ok(())
    }

    pub fn finish(&self) -> HashMap<ResourceScope, Arc<SessionLease>> {
        let mut state = self.state.lock().expect("local sessions poisoned");
        state.closed = true;
        std::mem::take(&mut state.leases)
    }
}

impl BasicExoHarness {
    pub(super) async fn agent_change(&self, agent: AgentId) -> Result<Option<File>> {
        match &self.sessions {
            Some(sessions) => Ok(Some(sessions.agent_change(agent).await?)),
            None => Ok(None),
        }
    }

    /// Enforce process ownership for inline execution and local provider servers.
    /// Clones and caller-scoped handles share ownership; read-only access and
    /// published TCP connections never claim a session.
    pub fn with_local_sessions(mut self, root: PathBuf) -> Self {
        self.sessions = Some(Arc::new(LocalSessions::new(root)));
        self
    }

    pub(super) async fn claim_local_scope(
        &self,
        scope: ResourceScope,
    ) -> Result<Option<Arc<SessionLease>>> {
        let Some(sessions) = &self.sessions else {
            return Ok(None);
        };
        self.check(scope).await?;
        let lease = if sessions.owns(scope) {
            sessions.claim(scope)?
        } else {
            let _namespace = sessions
                .agent_change(scope.agent_id().context("session requires an agent")?)
                .await?;
            anyhow::ensure!(
                self.inner
                    .storage
                    .get_bytes_if_exists(self.owner_dir(scope).join("record.json"))
                    .await?
                    .is_some(),
                "session owner no longer exists"
            );
            sessions.claim(scope)?
        };
        lease.recovered.get_or_try_init(|| async {
            // Recovery uses the lease we already hold, avoiding recursive
            // acquisition while stopping machines left by a crashed owner.
            let mut operator = self.clone();
            operator.caller = None;
            let sandboxes = scope_sandboxes(&operator, scope);
            if !owned_running_sandbox_ids(&sandboxes).await?.is_empty() {
                tracing::info!(target: "exoharness::progress", "Stopping sandboxes left by the previous local session...");
                stop_owned_sandboxes(&sandboxes).await?;
            }
            Ok::<_, anyhow::Error>(())
        }).await?;
        Ok(Some(lease))
    }

    /// Stop owned local sandboxes and release their leases after execution drains.
    pub async fn release_local_sessions(&self) -> Result<()> {
        let Some(sessions) = &self.sessions else {
            return Ok(());
        };
        let leases = sessions.finish();
        let mut operator = self.clone();
        operator.caller = None;
        let mut failure = None;
        for &scope in leases.keys() {
            let owned = scope_sandboxes(&operator, scope);
            if let Err(error) = stop_owned_sandboxes(&owned).await {
                tracing::error!(%error, ?scope, "failed to stop session sandboxes");
                failure = Some(error);
            }
        }
        // Hold every lease until all cleanup has finished.
        drop(leases);
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn scope_sandboxes(
    harness: &BasicExoHarness,
    scope: ResourceScope,
) -> BasicScopedSandboxHandle<'_> {
    match scope {
        ResourceScope::Thread {
            agent_id,
            thread_id,
        } => BasicScopedSandboxHandle::conversation(harness, agent_id, thread_id),
        ResourceScope::Agent { agent_id } => BasicScopedSandboxHandle::agent(harness, agent_id),
        ResourceScope::Global => unreachable!("global scopes cannot own a local session"),
    }
}

async fn stop_owned_sandboxes(scope: &BasicScopedSandboxHandle<'_>) -> Result<()> {
    let mut failure = None;
    for sandbox_id in owned_running_sandbox_ids(scope).await? {
        if let Err(error) = scope.stop_sandbox(sandbox_id.clone()).await {
            tracing::error!(%error, %sandbox_id, "failed to stop sandbox");
            failure = Some(error);
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn owned_running_sandbox_ids(scope: &BasicScopedSandboxHandle<'_>) -> Result<Vec<SandboxId>> {
    Ok(scope
        .harness
        .inner
        .storage
        .list_json_matching_suffix::<StoredSandbox>(scope.sandboxes_dir(), ".json")
        .await?
        .into_iter()
        .filter(|sandbox| sandbox.running && sandbox.attachment.is_none())
        .map(|sandbox| sandbox.id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AttachSandboxRequest, ExoHarness, LocalProcessSandboxBackend, ManagedSandboxBackend,
        ManagedSandboxHandle, NewThreadRequest, SandboxAttachment, SandboxBackendRegistration,
        SandboxProvider, SandboxRequest, SnapshotFormat, SnapshotPayload, WriteArtifactRequest,
        test_support,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecoveryBackend {
        acquisitions: AtomicUsize,
        pause_acquisitions: AtomicBool,
        resume_acquisition: tokio::sync::Notify,
        stops: AtomicUsize,
        terminations: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ManagedSandboxBackend for RecoveryBackend {
        fn is_local(&self) -> bool {
            true
        }
        fn stop_requires_provider_state(&self) -> bool {
            false
        }
        fn consumable_snapshot_formats(&self) -> &[SnapshotFormat] {
            &[]
        }
        async fn acquire(&self, request: SandboxRequest) -> Result<Arc<dyn ManagedSandboxHandle>> {
            self.acquisitions.fetch_add(1, Ordering::SeqCst);
            if self.pause_acquisitions.load(Ordering::SeqCst) {
                self.resume_acquisition.notified().await;
            }
            LocalProcessSandboxBackend.acquire(request).await
        }
        async fn stop(&self, _request: SandboxRequest) -> Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn terminate(&self, request: SandboxRequest) -> Result<()> {
            self.terminations.fetch_add(1, Ordering::SeqCst);
            LocalProcessSandboxBackend.terminate(request).await
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

    async fn harness(root: &std::path::Path) -> Result<BasicExoHarness> {
        Ok(BasicExoHarness::new(test_support::local_test_config(root))
            .await?
            .with_local_sessions(root.to_owned()))
    }

    #[tokio::test]
    async fn independent_threads_coexist_and_same_thread_mutations_are_rejected() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let server = harness(temp.path()).await?;
        let agent = test_support::new_test_agent(&server, "ownership").await?;
        let thread = agent.new_thread(Default::default()).await?;
        let sandbox = thread
            .create_sandbox(test_support::sandbox_request())
            .await?;
        let fork = thread.fork(Default::default()).await?;
        assert!(fork.list_sandboxes().await?.is_empty());
        assert!(thread.list_sandboxes().await?[0].running);
        let inline = harness(temp.path()).await?;
        let other_agent = inline.get_agent(&agent.record().id).await?.unwrap();
        let busy = other_agent.get_thread(&thread.record().id).await?.unwrap();
        assert!(
            busy.activate_caller()
                .await
                .unwrap_err()
                .to_string()
                .contains("owned by another local process")
        );
        let fork_elsewhere = other_agent.get_thread(&fork.record().id).await?.unwrap();
        assert!(fork_elsewhere.activate_caller().await.is_err());
        assert!(agent.delete_thread(&fork.record().id).await?);
        assert!(
            !temp
                .path()
                .join("sessions")
                .join(agent.record().id.to_string())
                .join(format!("{}.lock", fork.record().id))
                .exists()
        );
        assert!(
            busy.create_sandbox(test_support::sandbox_request())
                .await
                .is_err()
        );
        assert!(busy.stop_sandbox(sandbox.clone()).await.is_err());
        assert!(
            busy.write_artifact(WriteArtifactRequest {
                path: "config.json".into(),
                contents: b"changed".to_vec()
            })
            .await
            .is_err()
        );
        let turn = thread.begin_turn(Default::default()).await?;
        let busy_turn = busy.turn_handle(turn.record().clone()).await?;
        assert!(
            busy_turn
                .add_events(vec![crate::EventData::SessionStarted])
                .await
                .is_err()
        );
        assert!(
            busy_turn
                .write_artifact(WriteArtifactRequest {
                    path: "turn-config.json".into(),
                    contents: b"changed".to_vec(),
                })
                .await
                .is_err()
        );
        assert!(busy_turn.finish().await.is_err());
        turn.finish().await?;
        assert!(
            other_agent
                .delete_thread(&thread.record().id)
                .await
                .is_err()
        );
        assert!(thread.list_sandboxes().await?[0].running);
        let other = other_agent.new_thread(Default::default()).await?;
        other
            .create_sandbox(test_support::sandbox_request())
            .await?;
        inline.release_local_sessions().await?;
        assert!(!other.list_sandboxes().await?[0].running);
        assert!(thread.list_sandboxes().await?[0].running);
        server.release_local_sessions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn crash_recovery_runs_once_per_owner_without_event_history() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let first = harness(temp.path()).await?;
        let agent = test_support::new_test_agent(&first, "recovery").await?;
        let thread = agent.new_thread(Default::default()).await?;
        thread
            .create_sandbox(test_support::sandbox_request())
            .await?;
        let (agent_id, thread_id) = (agent.record().id, thread.record().id);
        let (event_file, original_event) =
            test_support::corrupt_thread_history(temp.path(), agent_id, thread_id)?;
        assert!(thread.get_events(None).await.is_err());
        // Process death releases leases without stopping its persisted VMs.
        drop((thread, agent, first));
        let backend = Arc::new(RecoveryBackend::default());
        let mut config = test_support::local_test_config(temp.path());
        config.sandbox_backends = vec![SandboxBackendRegistration::from_backend(
            SandboxProvider::LocalProcess,
            backend.clone(),
        )];
        let recovered = BasicExoHarness::new(config)
            .await?
            .with_local_sessions(temp.path().to_owned());
        let agent = recovered.get_agent(&agent_id).await?.unwrap();
        let thread = agent.get_thread(&thread_id).await?.unwrap();
        tokio::try_join!(thread.activate_caller(), thread.activate_caller())
            .context("recovering the abandoned VM")?;
        assert!(!thread.list_sandboxes().await?[0].running);
        assert_eq!(backend.stops.load(Ordering::SeqCst), 1);
        assert_eq!(backend.acquisitions.load(Ordering::SeqCst), 0);
        // Repair history before exercising normal sandbox acquisition, which
        // can load provider state. Ownership recovery above never reads it.
        std::fs::write(&event_file, original_event)?;
        backend.pause_acquisitions.store(true, Ordering::SeqCst);
        let restart = thread.start_sandbox(crate::StartSandboxRequest {
            id: thread.list_sandboxes().await?[0].id.clone(),
            snapshot_id: None,
            idle_seconds: None,
            provider: None,
        });
        tokio::pin!(restart);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut restart)
                .await
                .is_err()
        );
        assert_eq!(backend.acquisitions.load(Ordering::SeqCst), 1);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            test_support::new_test_agent(&recovered, "independent"),
        )
        .await??;
        backend.resume_acquisition.notify_one();
        restart.await.context("resuming a sandbox after recovery")?;
        thread.activate_caller().await?;
        assert_eq!(backend.stops.load(Ordering::SeqCst), 1);
        assert!(thread.list_sandboxes().await?.iter().any(|s| s.running));
        recovered.release_local_sessions().await?;
        assert!(thread.list_sandboxes().await?.iter().all(|s| !s.running));
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_stops_managed_sandboxes_and_keeps_attachments_without_history() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let mut config = test_support::local_test_config(temp.path());
        config
            .sandbox_backends
            .push(SandboxBackendRegistration::from_backend(
                SandboxProvider::Docker,
                Arc::new(RecoveryBackend::default()),
            ));
        let owner = BasicExoHarness::new(config)
            .await?
            .with_local_sessions(temp.path().to_owned());
        let agent = test_support::new_test_agent(&owner, "shutdown").await?;
        let thread = agent.new_thread(Default::default()).await?;
        let managed = thread
            .create_sandbox(test_support::sandbox_request())
            .await?;
        let attached = thread
            .attach_sandbox(AttachSandboxRequest {
                attachment: SandboxAttachment::DockerContainer {
                    container_id: "external".into(),
                },
                default_workdir: None,
            })
            .await?;
        test_support::corrupt_thread_history(temp.path(), agent.record().id, thread.record().id)?;
        assert!(thread.get_events(None).await.is_err());
        owner.release_local_sessions().await?;
        let sandboxes = thread.list_sandboxes().await?;
        assert!(!sandboxes.iter().find(|s| s.id == managed).unwrap().running);
        assert!(sandboxes.iter().find(|s| s.id == attached).unwrap().running);
        Ok(())
    }

    #[tokio::test]
    async fn rejected_agent_deletion_does_not_stop_or_claim_other_threads() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let server = harness(temp.path()).await?;
        let agent = test_support::new_test_agent(&server, "deletion").await?;
        let active = agent.new_thread(Default::default()).await?;
        active
            .create_sandbox(test_support::sandbox_request())
            .await?;
        let abandoned = agent.new_thread(Default::default()).await?;
        // Create an unowned VM to model another process that has crashed.
        let unowned = BasicExoHarness::new(test_support::local_test_config(temp.path())).await?;
        let unowned_agent = unowned.get_agent(&agent.record().id).await?.unwrap();
        let unowned_thread = unowned_agent
            .get_thread(&abandoned.record().id)
            .await?
            .unwrap();
        unowned_thread
            .create_sandbox(test_support::sandbox_request())
            .await?;
        // Give up the abandoned thread's lease while retaining the active one.
        let scope = ResourceScope::Thread {
            agent_id: agent.record().id,
            thread_id: abandoned.record().id,
        };
        server
            .sessions
            .as_ref()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .leases
            .remove(&scope);
        let deleting = harness(temp.path()).await?;
        assert!(deleting.delete_agent(&agent.record().id).await.is_err());
        assert!(
            deleting
                .sessions
                .as_ref()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .leases
                .is_empty()
        );
        deleting.release_local_sessions().await?;
        assert!(active.list_sandboxes().await?[0].running);
        assert!(abandoned.list_sandboxes().await?[0].running);
        unowned_thread
            .stop_sandbox(unowned_thread.list_sandboxes().await?[0].id.clone())
            .await?;
        server.release_local_sessions().await?;
        let deleting = harness(temp.path()).await?;
        assert!(deleting.delete_agent(&agent.record().id).await?);
        deleting.release_local_sessions().await?;
        Ok(())
    }

    #[tokio::test]
    async fn deletion_terminates_abandoned_vms_without_resume_recovery() -> Result<()> {
        for delete_agent in [false, true] {
            let temp = tempfile::tempdir()?;
            let first = harness(temp.path()).await?;
            let agent = test_support::new_test_agent(&first, "deletion").await?;
            let thread = agent.new_thread(Default::default()).await?;
            thread
                .create_sandbox(test_support::sandbox_request())
                .await?;
            let (agent_id, thread_id) = (agent.record().id, thread.record().id);
            drop((thread, agent, first));

            let backend = Arc::new(RecoveryBackend::default());
            let mut config = test_support::local_test_config(temp.path());
            config.sandbox_backends = vec![SandboxBackendRegistration::from_backend(
                SandboxProvider::LocalProcess,
                backend.clone(),
            )];
            let deleting = BasicExoHarness::new(config)
                .await?
                .with_local_sessions(temp.path().to_owned());
            if delete_agent {
                assert!(deleting.delete_agent(&agent_id).await?);
            } else {
                let agent = deleting.get_agent(&agent_id).await?.unwrap();
                assert!(agent.delete_thread(&thread_id).await?);
            }
            assert!(
                deleting
                    .sessions
                    .as_ref()
                    .unwrap()
                    .state
                    .lock()
                    .unwrap()
                    .leases
                    .is_empty()
            );
            assert!(
                !temp
                    .path()
                    .join("sessions")
                    .join(agent_id.to_string())
                    .join(format!("{thread_id}.lock"))
                    .exists()
            );
            assert_eq!(backend.terminations.load(Ordering::SeqCst), 1);
            assert_eq!(backend.stops.load(Ordering::SeqCst), 0);
            assert_eq!(backend.acquisitions.load(Ordering::SeqCst), 0);
            deleting.release_local_sessions().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn agent_namespace_changes_wait_without_blocking_other_agents() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let server = harness(temp.path()).await?;
        let agent = test_support::new_test_agent(&server, "namespace").await?;
        let thread = agent.new_thread(Default::default()).await?;
        let inline = harness(temp.path()).await?;
        let other_agent = inline.get_agent(&agent.record().id).await?.unwrap();
        let deletion = server
            .sessions
            .as_ref()
            .unwrap()
            .agent_change(agent.record().id)
            .await?;
        let mut creating = Box::pin(other_agent.new_thread(NewThreadRequest::default()));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut creating)
                .await
                .is_err()
        );
        let independent = test_support::new_test_agent(&inline, "independent").await?;
        independent.new_thread(Default::default()).await?;
        drop(deletion);
        creating.await?;
        assert!(
            other_agent
                .get_thread(&thread.record().id)
                .await?
                .unwrap()
                .activate_caller()
                .await
                .is_err()
        );
        Ok(())
    }
}
