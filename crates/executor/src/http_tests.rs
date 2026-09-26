use std::{net::TcpListener, ops::Bound, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use exo_managed_agents::{
    AgentBackend,
    http::{RuntimeClient, protocol::*},
};
use exoharness::{
    AgentHandle, AgentId, BasicExoHarness, EventData, ExoHarness, ThreadHandle, TurnHandle,
};
use futures::StreamExt;
use tokio::sync::Semaphore;

use crate::execution_tracing::TurnExecutionTrace;
use crate::harness::HarnessTurnKey;
use crate::harness_executor::{ExecutorStreamMode, HarnessExecutor};
use crate::http_service::{RuntimeHttpService, server};
use crate::{
    AgentConfig, AgentHarnessKind, ConversationConfig, CreateAgentRequest, HttpProvider,
    LocalProvider, Runtime, SandboxProvider, SendRequest,
};

struct ControlledExecutor(Arc<Semaphore>);

#[async_trait]
impl HarnessExecutor for ControlledExecutor {
    fn name(&self) -> &'static str {
        "basic"
    }

    fn agent_config(
        &self,
        definition: &exo_managed_agents::AgentDefinition,
    ) -> Result<AgentConfig> {
        crate::managed_agents::agent_config(definition, SandboxProvider::LocalProcess, None, None)
    }

    async fn execute_turn(
        &self,
        _: &dyn AgentHandle,
        thread: Arc<dyn ThreadHandle>,
        turn: Arc<dyn TurnHandle>,
        _: &AgentConfig,
        config: &ConversationConfig,
        request: &SendRequest,
        stream: ExecutorStreamMode<'_>,
        _: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        crate::permissions::authorize(
            thread.as_ref(),
            turn.as_ref(),
            config.permissions.for_tool("test_tool"),
            &exoharness::ToolRequest {
                function_name: "test_tool".into(),
                namespace: None,
                arguments: serde_json::from_value(serde_json::json!({"value": 42}))?,
            },
            stream,
        )
        .await?;
        self.0.acquire().await?.forget();
        if request.input.is_empty() {
            return Err(anyhow::anyhow!("test execution failure").context("dispatching test turn"));
        }
        if let ExecutorStreamMode::Enabled(stream) = stream {
            stream.send(Ok(crate::ExecutionStreamEvent::Chunk(
                lingua::UniversalStreamChunk::new(
                    Some("test-chunk".into()),
                    None,
                    vec![],
                    None,
                    None,
                ),
            )))?;
        }
        turn.add_events(vec![EventData::Messages {
            messages: request.input.clone(),
            usage: None,
            response_id: None,
        }])
        .await?;
        Ok(())
    }
}

struct Fixture {
    runtime: Arc<Runtime>,
    client: RuntimeClient,
    agent_id: AgentId,
    release: Arc<Semaphore>,
    server: actix_web::dev::ServerHandle,
}

impl Fixture {
    async fn new() -> Result<Self> {
        Self::with_scope(false).await
    }

    async fn with_scope(scoped: bool) -> Result<Self> {
        let state: Arc<dyn ExoHarness> = Arc::new(
            BasicExoHarness::in_memory(crate::test_support::local_test_config("unused-http-test"))
                .await?,
        );
        state
            .put_binding(exoharness::Binding::Llm {
                name: "test-model".into(),
                model: "test-model".into(),
                base_url: None,
                secret: None,
            })
            .await?;
        let release = Arc::new(Semaphore::new(0));
        let provider =
            LocalProvider::new(state, Arc::new(ControlledExecutor(Arc::clone(&release))));
        let runtime = Arc::new(Runtime::new(provider, None));
        let agent = runtime
            .create_agent(CreateAgentRequest {
                slug: "http-test".into(),
                name: None,
                harness: AgentHarnessKind::Basic,
                typescript: None,
                enable_agent_tool_creation: false,
                sandbox_image: None,
                sandbox_provider: SandboxProvider::LocalProcess,
                sandbox_scope: None,
                enable_networking: false,
                model: "test-model".into(),
                max_output_tokens: None,
                max_tool_round_trips: None,
                braintrust: None,
            })
            .await?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let client = RuntimeClient::new(&format!("http://{}/exo", listener.local_addr()?))?
            .with_bearer_token("test-token".into());
        let mut service = RuntimeHttpService::new(Arc::clone(&runtime), Some("test-token"))?;
        if scoped {
            service = service.for_agent(agent.record().id);
        }
        let server = server(listener, Arc::new(service))?;
        let handle = server.handle();
        actix_web::rt::spawn(server);
        Ok(Self {
            runtime,
            client,
            agent_id: agent.record().id,
            release,
            server: handle,
        })
    }

