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
        _: Arc<dyn ThreadHandle>,
        turn: Arc<dyn TurnHandle>,
        _: &AgentConfig,
        _: &ConversationConfig,
        request: &SendRequest,
        stream: ExecutorStreamMode<'_>,
        _: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
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
        let state: Arc<dyn ExoHarness> = Arc::new(
            BasicExoHarness::in_memory(
                crate::test_support::local_test_config("unused-http-test"),
                None,
            )
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
        let server = server(
            listener,
            Arc::new(RuntimeHttpService::new(
                Arc::clone(&runtime),
                "test-token",
                crate::test_support::local_test_config("unused-http-test"),
            )?),
        )?;
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
async fn temporary_provider_cleanup_waits_for_turn_finalization() -> Result<()> {
    let f = Fixture::new().await?;
    let runtime = Runtime::new(
        LocalProvider::new(
            f.runtime.exoharness_handle(),
            Arc::new(ControlledExecutor(Arc::clone(&f.release))),
        )
        .with_managed_agents(crate::managed_agents::LocalAgentSetup {
            temporary: true,
            ..Default::default()
        }),
        None,
    );
    let agent = runtime
        .get_agent(&f.agent_id.to_string())
        .await?
        .context("agent")?;
    let thread = agent.new_thread(Default::default()).await?;
    let (_, mut stream) = runtime
        .start_turn(
            agent,
            thread,
            SendRequest {
                input: vec![crate::harness_helpers::user_message("hello")],
                session_id: None,
            },
            true,
            None,
        )
        .await?;
    runtime.shutdown().await?;
    let error = stream
        .next()
        .await
        .context("finalization result")?
        .err()
        .context("cancelled turn")?;
    assert_eq!(error.to_string(), "harness turn cancelled");
    assert!(runtime.list_agents().await?.is_empty());
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
