use std::sync::Arc;

use anyhow::anyhow;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

use crate::protocol::{
    ClientMessage, ConversationHandleInfo, Request, Response, ServerMessage, SnapshotScope,
};
use crate::vault::{VaultContext, require_vault};
use crate::{
    AgentHandle, AgentId, AttachSandboxRequest, CancelSandboxProcessRequest,
    CloseSandboxProcessInputRequest, ConversationHandle, ConversationId, CreateSandboxRequest,
    ExoHarness, ForkSandboxRequest, GetSandboxProcessEventsResult, ListConversationsResult,
    RestoreSandboxRequest, Result, SandboxAttachment, SandboxId, SandboxProcessEventQuery,
    SandboxProcessRecord, SandboxProcessStatus, SandboxRecord, SessionId, SnapshotId,
    StartSandboxProcessRequest, StartSandboxRequest, TurnHandle, TurnId, TurnRecord,
    WaitSandboxProcessRequest, WriteSandboxProcessInputRequest,
};
use crate::{ResourceScope, SandboxHandle, SnapshotHandle};

pub struct ExoHarnessServer {
    root: Arc<dyn ExoHarness>,
}

impl ExoHarnessServer {
    pub fn new(root: Arc<dyn ExoHarness>) -> Self {
        Self { root }
    }

    async fn vault_context(&self, scope: ResourceScope) -> Result<Arc<dyn VaultContext>> {
        match scope {
            ResourceScope::Global => Ok(self.root.clone()),
            ResourceScope::Agent { agent_id } => Ok(self.require_agent(&agent_id).await?),
            ResourceScope::Thread {
                agent_id,
                thread_id,
            } => Ok(self.require_conversation(agent_id, thread_id).await?),
        }
    }

    async fn sandbox_context(&self, scope: ResourceScope) -> Result<Arc<dyn SandboxHandle>> {
        match scope {
            ResourceScope::Global => Err(anyhow!("sandboxes require an agent or thread scope")),
            ResourceScope::Agent { agent_id } => Ok(self.require_agent(&agent_id).await?),
            ResourceScope::Thread {
                agent_id,
                thread_id,
            } => Ok(self.require_conversation(agent_id, thread_id).await?),
        }
    }

    async fn snapshot_context(&self, scope: SnapshotScope) -> Result<Arc<dyn SnapshotHandle>> {
        match scope {
            SnapshotScope::Resource { scope } => Ok(self.sandbox_context(scope).await?),
            SnapshotScope::Turn {
                agent_id,
                thread_id,
                session_id,
                turn_id,
            } => Ok(self
                .require_turn(agent_id, thread_id, session_id, turn_id)
                .await?),
        }
    }

