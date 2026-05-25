use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::anyhow;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

use crate::protocol::{
    ClientMessage, ConversationHandleInfo, HandleId, Request, Response, ServerMessage,
    TurnHandleInfo,
};
use crate::{
    AgentHandle, AgentId, ConversationHandle, ConversationId, ExoHarness, Result, TurnHandle,
};

pub struct ExoHarnessServer {
    root: Arc<dyn ExoHarness>,
    turns: RwLock<HashMap<HandleId, RegisteredTurn>>,
    next_handle_id: AtomicU64,
}

struct RegisteredTurn {
    conversation: ConversationHandleInfo,
    turn: Arc<dyn TurnHandle>,
}

impl ExoHarnessServer {
    pub fn new(root: Arc<dyn ExoHarness>) -> Self {
        Self {
            root,
            turns: RwLock::new(HashMap::new()),
            next_handle_id: AtomicU64::new(1),
        }
    }

    pub fn register_turn(
        &self,
        agent_id: AgentId,
        conversation_record: crate::ConversationRecord,
        turn: Arc<dyn TurnHandle>,
    ) -> TurnHandleInfo {
        let handle_id = self.next_handle_id.fetch_add(1, Ordering::Relaxed);
        let conversation = ConversationHandleInfo {
            agent_id,
            record: conversation_record,
        };
        self.turns.write().expect("turn registry poisoned").insert(
            handle_id,
            RegisteredTurn {
                conversation: conversation.clone(),
                turn: Arc::clone(&turn),
            },
        );
        TurnHandleInfo {
            handle_id,
            conversation,
            record: turn.record().clone(),
        }
    }

    pub async fn handle_request(&self, request: Request) -> Result<Response> {
        match request {
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
            Request::ListBindings => Ok(Response::Bindings {
                bindings: self.root.list_bindings().await?,
            }),
            Request::PutBinding { binding } => Ok(Response::BindingId {
                binding_id: self.root.put_binding(binding).await?,
            }),
            Request::GetBinding { binding_id } => Ok(Response::Binding {
                binding: self.root.get_binding(&binding_id).await?,
            }),
            Request::ListSecrets => Ok(Response::Secrets {
                secrets: self.root.list_secrets().await?,
            }),
            Request::PutSecret { request } => Ok(Response::SecretId {
                secret_id: self.root.put_secret(request).await?,
            }),
            Request::GetSecret { secret_id } => Ok(Response::Secret {
                secret: self.root.get_secret(&secret_id).await?,
            }),
            Request::ListConversations { agent_id } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Conversations {
                    conversations: agent
                        .list_conversations()
                        .await?
                        .into_iter()
                        .map(|conversation| ConversationHandleInfo {
                            agent_id,
                            record: conversation.record().clone(),
                        })
                        .collect(),
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
            Request::AgentListSecrets { agent_id } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Secrets {
                    secrets: agent.list_secrets().await?,
                })
            }
            Request::AgentPutSecret { agent_id, request } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::SecretId {
                    secret_id: agent.put_secret(request).await?,
                })
            }
            Request::AgentGetSecret {
                agent_id,
                secret_id,
            } => {
                let agent = self.require_agent(&agent_id).await?;
                Ok(Response::Secret {
                    secret: agent.get_secret(&secret_id).await?,
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
            Request::ConversationListSecrets {
                agent_id,
                conversation_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Secrets {
                    secrets: conversation.list_secrets().await?,
                })
            }
            Request::ConversationPutSecret {
                agent_id,
                conversation_id,
                request,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::SecretId {
                    secret_id: conversation.put_secret(request).await?,
                })
            }
            Request::ConversationGetSecret {
                agent_id,
                conversation_id,
                secret_id,
            } => {
                let conversation = self.require_conversation(agent_id, conversation_id).await?;
                Ok(Response::Secret {
                    secret: conversation.get_secret(&secret_id).await?,
                })
            }
            Request::TurnAddEvents { handle_id, data } => {
                let turn = self.require_turn(handle_id)?;
                Ok(Response::AddEvents {
                    result: turn.turn.add_events(data).await?,
                })
            }
            Request::TurnWriteArtifact { handle_id, request } => {
                let turn = self.require_turn(handle_id)?;
                Ok(Response::ArtifactVersion {
                    artifact: turn.turn.write_artifact(request).await?,
                })
            }
            Request::TurnFinish { handle_id } => {
                let turn = self
                    .turns
                    .write()
                    .expect("turn registry poisoned")
                    .remove(&handle_id)
                    .ok_or_else(|| anyhow!("turn handle {handle_id} not found"))?;
                Ok(Response::EventId {
                    event_id: turn.turn.finish().await?,
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

    fn require_turn(&self, handle_id: HandleId) -> Result<RegisteredTurn> {
        let turns = self.turns.read().expect("turn registry poisoned");
        let turn = turns
            .get(&handle_id)
            .ok_or_else(|| anyhow!("turn handle {handle_id} not found"))?;
        Ok(RegisteredTurn {
            conversation: turn.conversation.clone(),
            turn: Arc::clone(&turn.turn),
        })
    }
}
