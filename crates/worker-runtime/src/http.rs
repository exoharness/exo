//! Worker HTTP plumbing. All managed-agent behavior lives in executor's service.
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use executor::{Runtime, managed_agents::service as api, runtime_host::RuntimeHost};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::host::Host;

#[derive(Deserialize)]
pub(crate) struct Request {
    method: String,
    path: Vec<String>,
    query: String,
    body: String,
}

#[derive(Serialize)]
pub(crate) struct Response {
    status: u16,
    body: String,
}

fn json(value: impl Serialize) -> Result<Response> {
    Ok(Response {
        status: 200,
        body: serde_json::to_string(&value)?,
    })
}
fn body<T: DeserializeOwned>(request: &Request) -> Result<T> {
    Ok(serde_json::from_str(&request.body)?)
}
fn optional_body<T: DeserializeOwned + Default>(request: &Request) -> Result<T> {
    if request.body.is_empty() {
        Ok(T::default())
    } else {
        body(request)
    }
}
fn query<T: DeserializeOwned>(request: &Request) -> Result<T> {
    Ok(serde_urlencoded::from_str(&request.query)?)
}

pub(crate) async fn handle(
    runtime: &Runtime,
    host: &Arc<Host>,
    updates: &tokio::sync::Mutex<()>,
    progress: &tokio::sync::broadcast::Sender<exoharness::Event>,
    request: Request,
) -> Response {
    match route(runtime, host, updates, progress, &request).await {
        Ok(response) => response,
        Err(error) => {
            #[derive(Serialize)]
            struct ErrorBody {
                error: String,
            }
            Response {
                status: api::error_status(&error),
                body: serde_json::to_string(&ErrorBody {
                    error: error.to_string(),
                })
                .expect("serialize error"),
            }
        }
    }
}

pub(crate) async fn request(
    runtime: &Runtime,
    request: exoharness::protocol::Request,
) -> Result<exoharness::protocol::Response> {
    exoharness::server::ExoHarnessServer::new(runtime.exoharness_handle())
        .handle_request(request)
        .await
}