    async fn stop(self) -> Result<()> {
        self.runtime.shutdown().await?;
        self.server.stop(false).await;
        Ok(())
    }
}

#[actix_web::test]
async fn http_provider_auth_handles_and_exclusive_history_cursors() -> Result<()> {
    let f = Fixture::new().await?;
    for token in [None, Some("wrong-token")] {
        let mut client = RuntimeClient::new(f.client.endpoint().as_str())?;
        if let Some(token) = token {
            client = client.with_bearer_token(token.into());
        }
        assert!(
            client
                .list_agents(None)
                .await
                .unwrap_err()
                .to_string()
                .contains("401")
        );
    }
    let provider = HttpProvider::new(f.client.clone());
    let agents = provider.exoharness().list_agents().await?;
    assert_eq!(agents.len(), 1);
    let remote_agent = &agents[0];
    assert_eq!(remote_agent.record().id, f.agent_id);
    let thread = remote_agent.new_thread(Default::default()).await?;
    let id = thread.record().id;
    let fetched = f
        .client
        .get_thread(f.agent_id, id)
        .await?
        .context("direct thread lookup")?;
    assert_eq!(fetched.agent.id, f.agent_id);
    assert_eq!(fetched.thread.id, id);
    assert_eq!(
        remote_agent
            .get_thread(&id)
            .await?
            .context("remote thread lookup")?
            .record()
            .id,
        id
    );
    let missing = exoharness::Uuid7::now();
    assert!(f.client.get_thread(f.agent_id, missing).await?.is_none());
    assert!(remote_agent.get_thread(&missing).await?.is_none());
    let local = f
        .runtime
        .exoharness_handle()
        .get_agent(&f.agent_id)
        .await?
        .context("agent")?
        .get_thread(&id)
        .await?
        .context("thread")?;
    let ids = local
        .add_events(exoharness::AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: (0..205)
                .map(|i| EventData::Error {
                    message: format!("history {i}"),
                    metadata: None,
                })
                .collect(),
        })
        .await?
        .event_ids;
    let mut stream = thread.watch_events(Bound::Excluded(ids[0])).await?;
    let replayed = tokio::time::timeout(Duration::from_secs(5), async {
        let mut replayed = Vec::new();
        while replayed.len() < 204 {
            replayed.push(stream.next().await.context("stream ended")??.id);
        }
        Result::<_>::Ok(replayed)
    })
    .await??;
    assert_eq!(replayed, ids[1..]);
    drop(stream);
    let mut replay = f
        .client
        .watch(f.agent_id, id, &WatchQuery::default())
        .await?;
    let replayed = tokio::time::timeout(Duration::from_secs(5), async {
        let mut replayed = Vec::new();
        while replayed.len() < ids.len() {
            let event = replay.next().await.context("replay ended")??;
            if matches!(event.data, EventData::Error { .. }) {
                replayed.push(event.id);
            }
        }
        Result::<_>::Ok(replayed)
    })
    .await??;
    assert_eq!(replayed, ids);
    drop(replay);
    let page = thread
        .get_events(Some(exoharness::EventQuery {
            cursor: Some(ids[0]),
            limit: Some(2),
            types: Some(vec![exoharness::EventKind::ERROR]),
            ..Default::default()
        }))
        .await?;
    assert_eq!(
        page.events.iter().map(|e| e.id).collect::<Vec<_>>(),
        ids[1..3]
    );
    assert_eq!(page.cursor, Some(ids[2]));
    let foreign_agent = f
        .runtime
        .exoharness_handle()
        .new_agent(exoharness::NewAgentRequest {
            slug: "other".into(),
            name: "Other".into(),
            vaults: vec![],
        })
        .await?;
    assert!(
        f.client
            .get_thread(foreign_agent.record().id, id)
            .await?
            .is_none()
    );
    assert!(
        f.client
            .get_thread(exoharness::Uuid7::now(), id)
            .await?
            .is_none()
    );
    assert!(
        f.client
            .events(foreign_agent.record().id, id, &Default::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("404")
    );
    assert_eq!(provider.exoharness().list_vaults().await?.len(), 1);
    let fork = thread
        .fork(exoharness::ForkThreadRequest {
            name: Some("fork".into()),
            ..Default::default()
        })
        .await?;
    assert_ne!(fork.record().id, id);
    assert!(remote_agent.delete_thread(&fork.record().id).await?);
    local
        .add_events(exoharness::AddEventsRequest {
            data: (0..205)
                .map(|i| EventData::Messages {
                    messages: vec![crate::harness_helpers::user_message(&format!(
                        "message {i}"
                    ))],
                    usage: None,
                    response_id: None,
                })
                .collect(),
            session_id: None,
            turn_id: None,
        })
        .await?;
    assert_eq!(
        crate::materialize_conversation_messages(thread.as_ref())
            .await?
            .len(),
        205
    );
    assert!(
        f.client
            .list_thread_artifacts(foreign_agent.record().id, id)
            .await
            .unwrap_err()
            .to_string()
            .contains("404")
    );
    for (agent_id, thread_id, status) in [
        (exoharness::Uuid7::now(), id, "404"),
        (f.agent_id, exoharness::Uuid7::now(), "404"),
        (foreign_agent.record().id, id, "404"),
        (f.agent_id, id, "400"),
    ] {
        let error = f
            .client
            .approval_response(
                agent_id,
                thread_id,
                exoharness::Uuid7::now(),
                &ApprovalResponseBody {
                    session_id: exoharness::Uuid7::now(),
                    approval_id: exoharness::Uuid7::now().to_string(),
                    approved: true,
                    allow_for_tool: false,
                },
            )
            .await
            .expect_err("approval response requires an active turn");
        assert!(error.to_string().contains(status), "{error}");
    }
    f.stop().await
}

