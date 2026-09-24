use anyhow::{Result, bail};
use exoharness::{AgentId, EventStream, GetEventsResult, HttpClient, ThreadId, TurnId};
use reqwest::Method;
use serde::Serialize;
use url::Url;

use super::{protocol::*, sse};

#[derive(Clone)]
pub struct RuntimeClient {
    http: HttpClient,
    temporary_state: bool,
}

impl RuntimeClient {
    pub fn new(endpoint: &str) -> Result<Self> {
        let mut endpoint = Url::parse(endpoint)?;
        let path = format!("{}/", endpoint.path().trim_end_matches('/'));
        endpoint.set_path(&path);
        Ok(Self {
            http: HttpClient::new(endpoint)?,
            temporary_state: false,
        })
    }

    pub fn with_bearer_token(mut self, token: String) -> Self {
        self.http = self.http.with_bearer_token(token);
        self
    }

    pub fn with_context(
        mut self,
        context: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        self.http = self.http.with_context(context)?;
        Ok(self)
    }

    pub fn with_token_provider(
        mut self,
        provider: std::sync::Arc<dyn exoharness::AccessTokenProvider>,
    ) -> Self {
        self.http = self.http.with_token_provider(provider);
        self
    }

    pub fn with_temporary_state(mut self) -> Self {
        self.temporary_state = true;
        self
    }

    pub fn endpoint(&self) -> &Url {
        self.http.endpoint()
    }

    pub fn identity_url(&self) -> Result<Url> {
        Ok(self
            .http
            .request(Method::GET, "identity")?
            .build()?
            .url()
            .clone())
    }

    pub async fn identity(&self) -> Result<ProviderIdentity> {
        self.http
            .json(
                self.http
                    .request(Method::GET, "identity")?
                    .timeout(std::time::Duration::from_secs(30)),
            )
            .await
    }

    pub async fn create_agent(
        &self,
        request: &exoharness::NewAgentRequest,
    ) -> Result<exoharness::AgentRecord> {
        self.http
            .json(
                self.http
                    .request(
                        Method::POST,
                        if self.temporary_state {
                            "agent/temporary"
                        } else {
                            "agent"
                        },
                    )?
                    .json(request),
            )
            .await
    }

    pub async fn get_agent(&self, id: AgentId) -> Result<Option<exoharness::AgentRecord>> {
        self.http
            .json(self.http.request(Method::GET, &format!("agent/{id}"))?)
            .await
    }

    pub async fn delete_agent(&self, id: AgentId) -> Result<bool> {
        self.http
            .json(self.http.request(Method::DELETE, &format!("agent/{id}"))?)
            .await
    }

    pub async fn list_agent_artifacts(
        &self,
        id: AgentId,
    ) -> Result<Vec<exoharness::ArtifactVersion>> {
        self.http
            .json(
                self.http
                    .request(Method::GET, &format!("agent/{id}/artifact"))?,
            )
            .await
    }

    pub async fn read_agent_artifact(
        &self,
        id: AgentId,
        request: &exoharness::ReadArtifactRequest,
    ) -> Result<Option<exoharness::Artifact>> {
        self.http
            .json(
                self.http
                    .request(Method::GET, &format!("agent/{id}/artifact/read"))?
                    .query(request),
            )
            .await
    }

    pub async fn write_agent_artifact(
        &self,
        id: AgentId,
        request: &exoharness::WriteArtifactRequest,
    ) -> Result<exoharness::ArtifactVersion> {
        self.http
            .json(
                self.http
                    .request(Method::POST, &format!("agent/{id}/artifact"))?
                    .json(request),
            )
            .await
    }

    pub async fn list_vaults(
        &self,
        scope: exoharness::ResourceScope,
    ) -> Result<Vec<exoharness::vault::VaultRecord>> {
        self.http
            .json(
                self.http
                    .request(Method::GET, &format!("{}vault", scope_path(scope)))?,
            )
            .await
    }

    pub async fn list_secrets(
        &self,
        scope: exoharness::ResourceScope,
        id: exoharness::vault::VaultId,
    ) -> Result<Vec<exoharness::SecretMetadata>> {
        self.http
            .json(self.http.request(
                Method::GET,
                &format!("{}vault/{id}/secret", scope_path(scope)),
            )?)
            .await
    }

    pub async fn list_agents(&self, slug: Option<String>) -> Result<ListAgentsResult> {
        self.http
            .json(
                self.http
                    .request(Method::GET, "agent")?
                    .query(&AgentsQuery { slug }),
            )
            .await
    }

