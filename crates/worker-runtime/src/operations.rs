use std::sync::Arc;

use anyhow::Result;
use executor::Runtime;
use exoharness::{AgentId, ThreadId};
use serde::{Deserialize, Serialize};

use crate::host::Host;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Operation {
    Request {
        request: exoharness::protocol::Request,
    },
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
    HarnessRequest {
        thread_id: ThreadId,
        request: executor::typescript_runtime::RuntimeRequest,
    },
    HasUnfinishedTurns,
    Recover,
    Events {
        agent_id: AgentId,
        thread_id: ThreadId,
        after: Option<exoharness::EventId>,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Output {
    Http(crate::http::Response),
    Harness(executor::typescript_runtime::RuntimeResponsePayload),
    Exo(Box<exoharness::protocol::Response>),
    // JSON/SSE byte fields must be arrays; host process I/O stays Uint8Array.
    Json(serde_json::Value),
    Policy(std::collections::HashMap<String, String>),
    Headers(Vec<(String, String)>),
    Bool(bool),
    Unit(()),
}

pub(crate) async fn run(
    runtime: Runtime,
    execution: Arc<crate::execution::WorkerExecutor>,
    host: Arc<Host>,
    updates: Arc<tokio::sync::Mutex<()>>,
    operation: Operation,
) -> Result<Output> {
    let state = runtime.exoharness_handle();
    match operation {
        Operation::Request { request } => Ok(Output::Exo(Box::new(
            crate::http::request(&runtime, request).await?,
        ))),
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
            Ok(Output::Json(serde_json::to_value(progress)?))
        }
        Operation::Http { request } => Ok(Output::Http(
            crate::http::handle(&runtime, &host, &updates, request).await,
        )),
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
            Ok(Output::Json(serde_json::to_value(
                executor::managed_agents::service::wait_events(&service, &path, query).await?,
            )?))
        }
        Operation::SandboxPolicy { request } => {
            let _guard = updates.lock().await;
            Ok(Output::Policy(
                crate::policy::environment(&runtime, &host, request).await?,
            ))
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
            Ok(Output::Headers(
                crate::policy::proxy_headers(
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
            ))
        }
        Operation::HarnessRequest { thread_id, request } => Ok(Output::Harness(
            execution.request_runtime(thread_id, request).await?,
        )),
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
                    return Ok(Output::Bool(true));
                }
            }
            Ok(Output::Bool(false))
        }
        Operation::Recover => {
            runtime.recover_unfinished_turns().await?;
            Ok(Output::Unit(()))
        }
    }
}