#[actix_web::test]
async fn saved_http_turn_survives_disconnect_and_replays_completion() -> Result<()> {
    let f = Fixture::new().await?;
    let thread = f
        .client
        .create_thread(f.agent_id, &Default::default())
        .await?
        .thread;
    let mut stream = f
        .client
        .watch(f.agent_id, thread.id, &Default::default())
        .await?;
    let body = SubmitTurnBody::<()> {
        input: Some(OneOrMany::One(crate::harness_helpers::user_message(
            "hello",
        ))),
        ..Default::default()
    };
    let receipt = f.client.submit_turn(f.agent_id, thread.id, &body).await?;
    let cursor = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = stream.next().await.context("stream ended")??;
            if event.turn_id == Some(receipt.turn.id)
                && matches!(event.data, EventData::TurnStarted { .. })
            {
                return Result::<_>::Ok(event.id);
            }
        }
    })
    .await??;
    drop(stream);
    f.release.add_permits(1);
    let mut resumed = f
        .client
        .watch(
            f.agent_id,
            thread.id,
            &WatchQuery {
                after: Some(cursor),
            },
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = resumed.next().await.context("stream ended")??;
            assert_ne!(event.id, cursor);
            assert!(!matches!(event.data, EventData::Error { .. }));
            if matches!(event.data, EventData::TurnEnded) {
                return Result::<_>::Ok(());
            }
        }
    })
    .await??;
    drop(resumed);
    let history = f
        .client
        .events(f.agent_id, thread.id, &Default::default())
        .await?
        .events;
    assert!(
        history
            .iter()
            .any(|e| e.turn_id == Some(receipt.turn.id) && matches!(e.data, EventData::TurnEnded))
    );
    assert!(
        history
            .iter()
            .all(|e| !matches!(e.data, EventData::LinguaStreamChunk { .. }))
    );
    let second = f
        .client
        .submit_turn(f.agent_id, thread.id, &SubmitTurnBody::<()>::default())
        .await?;
    assert!(
        f.client
            .cancel_turn(f.agent_id, thread.id, second.turn.id)
            .await?
            .canceled_active_turn
    );
    let mut cancelled = f
        .client
        .watch(
            f.agent_id,
            thread.id,
            &WatchQuery {
                after: history.last().map(|e| e.id),
            },
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut failed = false;
        loop {
            let event = cancelled.next().await.context("stream ended")??;
            if event.turn_id != Some(second.turn.id) {
                continue;
            }
            match event.data {
                EventData::Error { .. } => failed = true,
                EventData::TurnEnded => {
                    assert!(failed);
                    return Result::<_>::Ok(());
                }
                _ => {}
            }
        }
    })
    .await??;
    drop(cancelled);
    f.stop().await
}