    pub async fn get_thread(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
    ) -> Result<Option<ThreadResult>> {
        match self
            .http
            .json(
                self.http
                    .request(Method::GET, &thread_path(agent_id, thread_id))?,
            )
            .await
        {
            Ok(thread) => Ok(Some(thread)),
            Err(error)
                if error
                    .downcast_ref::<exoharness::HttpResponseError>()
                    .is_some_and(|error| error.status == reqwest::StatusCode::NOT_FOUND) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn list_threads(
        &self,
        agent_id: AgentId,
        query: &ThreadsQuery,
    ) -> Result<ListThreadsResult> {
        self.http
            .json(
                self.http
                    .request(Method::GET, &format!("agent/{agent_id}/thread"))?
                    .query(query),
            )
            .await
    }

    pub async fn list_thread_artifacts(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
    ) -> Result<Vec<exoharness::ArtifactVersion>> {
        self.http
            .json(self.http.request(
                Method::GET,
                &format!("{}/artifact", thread_path(agent_id, thread_id)),
            )?)
            .await
    }

    pub async fn read_thread_artifact(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        request: &exoharness::ReadArtifactRequest,
    ) -> Result<Option<exoharness::Artifact>> {
        self.http
            .json(
                self.http
                    .request(
                        Method::GET,
                        &format!("{}/artifact/read", thread_path(agent_id, thread_id)),
                    )?
                    .query(request),
            )
            .await
    }

    pub async fn create_thread(
        &self,
        agent_id: AgentId,
        body: &CreateThreadBody,
    ) -> Result<CreateThreadResult> {
        self.http
            .json(
                self.http
                    .request(Method::POST, &format!("agent/{agent_id}/thread"))?
                    .json(body),
            )
            .await
    }

    pub async fn delete_thread(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
    ) -> Result<DeleteThreadResult> {
        self.http
            .json(
                self.http
                    .request(Method::DELETE, &thread_path(agent_id, thread_id))?,
            )
            .await
    }

    pub async fn fork_thread(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        body: &ForkThreadBody,
    ) -> Result<ThreadResult> {
        self.http
            .json(
                self.http
                    .request(
                        Method::POST,
                        &format!("{}/fork", thread_path(agent_id, thread_id)),
                    )?
                    .json(body),
            )
            .await
    }

    pub async fn submit_turn<P: Serialize>(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        body: &SubmitTurnBody<P>,
    ) -> Result<SubmitTurnResult> {
        self.http
            .json(
                self.http
                    .request(
                        Method::POST,
                        &format!("{}/turn", thread_path(agent_id, thread_id)),
                    )?
                    .json(body),
            )
            .await
    }

    pub async fn cancel_turn(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        turn_id: TurnId,
    ) -> Result<CancelTurnResult> {
        self.http
            .json(self.http.request(
                Method::POST,
                &format!("{}/turn/{turn_id}/cancel", thread_path(agent_id, thread_id)),
            )?)
            .await
    }

    pub async fn approval_response(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        turn_id: TurnId,
        body: &ApprovalResponseBody,
    ) -> Result<EventResult> {
        self.http
            .json(
                self.http
                    .request(
                        Method::POST,
                        &format!(
                            "{}/turn/{turn_id}/approval-response",
                            thread_path(agent_id, thread_id)
                        ),
                    )?
                    .json(body),
            )
            .await
    }

    pub async fn frontend_tool_result(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        turn_id: TurnId,
        body: &FrontendToolResultBody,
    ) -> Result<EventResult> {
        self.http
            .json(
                self.http
                    .request(
                        Method::POST,
                        &format!(
                            "{}/turn/{turn_id}/frontend-tool-result",
                            thread_path(agent_id, thread_id)
                        ),
                    )?
                    .json(body),
            )
            .await
    }

    pub async fn events(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        query: &EventsQuery,
    ) -> Result<GetEventsResult> {
        self.http
            .json(
                self.http
                    .request(
                        Method::GET,
                        &format!("{}/event", thread_path(agent_id, thread_id)),
                    )?
                    .query(query),
            )
            .await
    }

    pub async fn watch(
        &self,
        agent_id: AgentId,
        thread_id: ThreadId,
        query: &WatchQuery,
    ) -> Result<EventStream> {
        let response = self
            .http
            .send(
                self.http
                    .request(
                        Method::GET,
                        &format!("{}/event/watch", thread_path(agent_id, thread_id)),
                    )?
                    .query(query)
                    .header("accept", "text/event-stream"),
            )
            .await?;
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if content_type.split(';').next().map(str::trim) != Some("text/event-stream") {
            bail!("runtime watch returned an unexpected content type: {content_type}");
        }
        Ok(sse::decode(response.bytes_stream()))
    }
}

fn thread_path(agent_id: AgentId, thread_id: ThreadId) -> String {
    format!("agent/{agent_id}/thread/{thread_id}")
}

fn scope_path(scope: exoharness::ResourceScope) -> String {
    match scope {
        exoharness::ResourceScope::Global => String::new(),
        exoharness::ResourceScope::Agent { agent_id } => format!("agent/{agent_id}/"),
        exoharness::ResourceScope::Thread {
            agent_id,
            thread_id,
        } => format!("{}/", thread_path(agent_id, thread_id)),
    }
}
