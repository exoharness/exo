use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use executor::{AgentConfig, ConversationModelConfig, Runtime, SendRequest};
use exo_managed_agents::{
    AgentDefinition,
    http::protocol::{
        ApprovalResponseBody, CancelTurnResult, CreateThreadResult, SubmitTurnResult,
    },
};
use exoharness::{AgentId, NewThreadRequest, ThreadId, TurnId};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::execution::worker_agent_config;
use crate::host::Host;
use executor::runtime_host::RuntimeHost;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Operation {
    Authenticate {
        token: Option<String>,
        authorization: Option<String>,
    },
    ConfigureAgent {
        agent_id: AgentId,
        source: String,
    },
    OpenThread {
        agent_id: AgentId,
        request: NewThreadRequest,
        model: Option<String>,
        harness: Option<String>,
    },
    StartTurn {
        agent_id: AgentId,
        thread_id: ThreadId,
        request: SendRequest,
        model: Option<String>,
        harness: Option<String>,
        system_prompt: Option<String>,
    },
    Cancel {
        thread_id: ThreadId,
        turn_id: TurnId,
    },
    Approve {
        agent_id: AgentId,
        thread_id: ThreadId,
        turn_id: TurnId,
        body: ApprovalResponseBody,
    },
    Recover,
    State {
        request: exoharness::protocol::Request,
    },
}

pub(crate) async fn run(runtime: Runtime, host: Arc<Host>, operation: Operation) -> Result<String> {
    let state = runtime.exoharness_handle();
    match operation {
        Operation::Authenticate {
            token,
            authorization,
        } => Ok(serde_json::to_string(&token.as_deref().is_some_and(
            |token| executor::http_auth::bearer_token_matches(token, authorization.as_deref()),
        ))?),
        Operation::State { request } => {
            let response: exoharness::protocol::Response = host
                .call(crate::host::HostRequest::State { request })
                .await?;
            Ok(serde_json::to_string(&response)?)
        }
        Operation::ConfigureAgent { agent_id, source } => {
            let definition = AgentDefinition::parse(source)?;
            worker_agent_config(&definition)?;
            let agent = state
                .get_agent(&agent_id)
                .await?
                .context("agent not found")?;
            let version = runtime.update_managed_agent(&agent, &definition).await?;
            Ok(serde_json::to_string(&version)?)
        }
        Operation::OpenThread {
            agent_id,
            request,
            model,
            harness,
        } => {
            let agent = state
                .get_agent(&agent_id)
                .await?
                .context("agent not found")?;
            let definition = exo_managed_agents::load_definition(agent.as_ref())
                .await?
                .context("save a managed agent definition first")?;
            worker_agent_config(&definition)?;
            ensure!(
                harness
                    .as_ref()
                    .is_none_or(|h| h == &definition.frontmatter.harness),
                "harness must match the agent definition"
            );
            let opened = runtime.open_managed_thread(&agent, None, request).await?;
            if let Some(model) = model {
                ensure!(!model.trim().is_empty(), "model must not be empty");
                executor::put_conversation_model_override(
                    opened.thread.as_ref(),
                    Some(ConversationModelConfig {
                        model,
                        max_output_tokens: None,
                    }),
                )
                .await?;
            }
            Ok(serde_json::to_string(&CreateThreadResult {
                agent: agent.record().clone(),
                thread: opened.thread.record().clone(),
                harness: definition.frontmatter.harness,
            })?)
        }
        Operation::StartTurn {
            agent_id,
            thread_id,
            request,
            model,
            harness,
            system_prompt,
        } => {
            ensure!(
                request
                    .input
                    .iter()
                    .all(|m| matches!(m, lingua::Message::User { .. })),
                "turn input must contain user messages"
            );
            let agent = state
                .get_agent(&agent_id)
                .await?
                .context("agent not found")?;
            let thread = agent
                .get_thread(&thread_id)
                .await?
                .context("thread not found")?;
            let definition = exo_managed_agents::load_definition(agent.as_ref())
                .await?
                .context("save a managed agent definition first")?;
            worker_agent_config(&definition)?;
            ensure!(
                harness
                    .as_ref()
                    .is_none_or(|h| h == &definition.frontmatter.harness),
                "harness must match the agent definition"
            );
            let override_config = if model.is_some() || system_prompt.is_some() {
                let mut config: AgentConfig = runtime.get_agent_config(agent.as_ref()).await?;
                if let Some(saved) =
                    executor::get_conversation_model_override(thread.as_ref()).await?
                {
                    config.model = saved.model;
                    config.max_output_tokens = saved.max_output_tokens;
                }
                if let Some(model) = model {
                    ensure!(!model.trim().is_empty(), "model must not be empty");
                    config.model = model;
                }
                if let Some(prompt) = system_prompt {
                    config.instructions = vec![lingua::Message::System {
                        content: lingua::universal::UserContent::String(prompt),
                    }];
                }
                Some(config)
            } else {
                None
            };
            // Persisted canonical events drive HTTP/SSE. The shared runtime's
            // completion stream must still be drained to finalize the turn.
            let (turn, mut stream) = runtime
                .start_turn(
                    agent.clone(),
                    thread.clone(),
                    request,
                    false,
                    override_config,
                )
                .await?;
            host.spawn(Box::pin(async move {
                while let Some(event) = stream.next().await {
                    if let Err(error) = event {
                        tracing::warn!(%error, "Worker turn failed");
                    }
                }
            }));
            let thread = agent
                .get_thread(&thread_id)
                .await?
                .context("thread disappeared")?;
            Ok(serde_json::to_string(&SubmitTurnResult {
                agent: agent.record().clone(),
                thread: thread.record().clone(),
                turn,
                harness: definition.frontmatter.harness,
            })?)
        }
        Operation::Cancel { thread_id, turn_id } => {
            let canceled = runtime
                .cancel_turn(executor::harness::HarnessTurnKey::new(thread_id, turn_id))
                .await?;
            Ok(serde_json::to_string(&CancelTurnResult {
                canceled_active_turn: canceled,
                finished_event_id: None,
            })?)
        }
        Operation::Approve {
            agent_id,
            thread_id,
            turn_id,
            body,
        } => {
            let event_id = runtime
                .approval_response(agent_id, thread_id, turn_id, &body)
                .await?;
            #[derive(Serialize)]
            struct Approved {
                event_id: exoharness::EventId,
            }
            Ok(serde_json::to_string(&Approved { event_id })?)
        }
        Operation::Recover => {
            runtime.recover_unfinished_turns().await?;
            Ok("{}".into())
        }
    }
}