#[actix_web::test]
async fn local_and_http_providers_use_the_same_runtime_contract() -> Result<()> {
    let f = Fixture::new().await?;
    let provider = HttpProvider::new(f.client.clone());
    let remote = Arc::new(Runtime::new(provider, None));
    for runtime in [&f.runtime, &remote] {
        let agent = runtime
            .get_agent(&f.agent_id.to_string())
            .await?
            .context("agent")?;
        let thread = agent.new_thread(Default::default()).await?;
        let request = SendRequest {
            input: vec![crate::harness_helpers::user_message("hello")],
            session_id: None,
        };
        let (turn, mut stream) = runtime
            .start_turn(agent.clone(), thread.clone(), request.clone(), true, None)
            .await?;
        f.release.add_permits(1);
        let completed = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                if let crate::ExecutionStreamEvent::Completed(result) = event? {
                    return Ok(result);
                }
            }
            bail!("turn did not complete")
        })
        .await??;
        assert_eq!(completed.turn_id, turn.id);
        assert_eq!(completed.session_id, turn.session_id);
        let history = thread.get_events(None).await?.events;
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(event.data, EventData::TurnStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(event.data, EventData::TurnEnded))
                .count(),
            1
        );
        assert_eq!(
            history.last().context("terminal event")?.id,
            completed.latest_event_id
        );

        let (turn, mut stream) = runtime
            .start_turn(agent, thread.clone(), request, true, None)
            .await?;
        runtime
            .cancel(HarnessTurnKey::new(thread.record().id, turn.id))
            .await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                if let Err(error) = event {
                    assert!(error.to_string().contains("cancel"));
                    return Ok(());
                }
            }
            bail!("cancellation did not finish the turn")
        })
        .await??;
    }
    remote.shutdown().await?;
    f.stop().await
}

