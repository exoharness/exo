use std::collections::{HashMap, HashSet};
use std::fmt::{self, Display, Formatter};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{
    Mutex as AsyncMutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock as AsyncRwLock, mpsc,
};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::storage::BasicObjectStore;

#[cfg(feature = "basic-backend")]
#[path = "basic/native.rs"]
mod native;
#[cfg(feature = "basic-backend")]
pub use native::*;
#[path = "basic/sandboxes.rs"]
mod sandboxes;
use crate::vault::{
    BasicVaultStore, VaultContext, VaultHandle, VaultId, compose_vaults, global_vault,
    require_vaults,
};
use crate::{
    AddEventsRequest, AddEventsResult, AgentHandle, AgentId, AgentRecord, Artifact,
    ArtifactVersion, AttachSandboxRequest, BeginTurnRequest, Binding, BindingId, BindingRecord,
    BindingType, CancelSandboxProcessRequest, CloseSandboxProcessInputRequest, ConversationHandle,
    ConversationId, ConversationRecord, CreateSandboxRequest, Event, EventData, EventId,
    EventQuery, EventQueryDirection, EventStream, ExoHarness, FileSystemMount,
    ForkConversationRequest, ForkSandboxRequest, GetEventsResult, GetSandboxProcessEventsResult,
    ListConversationsRequest, ListConversationsResult, NewAgentRequest, NewConversationRequest,
    ReadArtifactRequest, ResourceScope, RestoreSandboxRequest, Result, RunInSandboxRequest,
    SandboxAttachment, SandboxHandle, SandboxId, SandboxProcess, SandboxProcessEventQuery,
    SandboxProcessRecord, SandboxProcessStatus, SandboxProvider, SandboxRecord, Secret, SecretId,
    SecretMetadata, SessionId, SnapshotHandle, SnapshotId, StartSandboxProcessRequest,
    StartSandboxRequest, TurnHandle, TurnId, TurnRecord, Uuid7, WaitSandboxProcessRequest,
    WriteArtifactRequest, WriteSandboxProcessInputRequest,
};
pub use sandboxes::SandboxBackendRegistration;
use sandboxes::*;

#[path = "basic/access.rs"]
mod access;
#[cfg(feature = "basic-backend")]
#[path = "basic/migration.rs"]
mod migration;
#[path = "basic/vault_context.rs"]
mod vault_context;
use vault_context::ScopedVaultContext;
#[cfg(feature = "basic-backend")]
#[path = "basic/egress.rs"]
mod egress;
#[cfg(feature = "basic-backend")]
use egress::LocalEgressResolver;

#[cfg(all(test, feature = "basic-backend"))]
#[path = "basic/resource_tests.rs"]
mod resource_tests;

const UNFINISHED_TURNS_DIR: &str = "recovery/unfinished_turns";

#[derive(Serialize, Deserialize)]
struct StoredEventBatch {
    events: Vec<Event>,
}

#[derive(Default)]
struct EventBatchIndex {
    batches: Vec<IndexedEventBatch>,
}

struct IndexedEventBatch {
    first: EventId,
    last: EventId,
    max_last: EventId,
    key: String,
}

impl EventBatchIndex {
    fn from_keys(keys: &[String]) -> Self {
        let mut batches = keys
            .iter()
            .filter_map(|key| {
                let (first, last) = event_batch_range_from_key(key)?;
                Some(IndexedEventBatch {
                    first,
                    last,
                    max_last: last,
                    key: key.clone(),
                })
            })
            .collect::<Vec<_>>();
        batches.sort_by_key(|batch| batch.first);
        let mut max_last = None;
        for batch in &mut batches {
            batch.max_last =
                max_last.map_or(batch.last, |current: EventId| current.max(batch.last));
            max_last = Some(batch.max_last);
        }
        Self { batches }
    }

    fn insert(&mut self, first: EventId, last: EventId, key: String) {
        let offset = self.batches.partition_point(|batch| batch.first <= first);
        self.batches.insert(
            offset,
            IndexedEventBatch {
                first,
                last,
                max_last: last,
                key,
            },
        );
        let mut max_last = if offset == 0 {
            None
        } else {
            Some(self.batches[offset - 1].max_last)
        };
        for batch in &mut self.batches[offset..] {
            batch.max_last =
                max_last.map_or(batch.last, |current: EventId| current.max(batch.last));
            max_last = Some(batch.max_last);
        }
    }

    fn keys_containing(&self, id: EventId) -> Vec<String> {
        let end = self.batches.partition_point(|batch| batch.first <= id);
        let mut keys = Vec::new();
        for batch in self.batches[..end].iter().rev() {
            if batch.max_last < id {
                break;
            }
            if id <= batch.last {
                keys.push(batch.key.clone());
            }
        }
        keys
    }
}

fn event_batch_path(conversation_dir: &Path, first: EventId, last: EventId) -> PathBuf {
    conversation_dir
        .join("events")
        .join(format!("{first}_{last}.batch.json"))
}

fn remember_appended_batch(
    index: &Mutex<Option<EventBatchIndex>>,
    conversation_dir: &Path,
    added: &AddEventsResult,
) {
    if added.event_ids.len() < 2 {
        return;
    }
    let first = added.event_ids[0];
    let last = added.latest_event_id;
    index
        .lock()
        .expect("event batch index poisoned")
        .get_or_insert_with(EventBatchIndex::default)
        .insert(
            first,
            last,
            event_batch_path(conversation_dir, first, last)
                .to_string_lossy()
                .into_owned(),
        );
}

fn unfinished_turn_marker_path(
    agent_id: AgentId,
    thread_id: ConversationId,
    turn_id: TurnId,
) -> PathBuf {
    Path::new(UNFINISHED_TURNS_DIR)
        .join(agent_id.to_string())
        .join(thread_id.to_string())
        .join(format!("{turn_id}.json"))
}

fn unfinished_thread_markers_dir(agent_id: AgentId, thread_id: ConversationId) -> PathBuf {
    Path::new(UNFINISHED_TURNS_DIR)
        .join(agent_id.to_string())
        .join(thread_id.to_string())
}

#[derive(Clone)]
pub struct BasicExoHarness {
    inner: Arc<BasicExoHarnessInner>,
    caller: Option<crate::access::Caller>,
}

struct BasicExoHarnessInner {
    access_policy: std::sync::OnceLock<Arc<dyn crate::access::AccessPolicy>>,
    storage: BasicObjectStore,
    write_lock: AsyncMutex<()>,
    resource_locks: Mutex<HashMap<ResourceScope, Weak<AsyncRwLock<()>>>>,
    subscribers: Mutex<HashMap<ConversationId, Vec<mpsc::UnboundedSender<Result<Event>>>>>,
    #[cfg(feature = "basic-backend")]
    native: NativeState,
    sandbox_registry: HashMap<SandboxProvider, SandboxBackendRegistration>,
    sandbox_policy: Option<crate::EgressPolicy>,
    sandbox_backends: AsyncMutex<HashMap<SandboxProvider, Arc<dyn crate::ManagedSandboxBackend>>>,
    running_sandboxes: AsyncMutex<HashMap<SandboxId, Arc<dyn crate::ManagedSandboxHandle>>>,
    running_processes: AsyncMutex<HashMap<crate::SandboxProcessId, Arc<RunningSandboxProcess>>>,
    host: Arc<dyn crate::runtime_host::RuntimeHost>,
    vaults: BasicVaultStore,
}

impl BasicExoHarnessInner {
    fn resource_lock(&self, scope: ResourceScope) -> Arc<AsyncRwLock<()>> {
        let mut locks = self
            .resource_locks
            .lock()
            .expect("resource lock map poisoned");
        locks.retain(|_, lock| lock.strong_count() > 0);
        match locks.get(&scope).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(AsyncRwLock::new(()));
                locks.insert(scope, Arc::downgrade(&lock));
                lock
            }
        }
    }

    // Lock order is agent resources, thread resources, then write_lock. Waiting
    // for resource I/O must never hold the global metadata lock.
    async fn lock_thread_resources(
        &self,
        agent_id: AgentId,
        thread_id: ConversationId,
    ) -> (OwnedRwLockReadGuard<()>, OwnedRwLockWriteGuard<()>) {
        let agent = self
            .resource_lock(ResourceScope::Agent { agent_id })
            .read_owned()
            .await;
        let thread = self
            .resource_lock(ResourceScope::Thread {
                agent_id,
                thread_id,
            })
            .write_owned()
            .await;
        (agent, thread)
    }
}

