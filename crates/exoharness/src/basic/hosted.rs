use super::*;
use crate::secrets::{SecretCipher, StaticSecretKeyProvider};

impl BasicExoHarness {
    /// Runs the shared store with host byte storage and sandbox transport. The
    /// host owns its invocation and process lifetime, not metadata semantics.
    pub fn hosted(
        storage: Arc<dyn crate::Storage>,
        master_key: [u8; 32],
        sandboxes: Arc<dyn crate::ExoHttpTransport>,
    ) -> Self {
        let storage = BasicObjectStore::new(storage);
        let cipher = SecretCipher::new(Arc::new(StaticSecretKeyProvider::new(master_key)));
        Self {
            caller: None,
            inner: Arc::new(BasicExoHarnessInner {
                access_policy: Default::default(),
                vaults: BasicVaultStore::hosted(storage.clone(), cipher),
                storage,
                write_lock: AsyncMutex::new(()),
                resource_locks: Mutex::default(),
                subscribers: Mutex::default(),
                sandbox_rpc: crate::HttpExoHarness::from_transport(sandboxes),
            }),
        }
    }
}

pub(super) struct BasicScopedSandboxHandle<'a> {
    harness: &'a BasicExoHarness,
    owner: ResourceScope,
    remote: crate::HttpSandboxHandle,
}
impl std::ops::Deref for BasicScopedSandboxHandle<'_> {
    type Target = crate::HttpSandboxHandle;
    fn deref(&self) -> &Self::Target {
        &self.remote
    }
}
impl<'a> BasicScopedSandboxHandle<'a> {
    fn new(harness: &'a BasicExoHarness, owner: ResourceScope) -> Self {
        Self {
            harness,
            owner,
            remote: harness.inner.sandbox_rpc.sandbox_handle(owner),
        }
    }
    pub(super) fn agent(harness: &'a BasicExoHarness, agent_id: AgentId) -> Self {
        Self::new(harness, ResourceScope::Agent { agent_id })
    }
    pub(super) fn conversation(
        harness: &'a BasicExoHarness,
        agent_id: AgentId,
        thread_id: ConversationId,
    ) -> Self {
        Self::new(
            harness,
            ResourceScope::Thread {
                agent_id,
                thread_id,
            },
        )
    }
    pub(super) fn turn(
        harness: &'a BasicExoHarness,
        agent_id: AgentId,
        thread_id: ConversationId,
        _directory: PathBuf,
        _session_id: SessionId,
        _turn_id: TurnId,
        _state: &'a Mutex<BasicTurnState>,
    ) -> Self {
        Self::conversation(harness, agent_id, thread_id)
    }
    pub(super) async fn terminate_sandbox_locked(&self, id: SandboxId) -> Result<()> {
        self.harness.check(self.owner).await?;
        self.remote.terminate_sandbox(id).await
    }
}

pub(super) async fn terminate_running_sandboxes(
    scope: &BasicScopedSandboxHandle<'_>,
) -> Result<()> {
    scope.harness.check(scope.owner).await?;
    for sandbox in scope.list_sandboxes().await? {
        if sandbox.running {
            scope.terminate_sandbox(sandbox.id).await?;
        }
    }
    Ok(())
}
pub(super) async fn prepare_sandbox_scopes_for_deletion(
    _harness: &BasicExoHarness,
    scopes: &[BasicScopedSandboxHandle<'_>],
) -> Result<bool> {
    for scope in scopes {
        if scope
            .list_sandboxes()
            .await?
            .iter()
            .any(|sandbox| sandbox.running)
        {
            return Ok(false);
        }
    }
    Ok(true)
}
pub(super) async fn remove_thread_resources(
    _harness: &BasicExoHarness,
    _agent: AgentId,
    _thread: ConversationId,
) -> Result<()> {
    Ok(())
}

impl BasicAgentHandle {
    pub(super) async fn prepare_resources_impl(
        &self,
        resources: Vec<crate::resources::ResourceDefinition>,
    ) -> Result<Vec<crate::resources::PreparedResource>> {
        self.harness
            .check(ResourceScope::Agent {
                agent_id: self.record.id,
            })
            .await?;
        anyhow::ensure!(
            resources.is_empty(),
            "this host does not support filesystem resources"
        );
        Ok(Vec::new())
    }
}
impl BasicConversationHandle {
    pub(super) async fn materialize_resources_impl(
        &self,
        resources: Vec<crate::resources::PreparedResource>,
        _provider: SandboxProvider,
    ) -> Result<Vec<FileSystemMount>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        anyhow::ensure!(
            resources.is_empty(),
            "this host does not support filesystem resources"
        );
        Ok(Vec::new())
    }
    pub(super) fn check_resource_environment(&self, _provider: &SandboxProvider) -> Result<()> {
        Ok(())
    }
    pub(super) fn check_resource_fork(&self) -> Result<()> {
        Ok(())
    }
}
