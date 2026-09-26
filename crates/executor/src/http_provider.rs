use std::{
    collections::HashMap,
    ops::Bound,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use exo_managed_agents::AgentBackend;
use exo_managed_agents::http::{RuntimeClient, protocol::*};
use exoharness::protocol::{ConversationHandleInfo, Request, Response};
use exoharness::{
    AgentId, EventId, EventStream, ExoHarness, ExoHttpTransport, HttpExoHarness, ThreadId,
};
use futures::StreamExt;
use url::Url;

use crate::{
    Provider, ProviderTurn,
    harness::{Harness, HarnessCommand},
};

pub struct HttpProvider {
    state: HttpExoHarness,
    transport: Arc<RuntimeTransport>,
    watchers: Mutex<tokio::task::JoinSet<()>>,
}

impl HttpProvider {
    pub fn new(client: RuntimeClient) -> Self {
        Self::with_options(client, None, None)
    }

    pub fn with_options(
        client: RuntimeClient,
        model: Option<String>,
        harness: Option<String>,
    ) -> Self {
        let transport = Arc::new(RuntimeTransport {
            client,
            model,
            harness,
            threads: Arc::default(),
        });
        Self {
            state: HttpExoHarness::from_transport(transport.clone()),
            transport,
            watchers: Mutex::new(tokio::task::JoinSet::new()),
        }
    }

    pub async fn send_stream(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        body: SubmitTurnBody,
    ) -> Result<(exoharness::TurnRecord, crate::ExecutionStreamHandle)> {
        let client = self.transport.client.clone();
        let result = client.submit_turn(agent_id, thread_id, &body).await?;
        self.transport.remember(agent_id, thread_id);
        let record = result.turn.clone();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut watchers = self.watchers.lock().expect("HTTP watchers poisoned");
        while let Some(result) = watchers.try_join_next() {
            if let Err(error) = result {
                tracing::warn!(%error, "previous HTTP turn observer failed");
            }
        }
        watchers.spawn(async move {
            let observed = async {
                let events = client
                    .watch(
                        agent_id,
                        thread_id,
                        &WatchQuery {
                            after: result.thread.latest_event_id,
                        },
                    )
                    .await?;
                let stream = crate::harness_events::turn_stream(events, result.turn);
                futures::pin_mut!(stream);
                loop {
                    let event = tokio::select! {
                        () = sender.closed() => return Ok(()),
                        event = stream.next() => event,
                    };
                    let Some(event) = event else {
                        return Ok(());
                    };
                    if sender.send(event).is_err() {
                        return Ok(());
                    }
                }
            }
            .await;
            if let Err(error) = observed
                && sender.send(Err(error)).is_err()
            {
                tracing::debug!("remote turn observer closed");
            }
        });
        Ok((
            record,
            crate::ExecutionStreamHandle::new(
                tokio_stream::wrappers::UnboundedReceiverStream::new(receiver),
            ),
        ))
    }

    pub async fn cancel(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        turn_id: exoharness::TurnId,
    ) -> Result<()> {
        self.transport
            .client
            .cancel_turn(agent_id, thread_id, turn_id)
            .await?;
        Ok(())
    }
}

impl AgentBackend for HttpProvider {
    fn exoharness(&self) -> Arc<dyn ExoHarness> {
        Arc::new(self.state.clone())
    }
}

#[async_trait]
impl Provider for HttpProvider {
    async fn is_turn_active(
        &self,
        thread: &dyn exoharness::ThreadHandle,
        turn: exoharness::TurnId,
    ) -> Result<bool> {
        let agent = *self
            .transport
            .threads
            .lock()
            .expect("HTTP threads poisoned")
            .get(&thread.record().id)
            .context("resolve the thread through this provider before reconnecting it")?;
        Ok(self
            .transport
            .client
            .turn_status(agent, thread.record().id, turn)
            .await?
            .active)
    }

    async fn approval_response(
        &self,
        agent: AgentId,
        thread: ThreadId,
        turn: exoharness::TurnId,
        body: &ApprovalResponseBody,
    ) -> Result<EventId> {
        Ok(self
            .transport
            .client
            .approval_response(agent, thread, turn, body)
            .await?
            .event_id)
    }

    fn harness(&self) -> &dyn Harness<ProviderTurn> {
        self
    }
}

#[async_trait]
impl Harness<ProviderTurn> for HttpProvider {
    fn name(&self) -> &'static str {
        "http"
    }

    async fn shutdown(&self) -> Result<()> {
        self.watchers
            .lock()
            .expect("HTTP watchers poisoned")
            .abort_all();
        Ok(())
    }

    async fn submit(&self, command: HarnessCommand<ProviderTurn>) -> Result<()> {
        match command {
            HarnessCommand::StartTurn(work) => {
                if work.config_override.is_some() {
                    bail!("HTTP providers do not accept local runtime configuration overrides");
                }
                let result = self
                    .send_stream(
                        work.agent.record().id,
                        work.thread.record().id,
                        SubmitTurnBody {
                            input: Some(OneOrMany::Many(work.request.input)),
                            session_id: work.request.session_id,
                            model: self.transport.model.clone(),
                            harness: self.transport.harness.clone(),
                            ..Default::default()
                        },
                    )
                    .await?;
                if work.receipt.send(result).is_err() {
                    tracing::debug!("HTTP turn receipt receiver disconnected");
                }
                Ok(())
            }
            HarnessCommand::CancelTurn { key } => {
                let agent_id = *self
                    .transport
                    .threads
                    .lock()
                    .expect("HTTP threads poisoned")
                    .get(&key.thread_id)
                    .context("resolve the thread through this provider before cancelling it")?;
                self.cancel(agent_id, key.thread_id, key.turn_id).await
            }
        }
    }
}