impl BasicExoHarness {
    #[cfg(not(feature = "basic-backend"))]
    pub fn hosted(
        storage: Arc<dyn crate::Storage>,
        master_key: [u8; 32],
        backends: Vec<SandboxBackendRegistration>,
        host: Arc<dyn crate::runtime_host::RuntimeHost>,
    ) -> Result<Self> {
        let mut registry = HashMap::new();
        for backend in backends {
            let provider = backend.provider();
            anyhow::ensure!(
                registry.insert(provider.clone(), backend).is_none(),
                "duplicate sandbox provider {provider}"
            );
        }
        let storage = BasicObjectStore::new(storage);
        let cipher = crate::secrets::SecretCipher::new(Arc::new(
            crate::secrets::StaticSecretKeyProvider::new(master_key),
        ));
        Ok(Self {
            caller: None,
            inner: Arc::new(BasicExoHarnessInner {
                access_policy: Default::default(),
                vaults: BasicVaultStore::hosted(storage.clone(), cipher.clone()),
                storage,
                write_lock: AsyncMutex::new(()),
                resource_locks: Mutex::default(),
                subscribers: Mutex::default(),
                sandbox_registry: registry,
                sandbox_policy: None,
                sandbox_backends: AsyncMutex::default(),
                running_sandboxes: AsyncMutex::default(),
                running_processes: AsyncMutex::default(),
                host,
            }),
        })
    }

    fn command_env(
        &self,
        owner: ResourceScope,
        mounts: &[FileSystemMount],
        env: HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        #[cfg(feature = "basic-backend")]
        {
            self.inner.native.resources.command_env(owner, mounts, env)
        }
        #[cfg(not(feature = "basic-backend"))]
        {
            anyhow::ensure!(
                mounts.is_empty(),
                "filesystem mounts are not supported for {owner:?} on this host"
            );
            Ok(env)
        }
    }

    fn agents_dir(&self) -> PathBuf {
        PathBuf::from("agents")
    }

    fn owner_dir(&self, scope: ResourceScope) -> PathBuf {
        match scope {
            ResourceScope::Global => PathBuf::new(),
            ResourceScope::Agent { agent_id } => self.agents_dir().join(agent_id.to_string()),
            ResourceScope::Thread {
                agent_id,
                thread_id,
            } => self
                .agents_dir()
                .join(agent_id.to_string())
                .join("conversations")
                .join(thread_id.to_string()),
        }
    }

    /// Slug-uniqueness index: one marker file per slug, so the per-create
    /// uniqueness check is O(1) instead of a full record scan.
    fn slug_index_dir(&self) -> PathBuf {
        self.agents_dir().join("by-slug")
    }

    fn slug_marker_path(&self, slug: &str) -> PathBuf {
        // Encode defensively so a hostile slug cannot escape the directory.
        let encoded: String = slug
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c.to_string()
                } else {
                    format!("%{:02x}", c as u32)
                }
            })
            .collect();
        // ".slug", never ".json": a slug named "record" must not match the
        // list_agent_records key filter.
        self.slug_index_dir().join(format!("{encoded}.slug"))
    }

    fn bindings_dir(&self) -> PathBuf {
        PathBuf::from("bindings")
    }

    async fn list_agent_records(&self) -> Result<Vec<AgentRecord>> {
        let storage = &self.inner.storage;
        let directories = storage
            .list_directories(self.agents_dir())
            .await?
            .into_iter()
            .filter(|directory| {
                directory
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.parse::<AgentId>().is_ok())
            });
        let mut agents = stream::iter(directories)
            .map(|directory| async move {
                storage
                    .get_json_if_exists::<AgentRecord>(directory.join("record.json"))
                    .await
            })
            .buffer_unordered(16)
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        agents.sort_by_key(|record| record.id);
        Ok(agents)
    }
}

#[async_trait]
impl ExoHarness for BasicExoHarness {
    fn with_caller(&self, caller: crate::access::Caller) -> Result<Arc<dyn ExoHarness>> {
        self.inner
            .access_policy
            .get_or_init(|| caller.policy.clone());
        Ok(Arc::new(Self {
            inner: self.inner.clone(),
            caller: Some(caller),
        }))
    }

    async fn list_environments(&self) -> Result<Vec<crate::EnvironmentDefinition>> {
        self.check(ResourceScope::Global).await?;
        let mut definitions: Vec<crate::EnvironmentDefinition> = self
            .inner
            .storage
            .list_json_matching_suffix(Path::new("environments"), ".json")
            .await?;
        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(definitions)
    }

    async fn put_environment(&self, environment: crate::EnvironmentDefinition) -> Result<()> {
        if let Some(caller) = &self.caller {
            caller.policy.check_operator(&caller.principal).await?;
        }
        environment.validate()?;
        let _guard = self.inner.write_lock.lock().await;
        self.inner
            .storage
            .put_json(
                Path::new("environments").join(format!("{}.json", environment.name)),
                &environment,
            )
            .await
    }

    async fn delete_environment(&self, name: &str) -> Result<bool> {
        if let Some(caller) = &self.caller {
            caller.policy.check_operator(&caller.principal).await?;
        }
        crate::EnvironmentDefinition::validate_name(name)?;
        let _guard = self.inner.write_lock.lock().await;
        let path = Path::new("environments").join(format!("{name}.json"));
        if self
            .inner
            .storage
            .get_json_if_exists::<crate::EnvironmentDefinition>(&path)
            .await?
            .is_none()
        {
            return Ok(false);
        }
        self.inner.storage.delete_key_if_exists(path).await?;
        Ok(true)
    }

