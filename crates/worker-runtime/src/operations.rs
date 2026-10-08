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
    HasPendingTurns,
    Recover,
    Watch {
        agent_id: AgentId,
        thread_id: ThreadId,
        after: Option<exoharness::EventId>,
    },
    WatchState {
        agent_id: AgentId,
        thread_id: ThreadId,
        after: std::ops::Bound<exoharness::EventId>,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Output {
    Http(crate::http::Response),
    Harness(executor::typescript_runtime::RuntimeResponsePayload),
    Exo(Box<exoharness::protocol::Response>),
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
    progress: tokio::sync::broadcast::Sender<exoharness::Event>,
    id: u32,
    operation: Operation,
) -> Result<Output> {
    let service = executor::managed_agents::service::Service {
        runtime: &runtime,
        agent_id: None,
        definition_updates: &updates,
    };
    match operation {
        Operation::Request { request } => Ok(Output::Exo(Box::new(
            crate::http::request(&runtime, request).await?,
        ))),
        Operation::Progress { thread_id, event } => {
            execution.emit_stream(thread_id, event)?;
            Ok(Output::Unit(()))
        }
        Operation::Http { request } => Ok(Output::Http(
            crate::http::handle(&runtime, &host, &updates, &progress, request).await,
        )),
        Operation::Watch {
            agent_id,
            thread_id,
            after,
        } => {
            let agent = service.agent(agent_id).await?;
            let thread = service.thread(agent.as_ref(), thread_id).await?;
            let events = executor::managed_agents::service::watch_events(
                thread.as_ref(),
                progress.subscribe(),
                after,
            )
            .await?;
            forward_watch(&host, id, events).await?;
            Ok(Output::Unit(()))
        }
        Operation::WatchState {
            agent_id,
            thread_id,
            after,
        } => {
            let agent = service.agent(agent_id).await?;
            let thread = service.thread(agent.as_ref(), thread_id).await?;
            forward_watch(&host, id, thread.watch_events(after).await?).await?;
            Ok(Output::Unit(()))
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
        Operation::HasPendingTurns => Ok(Output::Bool(
            !execution.coordinator.pending_threads().await?.is_empty(),
        )),
        Operation::Recover => {
            runtime.recover_unfinished_turns().await?;
            Ok(Output::Unit(()))
        }
    }
}

async fn forward_watch(host: &Host, id: u32, mut events: exoharness::EventStream) -> Result<()> {
    use futures::StreamExt;
    while let Some(event) = events.next().await {
        host.watch_events
            .lock()
            .expect("Worker watches poisoned")
            .push((id, serde_json::to_value(event?)?));
    }
    Ok(())
}
