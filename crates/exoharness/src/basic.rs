use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::sandbox::{
    CliContainerSandboxBackend, LocalProcessSandboxBackend, ManagedSandboxBackend,
    ManagedSandboxHandle, SANDBOX_MAIN_MOUNT_DIR, SandboxCommand, SandboxKey,
    SandboxLifecycleConfig, SandboxMount, SandboxMountAccess, SandboxNetworkPolicy, SandboxRequest,
    SandboxSpec,
};
use crate::secrets::{
    AppleKeychainSecretKeyProvider, EncryptedSecret, FileBackedSecretKeyProvider, SecretCipher,
    SecretKeyProvider, StaticSecretKeyProvider, default_master_key_path,
};
use crate::storage::BasicObjectStore;
use crate::{
    AddEventsRequest, AddEventsResult, AgentHandle, AgentId, AgentRecord, Artifact,
    ArtifactVersion, BeginTurnRequest, Binding, BindingId, BindingMetadata, BindingType,
    ConversationHandle, ConversationId, ConversationRecord, CreateSandboxRequest, Event, EventData,
    EventId, EventQuery, EventQueryDirection, EventStream, ExoHarness, FileSystemMount,
    ForkConversationRequest, GetEventsResult, NewAgentRequest, NewConversationRequest,
    PutSecretRequest, ReadArtifactRequest, Result, RunInSandboxRequest, SandboxId, SandboxProcess,
    SandboxProcessParts, Secret, SecretId, SecretMetadata, SecretType, SessionId, SnapshotId,
    StartSandboxRequest, TurnHandle, TurnId, TurnRecord, Uuid7, WriteArtifactRequest,
};

#[derive(Debug, Clone)]
pub enum SecretBackendChoice {
    AppleKeychain,
    File { path: Option<PathBuf> },
    Static([u8; 32]),
}

#[derive(Debug, Clone, Copy)]
pub enum SandboxBackendChoice {
    AppleContainer,
    Docker,
    LocalProcess,
}

// TODO: as more knobs land here, swap to a builder pattern.
#[derive(Debug, Clone)]
pub struct BasicExoHarnessConfig {
    pub root: PathBuf,
    pub secret_backend: SecretBackendChoice,
    pub sandbox_backend: SandboxBackendChoice,
}

#[derive(Clone)]
pub struct BasicExoHarness {
    inner: Arc<BasicExoHarnessInner>,
}

struct BasicExoHarnessInner {
    storage: BasicObjectStore,
    write_lock: AsyncMutex<()>,
    subscribers: Mutex<HashMap<ConversationId, Vec<mpsc::UnboundedSender<Result<Event>>>>>,
    sandbox_backend: Arc<dyn ManagedSandboxBackend>,
    running_sandboxes: AsyncMutex<HashMap<SandboxId, Arc<dyn ManagedSandboxHandle>>>,
    secret_cipher: SecretCipher,
}

impl BasicExoHarness {
    pub async fn new(config: BasicExoHarnessConfig) -> Result<Self> {
        let BasicExoHarnessConfig {
            root,
            secret_backend,
            sandbox_backend,
        } = config;
        let storage = BasicObjectStore::local_filesystem(&root).await?;
        let secret_cipher =
            build_secret_cipher(secret_backend, root.to_string_lossy().to_string())?;
        let sandbox_backend = build_sandbox_backend(sandbox_backend);
        Ok(Self {
            inner: Arc::new(BasicExoHarnessInner {
                storage,
                write_lock: AsyncMutex::new(()),
                subscribers: Mutex::new(HashMap::new()),
                sandbox_backend,
                running_sandboxes: AsyncMutex::new(HashMap::new()),
                secret_cipher,
            }),
        })
    }

    fn agents_dir(&self) -> PathBuf {
        PathBuf::from("agents")
    }

    fn bindings_dir(&self) -> PathBuf {
        PathBuf::from("bindings")
    }

    fn secrets_dir(&self) -> PathBuf {
        PathBuf::from("secrets")
    }

    async fn list_agent_records(&self) -> Result<Vec<AgentRecord>> {
        let mut agents = Vec::new();
        for key in self.inner.storage.list_keys(self.agents_dir()).await? {
            if !key.ends_with("/record.json") || Path::new(&key).components().count() != 3 {
                continue;
            }
            agents.push(
                self.inner
                    .storage
                    .get_json::<AgentRecord>(Path::new(&key))
                    .await?,
            );
        }
        agents.sort_by_key(|record| record.id);
        Ok(agents)
    }
}