    async fn list_agents(&self) -> Result<Vec<Arc<dyn AgentHandle>>> {
        self.check(ResourceScope::Global).await?;
        Ok(stream::iter(self.list_agent_records().await?)
            .map(|record| async move {
                self.check(ResourceScope::Agent {
                    agent_id: record.id,
                })
                .await
                .ok()
                .map(|_| {
                    Arc::new(BasicAgentHandle {
                        harness: self.clone(),
                        record,
                    }) as Arc<dyn AgentHandle>
                })
            })
            .buffered(16)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect())
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<Arc<dyn AgentHandle>>> {
        self.check(ResourceScope::Global).await?;
        if self
            .check(ResourceScope::Agent { agent_id: *id })
            .await
            .is_err()
        {
            return Ok(None);
        }
        let record_path = self.agents_dir().join(id.to_string()).join("record.json");
        let Some(record) = self
            .inner
            .storage
            .get_json_if_exists::<AgentRecord>(&record_path)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(BasicAgentHandle {
            harness: self.clone(),
            record,
        })))
    }

    async fn new_agent(&self, request: NewAgentRequest) -> Result<Arc<dyn AgentHandle>> {
        self.check(ResourceScope::Global).await?;
        let _guard = self.inner.write_lock.lock().await;
        // TODO: claim the marker with a conditional put (put_json_if_absent,
        // arriving in PR #113) to close the cross-process create race.
        let marker = self.slug_marker_path(&request.slug);
        if self
            .inner
            .storage
            .get_json_if_exists::<Uuid7>(&marker)
            .await?
            .is_some()
        {
            bail!("agent slug already exists: {}", request.slug);
        }

        require_vaults(self, &request.vaults).await?;
        let record = AgentRecord {
            vaults: request.vaults,
            id: Uuid7::now(),
            slug: request.slug,
            name: request.name,
        };
        self.claim(ResourceScope::Agent {
            agent_id: record.id,
        })
        .await?;
        let agent_dir = self.agents_dir().join(record.id.to_string());
        self.inner
            .storage
            .put_json(agent_dir.join("record.json"), &record)
            .await?;
        self.inner.storage.put_json(marker, &record.id).await?;
        Ok(Arc::new(BasicAgentHandle {
            harness: self.clone(),
            record,
        }))
    }

    async fn delete_agent(&self, id: &AgentId) -> Result<bool> {
        self.check(ResourceScope::Global).await?;
        self.check(ResourceScope::Agent { agent_id: *id }).await?;
        let _resources = self
            .inner
            .resource_lock(ResourceScope::Agent { agent_id: *id })
            .write_owned()
            .await;
        let agent_dir = self.agents_dir().join(id.to_string());
        if self.inner.storage.list_keys(&agent_dir).await?.is_empty() {
            return Ok(false);
        }
        // Deleting the agent's prefix erases every sandbox record it and its
        // conversations own; without terminating those sandboxes first their
        // VMs (and registered in-process handles) would keep running with no
        // record left that could ever find them. Same protocol as
        // delete_conversation: terminate outside the write lock, since
        // terminate_sandbox takes that lock itself, then delete only after a
        // locked re-check sees nothing running — bounded, so racing sandbox
        // creation yields an error instead of a leaked VM.
        for _ in 0..5 {
            terminate_running_sandboxes(&BasicScopedSandboxHandle::agent(self, *id)).await?;
            for conversation_id in agent_conversation_ids(self, &agent_dir).await? {
                terminate_running_sandboxes(&BasicScopedSandboxHandle::conversation(
                    self,
                    *id,
                    conversation_id,
                ))
                .await?;
            }

            let _guard = self.inner.write_lock.lock().await;
            if self.inner.storage.list_keys(&agent_dir).await?.is_empty() {
                return Ok(false);
            }
            // Re-enumerate under the lock: a conversation created after the
            // sweep above would otherwise slip past the check.
            let mut scopes = vec![BasicScopedSandboxHandle::agent(self, *id)];
            for conversation_id in agent_conversation_ids(self, &agent_dir).await? {
                scopes.push(BasicScopedSandboxHandle::conversation(
                    self,
                    *id,
                    conversation_id,
                ));
            }
            if !prepare_sandbox_scopes_for_deletion(self, &scopes).await? {
                continue;
            }
            #[cfg(feature = "basic-backend")]
            for conversation_id in agent_conversation_ids(self, &agent_dir).await? {
                remove_thread_resources(self, *id, conversation_id).await?;
            }
            // Release the slug before the record (its source) disappears.
            if let Some(record) = self
                .inner
                .storage
                .get_json_if_exists::<AgentRecord>(&agent_dir.join("record.json"))
                .await?
            {
                self.inner
                    .storage
                    .delete_key_if_exists(self.slug_marker_path(&record.slug))
                    .await?;
            } else {
                tracing::warn!(
                    %id,
                    "agent dir exists without record.json; cannot release slug marker, continuing with delete"
                );
            }
            self.inner.storage.delete_prefix(agent_dir).await?;
            self.inner
                .storage
                .delete_prefix(Path::new(UNFINISHED_TURNS_DIR).join(id.to_string()))
                .await?;
            return Ok(true);
        }
        bail!("agent {id} kept acquiring sandboxes while it was being deleted")
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        self.check(ResourceScope::Global).await?;
        self.binding_records(&self.bindings_dir()).await
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        self.check_binding(&binding).await?;
        self.check(ResourceScope::Global).await?;
        let _guard = self.inner.write_lock.lock().await;
        let id = Uuid7::now();
        let record = stored_binding(id, binding);
        self.inner
            .storage
            .put_json(
                self.caller_bindings_dir(&self.bindings_dir())
                    .join(format!("{id}.json")),
                &record,
            )
            .await?;
        Ok(id)
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        self.check(ResourceScope::Global).await?;
        self.find_binding(&[self.bindings_dir()], id).await
    }

    async fn create_vault(&self, name: &str) -> Result<Arc<dyn VaultHandle>> {
        self.check(ResourceScope::Global).await?;
        let stored_name = if let Some(caller) = &self.caller {
            anyhow::ensure!(
                !self
                    .list_vaults()
                    .await?
                    .iter()
                    .any(|v| v.record().name == name),
                "vault already exists: {name}"
            );
            format!("{}:{name}", caller.principal)
        } else {
            name.to_owned()
        };
        let vault = self.inner.vaults.create_vault(&stored_name).await?;
        if let Some(caller) = &self.caller
            && let Err(error) = caller
                .policy
                .vault_created(&caller.principal, vault.record().id, name)
                .await
        {
            self.inner.vaults.delete_vault(&vault.record().id).await?;
            return Err(error);
        }
        self.scoped_vault(vault)
            .await?
            .context("new vault is unavailable")
    }
    async fn delete_vault(&self, id: &VaultId) -> Result<()> {
        let vault = crate::vault::require_vault(self, id).await?;
        if let Some(caller) = &self.caller {
            anyhow::ensure!(
                caller.policy.default_vault(&caller.principal).await? != *id,
                "the personal vault cannot be deleted"
            );
            anyhow::ensure!(
                caller
                    .policy
                    .vault(&caller.principal, vault.record(), true)
                    .await?
                    .is_some(),
                "vault is read-only"
            );
        }
        self.check(ResourceScope::Global).await?;
        let _guard = self.inner.write_lock.lock().await;
        let operator = Self {
            inner: self.inner.clone(),
            caller: None,
        };
        for agent in operator.list_agents().await? {
            anyhow::ensure!(
                !agent.record().vaults.contains(id),
                "vault is still attached to an agent"
            );
            for thread in agent
                .list_conversations(ListConversationsRequest::default())
                .await?
                .conversations
            {
                anyhow::ensure!(
                    !thread.record().vaults.contains(id),
                    "vault is still attached to a thread"
                );
            }
        }
        self.inner.vaults.delete_vault(id).await
    }
}

struct BasicAgentHandle {
    harness: BasicExoHarness,
    record: AgentRecord,
}

trait BasicSandboxScope {
    fn sandbox_handle(&self) -> BasicScopedSandboxHandle<'_>;
}

trait BasicFullSandboxScope: BasicSandboxScope {}

#[async_trait]
impl<T> SnapshotHandle for T
where
    T: BasicSandboxScope + Send + Sync,
{
    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        self.sandbox_handle().snapshot_sandbox(id).await
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        self.sandbox_handle().start_sandbox(request).await
    }
}

#[async_trait]
impl<T> SandboxHandle for T
where
    T: BasicFullSandboxScope + Send + Sync,
{
    async fn sandbox_activity(&self, id: SandboxId) -> Result<crate::SandboxActivity> {
        self.sandbox_handle().sandbox_activity(id).await
    }
    async fn list_sandboxes(&self) -> Result<Vec<SandboxRecord>> {
        self.sandbox_handle().list_sandboxes().await
    }

    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        self.sandbox_handle().create_sandbox(request).await
    }

    async fn fork_sandbox(&self, request: ForkSandboxRequest) -> Result<SandboxId> {
        self.sandbox_handle().fork_sandbox(request).await
    }

    async fn restore_sandbox(&self, request: RestoreSandboxRequest) -> Result<SandboxId> {
        self.sandbox_handle().restore_sandbox(request).await
    }

    async fn terminate_sandbox(&self, id: SandboxId) -> Result<()> {
        self.sandbox_handle().terminate_sandbox(id).await
    }

    async fn attach_sandbox(&self, request: AttachSandboxRequest) -> Result<SandboxId> {
        self.sandbox_handle().attach_sandbox(request).await
    }

    async fn detach_sandbox(&self, id: SandboxId) -> Result<SandboxAttachment> {
        self.sandbox_handle().detach_sandbox(id).await
    }

    async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        self.sandbox_handle().stop_sandbox(id).await
    }

    #[cfg(feature = "basic-backend")]
    async fn connect_sandbox_tcp(
        &self,
        id: SandboxId,
        port: u16,
    ) -> Result<Option<crate::BoxSandboxTcpStream>> {
        self.sandbox_handle().connect_sandbox_tcp(id, port).await
    }

    #[cfg(feature = "basic-backend")]
    async fn sandbox_supports_tcp(&self, id: SandboxId) -> Result<bool> {
        self.sandbox_handle().sandbox_supports_tcp(id).await
    }

    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        self.sandbox_handle().start_sandbox_process(request).await
    }

    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        self.sandbox_handle()
            .write_sandbox_process_input(request)
            .await
    }

    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        self.sandbox_handle()
            .close_sandbox_process_input(request)
            .await
    }

    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult> {
        self.sandbox_handle()
            .get_sandbox_process_events(query)
            .await
    }

    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        self.sandbox_handle().wait_sandbox_process(request).await
    }

    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        self.sandbox_handle().cancel_sandbox_process(request).await
    }

    async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        self.sandbox_handle().run_in_sandbox(request).await
    }
}

