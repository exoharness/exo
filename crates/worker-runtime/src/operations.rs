use std::sync::Arc;

use anyhow::{Context, Result};
use executor::Runtime;
use exoharness::{AgentId, ThreadId};
use serde::Deserialize;

use crate::host::Host;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Operation {
    Progress {
        thread_id: ThreadId,
        turn: exoharness::TurnRecord,
        event: executor::TypeScriptStreamEvent,
    },
    Http {
        request: crate::http::Request,
    },
    SandboxPolicy {
        request: exoharness::SandboxRequest,
    },
    ProxyHeaders {
        agent_id: AgentId,
        thread_id: ThreadId,
        sandbox_id: String,
        url: String,
        method: String,
        headers: Vec<(String, String)>,
    },
    AuthorizeTool {
        agent_id: AgentId,
        thread_id: ThreadId,
        turn: exoharness::TurnRecord,
        request: exoharness::ToolRequest,
    },
    HasUnfinishedTurns,
    Recover,
    Events {
        agent_id: AgentId,
        thread_id: ThreadId,
        after: Option<exoharness::EventId>,
    },
}

pub(crate) async fn run(
    runtime: Runtime,
    host: Arc<Host>,
    updates: Arc<tokio::sync::Mutex<()>>,
    operation: Operation,
) -> Result<String> {
    let state = runtime.exoharness_handle();
    match operation {
        Operation::Progress {
            thread_id,
            turn,
            event,
        } => {
            let progress = if let executor::ExecutionStreamEvent::Chunk(chunk) =
                executor::to_execution_stream_event(event)
            {
                let id = exoharness::Uuid7::now();
                Some(exoharness::Event {
                    id,
                    thread_id,
                    session_id: Some(turn.session_id),
                    turn_id: Some(turn.id),
                    created_at: id.timestamp().expect("uuid7 timestamp"),
                    data: exoharness::EventData::LinguaStreamChunk { chunk },
                })
            } else {
                None
            };
            Ok(serde_json::to_string(&progress)?)
        }
        Operation::Http { request } => Ok(serde_json::to_string(
            &crate::http::handle(&runtime, &host, &updates, request).await,
        )?),
        Operation::Events {
            agent_id,
            thread_id,
            after,
        } => {
            let service = executor::managed_agents::service::Service {
                runtime: &runtime,
                agent_id: None,
                definition_updates: &updates,
            };
            let path = executor::managed_agents::service::ThreadPath {
                agent_id,
                thread_id,
            };
            let query = exo_managed_agents::http::protocol::EventsQuery {
                after,
                limit: Some(1000),
                ..Default::default()
            };
            Ok(serde_json::to_string(
                &executor::managed_agents::service::wait_events(&service, &path, query).await?,
            )?)
        }
        Operation::SandboxPolicy { request } => {
            let _guard = updates.lock().await;
            Ok(serde_json::to_string(
                &crate::policy::environment(&runtime, &host, request).await?,
            )?)
        }
        Operation::ProxyHeaders {
            agent_id,
            thread_id,
            sandbox_id,
            url,
            method,
            headers,
        } => {
            let _guard = updates.lock().await;
            Ok(serde_json::to_string(
                &crate::policy::proxy_headers(
                    &runtime,
                    &host,
                    agent_id,
                    thread_id,
                    &sandbox_id,
                    &url,
                    &method,
                    headers,
                )
                .await?,
            )?)
        }
        Operation::AuthorizeTool {
            agent_id,
            thread_id,
            turn,
            request,
        } => {
            let agent = state
                .get_agent(&agent_id)
                .await?
                .context("agent not found")?;
            let thread = agent
                .get_thread(&thread_id)
                .await?
                .context("thread not found")?;
            let config = executor::load_conversation_config(thread.as_ref()).await?;
            let turn = thread.turn_handle(turn).await?;
            executor::permissions::authorize(
                thread.as_ref(),
                turn.as_ref(),
                config.permissions.for_tool(&request.function_name),
                None,
                None,
                false,
                &request,
                executor::ExecutorStreamMode::Disabled,
            )
            .await?;
            Ok(serde_json::to_string(
                &exoharness::protocol::Response::Unit,
            )?)
        }
        Operation::HasUnfinishedTurns => {
            for agent in state.list_agents().await? {
                if !agent
                    .list_conversations(exoharness::ListConversationsRequest {
                        unfinished_only: true,
                        limit: Some(1),
                        ..Default::default()
                    })
                    .await?
                    .conversations
                    .is_empty()
                {
                    return Ok(serde_json::to_string(&true)?);
                }
            }
            Ok(serde_json::to_string(&false)?)
        }
        Operation::Recover => {
            runtime.recover_unfinished_turns().await?;
            Ok(serde_json::to_string(&())?)
        }
    }
}