#[async_trait]
impl ExoHarness for BasicExoHarness {
    async fn list_agents(&self) -> Result<Vec<Arc<dyn AgentHandle>>> {
        let mut handles: Vec<Arc<dyn AgentHandle>> = Vec::new();
        for record in self.list_agent_records().await? {
            handles.push(Arc::new(BasicAgentHandle {
                harness: self.clone(),
                record,
            }));
        }
        Ok(handles)
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<Arc<dyn AgentHandle>>> {
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
        let _guard = self.inner.write_lock.lock().await;
        let existing = self.list_agent_records().await?;
        if existing.iter().any(|agent| agent.slug == request.slug) {
            bail!("agent slug already exists: {}", request.slug);
        }

        let record = AgentRecord {
            id: Uuid7::now(),
            slug: request.slug,
            name: request.name,
        };
        let agent_dir = self.agents_dir().join(record.id.to_string());
        self.inner
            .storage
            .put_json(agent_dir.join("record.json"), &record)
            .await?;
        Ok(Arc::new(BasicAgentHandle {
            harness: self.clone(),
            record,
        }))
    }

    async fn delete_agent(&self, id: &AgentId) -> Result<bool> {
        let _guard = self.inner.write_lock.lock().await;
        let agent_dir = self.agents_dir().join(id.to_string());
        if self.inner.storage.list_keys(&agent_dir).await?.is_empty() {
            return Ok(false);
        }
        self.inner.storage.delete_prefix(agent_dir).await?;
        Ok(true)
    }

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>> {
        list_binding_metadata(&self.inner.storage, &self.bindings_dir()).await
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        let _guard = self.inner.write_lock.lock().await;
        let id = Uuid7::now();
        let record = StoredBinding {
            metadata: BindingMetadata {
                id,
                r#type: binding_type(&binding),
                name: binding_name(&binding).to_string(),
                created_at: id.timestamp().expect("uuid7 timestamp"),
            },
            binding,
        };
        self.inner
            .storage
            .put_json(self.bindings_dir().join(format!("{id}.json")), &record)
            .await?;
        Ok(id)
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        let path = self.bindings_dir().join(format!("{id}.json"));
        Ok(self
            .inner
            .storage
            .get_json_if_exists::<StoredBinding>(&path)
            .await?
            .map(|record| record.binding))
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        list_secret_metadata(&self.inner.storage, &self.secrets_dir()).await
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        let _guard = self.inner.write_lock.lock().await;
        let id = Uuid7::now();
        let record = StoredSecret {
            metadata: SecretMetadata {
                id,
                r#type: secret_type(&request.secret),
                name: request.name,
                created_at: id.timestamp().expect("uuid7 timestamp"),
            },
            secret: self.inner.secret_cipher.encrypt_secret(&request.secret)?,
        };
        self.inner
            .storage
            .put_json(self.secrets_dir().join(format!("{id}.json")), &record)
            .await?;
        Ok(id)
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        let path = self.secrets_dir().join(format!("{id}.json"));
        let Some(record) = self
            .inner
            .storage
            .get_json_if_exists::<StoredSecret>(&path)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(
            self.inner.secret_cipher.decrypt_secret(&record.secret)?,
        ))
    }
}

struct BasicAgentHandle {
    harness: BasicExoHarness,
    record: AgentRecord,
}

#[async_trait]
impl AgentHandle for BasicAgentHandle {
    fn record(&self) -> &AgentRecord {
        &self.record
    }

    async fn list_conversations(&self) -> Result<Vec<Arc<dyn ConversationHandle>>> {
        let mut handles: Vec<Arc<dyn ConversationHandle>> = Vec::new();
        for record in self.list_conversation_records().await? {
            handles.push(Arc::new(BasicConversationHandle {
                harness: self.harness.clone(),
                agent_id: self.record.id,
                record,
            }));
        }
        Ok(handles)
    }