    pub async fn handle_request(&self, request: Request) -> Result<Response> {
        match request {
            Request::ListVaults { scope } => Ok(Response::Vaults {
                vaults: self
                    .vault_context(scope)
                    .await?
                    .list_vaults()
                    .await?
                    .into_iter()
                    .map(|v| v.record().clone())
                    .collect(),
            }),
            Request::GetVault { scope, vault_id } => Ok(Response::Vault {
                vault: self
                    .vault_context(scope)
                    .await?
                    .get_vault(&vault_id)
                    .await?
                    .map(|v| v.record().clone()),
            }),
            Request::CreateVault { name } => Ok(Response::Vault {
                vault: Some(self.root.create_vault(&name).await?.record().clone()),
            }),
            Request::DeleteVault { vault_id } => {
                self.root.delete_vault(&vault_id).await?;
                Ok(Response::Bool { value: true })
            }
            Request::VaultListSecrets { scope, vault_id } => Ok(Response::Secrets {
                secrets: require_vault(self.vault_context(scope).await?.as_ref(), &vault_id)
                    .await?
                    .list_secrets()
                    .await?,
            }),
            Request::VaultPutSecret {
                scope,
                vault_id,
                request,
            } => Ok(Response::SecretId {
                secret_id: require_vault(self.vault_context(scope).await?.as_ref(), &vault_id)
                    .await?
                    .put_secret(request)
                    .await?,
            }),
            Request::VaultGetSecret {
                scope,
                vault_id,
                secret_id,
            } => Ok(Response::Secret {
                secret: require_vault(self.vault_context(scope).await?.as_ref(), &vault_id)
                    .await?
                    .get_secret(&secret_id)
                    .await?,
            }),
            Request::VaultUpdateSecret {
                scope,
                vault_id,
                secret_id,
                secret,
                policy,
            } => Ok(Response::SecretMetadata {
                metadata: require_vault(self.vault_context(scope).await?.as_ref(), &vault_id)
                    .await?
                    .update_secret(&secret_id, crate::UpdateSecretRequest { secret, policy })
                    .await?,
            }),
            Request::VaultDeleteSecret {
                scope,
                vault_id,
                secret_id,
            } => {
                require_vault(self.vault_context(scope).await?.as_ref(), &vault_id)
                    .await?
                    .delete_secret(&secret_id)
                    .await?;
                Ok(Response::Bool { value: true })
            }
            Request::VaultResolveSecret {
                scope,
                vault_id,
                secret_id,
                target,
            } => {
                let resolved = require_vault(self.vault_context(scope).await?.as_ref(), &vault_id)
                    .await?
                    .resolve_secret(&secret_id, &target)
                    .await?;
                Ok(Response::ResolvedSecret {
                    revision: resolved.revision,
                    secret: resolved.secret,
                })
            }
            Request::VaultRefreshSecret {
                scope,
                vault_id,
                secret_id,
                target,
                rejected_revision,
            } => {
                let resolved = require_vault(self.vault_context(scope).await?.as_ref(), &vault_id)
                    .await?
                    .refresh_secret(&secret_id, &target, rejected_revision)
                    .await?;
                Ok(Response::ResolvedSecret {
                    revision: resolved.revision,
                    secret: resolved.secret,
                })
            }
            Request::ListAgents => Ok(Response::Agents {
                agents: self
                    .root
                    .list_agents()
                    .await?
                    .into_iter()
                    .map(|agent| agent.record().clone())
                    .collect(),
            }),
            Request::GetAgent { agent_id } => Ok(Response::Agent {
                agent: self
                    .root
                    .get_agent(&agent_id)
                    .await?
                    .map(|agent| agent.record().clone()),
            }),
            Request::NewAgent { request } => {
                let agent = self.root.new_agent(request).await?;
                Ok(Response::Agent {
                    agent: Some(agent.record().clone()),
                })
            }
            Request::DeleteAgent { agent_id } => Ok(Response::Bool {
                value: self.root.delete_agent(&agent_id).await?,
            }),
            Request::ListEnvironments => Ok(Response::Environments {
                environments: self.root.list_environments().await?,
            }),
            Request::PutEnvironment { environment } => {
                self.root.put_environment(environment).await?;
                Ok(Response::Bool { value: true })
            }
            Request::DeleteEnvironment { name } => Ok(Response::Bool {
                value: self.root.delete_environment(&name).await?,
            }),
            Request::ListBindings => Ok(Response::Bindings {
                bindings: self.root.list_bindings().await?,
            }),
            Request::PutBinding { binding } => Ok(Response::BindingId {
                binding_id: self.root.put_binding(binding).await?,
            }),
            Request::GetBinding { binding_id } => Ok(Response::Binding {
                binding: self.root.get_binding(&binding_id).await?,
            }),
            Request::ListConversations { agent_id, request } => {
                let agent = self.require_agent(&agent_id).await?;
                let result = agent.list_conversations(request).await?;
                Ok(Response::Conversations {
                    result: ListConversationsResult {
                        conversations: result
                            .conversations
                            .into_iter()
                            .map(|conversation| ConversationHandleInfo {
                                agent_id,
                                record: conversation.record().clone(),
                            })
                            .collect(),
                        next_cursor: result.next_cursor,
                    },
                })
            }
            Request::GetConversation {
                agent_id,
                conversation_id,
            } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Conversation {
                    conversation: agent.get_conversation(&conversation_id).await?.map(
                        |conversation| ConversationHandleInfo {
                            agent_id,
                            record: conversation.record().clone(),
                        },
                    ),
                })
            }
            Request::NewConversation { agent_id, request } => {
                let agent = self.require_agent(&agent_id).await?;
                let conversation = agent.new_conversation(request).await?;
                Ok(Response::Conversation {
                    conversation: Some(ConversationHandleInfo {
                        agent_id,
                        record: conversation.record().clone(),
                    }),
                })
            }
            Request::DeleteConversation {
                agent_id,
                conversation_id,
            } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Bool {
                    value: agent.delete_conversation(&conversation_id).await?,
                })
            }
            Request::AgentListArtifacts { agent_id } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::ArtifactVersions {
                    artifacts: agent.list_artifacts().await?,
                })
            }
            Request::AgentReadArtifact { agent_id, request } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Artifact {
                    artifact: agent.read_artifact(request).await?,
                })
            }
            Request::AgentWriteArtifact { agent_id, request } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::ArtifactVersion {
                    artifact: agent.write_artifact(request).await?,
                })
            }
            Request::ListSandboxes { scope } => Ok(Response::Sandboxes {
                sandboxes: self.list_sandboxes(scope).await?,
            }),
            Request::CreateSandbox { scope, request } => Ok(Response::SandboxId {
                sandbox_id: self.create_sandbox(scope, request).await?,
            }),
            Request::ForkSandbox { scope, request } => Ok(Response::SandboxId {
                sandbox_id: self.fork_sandbox(scope, request).await?,
            }),
            Request::RestoreSandbox { scope, request } => Ok(Response::SandboxId {
                sandbox_id: self.restore_sandbox(scope, request).await?,
            }),
            Request::TerminateSandbox { scope, sandbox_id } => {
                self.terminate_sandbox(scope, sandbox_id).await?;
                Ok(Response::Unit)
            }
            Request::AttachSandbox { scope, request } => Ok(Response::SandboxId {
                sandbox_id: self.attach_sandbox(scope, request).await?,
            }),
            Request::DetachSandbox { scope, sandbox_id } => Ok(Response::SandboxAttachment {
                attachment: self.detach_sandbox(scope, sandbox_id).await?,
            }),
            Request::SnapshotSandbox { scope, sandbox_id } => Ok(Response::SnapshotId {
                snapshot_id: self.snapshot_sandbox(scope, sandbox_id).await?,
            }),
            Request::StartSandbox { scope, request } => {
                self.start_sandbox(scope, request).await?;
                Ok(Response::Unit)
            }
            Request::StopSandbox { scope, sandbox_id } => {
                self.stop_sandbox(scope, sandbox_id).await?;
                Ok(Response::Unit)
            }
            Request::StartSandboxProcess { scope, request } => Ok(Response::SandboxProcess {
                process: self.start_sandbox_process(scope, request).await?,
            }),
            Request::WriteSandboxProcessInput { scope, request } => {
                self.write_sandbox_process_input(scope, request).await?;
                Ok(Response::Unit)
            }
            Request::CloseSandboxProcessInput { scope, request } => {
                self.close_sandbox_process_input(scope, request).await?;
                Ok(Response::Unit)
            }
            Request::GetSandboxProcessEvents { scope, query } => {
                Ok(Response::SandboxProcessEvents {
                    result: self.get_sandbox_process_events(scope, query).await?,
                })
            }
            Request::WaitSandboxProcess { scope, request } => Ok(Response::SandboxProcessStatus {
                status: self.wait_sandbox_process(scope, request).await?,
            }),
            Request::CancelSandboxProcess { scope, request } => {
                Ok(Response::SandboxProcessStatus {
                    status: self.cancel_sandbox_process(scope, request).await?,
                })
            }
            Request::AgentListBindings { agent_id } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Bindings {
                    bindings: agent.list_bindings().await?,
                })
            }
            Request::AgentPutBinding { agent_id, binding } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::BindingId {
                    binding_id: agent.put_binding(binding).await?,
                })
            }
            Request::AgentGetBinding {
                agent_id,
                binding_id,
            } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Binding {
                    binding: agent.get_binding(&binding_id).await?,
                })
            }
            Request::ConversationUpdateEnvironment {
                agent_id,
                conversation_id,
                environment,
            } => {
                let conversation = self
                    .require_conversation(agent_id, conversation_id)
                    .await?
                    .update_environment(environment)
                    .await?;
                Ok(Response::Conversation {
                    conversation: Some(ConversationHandleInfo {
                        agent_id,
                        record: conversation.record().clone(),
                    }),
                })
            }
            Request::ConversationAttachVaults {
                agent_id,
                conversation_id,
                vaults,
            } => {
                let conversation = self
                    .require_conversation(agent_id, conversation_id)
                    .await?
                    .attach_vaults(vaults)
                    .await?;
                Ok(Response::Conversation {
                    conversation: Some(ConversationHandleInfo {
                        agent_id,
                        record: conversation.record().clone(),
                    }),
                })
            }
            Request::ConversationStartSession {
                agent_id,
                conversation_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::SessionId {
                    session_id: conversation.start_session().await?,
                })
            }
            Request::ConversationEndSession {
                agent_id,
                conversation_id,
                session_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                conversation.end_session(session_id).await?;
                Ok(Response::Unit)
            }
            Request::ConversationBeginTurn {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                let turn = conversation.begin_turn(request).await?;
                Ok(Response::Turn {
                    turn: crate::protocol::TurnHandleInfo {
                        conversation: ConversationHandleInfo {
                            agent_id,
                            record: conversation.record().clone(),
                        },
                        record: turn.record().clone(),
                    },
                })
            }
            Request::ConversationGetEvents {
                agent_id,
                conversation_id,
                query,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Events {
                    result: conversation.get_events(query).await?,
                })
            }
            Request::ConversationGetEvent {
                agent_id,
                conversation_id,
                event_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Event {
                    event: conversation.get_event(event_id).await?,
                })
            }
            Request::ConversationAddEvents {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::AddEvents {
                    result: conversation.add_events(request).await?,
                })
            }
            Request::ConversationFork {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                let forked = conversation.fork(request).await?;
                Ok(Response::Conversation {
                    conversation: Some(ConversationHandleInfo {
                        agent_id,
                        record: forked.record().clone(),
                    }),
                })
            }
            Request::ConversationListArtifacts {
                agent_id,
                conversation_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::ArtifactVersions {
                    artifacts: conversation.list_artifacts().await?,
                })
            }
            Request::ConversationReadArtifact {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Artifact {
                    artifact: conversation.read_artifact(request).await?,
                })
            }
            Request::ConversationWriteArtifact {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::ArtifactVersion {
                    artifact: conversation.write_artifact(request).await?,
                })
            }
            Request::ConversationListBindings {
                agent_id,
                conversation_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Bindings {
                    bindings: conversation.list_bindings().await?,
                })
            }
            Request::ConversationPutBinding {
                agent_id,
                conversation_id,
                binding,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::BindingId {
                    binding_id: conversation.put_binding(binding).await?,
                })
            }
            Request::ConversationGetBinding {
                agent_id,
                conversation_id,
                binding_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Binding {
                    binding: conversation.get_binding(&binding_id).await?,
                })
            }
            Request::TurnAddEvents {
                agent_id,
                conversation_id,
                session_id,
                turn_id,
                data,
            } => {
                let turn = self
                    .require_turn(agent_id, conversation_id, session_id, turn_id)
                    .await?;
                Ok(Response::AddEvents {
                    result: turn.add_events(data).await?,
                })
            }
            Request::TurnWriteArtifact {
                agent_id,
                conversation_id,
                session_id,
                turn_id,
                request,
            } => {
                let turn = self
                    .require_turn(agent_id, conversation_id, session_id, turn_id)
                    .await?;
                Ok(Response::ArtifactVersion {
                    artifact: turn.write_artifact(request).await?,
                })
            }
            Request::TurnFinish {
                agent_id,
                conversation_id,
                session_id,
                turn_id,
            } => {
                let turn = self
                    .require_turn(agent_id, conversation_id, session_id, turn_id)
                    .await?;
                Ok(Response::EventId {
                    event_id: turn.finish().await?,
                })
            }
        }
    }

    pub async fn serve_jsonl<R, W>(&self, reader: R, writer: W) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        let mut writer = BufWriter::new(writer);

        while let Some(line) = lines.next_line().await? {
            let message: ClientMessage = serde_json::from_str(&line)?;
            let ClientMessage::Request { id, request } = message;
            let response = match self.handle_request(request).await {
                Ok(response) => ServerMessage::Response {
                    id,
                    ok: true,
                    response: Some(response),
                    error: None,
                },
                Err(error) => ServerMessage::Response {
                    id,
                    ok: false,
                    response: None,
                    error: Some(error.to_string()),
                },
            };
            let encoded = serde_json::to_vec(&response)?;
            writer.write_all(&encoded).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }

        Ok(())
    }

    async fn create_sandbox(
        &self,
        scope: ResourceScope,
        request: CreateSandboxRequest,
    ) -> Result<SandboxId> {
        self.sandbox_context(scope)
            .await?
            .create_sandbox(request)
            .await
    }

    async fn list_sandboxes(&self, scope: ResourceScope) -> Result<Vec<SandboxRecord>> {
        self.sandbox_context(scope).await?.list_sandboxes().await
    }

    async fn fork_sandbox(
        &self,
        scope: ResourceScope,
        request: ForkSandboxRequest,
    ) -> Result<SandboxId> {
        self.sandbox_context(scope)
            .await?
            .fork_sandbox(request)
            .await
    }

    async fn restore_sandbox(
        &self,
        scope: ResourceScope,
        request: RestoreSandboxRequest,
    ) -> Result<SandboxId> {
        self.sandbox_context(scope)
            .await?
            .restore_sandbox(request)
            .await
    }

    async fn terminate_sandbox(&self, scope: ResourceScope, sandbox_id: SandboxId) -> Result<()> {
        self.sandbox_context(scope)
            .await?
            .terminate_sandbox(sandbox_id)
            .await
    }

    async fn attach_sandbox(
        &self,
        scope: ResourceScope,
        request: AttachSandboxRequest,
    ) -> Result<SandboxId> {
        self.sandbox_context(scope)
            .await?
            .attach_sandbox(request)
            .await
    }

    async fn detach_sandbox(
        &self,
        scope: ResourceScope,
        sandbox_id: SandboxId,
    ) -> Result<SandboxAttachment> {
        self.sandbox_context(scope)
            .await?
            .detach_sandbox(sandbox_id)
            .await
    }

    async fn snapshot_sandbox(
        &self,
        scope: SnapshotScope,
        sandbox_id: SandboxId,
    ) -> Result<SnapshotId> {
        self.snapshot_context(scope)
            .await?
            .snapshot_sandbox(sandbox_id)
            .await
    }

    async fn start_sandbox(
        &self,
        scope: SnapshotScope,
        request: StartSandboxRequest,
    ) -> Result<()> {
        self.snapshot_context(scope)
            .await?
            .start_sandbox(request)
            .await
    }

    async fn stop_sandbox(&self, scope: ResourceScope, sandbox_id: SandboxId) -> Result<()> {
        self.sandbox_context(scope)
            .await?
            .stop_sandbox(sandbox_id)
            .await
    }

    async fn start_sandbox_process(
        &self,
        scope: ResourceScope,
        request: StartSandboxProcessRequest,
    ) -> Result<SandboxProcessRecord> {
        self.sandbox_context(scope)
            .await?
            .start_sandbox_process(request)
            .await
    }

    async fn write_sandbox_process_input(
        &self,
        scope: ResourceScope,
        request: WriteSandboxProcessInputRequest,
    ) -> Result<()> {
        self.sandbox_context(scope)
            .await?
            .write_sandbox_process_input(request)
            .await
    }

    async fn close_sandbox_process_input(
        &self,
        scope: ResourceScope,
        request: CloseSandboxProcessInputRequest,
    ) -> Result<()> {
        self.sandbox_context(scope)
            .await?
            .close_sandbox_process_input(request)
            .await
    }

    async fn get_sandbox_process_events(
        &self,
        scope: ResourceScope,
        query: SandboxProcessEventQuery,
    ) -> Result<GetSandboxProcessEventsResult> {
        self.sandbox_context(scope)
            .await?
            .get_sandbox_process_events(query)
            .await
    }

    async fn wait_sandbox_process(
        &self,
        scope: ResourceScope,
        request: WaitSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        self.sandbox_context(scope)
            .await?
            .wait_sandbox_process(request)
            .await
    }

    async fn cancel_sandbox_process(
        &self,
        scope: ResourceScope,
        request: CancelSandboxProcessRequest,
    ) -> Result<SandboxProcessStatus> {
        self.sandbox_context(scope)
            .await?
            .cancel_sandbox_process(request)
            .await
    }

    async fn require_agent(&self, agent_id: &AgentId) -> Result<Arc<dyn AgentHandle>> {
        self.root
            .get_agent(agent_id)
            .await?
            .ok_or_else(|| anyhow!("agent {agent_id} not found"))
    }

    async fn require_conversation(
        &self,
        agent_id: AgentId,
        conversation_id: ConversationId,
    ) -> Result<Arc<dyn ConversationHandle>> {
        self.require_agent(&agent_id)
            .await?
            .get_conversation(&conversation_id)
            .await?
            .ok_or_else(|| anyhow!("conversation {conversation_id} not found"))
    }

    async fn require_turn(
        &self,
        agent_id: AgentId,
        conversation_id: ConversationId,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<Arc<dyn TurnHandle>> {
        self.require_conversation(agent_id, conversation_id)
            .await?
            .turn_handle(TurnRecord {
                id: turn_id,
                session_id,
            })
            .await
    }
}
