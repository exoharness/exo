//! One route table for native HTTP servers and hosted runtimes.
use std::sync::Arc;

use crate::{AgentConfig, Runtime, managed_agents::service as api, runtime_host::RuntimeHost};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use exo_managed_agents::http::protocol::*;
use exoharness::{EventStream, ThreadHandle};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[async_trait]
pub trait Platform: Send + Sync {
    fn account_id(&self) -> &str;
    /// Raw store RPC is an account-admin endpoint, separate from managed turns.
    fn exposes_store_protocol(&self) -> bool {
        false
    }
    fn host(&self) -> Arc<dyn RuntimeHost>;
    fn streaming(&self, _config: &AgentConfig) -> bool {
        true
    }
    async fn previews(
        &self,
        _agent: &dyn exoharness::AgentHandle,
        _thread: Arc<dyn ThreadHandle>,
    ) -> Result<Option<PreviewEndpoint>> {
        Ok(None)
    }
    async fn cancel(
        &self,
        runtime: &Runtime,
        path: &api::TurnPath,
        _thread: Arc<dyn ThreadHandle>,
    ) -> Result<CancelTurnResult> {
        api::cancel_turn(runtime, path.agent_id, path.thread_id, path.turn_id).await
    }
    async fn frontend_tool_result(
        &self,
        _runtime: &Runtime,
        _path: &api::TurnPath,
        _body: FrontendToolResultBody,
    ) -> Result<EventResult> {
        bail!(api::UnsupportedRequest(
            "this runtime does not execute frontend tools"
        ))
    }
}

#[derive(Deserialize)]
pub struct Request {
    pub method: String,
    pub path: Vec<String>,
    pub query: String,
    pub body: String,
}

pub enum Response {
    Json { status: u16, body: String },
    Error { status: u16, message: String },
    Watch(EventStream),
}
fn json(value: impl Serialize) -> Result<Response> {
    Ok(Response::Json {
        status: 200,
        body: serde_json::to_string(&value)?,
    })
}
fn body<T: DeserializeOwned>(request: &Request) -> Result<T> {
    serde_json::from_str(&request.body).map_err(bad_request)
}
fn optional_body<T: DeserializeOwned + Default>(request: &Request) -> Result<T> {
    if request.body.is_empty() {
        Ok(T::default())
    } else {
        body(request)
    }
}
fn query<T: DeserializeOwned>(request: &Request) -> Result<T> {
    serde_urlencoded::from_str(&request.query).map_err(bad_request)
}

fn bad_request(error: impl std::fmt::Display) -> anyhow::Error {
    api::RequestError {
        status: 400,
        message: error.to_string(),
    }
    .into()
}

pub async fn handle(
    service: &api::Service<'_>,
    platform: &dyn Platform,
    progress: &tokio::sync::broadcast::Sender<exoharness::Event>,
    request: &Request,
) -> Response {
    match route(service, platform, progress, request).await {
        Ok(response) => response,
        Err(error) => Response::Error {
            status: api::error_status(&error),
            message: error.to_string(),
        },
    }
}