#[actix_web::test]
async fn managed_agents_created_locally_resume_over_http() -> Result<()> {
    let f = Fixture::new().await?;
    let state = f.runtime.exoharness_handle();
    state
        .put_binding(exoharness::Binding::Llm {
            name: "test-model".into(),
            model: "test-model".into(),
            base_url: None,
            secret: None,
        })
        .await?;
    let existing = state.get_agent(&f.agent_id).await?.context("agent")?;
    let config = crate::load_agent_config(existing.as_ref()).await?;
    let local = Runtime::new(
        LocalProvider::new(state, Arc::new(ControlledExecutor(Arc::clone(&f.release))))
            .with_managed_agents(crate::managed_agents::LocalAgentSetup {
                agent: Some(config),
                ..Default::default()
            }),
        None,
    );
    let remote = Runtime::new(HttpProvider::new(f.client.clone()), None);
    let definition = exo_managed_agents::AgentDefinition::parse(
        "---\nname: shared-agent\nharness: basic\nconfig:\n  model: test-model\n---\n\nKeep the saved instructions.".into(),
    )?;
    let saved = local
        .create_managed_agent(&definition, "local-managed")
        .await?;
    for (runtime, slug) in [(&local, "local-thread"), (&remote, "http-thread")] {
        let agent = runtime
            .get_agent(&saved.record().id.to_string())
            .await?
            .context("managed agent")?;
        assert_eq!(
            exo_managed_agents::load_definition(agent.as_ref())
                .await?
                .context("definition")?
                .source(),
            definition.source()
        );
        let opened = runtime
            .open_managed_thread(
                &agent,
                None,
                exoharness::NewThreadRequest {
                    slug: Some(slug.into()),
                    ..Default::default()
                },
            )
            .await?;
        assert!(opened.created);
        let resumed = runtime
            .open_managed_thread(&agent, Some(slug), Default::default())
            .await?;
        assert!(!resumed.created);
        assert_eq!(resumed.thread.record().id, opened.thread.record().id);
        assert_eq!(
            exo_managed_agents::list_threads(agent.as_ref())
                .await?
                .len(),
            1
        );
        let (turn, mut stream) = runtime
            .start_turn(
                agent.clone(),
                resumed.thread.clone(),
                SendRequest {
                    input: vec![crate::harness_helpers::user_message("hello")],
                    session_id: None,
                },
                true,
                None,
            )
            .await?;
        f.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                if let crate::ExecutionStreamEvent::Completed(result) = event? {
                    assert_eq!(result.turn_id, turn.id);
                    return Ok(());
                }
            }
            bail!("turn did not complete")
        })
        .await??;
        runtime
            .end_session(resumed.thread.as_ref(), turn.session_id)
            .await?;
        assert!(agent.delete_thread(&resumed.thread.record().id).await?);
    }
    assert!(remote.delete_agent(&saved.record().id.to_string()).await?);
    let uploaded = remote
        .create_managed_agent(&definition, "http-managed")
        .await?;
    assert_eq!(
        exo_managed_agents::load_definition(uploaded.as_ref())
            .await?
            .context("uploaded definition")?
            .source(),
        definition.source()
    );
    assert!(
        remote
            .delete_agent(&uploaded.record().id.to_string())
            .await?
    );
    local.shutdown().await?;
    remote.shutdown().await?;
    f.stop().await
}

#[actix_web::test]
async fn http_runtime_preserves_server_failure_and_turn_id() -> Result<()> {
    let f = Fixture::new().await?;
    let runtime = Runtime::new(HttpProvider::new(f.client.clone()), None);
    let agent = runtime
        .get_agent(&f.agent_id.to_string())
        .await?
        .context("agent")?;
    let thread = agent.new_thread(Default::default()).await?;
    let (turn, mut stream) = runtime
        .start_turn(
            agent,
            thread.clone(),
            SendRequest {
                input: vec![],
                session_id: None,
            },
            true,
            None,
        )
        .await?;
    f.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if let Err(error) = event {
                assert_eq!(
                    error.to_string(),
                    "dispatching test turn: test execution failure"
                );
                return Ok(());
            }
        }
        bail!("missing failure")
    })
    .await??;
    let events: Vec<_> = thread
        .get_events(None)
        .await?
        .events
        .into_iter()
        .filter(|event| event.turn_id == Some(turn.id))
        .collect();
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match &event.data {
                EventData::Error { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["dispatching test turn: test execution failure"],
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.data, EventData::TurnEnded))
            .count(),
        1
    );
    runtime.shutdown().await?;
    f.stop().await
}