    async fn get_conversation(
        &self,
        id: &ConversationId,
    ) -> Result<Option<Arc<dyn ConversationHandle>>> {
        let record_path = self
            .conversations_dir()
            .join(id.to_string())
            .join("record.json");
        let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<ConversationRecord>(&record_path)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(BasicConversationHandle {
            harness: self.harness.clone(),
            agent_id: self.record.id,
            record,
        })))
    }

    async fn new_conversation(
        &self,
        request: NewConversationRequest,
    ) -> Result<Arc<dyn ConversationHandle>> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let existing = self.list_conversation_records().await?;
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
        let record = ConversationRecord {
            id: Uuid7::now(),
            slug: slug.clone(),
            name: request.name.unwrap_or_else(|| slug_to_name(&slug)),
            latest_event_id: None,
        };
        let conversation_dir = self.conversations_dir().join(record.id.to_string());
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &record)
            .await?;
        Ok(Arc::new(BasicConversationHandle {
            harness: self.harness.clone(),
            agent_id: self.record.id,
            record,
        }))
    }

    async fn delete_conversation(&self, id: &ConversationId) -> Result<bool> {
        let _guard = self.harness.inner.write_lock.lock().await;
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
        self.harness
            .inner
            .storage
            .delete_prefix(conversation_dir)
            .await?;
        Ok(true)
    }

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>> {
        Ok(merge_binding_metadata(vec![
            list_binding_metadata(&self.harness.inner.storage, &self.harness.bindings_dir())
                .await?,
            list_binding_metadata(&self.harness.inner.storage, &self.bindings_dir()).await?,
        ]))
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let id = Uuid7::now();
        let record = StoredBinding {
            metadata: BindingMetadata {
                id,
                r#type: binding_type(&binding),
                name: binding_name(&binding).to_string(),
                created_at: id.timestamp().expect("uuid7 timestamp"),
            },
            binding,
        };
        self.harness
            .inner
            .storage
            .put_json(self.bindings_dir().join(format!("{id}.json")), &record)
            .await?;
        Ok(id)
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        let path = self.bindings_dir().join(format!("{id}.json"));
        if let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<StoredBinding>(&path)
            .await?
        {
            return Ok(Some(record.binding));
        }
        self.harness.get_binding(id).await
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        Ok(merge_secret_metadata(vec![
            list_secret_metadata(&self.harness.inner.storage, &self.harness.secrets_dir()).await?,
            list_secret_metadata(&self.harness.inner.storage, &self.secrets_dir()).await?,
        ]))
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let id = Uuid7::now();
        let record = StoredSecret {
            metadata: SecretMetadata {
                id,
                r#type: secret_type(&request.secret),
                name: request.name,
                created_at: id.timestamp().expect("uuid7 timestamp"),
            },
            secret: self
                .harness
                .inner
                .secret_cipher
                .encrypt_secret(&request.secret)?,
        };
        self.harness
            .inner
            .storage
            .put_json(self.secrets_dir().join(format!("{id}.json")), &record)
            .await?;
        Ok(id)
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        let path = self.secrets_dir().join(format!("{id}.json"));
        let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<StoredSecret>(&path)
            .await?
        else {
            return self.harness.get_secret(id).await;
        };
        Ok(Some(
            self.harness
                .inner
                .secret_cipher
                .decrypt_secret(&record.secret)?,
        ))
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        let _guard = self.harness.inner.write_lock.lock().await;
        write_artifact_version(&self.harness.inner, &self.artifacts_dir(), request).await
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
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
        load_artifact_versions(&self.harness.inner.storage, &self.artifacts_dir()).await
    }
}

impl BasicAgentHandle {
    fn agent_dir(&self) -> PathBuf {
        self.harness.agents_dir().join(self.record.id.to_string())
    }

    fn conversations_dir(&self) -> PathBuf {
        self.agent_dir().join("conversations")
    }

    fn bindings_dir(&self) -> PathBuf {
        self.agent_dir().join("bindings")
    }

    fn secrets_dir(&self) -> PathBuf {
        self.agent_dir().join("secrets")
    }

    fn artifacts_dir(&self) -> PathBuf {
        self.agent_dir().join("artifacts")
    }

    async fn list_conversation_records(&self) -> Result<Vec<ConversationRecord>> {
        let mut conversations = Vec::new();
        for key in self
            .harness
            .inner
            .storage
            .list_keys(self.conversations_dir())
            .await?
        {
            if !key.ends_with("/record.json") || Path::new(&key).components().count() != 5 {
                continue;
            }
            conversations.push(
                self.harness
                    .inner
                    .storage
                    .get_json::<ConversationRecord>(Path::new(&key))
                    .await?,
            );
        }
        conversations.sort_by_key(|record| record.id);
        Ok(conversations)
    }
}

struct BasicConversationHandle {
    harness: BasicExoHarness,
    agent_id: AgentId,
    record: ConversationRecord,
}

#[async_trait]
impl ConversationHandle for BasicConversationHandle {
    fn record(&self) -> &ConversationRecord {
        &self.record
    }

    async fn start_session(&self) -> Result<SessionId> {
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
        self.append_events_internal(Some(id), None, None, vec![EventData::SessionEnded])
            .await?;
        Ok(())
    }

    async fn begin_turn(&self, request: BeginTurnRequest) -> Result<Arc<dyn TurnHandle>> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut record = self.load_record().await?;
        let conversation_dir = self.conversation_dir();

        let session_id = request.session_id.unwrap_or_else(Uuid7::now);
        let turn_record = TurnRecord {
            id: Uuid7::now(),
            session_id,
        };
        let mut events_to_append = Vec::new();

