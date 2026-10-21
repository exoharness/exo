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
        account_id: String,
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
        rejected: Option<Vec<(String, String)>>,
    },
    HarnessRequest {
        thread_id: ThreadId,
        request: executor::typescript_runtime::RuntimeRequest,
    },
    McpHttp {
        request: crate::mcp::HttpRequest,
    },
    DrainQueue,
    WatchNext {
        id: u32,
    },
    WatchClose {
        id: u32,
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
    Headers(crate::policy::ProxyReply),
    Bool(bool),
    Watch(u32),
    Frame(Option<String>),
    McpHttp(crate::mcp::HttpResponse),
    Unit(()),
}

pub(crate) async fn run(
    runtime: Runtime,
    provider: executor::LocalProvider,
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
            exoharness::server::ExoHarnessServer::new(runtime.exoharness_handle())
                .handle_request(request)
                .await?,
        ))),
        Operation::Progress { thread_id, event } => {
            execution.emit_stream(thread_id, event)?;
            Ok(Output::Unit(()))
        }
        Operation::Http {
            request,
            account_id,
        } => Ok(Output::Http(
            crate::http::handle(&service, &host, &progress, request, &account_id, id).await,
        )),
        Operation::WatchNext { id } => {
            Ok(Output::Frame(host.watches.next(host.as_ref(), id).await?))
        }
        Operation::WatchClose { id } => {
            host.watches.close(id);
            Ok(Output::Unit(()))
        }
        Operation::WatchState {
            agent_id,
            thread_id,
            after,
        } => {
            let agent = service.agent(agent_id).await?;
            let thread = service.thread(agent.as_ref(), thread_id).await?;
            Ok(Output::Watch(host.watches.open(
                id,
                thread.watch_events(after).await?,
                false,
            )))
        }
        Operation::SandboxPolicy { request } => Ok(Output::Policy(
            crate::policy::environment(&runtime, &host, request).await?,
        )),
        Operation::ProxyHeaders {
            agent_id,
            thread_id,
            sandbox_id,
            url,
            method,
            headers,
            rejected,
        } => {
            let result = crate::policy::proxy_headers(
                &runtime,
                &host,
                agent_id,
                thread_id,
                &sandbox_id,
                &url,
                &method,
                headers,
                rejected,
            )
            .await;
            Ok(Output::Headers(match result {
                Ok(headers) => crate::policy::ProxyReply::Allowed(headers),
                Err(error) => crate::policy::ProxyReply::Error {
                    status: if error.is::<exoharness::egress_credentials::EgressDenied>() {
                        403
                    } else {
                        500
                    },
                    error: format!("{error:#}"),
                },
            }))
        }
        Operation::HarnessRequest { thread_id, request } => Ok(Output::Harness(
            execution.request_runtime(thread_id, request).await?,
        )),
        Operation::McpHttp { request } => Ok(Output::McpHttp(execution.mcp.fetch(request).await?)),
        Operation::DrainQueue => {
            for thread in execution.coordinator.pending_threads().await? {
                provider.wake_turn_queue(&runtime, thread).await?;
            }
            Ok(Output::Bool(
                !execution.coordinator.pending_threads().await?.is_empty(),
            ))
        }
    }
}