#[async_trait]
impl AgentHandle for BasicAgentHandle {
    async fn prepare_resources(
        &self,
        resources: Vec<crate::resources::ResourceDefinition>,
    ) -> Result<Vec<crate::resources::PreparedResource>> {
        #[cfg(feature = "basic-backend")]
        {
            self.prepare_resources_impl(resources).await
        }
        #[cfg(not(feature = "basic-backend"))]
        {
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

    fn record(&self) -> &AgentRecord {
        &self.record
    }

    async fn list_conversations(
        &self,
        request: ListConversationsRequest,
    ) -> Result<ListConversationsResult<Arc<dyn ConversationHandle>>> {
        self.harness
            .check(ResourceScope::Agent {
                agent_id: self.record.id,
            })
            .await?;
        let result = self.list_conversation_records(request).await?;
        let handles = stream::iter(result.conversations)
            .map(|record| async move {
                self.harness
                    .check(ResourceScope::Thread {
                        agent_id: self.record.id,
                        thread_id: record.id,
                    })
                    .await
                    .ok()
                    .map(|_| {
                        Arc::new(BasicConversationHandle {
                            harness: self.harness.clone(),
                            agent_id: self.record.id,
                            record,
                            event_batch_index: Arc::new(Mutex::new(None)),
                        }) as Arc<dyn ConversationHandle>
                    })
            })
            .buffered(16)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect();
        Ok(ListConversationsResult {
            conversations: handles,
            next_cursor: result.next_cursor,
        })
    }

    async fn get_conversation(
        &self,
        id: &ConversationId,
    ) -> Result<Option<Arc<dyn ConversationHandle>>> {
        self.harness
            .check(ResourceScope::Agent {
                agent_id: self.record.id,
            })
            .await?;
        if self
            .harness
            .check(ResourceScope::Thread {
                agent_id: self.record.id,
                thread_id: *id,
            })
            .await
            .is_err()
        {
            return Ok(None);
        }
        let record_path = self
            .conversations_dir()
            .join(id.to_string())
            .join("record.json");
        let Some(mut record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<ConversationRecord>(&record_path)
            .await?
        else {
            return Ok(None);
        };
        let conversation_dir = record_path.parent().expect("record has a parent");
        let event_keys = self
            .harness
            .inner
            .storage
            .list_keys(conversation_dir.join("events"))
            .await?;
        record.latest_event_id = latest_event_id_from_keys(&event_keys);
        Ok(Some(Arc::new(BasicConversationHandle {
            harness: self.harness.clone(),
            agent_id: self.record.id,
            record,
            event_batch_index: Arc::new(Mutex::new(Some(EventBatchIndex::from_keys(&event_keys)))),
        })))
    }

    async fn new_conversation(
        &self,
        request: NewConversationRequest,
    ) -> Result<Arc<dyn ConversationHandle>> {
        self.harness
            .check(ResourceScope::Agent {
                agent_id: self.record.id,
            })
            .await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        let existing = self
            .list_conversation_records(ListConversationsRequest::default())
            .await?
            .conversations;
        let slug = match request.slug {
            Some(slug) => {
                if existing
                    .iter()
                    .any(|conversation| conversation.slug == slug)
                {
                    bail!("conversation slug already exists for agent: {slug}");
                }
                slug
            }
            None => derive_unique_slug("conversation", &existing),
        };
        if let Some(environment) = &request.environment {
            environment.validate()?;
            self.harness.check_environment(environment).await?;
        }
        require_vaults(&self.harness, &request.vaults).await?;
        let record = ConversationRecord {
            environment: request.environment,
            vaults: request.vaults,
            id: Uuid7::now(),
            slug: slug.clone(),
            name: request.name.unwrap_or_else(|| slug_to_name(&slug)),
            latest_event_id: None,
        };
        self.harness
            .claim(ResourceScope::Thread {
                agent_id: self.record.id,
                thread_id: record.id,
            })
            .await?;
        let conversation_dir = self.conversations_dir().join(record.id.to_string());
        let mut record = record;
        append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            record.id,
            None,
            None,
            None,
            vec![EventData::ThreadCreated {
                slug: record.slug.clone(),
                name: record.name.clone(),
            }],
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &record)
            .await?;
        Ok(Arc::new(BasicConversationHandle {
            harness: self.harness.clone(),
            agent_id: self.record.id,
            record,
            event_batch_index: Arc::new(Mutex::new(Some(EventBatchIndex::default()))),
        }))
    }

    async fn delete_conversation(&self, id: &ConversationId) -> Result<bool> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.record.id,
                thread_id: *id,
            })
            .await?;
        self.harness
            .check(ResourceScope::Agent {
                agent_id: self.record.id,
            })
            .await?;
        let _resources = self
            .harness
            .inner
            .lock_thread_resources(self.record.id, *id)
            .await;
        let conversation_dir = self.conversations_dir().join(id.to_string());
        if self
            .harness
            .inner
            .storage
            .list_keys(&conversation_dir)
            .await?
            .is_empty()
        {
            return Ok(false);
        }

        let sandbox_handle =
            BasicScopedSandboxHandle::conversation(&self.harness, self.record.id, *id);
        // Sandbox creation persists its record under the write lock, so the
        // only way to guarantee no VM outlives its conversation record is to
        // observe "no running sandboxes" while holding that lock and delete
        // without releasing it. terminate_sandbox takes the write lock itself,
        // so terminations run outside it and the locked check loops until it
        // finds nothing new — bounded, so a caller racing sandbox creation
        // against deletion gets an error instead of a silently leaked VM.
        for _ in 0..5 {
            terminate_running_sandboxes(&sandbox_handle).await?;

            let _guard = self.harness.inner.write_lock.lock().await;
            if self
                .harness
                .inner
                .storage
                .list_keys(&conversation_dir)
                .await?
                .is_empty()
            {
                return Ok(false);
            }
            if !prepare_sandbox_scopes_for_deletion(
                &self.harness,
                std::slice::from_ref(&sandbox_handle),
            )
            .await?
            {
                continue;
            }
            if let Ok(mut record) =
                load_conversation_record(&self.harness.inner.storage, &conversation_dir).await
            {
                append_events_to_conversation(
                    &self.harness.inner,
                    &conversation_dir,
                    record.id,
                    None,
                    None,
                    record.latest_event_id,
                    vec![EventData::ThreadDeleted],
                    &mut record,
                )
                .await?;
            }
            #[cfg(feature = "basic-backend")]
            remove_thread_resources(&self.harness, self.record.id, *id).await?;
            self.harness
                .inner
                .storage
                .delete_prefix(conversation_dir)
                .await?;
            self.harness
                .inner
                .storage
                .delete_prefix(unfinished_thread_markers_dir(self.record.id, *id))
                .await?;
            return Ok(true);
        }
        bail!("conversation {id} kept acquiring sandboxes while it was being deleted")
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        self.harness
            .check(ResourceScope::Agent {
                agent_id: self.record.id,
            })
            .await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        write_artifact_version(&self.harness.inner, &self.artifacts_dir(), request).await
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        self.harness
            .check(ResourceScope::Agent {
                agent_id: self.record.id,
            })
            .await?;
        let versions =
            load_artifact_versions(&self.harness.inner.storage, &self.artifacts_dir()).await?;
        let selected = versions
            .into_iter()
            .filter(|artifact| artifact.artifact_id == request.artifact_id)
            .filter(|artifact| {
                request
                    .version
                    .is_none_or(|version| artifact.version == version)
            })
            .max_by_key(|artifact| artifact.version);
        let Some(selected) = selected else {
            return Ok(None);
        };
        let artifact_dir = self.artifacts_dir().join(selected.artifact_id.to_string());
        let contents =
            load_artifact_contents(&self.harness.inner.storage, &artifact_dir, selected.version)
                .await?;
        Ok(Some(Artifact {
            version: selected,
            contents,
        }))
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        self.harness
            .check(ResourceScope::Agent {
                agent_id: self.record.id,
            })
            .await?;
        load_artifact_versions(&self.harness.inner.storage, &self.artifacts_dir()).await
    }
}

impl BasicSandboxScope for BasicAgentHandle {
    fn sandbox_handle(&self) -> BasicScopedSandboxHandle<'_> {
        BasicScopedSandboxHandle::agent(&self.harness, self.record.id)
    }
}

impl BasicFullSandboxScope for BasicAgentHandle {}

impl BasicAgentHandle {
    fn agent_dir(&self) -> PathBuf {
        self.harness.agents_dir().join(self.record.id.to_string())
    }

    fn conversations_dir(&self) -> PathBuf {
        self.agent_dir().join("conversations")
    }

    fn artifacts_dir(&self) -> PathBuf {
        self.agent_dir().join("artifacts")
    }

    async fn list_conversation_records(
        &self,
        request: ListConversationsRequest,
    ) -> Result<ListConversationsResult<ConversationRecord>> {
        let storage = &self.harness.inner.storage;
        let paths = if request.unfinished_only {
            let prefix = Path::new(UNFINISHED_TURNS_DIR).join(self.record.id.to_string());
            let mut thread_ids = HashSet::new();
            for key in storage.list_keys(&prefix).await? {
                let Ok(relative) = Path::new(&key).strip_prefix(&prefix) else {
                    continue;
                };
                if relative.components().count() != 2 || !key.ends_with(".json") {
                    continue;
                }
                let Some(thread_id) = relative.components().next() else {
                    continue;
                };
                thread_ids.insert(thread_id.as_os_str().to_string_lossy().into_owned());
            }
            thread_ids
                .into_iter()
                .map(|id| self.conversations_dir().join(id).join("record.json"))
                .collect::<Vec<_>>()
        } else {
            storage
                .list_directories(self.conversations_dir())
                .await?
                .into_iter()
                .map(|directory| directory.join("record.json"))
                .collect::<Vec<_>>()
        };
        let mut conversations = stream::iter(paths)
            .map(|path| async move {
                let Some(mut record) = storage
                    .get_json_if_exists::<ConversationRecord>(&path)
                    .await?
                else {
                    return Ok::<_, anyhow::Error>(None);
                };
                record.latest_event_id =
                    latest_committed_event_id(storage, path.parent().expect("record has a parent"))
                        .await?;
                Ok(Some(record))
            })
            .buffer_unordered(16)
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        conversations.sort_by_key(conversation_recency_key);
        conversations.reverse();
        paginate_conversation_records(conversations, request)
    }
}