        if request.session_id.is_none() {
            events_to_append.push(EventData::SessionStarted);
        }
        events_to_append.push(EventData::TurnStarted);
        if !request.input.is_empty() {
            events_to_append.push(EventData::Messages {
                messages: request.input,
                response_id: None,
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
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &record)
            .await?;

        Ok(Arc::new(BasicTurnHandle {
            harness: self.harness.clone(),
            conversation_dir,
            conversation_id: self.record.id,
            record: turn_record,
            state: Mutex::new(BasicTurnState {
                latest_event_id: Some(add_result.latest_event_id),
                finished: false,
            }),
        }))
    }

    async fn get_events(&self, query: Option<EventQuery>) -> Result<GetEventsResult> {
        let mut events = load_events(&self.harness.inner.storage, &self.events_dir()).await?;
        if let Some(query) = query {
            if let Some(session_id) = query.session_id {
                events.retain(|event| event.session_id == Some(session_id));
            }
            if let Some(turn_id) = query.turn_id {
                events.retain(|event| event.turn_id == Some(turn_id));
            }
            if let Some(types) = query.types {
                events.retain(|event| types.iter().any(|ty| event_type(&event.data) == *ty));
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
        let cursor = events.last().map(|event| event.id);
        Ok(GetEventsResult { events, cursor })
    }

    async fn watch_events(&self, after_exclusive: Bound<EventId>) -> Result<EventStream> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let existing = match after_exclusive {
            Bound::Unbounded => Vec::new(),
            _ => {
                let events = load_events(&self.harness.inner.storage, &self.events_dir()).await?;
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
        let path = self.events_dir().join(format!("{id}.json"));
        self.harness.inner.storage.get_json_if_exists(&path).await
    }

    async fn add_events(&self, request: AddEventsRequest) -> Result<AddEventsResult> {
        self.append_events_internal(
            request.session_id,
            request.turn_id,
            request.expected_head,
            request.data,
        )
        .await
    }

    async fn fork(&self, request: ForkConversationRequest) -> Result<Arc<dyn ConversationHandle>> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let agent = BasicAgentHandle {
            harness: self.harness.clone(),
            record: self
                .harness
                .inner
                .storage
                .get_json::<AgentRecord>(self.agent_dir().join("record.json"))
                .await?,
        };
        let existing = agent.list_conversation_records().await?;
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
        let mut events = load_events(&self.harness.inner.storage, &self.events_dir()).await?;
        if let Some(limit) = request.up_to_inclusive {
            events.retain(|event| event.id <= limit);
        }
        let record = ConversationRecord {
            id: Uuid7::now(),
            slug: slug.clone(),
            name: request.name.unwrap_or_else(|| slug_to_name(&slug)),
            latest_event_id: None,
        };
        let conversation_dir = agent.conversations_dir().join(record.id.to_string());
        self.harness
            .inner
            .storage
            .copy_prefix(self.bindings_dir(), conversation_dir.join("bindings"))
            .await?;
        self.harness
            .inner
            .storage
            .copy_prefix(self.secrets_dir(), conversation_dir.join("secrets"))
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
            event.conversation_id = record.id;
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
            vec![EventData::ConversationForked {
                source_conversation_id: self.record.id,
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
        }))
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
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
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &record)
            .await?;
        Ok(artifact_version)
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
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
        load_artifact_versions(&self.harness.inner.storage, &self.artifacts_dir()).await
    }

    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let sandbox_id = format!("sandbox-{}", Uuid7::now());
        let conversation_dir = self.conversation_dir();
        let metadata = StoredSandbox {
            id: sandbox_id.clone(),
            image: request.image.clone(),
            default_workdir: request.default_workdir.clone(),
            file_system_mounts: request.file_system_mounts.clone().unwrap_or_default(),
            enable_networking: request.enable_networking.unwrap_or(true),
            idle_seconds: request.idle_seconds.unwrap_or(60),
            running: true,
            latest_snapshot_id: None,
        };
        let sandbox_handle = self
            .harness
            .inner
            .sandbox_backend
            .acquire(sandbox_request(self.record.id, &sandbox_id, &metadata))
            .await?;
        self.harness
            .inner
            .storage
            .put_json(
                self.sandboxes_dir().join(format!("{sandbox_id}.json")),
                &metadata,
            )
            .await?;
        self.harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .insert(sandbox_id.clone(), sandbox_handle);
        let mut record = self.load_record().await?;
        append_events_to_conversation(
            &self.harness.inner,
            &conversation_dir,
            self.record.id,
            None,
            None,
            record.latest_event_id,
            vec![
                EventData::SandboxCreated {
                    sandbox_id: sandbox_id.clone(),
                    image: request.image,
                    default_workdir: metadata.default_workdir.clone().unwrap_or_default(),
                    file_system_mounts: metadata.file_system_mounts.clone(),
                    enable_networking: metadata.enable_networking,
                    idle_seconds: metadata.idle_seconds,
                },
                EventData::SandboxStarted {
                    sandbox_id: sandbox_id.clone(),
                    snapshot_id: None,
                },
            ],
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &record)
            .await?;
        Ok(sandbox_id)
    }

    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut sandbox = self.load_sandbox(&id).await?;
        let snapshot_id = Uuid7::now();
        sandbox.latest_snapshot_id = Some(snapshot_id);
        self.harness
            .inner
            .storage
            .put_json(self.sandboxes_dir().join(format!("{id}.json")), &sandbox)
            .await?;
        let mut record = self.load_record().await?;
        append_events_to_conversation(
            &self.harness.inner,
            &self.conversation_dir(),
            self.record.id,
            None,
            None,
            record.latest_event_id,
            vec![EventData::SandboxSnapshotted {
                sandbox_id: id,
                snapshot_id,
            }],
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(self.conversation_dir().join("record.json"), &record)
            .await?;
        Ok(snapshot_id)
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut sandbox = self.load_sandbox(&request.id).await?;
        sandbox.running = true;
        sandbox.latest_snapshot_id = Some(request.snapshot_id);
        if let Some(idle_seconds) = request.idle_seconds {
            sandbox.idle_seconds = idle_seconds;
        }
        let previous_handle = self
            .harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .remove(&request.id);
        if let Some(previous_handle) = previous_handle {
            previous_handle.stop().await?;
        }
        let sandbox_handle = self
            .harness
            .inner
            .sandbox_backend
            .acquire(sandbox_request(self.record.id, &request.id, &sandbox))
            .await?;
        self.harness
            .inner
            .storage
            .put_json(
                self.sandboxes_dir().join(format!("{}.json", request.id)),
                &sandbox,
            )
            .await?;
        self.harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .insert(request.id.clone(), sandbox_handle);
        let mut record = self.load_record().await?;
        append_events_to_conversation(
            &self.harness.inner,
            &self.conversation_dir(),
            self.record.id,
            None,
            None,
            record.latest_event_id,
            vec![EventData::SandboxStarted {
                sandbox_id: request.id,
                snapshot_id: Some(request.snapshot_id),
            }],
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(self.conversation_dir().join("record.json"), &record)
            .await?;
        Ok(())
    }

    async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let mut sandbox = self.load_sandbox(&id).await?;
        if !sandbox.running {
            return Ok(());
        }
        let sandbox_handle = self
            .harness
            .inner
            .running_sandboxes
            .lock()
            .await
            .remove(&id);
        sandbox.running = false;
        self.harness
            .inner
            .storage
            .put_json(self.sandboxes_dir().join(format!("{id}.json")), &sandbox)
            .await?;
        if let Some(sandbox_handle) = sandbox_handle {
            sandbox_handle.stop().await?;
        }
        let mut record = self.load_record().await?;
        append_events_to_conversation(
            &self.harness.inner,
            &self.conversation_dir(),
            self.record.id,
            None,
            None,
            record.latest_event_id,
            vec![EventData::SandboxStopped { sandbox_id: id }],
            &mut record,
        )
        .await?;
        self.harness
            .inner
            .storage
            .put_json(self.conversation_dir().join("record.json"), &record)
            .await?;
        Ok(())
    }