async fn route(
    runtime: &Runtime,
    host: &Arc<Host>,
    updates: &tokio::sync::Mutex<()>,
    progress: &tokio::sync::broadcast::Sender<exoharness::Event>,
    r: &Request,
) -> Result<Response> {
    let service = api::Service {
        runtime,
        agent_id: None,
        definition_updates: updates,
    };
    let path = r.path.iter().map(String::as_str).collect::<Vec<_>>();
    let method = r.method.as_str();
    if path == ["request"] && method == "POST" {
        return json(
            exoharness::server::ExoHarnessServer::new(runtime.exoharness_handle())
                .handle_message(body(r)?)
                .await,
        );
    }
    match (method, path.as_slice()) {
        ("GET", ["environment"]) => return json(api::list_environments(&service).await?),
        ("PUT", ["environment"]) => return json(api::put_environment(&service, body(r)?).await?),
        ("DELETE", ["environment", name]) => {
            return json(api::delete_environment(&service, name).await?);
        }
        ("GET", ["agent"]) => return json(api::list_agents(&service, query(r)?).await?),
        ("POST", ["agent"]) => return json(api::create_agent(&service, body(r)?).await?),
        _ => {}
    }
    let mut vault = api::VaultPath {
        agent_id: None,
        thread_id: None,
        vault_id: None,
        secret_id: None,
    };
    let vault_tail = match path.as_slice() {
        ["vault", tail @ ..] => Some(tail),
        ["agent", agent, "vault", tail @ ..] => {
            vault.agent_id = Some(agent.parse()?);
            Some(tail)
        }
        ["agent", agent, "thread", thread, "vault", tail @ ..] => {
            vault.agent_id = Some(agent.parse()?);
            vault.thread_id = Some(thread.parse()?);
            Some(tail)
        }
        _ => None,
    };
    if let Some(tail) = vault_tail {
        if let Some(id) = tail.first() {
            vault.vault_id = Some(id.parse()?);
        }
        if let Some(id) = tail.get(2) {
            vault.secret_id = Some(id.parse()?);
        }
        match (method, tail) {
            ("GET", []) => return json(api::list_vaults(&service, &vault).await?),
            ("POST", []) if vault.thread_id.is_some() => {
                let path = api::ThreadPath {
                    agent_id: vault.agent_id.context("agent ID missing")?,
                    thread_id: vault.thread_id.context("thread ID missing")?,
                };
                return json(api::attach_thread_vaults(&service, &path, body(r)?).await?);
            }
            ("POST", []) if vault.agent_id.is_none() => {
                return json(api::create_vault(&service, body(r)?).await?);
            }
            ("DELETE", [_]) if vault.agent_id.is_none() => {
                return json(api::delete_vault(&service, &vault).await?);
            }
            ("GET", [_, "secret"]) => return json(api::list_secrets(&service, &vault).await?),
            ("POST", [_, "secret"]) => {
                return json(api::put_secret(&service, &vault, body(r)?).await?);
            }
            ("PUT", [_, "secret", _]) => {
                return json(api::update_secret(&service, &vault, body(r)?).await?);
            }
            ("DELETE", [_, "secret", _]) => {
                return json(api::delete_secret(&service, &vault).await?);
            }
            _ => {}
        }
    }
    if let ["agent", id, tail @ ..] = path.as_slice() {
        let agent = api::AgentPath {
            agent_id: id.parse()?,
        };
        match (method, tail) {
            ("GET", []) => return json(api::get_agent(&service, &agent).await?),
            ("DELETE", []) => return json(api::delete_agent(&service, &agent).await?),
            ("GET", ["artifact"]) => return json(api::list_artifacts(&service, &agent).await?),
            ("GET", ["artifact", "read"]) => {
                return json(api::read_artifact(&service, &agent, query(r)?).await?);
            }
            ("POST", ["artifact"]) => {
                return json(api::write_artifact(&service, &agent, body(r)?).await?);
            }
            ("GET", ["thread"]) => {
                return json(api::list_threads(&service, &agent, query(r)?).await?);
            }
            ("POST", ["thread"]) => {
                let agent = service.agent(agent.agent_id).await?;
                return json(api::create_thread(runtime, &agent, optional_body(r)?).await?);
            }
            _ => {}
        }
        if let ["thread", id, tail @ ..] = tail {
            let thread = api::ThreadPath {
                agent_id: agent.agent_id,
                thread_id: id.parse()?,
            };
            match (method, tail) {
                ("GET", []) => return json(api::get_thread(&service, &thread).await?),
                ("GET", ["previews"]) => {
                    api::get_thread(&service, &thread).await?;
                    // Workers do not host the native browser preview listener.
                    return json(None::<exo_managed_agents::http::protocol::PreviewEndpoint>);
                }
                ("DELETE", []) => return json(api::delete_thread(&service, &thread).await?),
                ("PUT", ["environment"]) => {
                    return json(
                        api::update_thread_environment(&service, &thread, body(r)?).await?,
                    );
                }
                ("POST", ["fork"]) => {
                    return json(api::fork_thread(&service, &thread, optional_body(r)?).await?);
                }
                ("GET", ["artifact"]) => {
                    return json(api::list_thread_artifacts(&service, &thread).await?);
                }
                ("GET", ["artifact", "read"]) => {
                    return json(api::read_thread_artifact(&service, &thread, query(r)?).await?);
                }
                ("GET", ["event"]) => {
                    return json(api::events(&service, &thread, query(r)?).await?);
                }
                ("POST", ["turn"]) => {
                    let agent = service.agent(thread.agent_id).await?;
                    let conversation = service.thread(agent.as_ref(), thread.thread_id).await?;
                    let prepared =
                        api::prepare_turn(runtime, agent.as_ref(), conversation.as_ref(), body(r)?)
                            .await?;
                    // WorkerModel only supports non-streaming responses; the
                    // TypeScript harness supplies its own model stream.
                    let streaming =
                        prepared.config.harness == executor::AgentHarnessKind::TypeScript;
                    let (result, stream) = api::submit_turn(
                        runtime,
                        agent.clone(),
                        conversation.clone(),
                        prepared,
                        streaming,
                    )
                    .await?;
                    let turn = result.turn.clone();
                    let progress = progress.clone();
                    host.spawn(Box::pin(async move {
                        api::forward_progress(stream, thread.thread_id, turn, progress).await;
                    }));
                    let mut response = json(result)?;
                    response.status = 202;
                    return Ok(response);
                }
                _ => {}
            }
            if let ["turn", id, tail @ ..] = tail {
                let turn = api::TurnPath {
                    agent_id: thread.agent_id,
                    thread_id: thread.thread_id,
                    turn_id: id.parse()?,
                };
                match (method, tail) {
                    ("GET", []) => return json(api::turn_status(&service, &turn).await?),
                    ("POST", ["approval-response"]) => {
                        return json(api::approval_response(&service, &turn, body(r)?).await?);
                    }
                    ("POST", ["cancel"]) => {
                        let agent = service.agent(turn.agent_id).await?;
                        service.thread(agent.as_ref(), turn.thread_id).await?;
                        return json(
                            api::cancel_turn(runtime, turn.agent_id, turn.thread_id, turn.turn_id)
                                .await?,
                        );
                    }
                    ("POST", ["frontend-tool-result"]) => bail!(api::UnsupportedRequest(
                        "this runtime does not execute frontend tools"
                    )),
                    _ => {}
                }
            }
        }
    }
    Err(api::RequestError {
        status: 404,
        message: "Not found".into(),
    }
    .into())
}