fn conversation_recency_key(record: &ConversationRecord) -> Uuid7 {
    record.latest_event_id.unwrap_or(record.id)
}

fn paginate_conversation_records(
    conversations: Vec<ConversationRecord>,
    request: ListConversationsRequest,
) -> Result<ListConversationsResult<ConversationRecord>> {
    let start = match request.cursor {
        Some(cursor) => conversations
            .iter()
            .position(|conversation| conversation_recency_key(conversation) == cursor)
            .map(|index| index + 1)
            .ok_or_else(|| anyhow!("conversation cursor not found: {cursor}"))?,
        None => 0,
    };
    let remaining = conversations.len().saturating_sub(start);
    let Some(limit) = request.limit.filter(|limit| *limit > 0) else {
        return Ok(ListConversationsResult {
            conversations: conversations.into_iter().skip(start).collect(),
            next_cursor: None,
        });
    };
    let has_more = remaining > limit;
    let page: Vec<_> = conversations.into_iter().skip(start).take(limit).collect();
    let next_cursor = if has_more {
        page.last().map(conversation_recency_key)
    } else {
        None
    };
    Ok(ListConversationsResult {
        conversations: page,
        next_cursor,
    })
}

// Deletion helpers shared by delete_agent and delete_conversation: an owner's
// storage prefix must never be removed while sandboxes it owns are running,
// or their VMs would outlive every record that could find them.
// Called under the write lock immediately before deleting these scopes. The
// check happens for every scope before any handles are released, so a racing
// sandbox creation retries the whole deletion. Attached sandboxes keep
// running externally, but their in-process handles must not outlive the
// records that identify them.
async fn agent_conversation_ids(
    harness: &BasicExoHarness,
    agent_dir: &Path,
) -> Result<Vec<ConversationId>> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for key in harness
        .inner
        .storage
        .list_keys(agent_dir.join("conversations"))
        .await?
    {
        let Some((_, rest)) = key.split_once("/conversations/") else {
            continue;
        };
        let Some(component) = rest.split('/').next() else {
            continue;
        };
        if !seen.insert(component.to_string()) {
            continue;
        }
        // A component that is not a conversation id means the storage layout
        // is corrupt; refusing the delete beats leaking whatever lives there.
        ids.push(component.parse::<ConversationId>().map_err(|error| {
            anyhow!("parsing conversation id {component:?} under the agent directory: {error}")
        })?);
    }
    Ok(ids)
}

struct BasicConversationHandle {
    harness: BasicExoHarness,
    agent_id: AgentId,
    record: ConversationRecord,
    event_batch_index: Arc<Mutex<Option<EventBatchIndex>>>,
}

#[async_trait]
impl ConversationHandle for BasicConversationHandle {
    async fn activate_caller(&self) -> Result<bool> {
        let Some(caller) = &self.harness.caller else {
            return Ok(false);
        };
        caller
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let path = self.conversation_dir().join("caller.json");
        let active: Option<String> = self.harness.inner.storage.get_json_if_exists(&path).await?;
        if active.as_deref() == Some(&caller.principal) {
            return Ok(false);
        }
        let operator = BasicExoHarness {
            inner: self.harness.inner.clone(),
            caller: None,
        };
        let scope =
            BasicScopedSandboxHandle::conversation(&operator, self.agent_id, self.record.id);
        let reset = active.is_some() || !scope.list_sandboxes().await?.is_empty();
        terminate_running_sandboxes(&scope).await?;
        self.harness
            .inner
            .storage
            .put_json(path, &caller.principal)
            .await?;
        Ok(reset)
    }