    async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        let sandbox = self.load_sandbox(&request.id).await?;
        if !sandbox.running {
            bail!("sandbox is not running: {}", request.id);
        }
        if request.command.is_empty() {
            bail!("sandbox command must not be empty");
        }
        let sandbox_handle = {
            let running_sandboxes = self.harness.inner.running_sandboxes.lock().await;
            running_sandboxes.get(&request.id).cloned()
        };
        let sandbox_handle = match sandbox_handle {
            Some(sandbox_handle) => sandbox_handle,
            None => {
                let sandbox_handle = self
                    .harness
                    .inner
                    .sandbox_backend
                    .acquire(sandbox_request(self.record.id, &request.id, &sandbox))
                    .await?;
                self.harness
                    .inner
                    .running_sandboxes
                    .lock()
                    .await
                    .insert(request.id.clone(), sandbox_handle.clone());
                sandbox_handle
            }
        };
        let parts = sandbox_handle
            .start_process(&SandboxCommand {
                argv: request.command.clone(),
                env: request.env.clone(),
                display_argv: Some(request.command),
                cwd: None,
                timeout: None,
            })
            .await
            .with_context(|| format!("failed to run command in sandbox {}", request.id))?;
        Ok(Box::new(LiveSandboxProcess::new(parts)))
    }

    async fn list_bindings(&self) -> Result<Vec<BindingMetadata>> {
        Ok(merge_binding_metadata(vec![
            list_binding_metadata(&self.harness.inner.storage, &self.harness.bindings_dir())
                .await?,
            list_binding_metadata(
                &self.harness.inner.storage,
                &agent_bindings_dir(&self.harness, self.agent_id),
            )
            .await?,
            list_binding_metadata(&self.harness.inner.storage, &self.bindings_dir()).await?,
        ]))
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let id = Uuid7::now();
        let record = StoredBinding {
            metadata: BindingMetadata {
                id,
                r#type: binding_type(&binding),
                name: binding_name(&binding).to_string(),
                created_at: id.timestamp().expect("uuid7 timestamp"),
            },
            binding,
        };
        self.harness
            .inner
            .storage
            .put_json(self.bindings_dir().join(format!("{id}.json")), &record)
            .await?;
        Ok(id)
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        let local_path = self.bindings_dir().join(format!("{id}.json"));
        if let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<StoredBinding>(&local_path)
            .await?
        {
            return Ok(Some(record.binding));
        }
        let agent_path =
            agent_bindings_dir(&self.harness, self.agent_id).join(format!("{id}.json"));
        if let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<StoredBinding>(&agent_path)
            .await?
        {
            return Ok(Some(record.binding));
        }
        self.harness.get_binding(id).await
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        Ok(merge_secret_metadata(vec![
            list_secret_metadata(&self.harness.inner.storage, &self.harness.secrets_dir()).await?,
            list_secret_metadata(
                &self.harness.inner.storage,
                &agent_secrets_dir(&self.harness, self.agent_id),
            )
            .await?,
            list_secret_metadata(&self.harness.inner.storage, &self.secrets_dir()).await?,
        ]))
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let id = Uuid7::now();
        let record = StoredSecret {
            metadata: SecretMetadata {
                id,
                r#type: secret_type(&request.secret),
                name: request.name,
                created_at: id.timestamp().expect("uuid7 timestamp"),
            },
            secret: self
                .harness
                .inner
                .secret_cipher
                .encrypt_secret(&request.secret)?,
        };
        self.harness
            .inner
            .storage
            .put_json(self.secrets_dir().join(format!("{id}.json")), &record)
            .await?;
        Ok(id)
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        let local_path = self.secrets_dir().join(format!("{id}.json"));
        if let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<StoredSecret>(&local_path)
            .await?
        {
            return Ok(Some(
                self.harness
                    .inner
                    .secret_cipher
                    .decrypt_secret(&record.secret)?,
            ));
        }
        let agent_path = agent_secrets_dir(&self.harness, self.agent_id).join(format!("{id}.json"));
        let Some(record) = self
            .harness
            .inner
            .storage
            .get_json_if_exists::<StoredSecret>(&agent_path)
            .await?
        else {
            return self.harness.get_secret(id).await;
        };
        Ok(Some(
            self.harness
                .inner
                .secret_cipher
                .decrypt_secret(&record.secret)?,
        ))
    }
}