async fn route(
    service: &api::Service<'_>,
    platform: &dyn Platform,
    progress: &tokio::sync::broadcast::Sender<exoharness::Event>,
    r: &Request,
) -> Result<Response> {
    let runtime = service.runtime;
    // Ordinary routes have the same typed service call and JSON response.
    macro_rules! reply {
        ($handler:ident $(, $arg:expr)*) => {
            return json(api::$handler(service $(, $arg)*).await?)
        };
    }

    // Match paths from the same declarations when the HTTP method differs.
    macro_rules! routes {
        ($method:expr, $path:expr; $(($verb:literal, $pattern:pat) $(if $guard:expr)? => $handler:expr,)*) => {
            match ($method, $path) {
                $(($verb, $pattern) $(if $guard)? => $handler,)*
                _ => {
                    #[allow(unused_variables, unreachable_patterns)]
                    let known = match $path {
                        $($pattern $(if $guard)? => true,)*
                        _ => false,
                    };
                    if known {
                        return Err(api::RequestError { status: 405, message: "Method Not Allowed".into() }.into());
                    }
                }
            }
        };
    }
    let path = r.path.iter().map(String::as_str).collect::<Vec<_>>();
    let method = r.method.as_str();
    if platform.exposes_store_protocol() && path == ["request"] && method == "POST" {
        return json(
            exoharness::server::ExoHarnessServer::new(runtime.exoharness_handle())
                .handle_message(body(r)?)
                .await,
        );
    }
    routes!(method, path.as_slice();
    ("GET", ["identity"]) => {
        return json(ProviderIdentity {
            account_id: platform.account_id().into(),
        });
    },
    ("GET", ["environment"]) => reply!(list_environments),
    ("PUT", ["environment"]) => reply!(put_environment, body(r)?),
    ("DELETE", ["environment", name]) => reply!(delete_environment, name),
    ("GET", ["agent"]) => reply!(list_agents, query(r)?),
    ("POST", ["agent"]) => reply!(create_agent, body(r)?),
    ("POST", ["agent", "managed"]) => reply!(create_managed_agent, body(r)?),
    );
    let mut vault = api::VaultPath {
        agent_id: None,
        thread_id: None,
        vault_id: None,
        secret_id: None,
    };
    let vault_tail = match path.as_slice() {
        ["vault", tail @ ..] => Some(tail),
        ["agent", agent, "vault", tail @ ..] => {
            vault.agent_id = Some(agent.parse().map_err(bad_request)?);
            Some(tail)
        }
        ["agent", agent, "thread", thread, "vault", tail @ ..] => {
            vault.agent_id = Some(agent.parse().map_err(bad_request)?);
            vault.thread_id = Some(thread.parse().map_err(bad_request)?);
            Some(tail)
        }
        _ => None,
    };
    if let Some(tail) = vault_tail {
        if let Some(id) = tail.first() {
            vault.vault_id = Some(id.parse().map_err(bad_request)?);
        }
        if let Some(id) = tail.get(2) {
            vault.secret_id = Some(id.parse().map_err(bad_request)?);
        }
        routes!(method, tail;
        ("GET", []) => reply!(list_vaults, &vault),
        ("POST", []) if vault.thread_id.is_some() => {
            let path = api::ThreadPath {
                agent_id: vault.agent_id.context("agent ID missing")?,
                thread_id: vault.thread_id.context("thread ID missing")?,
            };
            reply!(attach_thread_vaults, &path, body(r)?);
        },
        ("POST", []) if vault.agent_id.is_none() => reply!(create_vault, body(r)?),
        ("DELETE", [_]) if vault.agent_id.is_none() => reply!(delete_vault, &vault),
        ("GET", [_, "secret"]) => reply!(list_secrets, &vault),
        ("POST", [_, "secret"]) => reply!(put_secret, &vault, body(r)?),
        ("PUT", [_, "secret", _]) => reply!(update_secret, &vault, body(r)?),
        ("DELETE", [_, "secret", _]) => reply!(delete_secret, &vault),
            );
    }
    if let ["agent", id, tail @ ..] = path.as_slice() {
        let agent = api::AgentPath {
            agent_id: id.parse().map_err(bad_request)?,
        };
        routes!(method, tail;
        ("GET", []) => reply!(get_agent, &agent),
        ("DELETE", []) => reply!(delete_agent, &agent),
        ("PUT", ["definition"]) => reply!(write_agent_definition, &agent, body(r)?),
        ("GET", ["artifact"]) => reply!(list_artifacts, &agent),
        ("GET", ["artifact", "read"]) => reply!(read_artifact, &agent, query(r)?),
        ("POST", ["artifact"]) => reply!(write_artifact, &agent, body(r)?),
        ("GET", ["thread"]) => reply!(list_threads, &agent, query(r)?),
        ("POST", ["thread"]) => {
            let agent = service.agent(agent.agent_id).await?;
            return json(api::create_thread(runtime, &agent, optional_body(r)?).await?);
        },
            );
        if let ["thread", id, tail @ ..] = tail {
            let thread = api::ThreadPath {
                agent_id: agent.agent_id,
                thread_id: id.parse().map_err(bad_request)?,
            };
            routes!(method, tail;
            ("GET", []) => reply!(get_thread, &thread),
            ("GET", ["previews"]) => {
                let agent = service.agent(thread.agent_id).await?;
                let conversation = service.thread(agent.as_ref(), thread.thread_id).await?;
                return json(platform.previews(agent.as_ref(), conversation).await?);
            },
            ("GET", ["event", "watch"]) => {
                let agent = service.agent(thread.agent_id).await?;
                let conversation = service.thread(agent.as_ref(), thread.thread_id).await?;
                let query: WatchQuery = query(r)?;
                return Ok(Response::Watch(
                    api::watch_events(conversation.as_ref(), progress.subscribe(), query.after)
                        .await?,
                ));
            },
            ("DELETE", []) => reply!(delete_thread, &thread),
            ("PUT", ["environment"]) => reply!(update_thread_environment, &thread, body(r)?),
            ("POST", ["fork"]) => reply!(fork_thread, &thread, optional_body(r)?),
            ("GET", ["artifact"]) => reply!(list_thread_artifacts, &thread),
            ("GET", ["artifact", "read"]) => reply!(read_thread_artifact, &thread, query(r)?),
            ("GET", ["event"]) => reply!(events, &thread, query(r)?),
            ("POST", ["turn"]) => {
                let agent = service.agent(thread.agent_id).await?;
                let conversation = service.thread(agent.as_ref(), thread.thread_id).await?;
                let prepared =
                    api::prepare_turn(runtime, agent.as_ref(), conversation.as_ref(), body(r)?)
                        .await?;
                let streaming = platform.streaming(&prepared.config);
                let runtime = runtime.clone();
                let progress = progress.clone();
                let (receipt, received) = futures::channel::oneshot::channel();
                // Admission outlives a disconnected requester on every host.
                platform.host().spawn(Box::pin(async move {
                    let (result, stream) = match api::submit_turn(&runtime, agent, conversation, prepared, streaming).await {
                        Ok(admitted) => admitted,
                        Err(error) => {
                            if receipt.send(Err(error)).is_err() { tracing::debug!("turn requester disconnected before admission failed"); }
                            return;
                        }
                    };
                    let turn = result.turn.clone();
                    if receipt.send(Ok(result)).is_err() { tracing::debug!(turn_id = %turn.id, "turn requester disconnected after admission"); }
                    api::forward_progress(stream, thread.thread_id, turn, progress).await;
                }));
                let result = received.await??;
                return Ok(Response::Json {
                    status: 202,
                    body: serde_json::to_string(&result)?,
                });
            },
                    );
            if let ["turn", id, tail @ ..] = tail {
                let turn = api::TurnPath {
                    agent_id: thread.agent_id,
                    thread_id: thread.thread_id,
                    turn_id: id.parse().map_err(bad_request)?,
                };
                routes!(method, tail;
                ("GET", []) => reply!(turn_status, &turn),
                ("POST", ["approval-response"]) => reply!(approval_response, &turn, body(r)?),
                ("POST", ["cancel"]) => {
                    let agent = service.agent(turn.agent_id).await?;
                    let conversation = service.thread(agent.as_ref(), turn.thread_id).await?;
                    return json(platform.cancel(runtime, &turn, conversation).await?);
                },
                ("POST", ["frontend-tool-result"]) => {
                    return json(
                        platform
                            .frontend_tool_result(runtime, &turn, body(r)?)
                            .await?,
                    );
                },
                            );
            }
        }
    }
    Err(api::RequestError {
        status: 404,
        message: "Not found".into(),
    }
    .into())
}