    async fn update_environment(
        &self,
        environment: crate::EnvironmentDefinition,
    ) -> Result<Arc<dyn ConversationHandle>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        environment.validate()?;
        self.harness.check_environment(&environment).await?;
        let _resources = self
            .harness
            .inner
            .lock_thread_resources(self.agent_id, self.record.id)
            .await;
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut record = self.load_record().await?;
        if record.environment.as_ref() != Some(&environment) {
            #[cfg(feature = "basic-backend")]
            self.check_resource_environment(&environment.config.provider)?;
            let scope = self.sandbox_handle();
            for sandbox in scope.list_sandboxes().await? {
                scope.terminate_sandbox_locked(sandbox.id).await?;
            }
            record = self.load_record().await?;
            record.environment = Some(environment);
            self.harness
                .inner
                .storage
                .put_json(self.conversation_dir().join("record.json"), &record)
                .await?;
        }
        Ok(Arc::new(Self {
            harness: self.harness.clone(),
            agent_id: self.agent_id,
            record,
            event_batch_index: Arc::clone(&self.event_batch_index),
        }))
    }

    async fn attach_vaults(&self, vaults: Vec<VaultId>) -> Result<Arc<dyn ConversationHandle>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut record = self.load_record().await?;
        require_vaults(&self.harness, &vaults).await?;
        for vault in vaults {
            if !record.vaults.contains(&vault) {
                record.vaults.push(vault);
            }
        }
        self.harness
            .inner
            .storage
            .put_json(self.conversation_dir().join("record.json"), &record)
            .await?;
        Ok(Arc::new(Self {
            harness: self.harness.clone(),
            agent_id: self.agent_id,
            record,
            event_batch_index: Arc::clone(&self.event_batch_index),
        }))
    }

    async fn materialize_resources(
        &self,
        resources: Vec<crate::resources::PreparedResource>,
        provider: SandboxProvider,
    ) -> Result<Vec<FileSystemMount>> {
        #[cfg(feature = "basic-backend")]
        {
            self.materialize_resources_impl(resources, provider).await
        }
        #[cfg(not(feature = "basic-backend"))]
        {
            self.harness
                .check(ResourceScope::Thread {
                    agent_id: self.agent_id,
                    thread_id: self.record.id,
                })
                .await?;
            anyhow::ensure!(
                resources.is_empty(),
                "provider {provider} does not support filesystem resources on this host"
            );
            Ok(Vec::new())
        }
    }

    fn record(&self) -> &ConversationRecord {
        &self.record
    }
    async fn start_session(&self) -> Result<SessionId> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let session_id = Uuid7::now();
        self.append_events_internal(
            Some(session_id),
            None,
            None,
            vec![EventData::SessionStarted],
        )
        .await?;
        Ok(session_id)
    }

    async fn end_session(&self, id: SessionId) -> Result<()> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        self.append_events_internal(Some(id), None, None, vec![EventData::SessionEnded])
            .await?;
        Ok(())
    }

    async fn begin_turn(&self, request: BeginTurnRequest) -> Result<Arc<dyn TurnHandle>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut record = self.load_record().await?;
        let conversation_dir = self.conversation_dir();

        let session_id = request.session_id.unwrap_or_else(Uuid7::now);
        let turn_record = TurnRecord {
            id: Uuid7::now(),
            session_id,
        };
        // Write the marker first: a crash may leave an extra candidate to scan,
        // but cannot leave an admitted turn absent from the recovery index.
        self.harness
            .inner
            .storage
            .put_bytes(
                unfinished_turn_marker_path(self.agent_id, self.record.id, turn_record.id),
                Vec::new(),
            )
            .await?;
        let mut events_to_append = Vec::new();

        if request.session_id.is_none() {
            events_to_append.push(EventData::SessionStarted);
        }
        events_to_append.push(EventData::TurnStarted {
            user_id: self.harness.caller.as_ref().map(|c| c.principal.clone()),
        });
        events_to_append.extend(request.initial_events);
        if !request.input.is_empty() {
            events_to_append.push(EventData::Messages {
                messages: request.input,
                response_id: None,
                usage: None,
            });
        }

        let add_result = append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            self.record.id,
            Some(session_id),
            Some(turn_record.id),
            record.latest_event_id,
            events_to_append,
            &mut record,
        )
        .await?;
        remember_appended_batch(&self.event_batch_index, &conversation_dir, &add_result);

        Ok(Arc::new(BasicTurnHandle {
            harness: self.harness.clone(),
            agent_id: self.agent_id,
            conversation_dir,
            conversation_id: self.record.id,
            record: turn_record,
            event_batch_index: Arc::clone(&self.event_batch_index),
            state: Mutex::new(BasicTurnState {
                latest_event_id: Some(add_result.latest_event_id),
                finished: false,
            }),
        }))
    }

    async fn turn_handle(&self, record: TurnRecord) -> Result<Arc<dyn TurnHandle>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let events = load_events(&self.harness.inner.storage, &self.conversation_dir()).await?;
        let mut latest_event_id = None;
        let mut finished = false;
        for event in events
            .into_iter()
            .filter(|event| event.session_id == Some(record.session_id))
            .filter(|event| event.turn_id == Some(record.id))
        {
            latest_event_id = Some(event.id);
            finished = matches!(event.data, EventData::TurnEnded);
        }
        if latest_event_id.is_none() {
            bail!(
                "turn {} in session {} was not found",
                record.id,
                record.session_id
            );
        }
        Ok(Arc::new(BasicTurnHandle {
            harness: self.harness.clone(),
            agent_id: self.agent_id,
            conversation_dir: self.conversation_dir(),
            conversation_id: self.record.id,
            record,
            event_batch_index: Arc::clone(&self.event_batch_index),
            state: Mutex::new(BasicTurnState {
                latest_event_id,
                finished,
            }),
        }))
    }

    async fn get_events(&self, query: Option<EventQuery>) -> Result<GetEventsResult> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let mut events = load_events(&self.harness.inner.storage, &self.conversation_dir()).await?;
        if let Some(query) = query {
            if let Some(session_id) = query.session_id {
                events.retain(|event| event.session_id == Some(session_id));
            }
            if let Some(turn_id) = query.turn_id {
                events.retain(|event| event.turn_id == Some(turn_id));
            }
            if let Some(types) = query.types {
                events.retain(|event| {
                    let event_kind = event.data.kind();
                    types.iter().any(|kind| kind.matches(&event_kind))
                });
            }
            match query.direction.unwrap_or(EventQueryDirection::Asc) {
                EventQueryDirection::Asc => {
                    if let Some(cursor) = query.cursor {
                        events.retain(|event| event.id > cursor);
                    }
                }
                EventQueryDirection::Desc => {
                    events.reverse();
                    if let Some(cursor) = query.cursor {
                        events.retain(|event| event.id < cursor);
                    }
                }
            }
            if let Some(limit) = query.limit {
                events.truncate(limit as usize);
            }
        }
        // The cursor is a resume position, even on the final page; Basic's history
        // cache needs it to avoid replaying events. Callers that scan until an empty
        // page pay another full log read here, which is acceptable for Basic.
        let cursor = events.last().map(|event| event.id);
        Ok(GetEventsResult { events, cursor })
    }

    async fn watch_events(&self, after_exclusive: Bound<EventId>) -> Result<EventStream> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        let existing = match after_exclusive {
            Bound::Unbounded => Vec::new(),
            _ => {
                let events =
                    load_events(&self.harness.inner.storage, &self.conversation_dir()).await?;
                events
                    .into_iter()
                    .filter(|event| matches_bound(event.id, &after_exclusive))
                    .collect::<Vec<_>>()
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        self.harness
            .inner
            .subscribers
            .lock()
            .expect("subscribers poisoned")
            .entry(self.record.id)
            .or_default()
            .push(tx);
        let existing_stream: BoxStream<'static, Result<Event>> =
            stream::iter(existing.into_iter().map(Ok)).boxed();
        let live_stream = UnboundedReceiverStream::new(rx);
        Ok(Box::pin(existing_stream.chain(live_stream)))
    }

    async fn get_event(&self, id: EventId) -> Result<Option<Event>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let cached_keys = self
            .event_batch_index
            .lock()
            .expect("event batch index poisoned")
            .as_ref()
            .map(|index| index.keys_containing(id));
        if let Some(keys) = &cached_keys
            && let Some(event) =
                read_event_from_batches(&self.harness.inner.storage, keys.clone(), id).await?
        {
            return Ok(Some(event));
        }

        let path = self.events_dir().join(format!("{id}.json"));
        if let Some(event) = self.harness.inner.storage.get_json_if_exists(&path).await? {
            return Ok(Some(event));
        }

        // A miss may be a batch appended by another handle or process.
        let event_keys = self
            .harness
            .inner
            .storage
            .list_keys(self.events_dir())
            .await?;
        let index = EventBatchIndex::from_keys(&event_keys);
        let checked = cached_keys
            .unwrap_or_default()
            .into_iter()
            .collect::<HashSet<_>>();
        let new_keys = index
            .keys_containing(id)
            .into_iter()
            .filter(|key| !checked.contains(key))
            .collect();
        *self
            .event_batch_index
            .lock()
            .expect("event batch index poisoned") = Some(index);
        read_event_from_batches(&self.harness.inner.storage, new_keys, id).await
    }

    async fn add_events(&self, request: AddEventsRequest) -> Result<AddEventsResult> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        self.append_events_internal(request.session_id, request.turn_id, None, request.data)
            .await
    }

    async fn fork(&self, request: ForkConversationRequest) -> Result<Arc<dyn ConversationHandle>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let _resources = self
            .harness
            .inner
            .lock_thread_resources(self.agent_id, self.record.id)
            .await;
        let _guard = self.harness.inner.write_lock.lock().await;
        #[cfg(feature = "basic-backend")]
        self.check_resource_fork()?;
        let agent = BasicAgentHandle {
            harness: self.harness.clone(),
            record: self
                .harness
                .inner
                .storage
                .get_json::<AgentRecord>(self.agent_dir().join("record.json"))
                .await?,
        };
        let existing = agent
            .list_conversation_records(ListConversationsRequest::default())
            .await?
            .conversations;
        let slug = match request.slug {
            Some(slug) => {
                if existing
                    .iter()
                    .any(|conversation| conversation.slug == slug)
                {
                    bail!("conversation slug already exists for agent: {slug}");
                }
                slug
            }
            None => derive_unique_slug("fork", &existing),
        };
        let mut events = load_events(&self.harness.inner.storage, &self.conversation_dir()).await?;
        if let Some(limit) = request.up_to_inclusive {
            events.retain(|event| event.id <= limit);
        }
        let record = ConversationRecord {
            environment: self.record.environment.clone(),
            vaults: self.record.vaults.clone(),
            id: Uuid7::now(),
            slug: slug.clone(),
            name: request.name.unwrap_or_else(|| slug_to_name(&slug)),
            latest_event_id: None,
        };
        self.harness
            .claim(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: record.id,
            })
            .await?;
        let conversation_dir = agent.conversations_dir().join(record.id.to_string());
        self.harness
            .inner
            .storage
            .copy_prefix(self.bindings_dir(), conversation_dir.join("bindings"))
            .await?;
        self.harness
            .inner
            .storage
            .copy_prefix(self.artifacts_dir(), conversation_dir.join("artifacts"))
            .await?;
        self.harness
            .inner
            .storage
            .copy_prefix(self.sandboxes_dir(), conversation_dir.join("sandboxes"))
            .await?;

        let mut latest_event_id = None;
        for mut event in events {
            let new_event_id = Uuid7::now();
            event.id = new_event_id;
            event.thread_id = record.id;
            event.created_at = new_event_id.timestamp().expect("uuid7 timestamp");
            latest_event_id = Some(new_event_id);
            self.harness
                .inner
                .storage
                .put_json(
                    conversation_dir
                        .join("events")
                        .join(format!("{}.json", event.id)),
                    &event,
                )
                .await?;
        }

        let mut fork_record = record.clone();
        fork_record.latest_event_id = latest_event_id;
        append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            record.id,
            None,
            None,
            fork_record.latest_event_id,
            vec![EventData::ThreadForked {
                source_thread_id: self.record.id,
                up_to_inclusive: request.up_to_inclusive,
            }],
            &mut fork_record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &fork_record)
            .await?;
        Ok(Arc::new(BasicConversationHandle {
            harness: self.harness.clone(),
            agent_id: self.agent_id,
            record: fork_record,
            event_batch_index: Arc::new(Mutex::new(Some(EventBatchIndex::default()))),
        }))
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let _guard = self.harness.inner.write_lock.lock().await;
        let artifact_version =
            write_artifact_version(&self.harness.inner, &self.artifacts_dir(), request).await?;
        let conversation_dir = self.conversation_dir();
        let mut record = self.load_record().await?;
        append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            self.record.id,
            None,
            None,
            record.latest_event_id,
            vec![EventData::ArtifactWritten {
                artifact_id: artifact_version.artifact_id,
                path: artifact_version.path.clone(),
                version: artifact_version.version,
            }],
            &mut record,
        )
        .await?;
        Ok(artifact_version)
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        let versions =
            load_artifact_versions(&self.harness.inner.storage, &self.artifacts_dir()).await?;
        let selected = versions
            .into_iter()
            .filter(|artifact| artifact.artifact_id == request.artifact_id)
            .filter(|artifact| {
                request
                    .version
                    .is_none_or(|version| artifact.version == version)
            })
            .max_by_key(|artifact| artifact.version);
        let Some(selected) = selected else {
            return Ok(None);
        };
        let artifact_dir = self.artifacts_dir().join(selected.artifact_id.to_string());
        let contents =
            load_artifact_contents(&self.harness.inner.storage, &artifact_dir, selected.version)
                .await?;
        Ok(Some(Artifact {
            version: selected,
            contents,
        }))
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        load_artifact_versions(&self.harness.inner.storage, &self.artifacts_dir()).await
    }
}