#[actix_web::test]
async fn approval_decisions_cancellation_sessions_and_reconnect() -> Result<()> {
    for remote in [false, true] {
        let f = Fixture::with_scope(true).await?;
        let runtime = if remote {
            Arc::new(Runtime::new(HttpProvider::new(f.client.clone()), None))
        } else {
            f.runtime.clone()
        };
        let agent = runtime
            .exoharness_handle()
            .get_agent(&f.agent_id)
            .await?
            .context("agent")?;
        let thread = runtime
            .open_managed_thread(&agent, None, Default::default())
            .await?
            .thread;
        let local_agent = f
            .runtime
            .exoharness_handle()
            .get_agent(&f.agent_id)
            .await?
            .context("agent")?;
        let local_thread = local_agent
            .get_thread(&thread.record().id)
            .await?
            .context("thread")?;
        let mut config = f
            .runtime
            .get_conversation_config(local_thread.as_ref())
            .await?;
        config.permissions.permission_policy =
            exo_managed_agents::permissions::PermissionPolicy::AlwaysAsk {};
        f.runtime
            .put_conversation_config(local_thread.as_ref(), config)
            .await?;
        let mut session = None;
        for action in ["deny", "allow_session", "already_allowed", "cancel"] {
            if action == "cancel" {
                session = None;
            }
            f.release.add_permits(1);
            let permits = f.release.available_permits();
            let (turn, mut stream) = runtime
                .start_turn(
                    agent.clone(),
                    thread.clone(),
                    SendRequest {
                        input: vec![crate::harness_helpers::user_message("perform tool")],
                        session_id: session,
                    },
                    true,
                    None,
                )
                .await?;
            session = Some(turn.session_id);
            if action != "already_allowed" {
                let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
                    .await?
                    .context("approval event")??;
                let crate::ExecutionStreamEvent::ApprovalRequested { approval, .. } = first else {
                    panic!("expected approval, got {first:?}")
                };
                assert_eq!(
                    f.release.available_permits(),
                    permits,
                    "tool ran before approval"
                );
                if remote {
                    drop(stream);
                    let (_, resumed) = runtime
                        .reconnect_turn(thread.as_ref())
                        .await?
                        .context("saved active turn")?;
                    stream = resumed;
                    let crate::ExecutionStreamEvent::ApprovalRequested {
                        approval: replay, ..
                    } = stream.next().await.context("pending approval")??
                    else {
                        panic!("pending approval was not replayed")
                    };
                    assert_eq!(approval.approval_id, replay.approval_id);
                }
                let mut body = ApprovalResponseBody {
                    session_id: exoharness::Uuid7::now(),
                    approval_id: approval.approval_id,
                    approved: action != "deny",
                    allow_for_tool: action == "allow_session",
                };
                assert!(
                    runtime
                        .approval_response(f.agent_id, thread.record().id, turn.id, &body)
                        .await
                        .is_err()
                );
                body.session_id = turn.session_id;
                assert!(
                    runtime
                        .approval_response(
                            f.agent_id,
                            thread.record().id,
                            exoharness::Uuid7::now(),
                            &body
                        )
                        .await
                        .is_err()
                );
                if action == "cancel" {
                    runtime
                        .cancel(HarnessTurnKey::new(thread.record().id, turn.id))
                        .await?;
                    assert!(
                        runtime
                            .approval_response(f.agent_id, thread.record().id, turn.id, &body)
                            .await
                            .is_err()
                    );
                } else {
                    runtime
                        .approval_response(f.agent_id, thread.record().id, turn.id, &body)
                        .await?;
                    assert!(
                        runtime
                            .approval_response(f.agent_id, thread.record().id, turn.id, &body)
                            .await
                            .is_err(),
                        "duplicate approval accepted"
                    );
                }
            }
            let mut failed = false;
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(crate::ExecutionStreamEvent::ApprovalRequested { .. }) => {
                            panic!("unexpected second approval")
                        }
                        Err(_) => failed = true,
                        _ => {}
                    }
                }
            })
            .await?;
            assert_eq!(failed, matches!(action, "deny" | "cancel"));
            assert_eq!(
                f.release.available_permits(),
                permits - usize::from(matches!(action, "allow_session" | "already_allowed"))
            );
            assert!(runtime.reconnect_turn(thread.as_ref()).await?.is_none());
        }
        if remote {
            runtime.shutdown().await?;
        }
        f.stop().await?;
    }
    Ok(())
}

