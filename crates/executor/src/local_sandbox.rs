use std::collections::HashMap;
use std::ops::Bound;
use std::sync::Arc;

use async_trait::async_trait;
use exoharness::{
    AddEventsRequest, AddEventsResult, AgentHandle, AgentId, Artifact, ArtifactVersion, Binding,
    BindingId, BindingRecord, CancelSandboxProcessRequest, CloseSandboxProcessInputRequest,
    ConversationHandle, ConversationId, CreateSandboxRequest, Event, EventData, EventId,
    EventStream, ExoHarness, ForkConversationRequest, GetEventsResult, NewAgentRequest,
    NewConversationRequest, PutSecretRequest, ReadArtifactRequest, Result, RunInSandboxRequest,
    SandboxId, SandboxProcess, SandboxProcessEventQuery, SandboxProcessRecord,
    SandboxProcessStatus, Secret, SecretId, SecretMetadata, SnapshotId, StartSandboxProcessRequest,
    StartSandboxRequest, TurnHandle, TurnRecord, Uuid7, WaitSandboxProcessRequest,
    WriteArtifactRequest, WriteSandboxProcessInputRequest,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const LOCAL_SANDBOX_AGENT_SLUG: &str = "__exo_local_sandbox";
const LOCAL_SANDBOX_MAP_EVENT: &str = "local_sandbox_mapped";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalSandboxMapEvent {
    remote_sandbox_id: SandboxId,
    local_sandbox_id: SandboxId,
}

pub struct LocalSandboxExoHarness {
    state: Arc<LocalSandboxState>,
}

struct LocalSandboxState {
    remote: Arc<dyn ExoHarness>,
    local: Arc<dyn ExoHarness>,
    conversations: Mutex<HashMap<ConversationId, Arc<dyn ConversationHandle>>>,
    conversation_init: Mutex<()>,
    sandboxes: Mutex<HashMap<SandboxId, SandboxId>>,
    force_local: bool,
}

impl LocalSandboxExoHarness {
    pub fn new(remote: Arc<dyn ExoHarness>, local: Arc<dyn ExoHarness>) -> Self {
        Self::new_with_force_local(remote, local, true)
    }

    pub fn new_with_force_local(
        remote: Arc<dyn ExoHarness>,
        local: Arc<dyn ExoHarness>,
        force_local: bool,
    ) -> Self {
        Self {
            state: Arc::new(LocalSandboxState {
                remote,
                local,
                conversations: Mutex::new(HashMap::new()),
                conversation_init: Mutex::new(()),
                sandboxes: Mutex::new(HashMap::new()),
                force_local,
            }),
        }
    }
}

#[async_trait]
impl ExoHarness for LocalSandboxExoHarness {
    async fn list_agents(&self) -> Result<Vec<Arc<dyn AgentHandle>>> {
        Ok(self
            .state
            .remote
            .list_agents()
            .await?
            .into_iter()
            .map(|remote| wrap_agent(Arc::clone(&self.state), remote))
            .collect())
    }

    async fn get_agent(&self, id: &AgentId) -> Result<Option<Arc<dyn AgentHandle>>> {
        Ok(self
            .state
            .remote
            .get_agent(id)
            .await?
            .map(|remote| wrap_agent(Arc::clone(&self.state), remote)))
    }

    async fn new_agent(&self, request: NewAgentRequest) -> Result<Arc<dyn AgentHandle>> {
        let remote = self.state.remote.new_agent(request).await?;
        Ok(wrap_agent(Arc::clone(&self.state), remote))
    }

    async fn delete_agent(&self, id: &AgentId) -> Result<bool> {
        self.state.remote.delete_agent(id).await
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        self.state.remote.list_bindings().await
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        self.state.remote.put_binding(binding).await
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        self.state.remote.get_binding(id).await
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        self.state.remote.list_secrets().await
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        self.state.remote.put_secret(request).await
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        self.state.remote.get_secret(id).await
    }
}

struct LocalSandboxAgent {
    state: Arc<LocalSandboxState>,
    remote: Arc<dyn AgentHandle>,
}

fn wrap_agent(state: Arc<LocalSandboxState>, remote: Arc<dyn AgentHandle>) -> Arc<dyn AgentHandle> {
    Arc::new(LocalSandboxAgent { state, remote })
}

#[async_trait]
impl AgentHandle for LocalSandboxAgent {
    fn record(&self) -> &exoharness::AgentRecord {
        self.remote.record()
    }

    async fn list_conversations(&self) -> Result<Vec<Arc<dyn ConversationHandle>>> {
        Ok(self
            .remote
            .list_conversations()
            .await?
            .into_iter()
            .map(|remote| wrap_conversation(Arc::clone(&self.state), remote))
            .collect())
    }

    async fn get_conversation(
        &self,
        id: &ConversationId,
    ) -> Result<Option<Arc<dyn ConversationHandle>>> {
        Ok(self
            .remote
            .get_conversation(id)
            .await?
            .map(|remote| wrap_conversation(Arc::clone(&self.state), remote)))
    }

    async fn new_conversation(
        &self,
        request: NewConversationRequest,
    ) -> Result<Arc<dyn ConversationHandle>> {
        let remote = self.remote.new_conversation(request).await?;
        Ok(wrap_conversation(Arc::clone(&self.state), remote))
    }

    async fn delete_conversation(&self, id: &ConversationId) -> Result<bool> {
        self.remote.delete_conversation(id).await
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        self.remote.list_bindings().await
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        self.remote.put_binding(binding).await
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        self.remote.get_binding(id).await
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        self.remote.list_secrets().await
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        self.remote.put_secret(request).await
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        self.remote.get_secret(id).await
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        self.remote.write_artifact(request).await
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        self.remote.read_artifact(request).await
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        self.remote.list_artifacts().await
    }
}

struct LocalSandboxConversation {
    state: Arc<LocalSandboxState>,
    remote: Arc<dyn ConversationHandle>,
}

fn wrap_conversation(
    state: Arc<LocalSandboxState>,
    remote: Arc<dyn ConversationHandle>,
) -> Arc<dyn ConversationHandle> {
    Arc::new(LocalSandboxConversation { state, remote })
}

impl LocalSandboxConversation {
    async fn local_conversation(&self) -> Result<Arc<dyn ConversationHandle>> {
        {
            let conversations = self.state.conversations.lock().await;
            if let Some(conversation) = conversations.get(&self.remote.record().id) {
                return Ok(Arc::clone(conversation));
            }
        }

        let _init_guard = self.state.conversation_init.lock().await;
        {
            let conversations = self.state.conversations.lock().await;
            if let Some(conversation) = conversations.get(&self.remote.record().id) {
                return Ok(Arc::clone(conversation));
            }
        }

        let local_agent = match self
            .state
            .local
            .list_agents()
            .await?
            .into_iter()
            .find(|agent| agent.record().slug == LOCAL_SANDBOX_AGENT_SLUG)
        {
            Some(agent) => agent,
            None => {
                self.state
                    .local
                    .new_agent(NewAgentRequest {
                        slug: LOCAL_SANDBOX_AGENT_SLUG.to_string(),
                        name: "Local sandbox".to_string(),
                    })
                    .await?
            }
        };

        let slug = format!("remote-{}", self.remote.record().id);
        let local_conversation = match local_agent
            .list_conversations()
            .await?
            .into_iter()
            .find(|conversation| conversation.record().slug == slug)
        {
            Some(conversation) => conversation,
            None => {
                local_agent
                    .new_conversation(NewConversationRequest {
                        slug: Some(slug),
                        name: Some(format!("Local sandbox for {}", self.remote.record().slug)),
                    })
                    .await?
            }
        };

        let mut conversations = self.state.conversations.lock().await;
        conversations.insert(self.remote.record().id, Arc::clone(&local_conversation));
        Ok(local_conversation)
    }

    fn wants_local_sandbox(&self, request: &CreateSandboxRequest) -> bool {
        self.state.force_local || request.provider.is_local()
    }

    async fn local_sandbox_id(&self, sandbox_id: &SandboxId) -> Result<Option<SandboxId>> {
        if let Some(local_id) = self.state.sandboxes.lock().await.get(sandbox_id).cloned() {
            return Ok(Some(local_id));
        }

        let local_conversation = self.local_conversation().await?;
        let events = local_conversation
            .get_events(Some(exoharness::EventQuery {
                cursor: None,
                direction: Some(exoharness::EventQueryDirection::Desc),
                limit: None,
                session_id: None,
                turn_id: None,
                types: Some(vec![LOCAL_SANDBOX_MAP_EVENT.to_string()]),
            }))
            .await?
            .events;

        for event in events {
            let EventData::Custom {
                event_type,
                payload,
            } = event.data
            else {
                continue;
            };
            if event_type != LOCAL_SANDBOX_MAP_EVENT {
                continue;
            }
            let mapping: LocalSandboxMapEvent = serde_json::from_value(payload)?;
            self.state.sandboxes.lock().await.insert(
                mapping.remote_sandbox_id.clone(),
                mapping.local_sandbox_id.clone(),
            );
            if mapping.remote_sandbox_id == *sandbox_id {
                return Ok(Some(mapping.local_sandbox_id));
            }
        }

        Ok(None)
    }

    async fn map_local_sandbox(&self, remote_id: SandboxId, local_id: SandboxId) -> Result<()> {
        self.state
            .sandboxes
            .lock()
            .await
            .insert(remote_id.clone(), local_id.clone());
        self.local_conversation()
            .await?
            .add_events(AddEventsRequest {
                session_id: None,
                turn_id: None,
                expected_head: None,
                data: vec![EventData::Custom {
                    event_type: LOCAL_SANDBOX_MAP_EVENT.to_string(),
                    payload: serde_json::to_value(LocalSandboxMapEvent {
                        remote_sandbox_id: remote_id,
                        local_sandbox_id: local_id,
                    })?,
                }],
            })
            .await?;
        Ok(())
    }

    async fn append_remote_sandbox_events(&self, data: Vec<EventData>) -> Result<()> {
        self.remote
            .add_events(AddEventsRequest {
                session_id: None,
                turn_id: None,
                expected_head: None,
                data,
            })
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ConversationHandle for LocalSandboxConversation {
    fn record(&self) -> &exoharness::ConversationRecord {
        self.remote.record()
    }

    async fn start_session(&self) -> Result<exoharness::SessionId> {
        self.remote.start_session().await
    }

    async fn end_session(&self, id: exoharness::SessionId) -> Result<()> {
        self.remote.end_session(id).await
    }

    async fn begin_turn(
        &self,
        request: exoharness::BeginTurnRequest,
    ) -> Result<Arc<dyn TurnHandle>> {
        self.remote.begin_turn(request).await
    }

    async fn turn_handle(&self, record: TurnRecord) -> Result<Arc<dyn TurnHandle>> {
        self.remote.turn_handle(record).await
    }

    async fn get_events(&self, query: Option<exoharness::EventQuery>) -> Result<GetEventsResult> {
        self.remote.get_events(query).await
    }

    async fn watch_events(&self, after_exclusive: Bound<EventId>) -> Result<EventStream> {
        self.remote.watch_events(after_exclusive).await
    }

    async fn get_event(&self, id: EventId) -> Result<Option<Event>> {
        self.remote.get_event(id).await
    }

    async fn add_events(&self, request: AddEventsRequest) -> Result<AddEventsResult> {
        self.remote.add_events(request).await
    }

    async fn fork(&self, request: ForkConversationRequest) -> Result<Arc<dyn ConversationHandle>> {
        let remote = self.remote.fork(request).await?;
        Ok(wrap_conversation(Arc::clone(&self.state), remote))
    }

    async fn write_artifact(&self, request: WriteArtifactRequest) -> Result<ArtifactVersion> {
        self.remote.write_artifact(request).await
    }

    async fn read_artifact(&self, request: ReadArtifactRequest) -> Result<Option<Artifact>> {
        self.remote.read_artifact(request).await
    }

    async fn list_artifacts(&self) -> Result<Vec<ArtifactVersion>> {
        self.remote.list_artifacts().await
    }

    async fn create_sandbox(&self, request: CreateSandboxRequest) -> Result<SandboxId> {
        if !self.wants_local_sandbox(&request) {
            return self.remote.create_sandbox(request).await;
        }

        let remote_id = format!("sandbox-{}", Uuid7::now());
        let local_id = self
            .local_conversation()
            .await?
            .create_sandbox(request.clone())
            .await?;
        self.map_local_sandbox(remote_id.clone(), local_id).await?;
        self.append_remote_sandbox_events(vec![
            EventData::SandboxCreated {
                sandbox_id: remote_id.clone(),
                provider: request.provider,
                image: request.image,
                default_workdir: request.default_workdir.unwrap_or_default(),
                file_system_mounts: request.file_system_mounts.unwrap_or_default(),
                enable_networking: request.enable_networking.unwrap_or(true),
                idle_seconds: request.idle_seconds.unwrap_or(60),
            },
            EventData::SandboxStarted {
                sandbox_id: remote_id.clone(),
                snapshot_id: None,
            },
        ])
        .await?;
        Ok(remote_id)
    }

    async fn snapshot_sandbox(&self, id: SandboxId) -> Result<SnapshotId> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.snapshot_sandbox(id).await;
        };
        let snapshot_id = self
            .local_conversation()
            .await?
            .snapshot_sandbox(local_id)
            .await?;
        self.append_remote_sandbox_events(vec![EventData::SandboxSnapshotted {
            sandbox_id: id,
            snapshot_id,
        }])
        .await?;
        Ok(snapshot_id)
    }

    async fn start_sandbox(&self, request: StartSandboxRequest) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&request.id).await? else {
            return self.remote.start_sandbox(request).await;
        };
        let remote_id = request.id;
        self.local_conversation()
            .await?
            .start_sandbox(StartSandboxRequest {
                id: local_id,
                snapshot_id: request.snapshot_id,
                idle_seconds: request.idle_seconds,
            })
            .await?;
        self.append_remote_sandbox_events(vec![EventData::SandboxStarted {
            sandbox_id: remote_id,
            snapshot_id: Some(request.snapshot_id),
        }])
        .await
    }

    async fn stop_sandbox(&self, id: SandboxId) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&id).await? else {
            return self.remote.stop_sandbox(id).await;
        };
        self.local_conversation()
            .await?
            .stop_sandbox(local_id)
            .await?;
        self.append_remote_sandbox_events(vec![EventData::SandboxStopped { sandbox_id: id }])
            .await
    }

    async fn start_sandbox_process(
        &self,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.start_sandbox_process(request).await;
        };
        let remote_id = request.sandbox_id.clone();
        let mut process = self
            .local_conversation()
            .await?
            .start_sandbox_process(StartSandboxProcessRequest {
                sandbox_id: local_id,
                ..request
            })
            .await?;
        process.sandbox_id = remote_id;
        Ok(process)
    }

    async fn write_sandbox_process_input(
        &self,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.write_sandbox_process_input(request).await;
        };
        self.local_conversation()
            .await?
            .write_sandbox_process_input(WriteSandboxProcessInputRequest {
                sandbox_id: local_id,
                ..request
            })
            .await
    }

    async fn close_sandbox_process_input(
        &self,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.close_sandbox_process_input(request).await;
        };
        self.local_conversation()
            .await?
            .close_sandbox_process_input(CloseSandboxProcessInputRequest {
                sandbox_id: local_id,
                ..request
            })
            .await
    }

    async fn get_sandbox_process_events(
        &self,
        query: SandboxProcessEventQuery,
    ) -> Result<exoharness::GetSandboxProcessEventsResult> {
        let Some(local_id) = self.local_sandbox_id(&query.sandbox_id).await? else {
            return self.remote.get_sandbox_process_events(query).await;
        };
        self.local_conversation()
            .await?
            .get_sandbox_process_events(SandboxProcessEventQuery {
                sandbox_id: local_id,
                ..query
            })
            .await
    }

    async fn wait_sandbox_process(
        &self,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.wait_sandbox_process(request).await;
        };
        self.local_conversation()
            .await?
            .wait_sandbox_process(WaitSandboxProcessRequest {
                sandbox_id: local_id,
                ..request
            })
            .await
    }

    async fn cancel_sandbox_process(
        &self,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        let Some(local_id) = self.local_sandbox_id(&request.sandbox_id).await? else {
            return self.remote.cancel_sandbox_process(request).await;
        };
        self.local_conversation()
            .await?
            .cancel_sandbox_process(CancelSandboxProcessRequest {
                sandbox_id: local_id,
                ..request
            })
            .await
    }

    async fn run_in_sandbox(
        &self,
        request: RunInSandboxRequest,
    ) -> Result<Box<dyn SandboxProcess>> {
        let Some(local_id) = self.local_sandbox_id(&request.id).await? else {
            return self.remote.run_in_sandbox(request).await;
        };
        self.local_conversation()
            .await?
            .run_in_sandbox(RunInSandboxRequest {
                id: local_id,
                ..request
            })
            .await
    }

    async fn list_bindings(&self) -> Result<Vec<BindingRecord>> {
        self.remote.list_bindings().await
    }

    async fn put_binding(&self, binding: Binding) -> Result<BindingId> {
        self.remote.put_binding(binding).await
    }

    async fn get_binding(&self, id: &BindingId) -> Result<Option<Binding>> {
        self.remote.get_binding(id).await
    }

    async fn list_secrets(&self) -> Result<Vec<SecretMetadata>> {
        self.remote.list_secrets().await
    }

    async fn put_secret(&self, request: PutSecretRequest) -> Result<SecretId> {
        self.remote.put_secret(request).await
    }

    async fn get_secret(&self, id: &SecretId) -> Result<Option<Secret>> {
        self.remote.get_secret(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exoharness::{
        BasicExoHarness, BasicExoHarnessConfig, EventQuery, EventQueryDirection,
        SandboxBackendChoice, SandboxProvider, SecretBackendChoice,
    };
    use tempfile::TempDir;

    fn test_config(root: impl Into<std::path::PathBuf>) -> BasicExoHarnessConfig {
        BasicExoHarnessConfig {
            root: root.into(),
            secret_backend: SecretBackendChoice::Static([7u8; 32]),
            sandbox_backend: SandboxBackendChoice::LocalProcess,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_sandbox_creation_only_records_remote_events() {
        let tempdir = TempDir::new().expect("tempdir should exist");
        let remote = Arc::new(
            BasicExoHarness::new(test_config(tempdir.path().join("remote")))
                .await
                .expect("remote harness should initialize"),
        );
        let local = Arc::new(
            BasicExoHarness::new(test_config(tempdir.path().join("local")))
                .await
                .expect("local harness should initialize"),
        );
        let remote_harness: Arc<dyn ExoHarness> = remote.clone();
        let local_harness: Arc<dyn ExoHarness> = local;
        let wrapper = LocalSandboxExoHarness::new(remote_harness, local_harness);

        let agent = wrapper
            .new_agent(NewAgentRequest {
                slug: "demo".to_string(),
                name: "Demo".to_string(),
            })
            .await
            .expect("agent should be created");
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("session".to_string()),
                name: Some("Session".to_string()),
            })
            .await
            .expect("conversation should be created");
        let sandbox_id = conversation
            .create_sandbox(CreateSandboxRequest {
                provider: SandboxProvider::Docker,
                image: "local-image".to_string(),
                default_workdir: Some("/workspace".to_string()),
                file_system_mounts: Some(Vec::new()),
                enable_networking: Some(false),
                idle_seconds: Some(120),
            })
            .await
            .expect("sandbox should be created");

        let remote_events = conversation
            .get_events(Some(EventQuery {
                cursor: None,
                direction: Some(EventQueryDirection::Asc),
                limit: None,
                session_id: None,
                turn_id: None,
                types: Some(vec![
                    "sandbox_created".to_string(),
                    "sandbox_started".to_string(),
                ]),
            }))
            .await
            .expect("remote events should load")
            .events;
        assert_eq!(remote_events.len(), 2);

        let remote_agent = remote
            .get_agent(&agent.record().id)
            .await
            .expect("remote get agent should succeed")
            .expect("remote agent should exist");
        let remote_conversation = remote_agent
            .get_conversation(&conversation.record().id)
            .await
            .expect("remote get conversation should succeed")
            .expect("remote conversation should exist");
        let remote_process = remote_conversation
            .run_in_sandbox(RunInSandboxRequest {
                id: sandbox_id,
                command: vec!["true".to_string()],
                env: Default::default(),
            })
            .await;
        assert!(remote_process.is_err());
    }
}