impl BasicSandboxScope for BasicConversationHandle {
    fn sandbox_handle(&self) -> BasicScopedSandboxHandle<'_> {
        BasicScopedSandboxHandle::conversation(&self.harness, self.agent_id, self.record.id)
    }
}

impl BasicFullSandboxScope for BasicConversationHandle {}

impl BasicConversationHandle {
    fn agent_dir(&self) -> PathBuf {
        self.harness.agents_dir().join(self.agent_id.to_string())
    }

    fn conversation_dir(&self) -> PathBuf {
        self.harness.owner_dir(ResourceScope::Thread {
            agent_id: self.agent_id,
            thread_id: self.record.id,
        })
    }

    fn events_dir(&self) -> PathBuf {
        self.conversation_dir().join("events")
    }

    fn bindings_dir(&self) -> PathBuf {
        self.conversation_dir().join("bindings")
    }

    fn artifacts_dir(&self) -> PathBuf {
        self.conversation_dir().join("artifacts")
    }

    fn sandboxes_dir(&self) -> PathBuf {
        self.conversation_dir().join("sandboxes")
    }

    async fn load_record(&self) -> Result<ConversationRecord> {
        load_conversation_record(&self.harness.inner.storage, &self.conversation_dir()).await
    }

    async fn append_events_internal(
        &self,
        session_id: Option<SessionId>,
        turn_id: Option<TurnId>,
        expected_head: Option<EventId>,
        data: Vec<EventData>,
    ) -> Result<AddEventsResult> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let conversation_dir = self.conversation_dir();
        let mut record = self.load_record().await?;
        let add_result = append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            self.record.id,
            session_id,
            turn_id,
            expected_head,
            data,
            &mut record,
        )
        .await?;
        remember_appended_batch(&self.event_batch_index, &conversation_dir, &add_result);
        Ok(add_result)
    }
}

struct BasicTurnHandle {
    harness: BasicExoHarness,
    agent_id: AgentId,
    conversation_dir: PathBuf,
    conversation_id: ConversationId,
    record: TurnRecord,
    event_batch_index: Arc<Mutex<Option<EventBatchIndex>>>,
    state: Mutex<BasicTurnState>,
}

struct BasicTurnState {
    latest_event_id: Option<EventId>,
    finished: bool,
}

impl BasicSandboxScope for BasicTurnHandle {
    fn sandbox_handle(&self) -> BasicScopedSandboxHandle<'_> {
        BasicScopedSandboxHandle::turn(
            &self.harness,
            self.agent_id,
            self.conversation_id,
            self.conversation_dir.clone(),
            self.record.session_id,
            self.record.id,
            &self.state,
        )
    }
}

#[async_trait]
impl TurnHandle for BasicTurnHandle {
    fn record(&self) -> &TurnRecord {
        &self.record
    }

    async fn add_events(&self, data: Vec<EventData>) -> Result<AddEventsResult> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut record =
            load_conversation_record(&self.harness.inner.storage, &self.conversation_dir).await?;
        let expected_head = record.latest_event_id;
        let add_result = append_events_to_conversation(
            &self.harness.inner,
            &self.conversation_dir,
            self.conversation_id,
            Some(self.record.session_id),
            Some(self.record.id),
            expected_head,
            data,
            &mut record,
        )
        .await?;
        remember_appended_batch(&self.event_batch_index, &self.conversation_dir, &add_result);
        self.state
            .lock()
            .expect("turn state poisoned")
            .latest_event_id = Some(add_result.latest_event_id);
        Ok(add_result)
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut record =
            load_conversation_record(&self.harness.inner.storage, &self.conversation_dir).await?;
        let expected_head = record.latest_event_id;
        let artifact_version = write_artifact_version(
            &self.harness.inner,
            &self.conversation_dir.join("artifacts"),
            request,
        )
        .await?;
        let add_result = append_events_to_conversation(
            &self.harness.inner,
            &self.conversation_dir,
            self.conversation_id,
            Some(self.record.session_id),
            Some(self.record.id),
            expected_head,
            vec![EventData::ArtifactWritten {
                artifact_id: artifact_version.artifact_id,
                path: artifact_version.path.clone(),
                version: artifact_version.version,
            }],
            &mut record,
        )
        .await?;
        self.state
            .lock()
            .expect("turn state poisoned")
            .latest_event_id = Some(add_result.latest_event_id);
        Ok(artifact_version)
    }

    async fn finish(&self) -> Result<EventId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let finished_event_id = {
            let state = self.state.lock().expect("turn state poisoned");
            if state.finished {
                Some(
                    state
                        .latest_event_id
                        .ok_or_else(|| anyhow!("turn has no latest event id"))?,
                )
            } else {
                None
            }
        };
        if let Some(event_id) = finished_event_id {
            self.harness
                .inner
                .storage
                .delete_key_if_exists(unfinished_turn_marker_path(
                    self.agent_id,
                    self.conversation_id,
                    self.record.id,
                ))
                .await?;
            return Ok(event_id);
        }
        let mut record =
            load_conversation_record(&self.harness.inner.storage, &self.conversation_dir).await?;
        let expected_head = record.latest_event_id;
        let add_result = append_events_to_conversation(
            &self.harness.inner,
            &self.conversation_dir,
            self.conversation_id,
            Some(self.record.session_id),
            Some(self.record.id),
            expected_head,
            vec![EventData::TurnEnded],
            &mut record,
        )
        .await?;
        let latest = add_result.latest_event_id;
        {
            let mut state = self.state.lock().expect("turn state poisoned");
            state.latest_event_id = Some(latest);
            state.finished = true;
        }
        self.harness
            .inner
            .storage
            .delete_key_if_exists(unfinished_turn_marker_path(
                self.agent_id,
                self.conversation_id,
                self.record.id,
            ))
            .await?;
        Ok(latest)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredBinding {
    record: BindingRecord,
}