#[actix_web::test]
async fn saved_policy_changes_apply_to_existing_local_and_http_threads() -> Result<()> {
    for remote in [false, true] {
        let f = Fixture::new().await?;
        let runtime = if remote {
            Arc::new(Runtime::new(HttpProvider::new(f.client.clone()), None))
        } else {
            f.runtime.clone()
        };
        let agent = runtime
            .get_agent(&f.agent_id.to_string())
            .await?
            .context("agent")?;
        let local_agent = f
            .runtime
            .get_agent(&f.agent_id.to_string())
            .await?
            .context("local agent")?;
        let thread = runtime
            .open_managed_thread(&agent, None, Default::default())
            .await?
            .thread;
        for ask in [false, true, false] {
            let policy = if ask { "always_ask" } else { "always_allow" };
            local_agent.write_artifact(exoharness::WriteArtifactRequest {
                path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
                contents: format!("---\nname: Policy test\nharness: basic\npermission_policy: {{type: {policy}}}\nconfig:\n  model: test-model\n---\nUse tools.").into_bytes(),
            }).await?;
            f.release.add_permits(1);
            let (turn, mut stream) = runtime
                .start_turn(
                    agent.clone(),
                    thread.clone(),
                    SendRequest {
                        input: vec![crate::harness_helpers::user_message("perform tool")],
                        session_id: None,
                    },
                    true,
                    None,
                )
                .await?;
            let mut saw_approval = false;
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(event) = stream.next().await {
                    match event? {
                        crate::ExecutionStreamEvent::ApprovalRequested { approval, .. } => {
                            assert!(
                                ask && !saw_approval,
                                "unexpected approval after setting {policy}"
                            );
                            saw_approval = true;
                            assert_eq!(
                                f.release.available_permits(),
                                1,
                                "tool ran before approval"
                            );
                            runtime
                                .approval_response(
                                    f.agent_id,
                                    thread.record().id,
                                    turn.id,
                                    &ApprovalResponseBody {
                                        session_id: turn.session_id,
                                        approval_id: approval.approval_id,
                                        approved: true,
                                        allow_for_tool: false,
                                    },
                                )
                                .await?;
                        }
                        crate::ExecutionStreamEvent::Completed(_) => return Ok(()),
                        _ => {}
                    }
                }
                bail!("turn did not complete")
            })
            .await??;
            assert_eq!(saw_approval, ask, "stale policy after setting {policy}");
        }
        if remote {
            runtime.shutdown().await?;
        }
        f.stop().await?;
    }
    Ok(())
}

#[actix_web::test]
async fn reconnect_skips_orphaned_turns_and_follows_live_turns() -> Result<()> {
    let f = Fixture::new().await?;
    let remote = Arc::new(Runtime::new(HttpProvider::new(f.client.clone()), None));
    let local_agent = f
        .runtime
        .get_agent(&f.agent_id.to_string())
        .await?
        .context("agent")?;
    for runtime in [&f.runtime, &remote] {
        let agent = runtime
            .get_agent(&f.agent_id.to_string())
            .await?
            .context("agent")?;
        for pending in [false, true] {
            let local_thread = local_agent.new_thread(Default::default()).await?;
            let orphan = local_thread
                .begin_turn(exoharness::BeginTurnRequest::default())
                .await?;
            if pending {
                orphan
                    .add_events(vec![EventData::Custom {
                        event_type: crate::permissions::APPROVAL_REQUESTED.into(),
                        payload: serde_json::to_value(crate::permissions::ApprovalRequest {
                            approval_id: "orphaned-approval".into(),
                            request: exoharness::ToolRequest {
                                namespace: None,
                                function_name: "test_tool".into(),
                                arguments: Default::default(),
                            },
                        })?,
                    }])
                    .await?;
            }
            let thread = runtime
                .get_conversation(agent.as_ref(), &local_thread.record().id.to_string())
                .await?
                .context("thread")?;
            assert!(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    runtime.reconnect_turn(thread.as_ref())
                )
                .await??
                .is_none()
            );
            let (turn, original) = runtime
                .start_turn(
                    agent.clone(),
                    thread.clone(),
                    SendRequest {
                        input: vec![crate::harness_helpers::user_message(
                            "continue after restart",
                        )],
                        session_id: None,
                    },
                    true,
                    None,
                )
                .await?;
            let (reconnected, mut events) = tokio::time::timeout(
                Duration::from_secs(5),
                runtime.reconnect_turn(thread.as_ref()),
            )
            .await??
            .context("live turn")?;
            assert_eq!(reconnected.id, turn.id);
            f.release.add_permits(1);
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(event) = events.next().await {
                    if let crate::ExecutionStreamEvent::Completed(result) = event? {
                        assert_eq!(result.turn_id, turn.id);
                        assert!(events.next().await.is_none());
                        return Ok(());
                    }
                }
                bail!("live reconnect did not finish")
            })
            .await??;
            drop(original);
            assert!(runtime.reconnect_turn(thread.as_ref()).await?.is_none());
        }
    }
    remote.shutdown().await?;
    f.stop().await
}