impl BasicConversationHandle {
    fn agent_dir(&self) -> PathBuf {
        self.harness.agents_dir().join(self.agent_id.to_string())
    }

    fn conversation_dir(&self) -> PathBuf {
        self.agent_dir()
            .join("conversations")
            .join(self.record.id.to_string())
    }

    fn events_dir(&self) -> PathBuf {
        self.conversation_dir().join("events")
    }

    fn bindings_dir(&self) -> PathBuf {
        self.conversation_dir().join("bindings")
    }

    fn secrets_dir(&self) -> PathBuf {
        self.conversation_dir().join("secrets")
    }

    fn artifacts_dir(&self) -> PathBuf {
        self.conversation_dir().join("artifacts")
    }

    fn sandboxes_dir(&self) -> PathBuf {
        self.conversation_dir().join("sandboxes")
    }

    async fn load_record(&self) -> Result<ConversationRecord> {
        self.harness
            .inner
            .storage
            .get_json(self.conversation_dir().join("record.json"))
            .await
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
        self.harness
            .inner
            .storage
            .put_json(conversation_dir.join("record.json"), &record)
            .await?;
        Ok(add_result)
    }

    async fn load_sandbox(&self, id: &str) -> Result<StoredSandbox> {
        self.harness
            .inner
            .storage
            .get_json(self.sandboxes_dir().join(format!("{id}.json")))
            .await
    }
}

struct BasicTurnHandle {
    harness: BasicExoHarness,
    conversation_dir: PathBuf,
    conversation_id: ConversationId,
    record: TurnRecord,
    state: Mutex<BasicTurnState>,
}

struct BasicTurnState {
    latest_event_id: Option<EventId>,
    finished: bool,
}

#[async_trait]
impl TurnHandle for BasicTurnHandle {
    fn record(&self) -> &TurnRecord {
        &self.record
    }

