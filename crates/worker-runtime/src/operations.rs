use std::sync::Arc;

use anyhow::{Context, Result};
use executor::Runtime;
use exoharness::{AgentId, ThreadId};
use futures::StreamExt;
use serde::Deserialize;

use crate::host::Host;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Operation {
    Http {
        request: crate::http::Request,
    },
    SandboxPolicy {
        agent_id: AgentId,
        thread_id: ThreadId,
    },
    ProxyHeaders {
        agent_id: AgentId,
        thread_id: ThreadId,
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
    State {
        request: exoharness::protocol::Request,
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
        Operation::Http { request } => Ok(serde_json::to_string(
            &crate::http::handle(&runtime, &host, &updates, request).await,
        )?),
        Operation::Events {
            agent_id,
            thread_id,
            after,
        } => {
            let agent = state
                .get_agent(&agent_id)
                .await?
                .context("agent not found")?;
            let thread = agent
                .get_thread(&thread_id)
                .await?
                .context("thread not found")?;
            let mut events = thread
                .watch_events(
                    after
                        .map(std::ops::Bound::Excluded)
                        .unwrap_or(std::ops::Bound::Unbounded),
                )
                .await?;
            let mut page = thread
                .get_events(Some(exoharness::EventQuery {
                    cursor: after,
                    limit: Some(1000),
                    ..Default::default()
                }))
                .await?;
            if page.events.is_empty() {
                page.events
                    .push(events.next().await.context("event stream closed")??);
                page.cursor = page.events.last().map(|event| event.id);
            }
            Ok(serde_json::to_string(&page)?)
        }
        Operation::State { request } => {
            let response = exoharness::server::ExoHarnessServer::new(state)
                .handle_request(request)
                .await?;
            Ok(serde_json::to_string(&response)?)
        }
        Operation::SandboxPolicy {
            agent_id,
            thread_id,
        } => {
            let _guard = updates.lock().await;
            Ok(serde_json::to_string(
                &crate::policy::environment(&runtime, &host, agent_id, thread_id).await?,
            )?)
        }
        Operation::ProxyHeaders {
            agent_id,
            thread_id,
            url,
            method,
            headers,
        } => {
            let _guard = updates.lock().await;
            Ok(serde_json::to_string(
                &crate::policy::proxy_headers(
                    &runtime, &host, agent_id, thread_id, &url, &method, headers,
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
            Ok("{\"type\":\"unit\"}".into())
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
                    return Ok("true".into());
                }
            }
            Ok("false".into())
        }
        Operation::Recover => {
            runtime.recover_unfinished_turns().await?;
            Ok("{}".into())
        }
    }
}