#[actix_web::test]
async fn canceled_http_observer_does_not_orphan_the_next_accepted_turn() -> Result<()> {
    let f = Fixture::new().await?;
    let provider = HttpProvider::new(f.client.clone());
    let thread = f
        .client
        .create_thread(f.agent_id, &Default::default())
        .await?
        .thread;
    let body = SubmitTurnBody {
        input: Some(OneOrMany::One(crate::harness_helpers::user_message(
            "hello",
        ))),
        ..Default::default()
    };
    let (_, mut abandoned) = provider
        .send_stream(f.agent_id, thread.id, body.clone())
        .await?;
    crate::harness::Harness::shutdown(&provider).await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), abandoned.next())
            .await?
            .is_none()
    );
    f.release.add_permits(2);
    let (turn, mut stream) = provider.send_stream(f.agent_id, thread.id, body).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            if let crate::ExecutionStreamEvent::Completed(result) = event? {
                assert_eq!(result.turn_id, turn.id);
                return Ok(());
            }
        }
        bail!("accepted turn lost its observer")
    })
    .await??;
    crate::harness::Harness::shutdown(&provider).await?;
    f.stop().await
}

#[actix_web::test]
async fn rejected_agent_update_restores_the_saved_definition() -> Result<()> {
    let f = Fixture::new().await?;
    let source = "---\nname: restored-agent\nharness: basic\nconfig:\n  model: test-model\n---\n\nKeep the saved instructions.";
    let request = |source: &str| exoharness::WriteArtifactRequest {
        path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
        contents: source.as_bytes().to_vec(),
    };
    let agent = f
        .runtime
        .get_agent(&f.agent_id.to_string())
        .await?
        .context("agent")?;
    let invalid = source.replace("harness: basic", "harness: nonexistent-harness");
    assert!(
        f.client
            .write_agent_artifact(f.agent_id, &request(&invalid))
            .await
            .is_err()
    );
    assert!(
        exo_managed_agents::load_definition(agent.as_ref())
            .await?
            .is_none()
    );
    assert_eq!(
        crate::load_agent_config(agent.as_ref()).await?.model,
        "test-model"
    );
    f.client
        .write_agent_artifact(f.agent_id, &request(source))
        .await?;
    assert_eq!(
        f.runtime.get_agent_config(agent.as_ref()).await?.model,
        "test-model"
    );
    let invalid = source.replace("harness: basic", "harness: nonexistent-harness");
    let error = f
        .client
        .write_agent_artifact(f.agent_id, &request(&invalid))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("unknown harness: nonexistent-harness"));
    assert_eq!(
        exo_managed_agents::load_definition(agent.as_ref())
            .await?
            .context("restored definition")?
            .source(),
        source,
    );
    assert_eq!(
        crate::load_agent_config(agent.as_ref()).await?.model,
        "test-model"
    );
    f.client
        .create_thread(
            f.agent_id,
            &CreateThreadBody {
                harness: Some("basic".into()),
                ..Default::default()
            },
        )
        .await?;
    f.stop().await
}