impl StoredBinding {
    fn into_active_record(self) -> Option<BindingRecord> {
        (!matches!(self.record.binding, Binding::Llm { .. })).then_some(self.record)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredArtifactMetadata {
    #[serde(flatten)]
    version: ArtifactVersion,
}

async fn append_events_to_conversation(
    inner: &BasicExoHarnessInner,
    conversation_dir: &Path,
    conversation_id: ConversationId,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
    expected_head: Option<EventId>,
    data: Vec<EventData>,
    record: &mut ConversationRecord,
) -> Result<AddEventsResult> {
    if data.is_empty() {
        bail!("cannot append zero events");
    }
    if let Some(expected_head) = expected_head {
        ensure_conversation_head(
            record.latest_event_id,
            Some(expected_head),
            session_id,
            turn_id,
        )?;
    }
    let mut events = Vec::with_capacity(data.len());
    for data in data {
        let id = Uuid7::now();
        let event = Event {
            id,
            thread_id: conversation_id,
            session_id,
            turn_id,
            created_at: id.timestamp().expect("uuid7 timestamp"),
            data,
        };
        events.push(event);
    }
    let first_event_id = events.first().expect("at least one event").id;
    let latest_event_id = events.last().expect("at least one event").id;
    if events.len() == 1 {
        inner
            .storage
            .put_json(
                conversation_dir
                    .join("events")
                    .join(format!("{latest_event_id}.json")),
                &events[0],
            )
            .await?;
    } else {
        // One object put commits the whole batch. The filename bounds let
        // get_event skip unrelated batches without a separate index write.
        let batch = StoredEventBatch { events };
        inner
            .storage
            .put_json(
                event_batch_path(conversation_dir, first_event_id, latest_event_id),
                &batch,
            )
            .await?;
        events = batch.events;
    }
    let event_ids = events.iter().map(|event| event.id).collect();
    for event in events {
        notify_subscribers(inner, conversation_id, event);
    }
    record.latest_event_id = Some(latest_event_id);
    Ok(AddEventsResult {
        event_ids,
        latest_event_id,
    })
}

fn ensure_conversation_head(
    current_head: Option<EventId>,
    expected_head: Option<EventId>,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
) -> Result<()> {
    if current_head == expected_head {
        return Ok(());
    }
    Err(ConversationHeadMismatch {
        current_head,
        expected_head,
        session_id,
        turn_id,
    }
    .into())
}

#[derive(Debug, Clone)]
struct ConversationHeadMismatch {
    current_head: Option<EventId>,
    expected_head: Option<EventId>,
    session_id: Option<SessionId>,
    turn_id: Option<TurnId>,
}

impl Display for ConversationHeadMismatch {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let expected = format_event_head_timestamp(self.expected_head);
        let current = format_event_head_timestamp(self.current_head);
        if let Some(turn_id) = self.turn_id {
            let session = self
                .session_id
                .map(|session_id| session_id.to_string())
                .unwrap_or_else(|| "none".to_string());
            return write!(
                f,
                "turn is stale and cannot be resumed: conversation head advanced outside this turn \
                 (turn_id: {turn_id}, session_id: {session}, expected_head_at: {expected}, \
                 current_head_at: {current})"
            );
        }
        write!(
            f,
            "conversation head mismatch: expected_head_at: {expected}, current_head_at: {current}"
        )
    }
}

impl std::error::Error for ConversationHeadMismatch {}

fn format_event_head_timestamp(head: Option<EventId>) -> String {
    let Some(id) = head else {
        return "none".to_string();
    };
    id.timestamp()
        .map(|timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| "unknown".to_string())
}

fn notify_subscribers(inner: &BasicExoHarnessInner, conversation_id: ConversationId, event: Event) {
    let mut subscribers = inner.subscribers.lock().expect("subscribers poisoned");
    let Some(entries) = subscribers.get_mut(&conversation_id) else {
        return;
    };
    entries.retain(|sender| sender.send(Ok(event.clone())).is_ok());
}

fn matches_bound(event_id: EventId, bound: &Bound<EventId>) -> bool {
    match bound {
        Bound::Unbounded => false,
        Bound::Included(id) => event_id >= *id,
        Bound::Excluded(id) => event_id > *id,
    }
}

async fn read_event_from_batches(
    storage: &BasicObjectStore,
    keys: Vec<String>,
    id: EventId,
) -> Result<Option<Event>> {
    let matching_events = stream::iter(keys)
        .map(|key| async move {
            storage
                .get_json_if_exists::<StoredEventBatch>(Path::new(&key))
                .await
        })
        .buffered(16)
        .try_filter_map(|batch| async move {
            Ok(batch.and_then(|batch| batch.events.into_iter().find(|event| event.id == id)))
        });
    futures::pin_mut!(matching_events);
    matching_events.try_next().await
}

async fn load_events(storage: &BasicObjectStore, conversation_dir: &Path) -> Result<Vec<Event>> {
    let keys = storage
        .list_keys(conversation_dir.join("events"))
        .await?
        .into_iter()
        .filter(|key| event_id_from_key(key).is_some());
    let mut events = stream::iter(keys)
        .map(|key| async move {
            if is_event_batch_key(&key) {
                Ok::<_, anyhow::Error>(
                    storage
                        .get_json_if_exists::<StoredEventBatch>(Path::new(&key))
                        .await?
                        .map(|batch| batch.events)
                        .unwrap_or_default(),
                )
            } else {
                Ok(storage
                    .get_json_if_exists::<Event>(Path::new(&key))
                    .await?
                    .into_iter()
                    .collect())
            }
        })
        .buffered(16)
        .try_collect::<Vec<Vec<Event>>>()
        .await?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    events.sort_by_key(|event| event.id);
    Ok(events)
}

async fn load_conversation_record(
    storage: &BasicObjectStore,
    conversation_dir: &Path,
) -> Result<ConversationRecord> {
    let mut record = storage
        .get_json::<ConversationRecord>(conversation_dir.join("record.json"))
        .await?;
    // Event objects are authoritative. record.json also stores mutable thread
    // metadata, but its cached head need not be rewritten after every append.
    record.latest_event_id = latest_committed_event_id(storage, conversation_dir).await?;
    Ok(record)
}

async fn latest_committed_event_id(
    storage: &BasicObjectStore,
    conversation_dir: &Path,
) -> Result<Option<EventId>> {
    let keys = storage.list_keys(conversation_dir.join("events")).await?;
    Ok(latest_event_id_from_keys(&keys))
}

fn latest_event_id_from_keys(keys: &[String]) -> Option<EventId> {
    keys.iter().filter_map(|key| event_id_from_key(key)).max()
}

fn event_id_from_key(key: &str) -> Option<EventId> {
    if let Some((_, last)) = event_batch_range_from_key(key) {
        return Some(last);
    }
    key.rsplit('/').next()?.strip_suffix(".json")?.parse().ok()
}

fn event_batch_range_from_key(key: &str) -> Option<(EventId, EventId)> {
    let range = key.rsplit('/').next()?.strip_suffix(".batch.json")?;
    let (first, last) = range.split_once('_')?;
    Some((first.parse().ok()?, last.parse().ok()?))
}

fn is_event_batch_key(key: &str) -> bool {
    key.ends_with(".batch.json")
}

async fn load_artifact_versions(
    storage: &BasicObjectStore,
    artifacts_dir: &Path,
) -> Result<Vec<ArtifactVersion>> {
    let mut versions = storage
        .list_json_matching_suffix::<StoredArtifactMetadata>(artifacts_dir, ".json")
        .await?
        .into_iter()
        .map(|artifact| artifact.version)
        .collect::<Vec<_>>();
    versions.sort_by_key(|artifact| (artifact.artifact_id, artifact.version));
    Ok(versions)
}

async fn write_artifact_version(
    inner: &BasicExoHarnessInner,
    artifacts_dir: &Path,
    request: WriteArtifactRequest,
) -> Result<ArtifactVersion> {
    let versions = load_artifact_versions(&inner.storage, artifacts_dir).await?;
    let existing = versions
        .iter()
        .filter(|artifact| artifact.path == request.path)
        .max_by_key(|artifact| artifact.version);
    let artifact_id = existing
        .map(|artifact| artifact.artifact_id)
        .unwrap_or_else(Uuid7::now);
    let version = existing.map(|artifact| artifact.version + 1).unwrap_or(1);
    let created_at = Uuid7::now().timestamp().expect("uuid7 timestamp");
    let artifact_version = ArtifactVersion {
        artifact_id,
        path: request.path,
        version,
        created_at,
        size_bytes: request.contents.len() as u64,
    };
    let artifact_dir = artifacts_dir.join(artifact_id.to_string());
    inner
        .storage
        .put_json(
            artifact_dir.join(format!("{version}.json")),
            &StoredArtifactMetadata {
                version: artifact_version.clone(),
            },
        )
        .await?;
    inner
        .storage
        .put_bytes(
            artifact_dir.join(format!("{version}.bin")),
            request.contents,
        )
        .await?;
    Ok(artifact_version)
}

async fn load_artifact_contents(
    storage: &BasicObjectStore,
    artifact_dir: &Path,
    version: u64,
) -> Result<Vec<u8>> {
    let contents_path = artifact_dir.join(format!("{version}.bin"));
    if let Some(contents) = storage.get_bytes_if_exists(&contents_path).await? {
        return Ok(contents);
    }

    let metadata_path = artifact_dir.join(format!("{version}.json"));
    let legacy_artifact = storage
        .get_json_if_exists::<Artifact>(&metadata_path)
        .await?;
    let Some(legacy_artifact) = legacy_artifact else {
        bail!("missing artifact contents for {}", metadata_path.display());
    };
    Ok(legacy_artifact.contents)
}

async fn list_binding_records(
    storage: &BasicObjectStore,
    bindings_dir: &Path,
) -> Result<Vec<BindingRecord>> {
    let mut bindings = storage
        .list_json_matching_suffix::<StoredBinding>(bindings_dir, ".json")
        .await?
        .into_iter()
        .filter_map(StoredBinding::into_active_record)
        .collect::<Vec<_>>();
    bindings.sort_by_key(|metadata| metadata.id);
    Ok(bindings)
}

fn stored_binding(id: BindingId, binding: Binding) -> StoredBinding {
    StoredBinding {
        record: BindingRecord {
            id,
            r#type: binding_type(&binding),
            name: binding_name(&binding).to_string(),
            created_at: id.timestamp().expect("uuid7 timestamp"),
            binding,
        },
    }
}

fn binding_type(binding: &Binding) -> BindingType {
    match binding {
        Binding::Env { .. } => BindingType::Env,
        Binding::Mcp { .. } => BindingType::Mcp,
        Binding::Llm { .. } => BindingType::Llm,
        Binding::Sandbox { .. } => BindingType::Sandbox,
    }
}

fn binding_name(binding: &Binding) -> &str {
    match binding {
        Binding::Env { name, .. }
        | Binding::Mcp { name, .. }
        | Binding::Llm { name, .. }
        | Binding::Sandbox { name, .. } => name,
    }
}

fn derive_unique_slug(prefix: &str, existing: &[ConversationRecord]) -> String {
    let mut counter = 1usize;
    loop {
        let candidate = format!("{prefix}-{counter}");
        if existing
            .iter()
            .all(|conversation| conversation.slug != candidate)
        {
            return candidate;
        }
        counter += 1;
    }
}

fn slug_to_name(slug: &str) -> String {
    slug.replace('-', " ")
}