    async fn add_events(&self, data: Vec<EventData>) -> Result<AddEventsResult> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let expected_head = self
            .state
            .lock()
            .expect("turn state poisoned")
            .latest_event_id;
        let mut record = self
            .harness
            .inner
            .storage
            .get_json::<ConversationRecord>(self.conversation_dir.join("record.json"))
            .await?;
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
        self.harness
            .inner
            .storage
            .put_json(self.conversation_dir.join("record.json"), &record)
            .await?;
        self.state
            .lock()
            .expect("turn state poisoned")
            .latest_event_id = Some(add_result.latest_event_id);
        Ok(add_result)
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let expected_head = self
            .state
            .lock()
            .expect("turn state poisoned")
            .latest_event_id;
        let mut record = self
            .harness
            .inner
            .storage
            .get_json::<ConversationRecord>(self.conversation_dir.join("record.json"))
            .await?;
        ensure_conversation_head(
            record.latest_event_id,
            expected_head,
            Some(self.record.session_id),
            Some(self.record.id),
        )?;
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
        self.harness
            .inner
            .storage
            .put_json(self.conversation_dir.join("record.json"), &record)
            .await?;
        self.state
            .lock()
            .expect("turn state poisoned")
            .latest_event_id = Some(add_result.latest_event_id);
        Ok(artifact_version)
    }

    async fn finish(&self) -> Result<EventId> {
        let _guard = self.harness.inner.write_lock.lock().await;
        let expected_head = {
            let state = self.state.lock().expect("turn state poisoned");
            if state.finished {
                return state
                    .latest_event_id
                    .ok_or_else(|| anyhow!("turn has no latest event id"));
            }
            state.latest_event_id
        };
        let mut record = self
            .harness
            .inner
            .storage
            .get_json::<ConversationRecord>(self.conversation_dir.join("record.json"))
            .await?;
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
        self.harness
            .inner
            .storage
            .put_json(self.conversation_dir.join("record.json"), &record)
            .await?;
        let latest = add_result.latest_event_id;
        let mut state = self.state.lock().expect("turn state poisoned");
        state.latest_event_id = Some(latest);
        state.finished = true;
        Ok(latest)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredBinding {
    metadata: BindingMetadata,
    binding: Binding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSecret {
    metadata: SecretMetadata,
    secret: EncryptedSecret,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredArtifactMetadata {
    #[serde(flatten)]
    version: ArtifactVersion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSandbox {
    id: SandboxId,
    image: String,
    default_workdir: Option<String>,
    file_system_mounts: Vec<FileSystemMount>,
    enable_networking: bool,
    idle_seconds: u64,
    running: bool,
    latest_snapshot_id: Option<SnapshotId>,
}

struct LiveSandboxProcess {
    parts: Option<SandboxProcessParts>,
}

impl LiveSandboxProcess {
    fn new(parts: SandboxProcessParts) -> Self {
        Self { parts: Some(parts) }
    }
}

impl SandboxProcess for LiveSandboxProcess {
    fn into_parts(mut self: Box<Self>) -> SandboxProcessParts {
        self.parts
            .take()
            .expect("live sandbox process parts already consumed")
    }
}

fn sandbox_request(
    conversation_id: ConversationId,
    sandbox_id: &str,
    sandbox: &StoredSandbox,
) -> SandboxRequest {
    SandboxRequest {
        key: SandboxKey::ConversationSandbox {
            conversation_id: conversation_id.to_string(),
            sandbox_id: sandbox_id.to_string(),
        },
        spec: SandboxSpec {
            image: sandbox.image.clone(),
            mounts: sandbox
                .file_system_mounts
                .iter()
                .map(|mount| SandboxMount {
                    host_path: PathBuf::from(&mount.host_path),
                    guest_path: mount.mount_path.clone(),
                    access: match mount.mode {
                        crate::FileSystemMountMode::ReadOnly => SandboxMountAccess::ReadOnly,
                        crate::FileSystemMountMode::ReadWrite => SandboxMountAccess::ReadWrite,
                    },
                    internal: mount.internal.unwrap_or(false),
                })
                .collect(),
            network: if sandbox.enable_networking {
                SandboxNetworkPolicy::Enabled
            } else {
                SandboxNetworkPolicy::Disabled
            },
            default_workdir: sandbox
                .default_workdir
                .clone()
                .unwrap_or_else(|| SANDBOX_MAIN_MOUNT_DIR.to_string()),
        },
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(std::time::Duration::from_secs(sandbox.idle_seconds)),
        },
    }
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
    let mut event_ids = Vec::new();
    let mut latest_event_id = None;
    for data in data {
        let id = Uuid7::now();
        let event = Event {
            id,
            conversation_id,
            session_id,
            turn_id,
            created_at: id.timestamp().expect("uuid7 timestamp"),
            data,
        };
        inner
            .storage
            .put_json(
                conversation_dir
                    .join("events")
                    .join(format!("{}.json", event.id)),
                &event,
            )
            .await?;
        notify_subscribers(inner, conversation_id, event.clone());
        latest_event_id = Some(event.id);
        event_ids.push(event.id);
    }
    let latest_event_id = latest_event_id.expect("at least one event");
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

async fn load_events(storage: &BasicObjectStore, events_dir: &Path) -> Result<Vec<Event>> {
    let mut events = storage
        .list_json_matching_suffix::<Event>(events_dir, ".json")
        .await?;
    events.sort_by_key(|event| event.id);
    Ok(events)
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

async fn list_binding_metadata(
    storage: &BasicObjectStore,
    bindings_dir: &Path,
) -> Result<Vec<BindingMetadata>> {
    let mut bindings = storage
        .list_json_matching_suffix::<StoredBinding>(bindings_dir, ".json")
        .await?
        .into_iter()
        .map(|binding| binding.metadata)
        .collect::<Vec<_>>();
    bindings.sort_by_key(|metadata| metadata.id);
    Ok(bindings)
}

async fn list_secret_metadata(
    storage: &BasicObjectStore,
    secrets_dir: &Path,
) -> Result<Vec<SecretMetadata>> {
    let mut secrets = storage
        .list_json_matching_suffix::<StoredSecret>(secrets_dir, ".json")
        .await?
        .into_iter()
        .map(|secret| secret.metadata)
        .collect::<Vec<_>>();
    secrets.sort_by_key(|metadata| metadata.id);
    Ok(secrets)
}

fn merge_binding_metadata(scopes: Vec<Vec<BindingMetadata>>) -> Vec<BindingMetadata> {
    let mut effective = HashMap::<String, BindingMetadata>::new();
    for bindings in scopes {
        for binding in bindings {
            effective.insert(binding.name.clone(), binding);
        }
    }
    let mut bindings = effective.into_values().collect::<Vec<_>>();
    bindings.sort_by_key(|metadata| metadata.id);
    bindings
}

fn merge_secret_metadata(scopes: Vec<Vec<SecretMetadata>>) -> Vec<SecretMetadata> {
    let mut effective = HashMap::<String, SecretMetadata>::new();
    for secrets in scopes {
        for secret in secrets {
            effective.insert(secret.name.clone(), secret);
        }
    }
    let mut secrets = effective.into_values().collect::<Vec<_>>();
    secrets.sort_by_key(|metadata| metadata.id);
    secrets
}

fn event_type(data: &EventData) -> String {
    match data {
        EventData::ConversationForked { .. } => "conversation_forked".to_string(),
        EventData::SessionStarted => "session_started".to_string(),
        EventData::SessionEnded => "session_ended".to_string(),
        EventData::TurnStarted => "turn_started".to_string(),
        EventData::TurnEnded => "turn_ended".to_string(),
        EventData::Messages { .. } => "messages".to_string(),
        EventData::ToolRequested { .. } => "tool_requested".to_string(),
        EventData::ToolResult { .. } => "tool_result".to_string(),
        EventData::ArtifactWritten { .. } => "artifact_written".to_string(),
        EventData::SandboxCreated { .. } => "sandbox_created".to_string(),
        EventData::SandboxStarted { .. } => "sandbox_started".to_string(),
        EventData::SandboxStopped { .. } => "sandbox_stopped".to_string(),
        EventData::SandboxSnapshotted { .. } => "sandbox_snapshotted".to_string(),
        EventData::Custom { event_type, .. } => event_type.clone(),
    }
}

fn binding_type(binding: &Binding) -> BindingType {
    match binding {
        Binding::Env { .. } => BindingType::Env,
        Binding::Mcp { .. } => BindingType::Mcp,
        Binding::Llm { .. } => BindingType::Llm,
    }
}

fn binding_name(binding: &Binding) -> &str {
    match binding {
        Binding::Env { name, .. } | Binding::Mcp { name, .. } | Binding::Llm { name, .. } => name,
    }
}

fn secret_type(secret: &Secret) -> SecretType {
    match secret {
        Secret::Key { .. } => SecretType::Key,
        Secret::Oauth { .. } => SecretType::Oauth,
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

fn agent_bindings_dir(harness: &BasicExoHarness, agent_id: AgentId) -> PathBuf {
    harness
        .agents_dir()
        .join(agent_id.to_string())
        .join("bindings")
}

fn agent_secrets_dir(harness: &BasicExoHarness, agent_id: AgentId) -> PathBuf {
    harness
        .agents_dir()
        .join(agent_id.to_string())
        .join("secrets")
}

fn build_secret_cipher(
    choice: SecretBackendChoice,
    keychain_account: String,
) -> Result<SecretCipher> {
    let provider: Arc<dyn SecretKeyProvider> = match choice {
        SecretBackendChoice::AppleKeychain => {
            Arc::new(AppleKeychainSecretKeyProvider::new(keychain_account))
        }
        SecretBackendChoice::File { path } => {
            let path = match path {
                Some(path) => path,
                None => default_master_key_path()?,
            };
            Arc::new(FileBackedSecretKeyProvider::new(path))
        }
        SecretBackendChoice::Static(key) => Arc::new(StaticSecretKeyProvider::new(key)),
    };
    Ok(SecretCipher::new(provider))
}

fn build_sandbox_backend(choice: SandboxBackendChoice) -> Arc<dyn ManagedSandboxBackend> {
    match choice {
        SandboxBackendChoice::AppleContainer => {
            Arc::new(CliContainerSandboxBackend::apple_container())
        }
        SandboxBackendChoice::Docker => Arc::new(CliContainerSandboxBackend::docker()),
        SandboxBackendChoice::LocalProcess => Arc::new(LocalProcessSandboxBackend::new()),
    }
}