#[derive(Clone)]
struct RuntimeTransport {
    client: RuntimeClient,
    model: Option<String>,
    harness: Option<String>,
    threads: Arc<Mutex<HashMap<ThreadId, AgentId>>>,
}

impl RuntimeTransport {
    fn remember(&self, agent_id: AgentId, thread_id: ThreadId) {
        self.threads
            .lock()
            .expect("HTTP threads poisoned")
            .insert(thread_id, agent_id);
    }

    fn info(&self, agent_id: AgentId, record: exoharness::ThreadRecord) -> ConversationHandleInfo {
        self.remember(agent_id, record.id);
        ConversationHandleInfo { agent_id, record }
    }
}

#[async_trait]
impl ExoHttpTransport for RuntimeTransport {
    fn endpoint(&self) -> &Url {
        self.client.endpoint()
    }

    async fn request(&self, request: Request) -> Result<Response> {
        match request {
            Request::ListEnvironments => Ok(Response::Environments {
                environments: self.client.list_environments().await?,
            }),
            Request::PutEnvironment { environment } => Ok(Response::Bool {
                value: self.client.put_environment(&environment).await?,
            }),
            Request::DeleteEnvironment { name } => Ok(Response::Bool {
                value: self.client.delete_environment(&name).await?,
            }),
            Request::ListAgents => Ok(Response::Agents {
                agents: self.client.list_agents(None).await?.agents,
            }),
            Request::GetAgent { agent_id } => Ok(Response::Agent {
                agent: self.client.get_agent(agent_id).await?,
            }),
            Request::NewAgent { request } => Ok(Response::Agent {
                agent: Some(self.client.create_agent(&request).await?),
            }),
            Request::DeleteAgent { agent_id } => Ok(Response::Bool {
                value: self.client.delete_agent(agent_id).await?,
            }),
            Request::AgentListArtifacts { agent_id } => Ok(Response::ArtifactVersions {
                artifacts: self.client.list_agent_artifacts(agent_id).await?,
            }),
            Request::AgentReadArtifact { agent_id, request } => Ok(Response::Artifact {
                artifact: self.client.read_agent_artifact(agent_id, &request).await?,
            }),
            Request::AgentWriteArtifact { agent_id, request } => Ok(Response::ArtifactVersion {
                artifact: self.client.write_agent_artifact(agent_id, &request).await?,
            }),
            Request::CreateVault { name } => Ok(Response::Vault {
                vault: Some(self.client.create_vault(&name).await?),
            }),
            Request::DeleteVault { vault_id } => {
                self.client.delete_vault(vault_id).await?;
                Ok(Response::Unit)
            }
            Request::VaultPutSecret {
                scope,
                vault_id,
                request,
            } => Ok(Response::SecretId {
                secret_id: self.client.put_secret(scope, vault_id, &request).await?,
            }),
            Request::VaultUpdateSecret {
                scope,
                vault_id,
                secret_id,
                secret,
                target,
            } => Ok(Response::SecretMetadata {
                metadata: self
                    .client
                    .update_secret(
                        scope,
                        vault_id,
                        secret_id,
                        &exoharness::UpdateSecretRequest { secret, target },
                    )
                    .await?,
            }),
            Request::VaultDeleteSecret {
                scope,
                vault_id,
                secret_id,
            } => {
                self.client
                    .delete_secret(scope, vault_id, secret_id)
                    .await?;
                Ok(Response::Unit)
            }
            Request::ListVaults { scope } => Ok(Response::Vaults {
                vaults: self.client.list_vaults(scope).await?,
            }),
            Request::GetVault { scope, vault_id } => Ok(Response::Vault {
                vault: self
                    .client
                    .list_vaults(scope)
                    .await?
                    .into_iter()
                    .find(|vault| vault.id == vault_id),
            }),
            Request::VaultListSecrets { scope, vault_id } => Ok(Response::Secrets {
                secrets: self.client.list_secrets(scope, vault_id).await?,
            }),
            Request::ListConversations { agent_id, request } => {
                let result = self
                    .client
                    .list_threads(
                        agent_id,
                        &ThreadsQuery {
                            cursor: request.cursor,
                            limit: request.limit,
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(Response::Conversations {
                    result: exoharness::ListConversationsResult {
                        conversations: result
                            .threads
                            .into_iter()
                            .map(|thread| self.info(agent_id, thread))
                            .collect(),
                        next_cursor: result.next_cursor,
                    },
                })
            }
            Request::GetConversation {
                agent_id,
                conversation_id,
            } => Ok(Response::Conversation {
                conversation: self
                    .client
                    .get_thread(agent_id, conversation_id)
                    .await?
                    .map(|result| self.info(agent_id, result.thread)),
            }),
            Request::NewConversation { agent_id, request } => {
                let result = self
                    .client
                    .create_thread(
                        agent_id,
                        &CreateThreadBody {
                            environment: request.environment,
                            vaults: request.vaults,
                            thread_slug: request.slug,
                            thread_name: request.name,
                            model: self.model.clone(),
                            harness: self.harness.clone(),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(Response::Conversation {
                    conversation: Some(self.info(agent_id, result.thread)),
                })
            }
            Request::DeleteConversation {
                agent_id,
                conversation_id,
            } => {
                let result = self.client.delete_thread(agent_id, conversation_id).await?;
                Ok(Response::Bool {
                    value: result.deleted,
                })
            }
            Request::ConversationUpdateEnvironment {
                agent_id,
                conversation_id,
                environment,
            } => {
                let result = self
                    .client
                    .update_thread_environment(agent_id, conversation_id, &environment)
                    .await?;
                Ok(Response::Conversation {
                    conversation: Some(self.info(agent_id, result.thread)),
                })
            }
            Request::ConversationAttachVaults {
                agent_id,
                conversation_id,
                vaults,
            } => {
                let result = self
                    .client
                    .attach_thread_vaults(
                        agent_id,
                        conversation_id,
                        &AttachThreadVaultsBody { vaults },
                    )
                    .await?;
                Ok(Response::Conversation {
                    conversation: Some(self.info(agent_id, result.thread)),
                })
            }
            Request::ConversationListArtifacts {
                agent_id,
                conversation_id,
            } => Ok(Response::ArtifactVersions {
                artifacts: self
                    .client
                    .list_thread_artifacts(agent_id, conversation_id)
                    .await?,
            }),
            Request::ConversationReadArtifact {
                agent_id,
                conversation_id,
                request,
            } => Ok(Response::Artifact {
                artifact: self
                    .client
                    .read_thread_artifact(agent_id, conversation_id, &request)
                    .await?,
            }),
            Request::ConversationGetEvents {
                agent_id,
                conversation_id,
                query,
            } => {
                let query = query.unwrap_or_default();
                let result = self
                    .client
                    .events(
                        agent_id,
                        conversation_id,
                        &EventsQuery {
                            after: query.cursor,
                            limit: query.limit,
                            direction: query.direction,
                            session_id: query.session_id,
                            turn_id: query.turn_id,
                            event_type: query.types.map(|types| {
                                types
                                    .iter()
                                    .map(|kind| kind.as_str())
                                    .collect::<Vec<_>>()
                                    .join(",")
                            }),
                        },
                    )
                    .await?;
                Ok(Response::Events { result })
            }
            Request::ConversationFork {
                agent_id,
                conversation_id,
                request,
            } => {
                if request.up_to_inclusive.is_some() || request.slug.is_some() {
                    bail!("runtime HTTP fork supports a name but not a cursor or slug");
                }
                let result = self
                    .client
                    .fork_thread(
                        agent_id,
                        conversation_id,
                        &ForkThreadBody {
                            thread_name: request.name,
                        },
                    )
                    .await?;
                Ok(Response::Conversation {
                    conversation: Some(self.info(agent_id, result.thread)),
                })
            }
            request => bail!("runtime HTTP does not expose {}", request.kind()),
        }
    }

    async fn watch_events(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        after_exclusive: Bound<EventId>,
    ) -> Result<EventStream> {
        let after = match after_exclusive {
            Bound::Unbounded => None,
            Bound::Excluded(id) => Some(id),
            Bound::Included(_) => bail!("runtime HTTP event cursors are exclusive"),
        };
        self.client
            .watch(agent_id, thread_id, &WatchQuery { after })
            .await
    }
}
