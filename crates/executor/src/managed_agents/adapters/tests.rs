use super::*;
use async_trait::async_trait;
use exo_managed_agents::{AgentDefinition, identity::IdentityCommand};
use exoharness::{AgentHandle, EventData, ExoHarness, Storage, ThreadHandle, TurnHandle};
use lingua::{Message, universal::AssistantContent};
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, path},
};

#[derive(Clone, Default)]
struct Answer(Arc<AtomicUsize>, Arc<AtomicUsize>);

#[async_trait]
impl crate::harness_executor::HarnessExecutor for Answer {
    fn name(&self) -> &'static str {
        "basic"
    }
    fn with_state(
        &self,
        _: Arc<dyn ExoHarness>,
    ) -> Result<Arc<dyn crate::harness_executor::HarnessExecutor>> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(self.clone()))
    }
    fn agent_config(&self, definition: &AgentDefinition) -> Result<crate::AgentConfig> {
        crate::managed_agents::agent_config(
            definition,
            exoharness::SandboxProvider::LocalProcess,
            None,
            None,
        )
    }
    async fn execute_turn(
        &self,
        _: &dyn AgentHandle,
        _: Arc<dyn ThreadHandle>,
        turn: Arc<dyn TurnHandle>,
        _: &crate::AgentConfig,
        _: &crate::ConversationConfig,
        _: &SendRequest,
        _: crate::harness_executor::ExecutorStreamMode<'_>,
        _: Option<&dyn crate::execution_tracing::TurnExecutionTrace>,
    ) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        turn.add_events(vec![EventData::Messages {
            messages: vec![Message::Assistant {
                content: AssistantContent::String("2.".into()),
                id: None,
            }],
            usage: None,
            response_id: None,
        }])
        .await?;
        Ok(())
    }
}

struct FailingStorage {
    inner: Arc<dyn Storage>,
    fail: Mutex<Option<String>>,
    fail_after: Mutex<Option<String>>,
    reads: AtomicUsize,
}
#[async_trait]
impl Storage for FailingStorage {
    async fn put(&self, key: &str, bytes: Vec<u8>, blob: bool) -> Result<()> {
        {
            let mut fail = self.fail.lock().unwrap();
            if fail.as_deref() == Some(key) {
                *fail = None;
                anyhow::bail!("injected storage failure");
            }
        }
        self.inner.put(key, bytes, blob).await?;
        let mut fail = self.fail_after.lock().unwrap();
        if fail.as_deref() == Some(key) {
            *fail = None;
            anyhow::bail!("injected failure after commit");
        }
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner.list(prefix).await
    }
    async fn list_after(&self, prefix: &str, after: Option<&str>) -> Result<Vec<String>> {
        self.inner.list_after(prefix, after).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key).await
    }
    async fn copy(&self, source: &str, destination: &str) -> Result<()> {
        self.inner.copy(source, destination).await
    }
}

struct Fixture {
    service: Arc<AdapterService>,
    deployment: Deployment,
    storage: Arc<FailingStorage>,
    answer: Answer,
    slack: MockServer,
}
impl Fixture {
    async fn new() -> Result<Self> {
        Self::with_enterprise(None).await
    }
    async fn with_enterprise(enterprise: Option<&str>) -> Result<Self> {
        let state = Arc::new(
            exoharness::BasicExoHarness::in_memory(crate::test_support::local_test_config(
                "unused-slack-test",
            ))
            .await?,
        );
        state.create_vault("support-slack").await?;
        let answer = Answer::default();
        let root = Arc::new(Runtime::new(
            crate::LocalProvider::new(state.clone(), Arc::new(answer.clone())),
            None,
        ));
        let definition = AgentDefinition::parse("---\nharness: basic\nmodel:\n  name: test\nadapters:\n  - type: slack\n    transport: http\n    vault: support-slack\n    use_sender_vault: true\n---\nInvestigate.\n".into())?;
        let agent = root
            .create_managed_agent(&definition, "support", "support")
            .await?;
        let mut service = AdapterService::new(root, None, Some(agent.record().id)).await?;
        let storage = Arc::new(FailingStorage {
            inner: state.provider_storage()?,
            fail: Mutex::default(),
            fail_after: Mutex::default(),
            reads: AtomicUsize::default(),
        });
        Arc::get_mut(&mut service).unwrap().store = AdapterStore::new(storage.clone());
        let slack = MockServer::start().await;
        for (method, response) in [
            (
                "auth.test",
                serde_json::json!({"ok":true,"team":"Exo","team_id":"T1","user_id":"UBOT","bot_id":"B1","url":"https://exo.slack.com","enterprise_id":enterprise}),
            ),
            (
                "bots.info",
                serde_json::json!({"ok":true,"bot":{"name":"Exo","app_id":"A1","id":"B1"}}),
            ),
            ("reactions.add", serde_json::json!({"ok":true})),
            (
                "users.info",
                serde_json::json!({"ok":true,"user":{"id":"UALICE","team_id":"T1","name":"alice","is_bot":false,"is_stranger":false}}),
            ),
            (
                "assistant.threads.setStatus",
                serde_json::json!({"ok":true}),
            ),
            (
                "chat.postMessage",
                serde_json::json!({"ok":true,"channel":"D1","ts":"2.0","message":{"ts":"2.0","text":"2."}}),
            ),
            (
                "chat.startStream",
                serde_json::json!({"ok":true,"channel":"D1","ts":"2.0"}),
            ),
            (
                "chat.appendStream",
                serde_json::json!({"ok":true,"channel":"D1","ts":"2.0"}),
            ),
            (
                "chat.stopStream",
                serde_json::json!({"ok":true,"channel":"D1","ts":"2.0"}),
            ),
        ] {
            Mock::given(path(format!("/api/{method}")))
                .and(header("authorization", "Bearer bot-token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(response))
                .mount(&slack)
                .await;
        }
        slack::initialize_tls()?;
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()?
            .https_or_http()
            .enable_http1()
            .build();
        let client = Arc::new(slack_morphism::SlackClient::new(
            slack_morphism::hyper_tokio::SlackClientHyperConnector::with_connector(connector)
                .with_slack_api_url(&format!("{}/api", slack.uri())),
        ));
        let AdapterDefinition::Slack(config) = definition.frontmatter.adapters[0].clone();
        let id = AdapterStore::deployment(agent.record().id, "slack");
        let connection = slack::SlackConnection::connect_with_client(
            &service,
            &id,
            config,
            &BTreeMap::from([("bot-token".into(), "bot-token".into())]),
            client,
        )
        .await?;
        let deployment = Deployment {
            id: id.clone(),
            agent: agent.record().id,
            definition: definition.frontmatter.adapters[0].clone(),
            fingerprint: "test".into(),
            connection,
        };
        service
            .deployments
            .lock()
            .unwrap()
            .insert(id, deployment.clone());
        let weak = Arc::downgrade(&service);
        service
            .root
            .set_recovery_resolver(Arc::new(move |principal| {
                let weak = weak.clone();
                Box::pin(async move {
                    weak.upgrade()
                        .context("test service stopped")?
                        .recover_caller(&principal)
                        .await
                })
            }))?;
        Ok(Self {
            service,
            deployment,
            storage,
            answer,
            slack,
        })
    }
    fn input(&self, sender: &str, ts: &str) -> IncomingMessage {
        IncomingMessage {
            id: format!("T1/D1/{ts}"),
            account: "T1/UBOT".into(),
            conversation: "T1/D1".into(),
            sender: sender.into(),
            text: "What is 1+1?".into(),
            channel: "D1".into(),
            message_ts: ts.into(),
            reply_ts: None,
            direct: true,
            external: false,
        }
    }
    async fn inbox(&self, message: &IncomingMessage) -> Result<Inbox> {
        self.service
            .store
            .get(&AdapterStore::inbox(&self.deployment.id, &message.id))
            .await?
            .context("input receipt")
    }
    async fn private(&self, name: &str, email: &str) -> Result<exoharness::vault::VaultId> {
        let vault = self
            .service
            .root
            .exoharness_handle()
            .create_vault(name)
            .await?;
        self.service
            .identities
            .execute(IdentityCommand::BindVault {
                vault: name.into(),
                email: email.into(),
            })
            .await?;
        Ok(vault.record().id)
    }
    async fn pending_turn(
        &self,
        message: &IncomingMessage,
    ) -> Result<(Inbox, Arc<dyn ThreadHandle>, Arc<dyn TurnHandle>)> {
        self.service
            .accept(&self.deployment, message.clone(), None)
            .await?;
        let agent = self
            .service
            .root
            .exoharness_handle()
            .get_agent(&self.deployment.agent)
            .await?
            .unwrap();
        let thread = agent.new_thread(Default::default()).await?;
        let turn = thread
            .begin_turn(exoharness::BeginTurnRequest {
                turn: exoharness::test_support::new_test_turn_record(),
                new_session: true,
                input: Vec::new(),
                initial_events: Vec::new(),
            })
            .await?;
        let mut inbox = self.inbox(message).await?;
        inbox.thread = Some(thread.record().id);
        inbox.turn = Some(turn.record().clone());
        self.service.save_inbox(&self.deployment, &inbox).await?;
        Ok((inbox, thread, turn))
    }
}

#[tokio::test]
async fn observer_errors_reconnect_but_failed_completion_closes_the_input() -> Result<()> {
    let f = Fixture::new().await?;
    let message = f.input("UALICE", "1.0");
    let (mut inbox, thread, turn) = f.pending_turn(&message).await?;
    for _ in 0..=INVALID_PROGRESS_RETRY_LIMIT {
        f.service.streams.lock().unwrap().insert(
            stream_key(&f.deployment, &inbox),
            crate::ExecutionStreamHandle::new(futures::stream::iter(vec![Err(anyhow::anyhow!(
                "work retained for recovery"
            ))])),
        );
        f.service.drain_stream(&f.deployment, &mut inbox).await?;
        assert!(!inbox.complete);
        assert!(inbox.error.is_none());
    }
    turn.add_events(vec![EventData::Messages {
        messages: vec![Message::Assistant {
            content: AssistantContent::String("Recovered answer.".into()),
            id: None,
        }],
        usage: None,
        response_id: None,
    }])
    .await?;
    turn.finish().await?;
    let stream = f
        .service
        .root
        .watch_turn(thread.as_ref(), turn.record().clone(), inbox.cursor)
        .await?;
    f.service
        .streams
        .lock()
        .unwrap()
        .insert(stream_key(&f.deployment, &inbox), stream);
    f.service.drain_stream(&f.deployment, &mut inbox).await?;
    assert!(inbox.complete);
    assert_eq!(inbox.final_text.as_deref(), Some("Recovered answer."));
    f.service.finish_stream(&f.deployment, &mut inbox).await?;
    f.service.deliver(&f.deployment).await?;
    let failed = f.input("UALICE", "3.0");
    let (mut inbox, _, turn) = f.pending_turn(&failed).await?;
    turn.finish().await?;
    f.service.streams.lock().unwrap().insert(
        stream_key(&f.deployment, &inbox),
        crate::ExecutionStreamHandle::new(futures::stream::iter(vec![Err(
            crate::turn_queue::TurnFailed("executor failed".into()).into(),
        )])),
    );
    f.service.drain_stream(&f.deployment, &mut inbox).await?;
    assert!(inbox.complete);
    assert_eq!(
        inbox.error,
        Some(InboxError::TurnFailed("executor failed".into()))
    );
    f.service.root.shutdown().await
}

#[tokio::test]
async fn reconnecting_after_an_error_checkpoint_preserves_failed_completion() -> Result<()> {
    let f = Fixture::new().await?;
    let message = f.input("UALICE", "1.0");
    let (mut inbox, thread, turn) = f.pending_turn(&message).await?;
    turn.add_events(vec![
        EventData::Error {
            message: "model failed".into(),
            metadata: None,
        },
        EventData::Messages {
            messages: vec![Message::Assistant {
                content: AssistantContent::String("Partial answer.".into()),
                id: None,
            }],
            usage: None,
            response_id: None,
        },
    ])
    .await?;
    let checkpoint = thread
        .get_events(Some(exoharness::EventQuery {
            turn_id: Some(turn.record().id),
            direction: Some(exoharness::EventQueryDirection::Desc),
            limit: Some(1),
            ..Default::default()
        }))
        .await?
        .events[0]
        .id;
    turn.finish().await?;
    let stream = f
        .service
        .root
        .watch_turn(thread.as_ref(), turn.record().clone(), Some(checkpoint))
        .await?;
    f.service
        .streams
        .lock()
        .unwrap()
        .insert(stream_key(&f.deployment, &inbox), stream);
    f.service.drain_stream(&f.deployment, &mut inbox).await?;
    assert!(inbox.complete);
    assert_eq!(
        inbox.error,
        Some(InboxError::TurnFailed("model failed".into()))
    );
    assert!(inbox.final_text.is_none());
    f.service.root.shutdown().await
}

#[tokio::test]
async fn malformed_observer_events_back_off_then_stop_delivery_without_ending_the_turn()
-> Result<()> {
    let f = Fixture::new().await?;
    let message = f.input("UALICE", "1.0");
    let (mut inbox, thread, turn) = f.pending_turn(&message).await?;
    turn.add_events(vec![EventData::Custom {
        event_type: crate::permissions::APPROVAL_REQUESTED.into(),
        payload: serde_json::json!({"malformed":true}),
    }])
    .await?;
    for attempt in 1..=2 {
        let stream = f
            .service
            .root
            .watch_turn(thread.as_ref(), turn.record().clone(), inbox.cursor)
            .await?;
        f.service
            .streams
            .lock()
            .unwrap()
            .insert(stream_key(&f.deployment, &inbox), stream);
        f.service.drain_stream(&f.deployment, &mut inbox).await?;
        assert_eq!(inbox.observer_retry.as_ref().unwrap().attempts, attempt);
        assert!(
            inbox.observer_retry.as_ref().unwrap().retry_at
                >= chrono::Utc::now().timestamp() + 2_i64.pow(attempt) - 1
        );
        f.service.deliver(&f.deployment).await?;
    }
    f.storage.reads.store(0, Ordering::SeqCst);
    f.service.process_input(&f.deployment, &mut inbox).await?;
    assert_eq!(
        f.storage.reads.load(Ordering::SeqCst),
        1,
        "retry deadline must prevent replaying history"
    );
    assert_eq!(f.answer.0.load(Ordering::SeqCst), 0);
    let requests = f.slack.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/api/chat.startStream")
            .count(),
        1
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path() == "/api/chat.appendStream")
    );
    for attempt in 3..=INVALID_PROGRESS_RETRY_LIMIT {
        let stream = f
            .service
            .root
            .watch_turn(thread.as_ref(), turn.record().clone(), inbox.cursor)
            .await?;
        f.service
            .streams
            .lock()
            .unwrap()
            .insert(stream_key(&f.deployment, &inbox), stream);
        f.service.drain_stream(&f.deployment, &mut inbox).await?;
        assert_eq!(inbox.observer_retry.as_ref().unwrap().attempts, attempt);
        assert_eq!(inbox.complete, attempt == INVALID_PROGRESS_RETRY_LIMIT);
    }
    assert_eq!(inbox.error, Some(InboxError::InvalidProgress));
    assert!(
        !thread
            .get_events(None)
            .await?
            .events
            .iter()
            .any(|event| matches!(event.data, EventData::TurnEnded))
    );
    f.service.process_input(&f.deployment, &mut inbox).await?;
    f.service.deliver(&f.deployment).await?;
    assert_eq!(
        f.inbox(&message).await?.error,
        Some(InboxError::InvalidProgress)
    );
    assert!(
        f.service
            .store
            .pending::<Inbox>(&format!("adapters/v1/{}/inbox", f.deployment.id))
            .await?
            .is_empty()
    );
    assert_eq!(
        f.slack
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/api/chat.stopStream")
            .count(),
        1
    );
    turn.finish().await?;
    f.service.root.shutdown().await
}

#[tokio::test]
async fn output_commit_restores_cursor_when_the_inbox_write_fails() -> Result<()> {
    use slack_morphism::prelude::SlackStreamChunk;
    let f = Fixture::new().await?;
    let message = f.input("UALICE", "1.0");
    let (mut inbox, thread, turn) = f.pending_turn(&message).await?;
    for text in ["One.", "Two."] {
        turn.add_events(vec![EventData::Messages {
            messages: vec![Message::Assistant {
                content: AssistantContent::String(text.into()),
                id: None,
            }],
            usage: None,
            response_id: None,
        }])
        .await?;
    }
    let stream = f
        .service
        .root
        .watch_turn(thread.as_ref(), turn.record().clone(), None)
        .await?;
    f.service
        .streams
        .lock()
        .unwrap()
        .insert(stream_key(&f.deployment, &inbox), stream);
    *f.storage.fail.lock().unwrap() = Some(AdapterStore::inbox(&f.deployment.id, &message.id));
    assert!(
        f.service
            .drain_stream(&f.deployment, &mut inbox)
            .await
            .is_err()
    );
    f.service.streams.lock().unwrap().clear();
    inbox = f.inbox(&message).await?;
    assert!(inbox.cursor.is_none());
    assert!(
        f.service
            .queue_stream(
                &f.deployment,
                &inbox,
                vec![progress::Progress::status("Waiting for model")],
                false
            )
            .await
            .is_err(),
        "an approval/status update must not move the output receipt backwards"
    );
    f.service.restore_receipt(&f.deployment, &mut inbox).await?;
    assert!(inbox.cursor.is_some());
    turn.finish().await?;
    let stream = f
        .service
        .root
        .watch_turn(thread.as_ref(), turn.record().clone(), inbox.cursor)
        .await?;
    f.service
        .streams
        .lock()
        .unwrap()
        .insert(stream_key(&f.deployment, &inbox), stream);
    f.service.drain_stream(&f.deployment, &mut inbox).await?;
    f.service.finish_stream(&f.deployment, &mut inbox).await?;
    let outbox = f
        .service
        .store
        .get::<Outbox>(&AdapterStore::outbox(
            &f.deployment.id,
            &format!("{}:stream", message.id),
        ))
        .await?
        .unwrap();
    let DeliveryKind::Stream {
        chunks, complete, ..
    } = outbox.kind
    else {
        panic!("expected stream");
    };
    assert!(complete);
    let text: String = chunks
        .into_iter()
        .filter_map(|chunk| match chunk {
            SlackStreamChunk::MarkdownText { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(text, "One.\n\nTwo.");
    f.service.root.shutdown().await
}

#[tokio::test]
async fn pending_index_skips_history_and_recovers_writes_without_a_receipt() -> Result<()> {
    let f = Fixture::new().await?;
    for i in 1..=4 {
        let message = f.input("UALICE", &format!("{i}.0"));
        f.service
            .accept(&f.deployment, message.clone(), None)
            .await?;
        let mut inbox = f.inbox(&message).await?;
        inbox.complete = i != 4;
        f.service.save_inbox(&f.deployment, &inbox).await?;
    }
    let prefix = format!("adapters/v1/{}/inbox", f.deployment.id);
    assert_eq!(f.service.store.pending::<Inbox>(&prefix).await?.len(), 1);
    f.storage.reads.store(0, Ordering::SeqCst);
    assert_eq!(f.service.store.pending::<Inbox>(&prefix).await?.len(), 1);
    assert_eq!(f.storage.reads.load(Ordering::SeqCst), 1);
    let mut new = f.inbox(&f.input("UALICE", "4.0")).await?;
    new.message = f.input("UALICE", "5.0");
    let key = AdapterStore::inbox(&f.deployment.id, &new.message.id);
    *f.storage.fail_after.lock().unwrap() = Some(key.clone());
    assert!(f.service.store.save(&key, &new).await.is_err());
    assert_eq!(f.service.store.pending::<Inbox>(&prefix).await?.len(), 2);
    f.service.root.shutdown().await
}

#[tokio::test]
async fn invalid_definition_does_not_hide_other_agents_and_cache_tracks_updates() -> Result<()> {
    let f = Fixture::new().await?;
    let harness = f.service.root.exoharness_handle();
    let agent = harness.get_agent(&f.deployment.agent).await?.unwrap();
    let broken = harness
        .new_agent(exoharness::NewAgentRequest {
            slug: "broken".into(),
            name: "Broken".into(),
            vaults: Vec::new(),
        })
        .await?;
    broken
        .write_artifact(exoharness::WriteArtifactRequest {
            path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
            contents: b"---\nadapters: broken\n---\n".to_vec(),
        })
        .await?;
    let all = AdapterService::new(f.service.root.clone(), None, None).await?;
    assert_eq!(all.desired().await?.len(), 1);
    assert_eq!(all.webhook_paths().await?.len(), 1);
    let first = all.definitions.load(agent.as_ref()).await?.unwrap();
    let cached = all.definitions.load(agent.as_ref()).await?.unwrap();
    assert!(Arc::ptr_eq(&first, &cached));
    agent
        .write_artifact(exoharness::WriteArtifactRequest {
            path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
            contents: first
                .source()
                .replace("use_sender_vault: true", "use_sender_vault: false")
                .into_bytes(),
        })
        .await?;
    let updated = all.definitions.load(agent.as_ref()).await?.unwrap();
    assert!(!Arc::ptr_eq(&first, &updated));
    let AdapterDefinition::Slack(config) = &updated.frontmatter.adapters[0];
    assert!(!config.use_sender_vault);
    agent
        .write_artifact(exoharness::WriteArtifactRequest {
            path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
            contents: b"---\nadapters: invalid\n---\n".to_vec(),
        })
        .await?;
    f.service.reconcile().await?;
    assert!(
        Arc::ptr_eq(
            &f.service.deployment(&f.deployment.id).unwrap().connection,
            &f.deployment.connection
        ),
        "a failed load must retain the running connection"
    );
    agent
        .write_artifact(exoharness::WriteArtifactRequest {
            path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
            contents: b"---\nharness: basic\nmodel:\n  name: test\nadapters: []\n---\nWork\n"
                .to_vec(),
        })
        .await?;
    f.service.reconcile().await?;
    assert!(
        f.service.deployment(&f.deployment.id).is_none(),
        "a successful removal must stop the adapter"
    );
    f.service.root.shutdown().await
}

#[tokio::test]
async fn revoked_approval_does_not_block_other_deliveries() -> Result<()> {
    let f = Fixture::new().await?;
    f.private("alice", "alice@example.com").await?;
    f.service
        .identities
        .execute(IdentityCommand::BindSlack {
            workspace: "T1".into(),
            user: "UALICE".into(),
            account: "alice@example.com".into(),
        })
        .await?;
    let grant = f
        .service
        .identities
        .resolve_slack("T1", "UALICE", None)
        .await?
        .unwrap();
    let message = f.input("UALICE", "1.0");
    let (mut inbox, _, turn) = f.pending_turn(&message).await?;
    turn.finish().await?;
    inbox.grant = Some(grant);
    inbox.complete = true;
    f.service.save_inbox(&f.deployment, &inbox).await?;
    let decision = Decision {
        inbox: message.id.clone(),
        turn: turn.record().clone(),
        approval_id: "approval".into(),
        approved: true,
        applied: false,
        failure: None,
    };
    let decision_key = format!(
        "adapters/v1/{}/decisions/{}.json",
        f.deployment.id,
        digest(&format!("{}:approval:approval", message.id))
    );
    f.service.store.save(&decision_key, &decision).await?;
    f.service
        .identities
        .execute(IdentityCommand::UnbindSlack {
            workspace: "T1".into(),
            user: "UALICE".into(),
        })
        .await?;
    let other = f.input("UBOB", "2.0");
    f.service
        .queue_delivery(
            &f.deployment,
            &other,
            "answer",
            DeliveryKind::Text {
                text: "Healthy reply.".into(),
            },
        )
        .await?;
    f.service.process(&f.deployment).await?;
    let failed = f
        .service
        .store
        .get::<Decision>(&decision_key)
        .await?
        .unwrap();
    assert!(failed.failure.is_some());
    assert!(!failed.applied);
    let delivered = f
        .service
        .store
        .get::<Outbox>(&AdapterStore::outbox(
            &f.deployment.id,
            &format!("{}:answer", other.id),
        ))
        .await?
        .unwrap();
    assert!(delivered.sent);
    f.service.root.shutdown().await
}

#[tokio::test]
async fn caller_cache_avoids_authority_writes_and_releases_idle_runtimes() -> Result<()> {
    let f = Fixture::new().await?;
    let message = f.input("UALICE", "1.0");
    let (inbox, thread, turn) = f.pending_turn(&message).await?;
    turn.finish().await?;
    let authority = Authority {
        deployment: f.deployment.id.clone(),
        agent: f.deployment.agent,
        thread: thread.record().id,
        adapter: "slack".into(),
        source: (&message).into(),
        grant: None,
    };
    let principal = access::principal(&authority)?;
    let first = f.service.caller_runtime(authority.clone()).await?;
    *f.storage.fail.lock().unwrap() = Some(AdapterStore::authority(&principal));
    let again = f.service.caller_runtime(authority).await?;
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(f.answer.1.load(Ordering::SeqCst), 1);
    drop(first);
    drop(again);
    f.service
        .callers
        .lock()
        .unwrap()
        .get_mut(&principal)
        .unwrap()
        .last_used -= std::time::Duration::from_secs(600);
    f.service.touch_caller(&f.deployment, &inbox)?;
    f.service.prune_callers().await;
    assert_eq!(f.service.callers.lock().unwrap().len(), 1);
    f.service
        .callers
        .lock()
        .unwrap()
        .get_mut(&principal)
        .unwrap()
        .last_used -= std::time::Duration::from_secs(600);
    f.service.prune_callers().await;
    assert!(f.service.callers.lock().unwrap().is_empty());
    f.service.root.shutdown().await
}

#[tokio::test]
async fn removing_sender_access_closes_the_existing_stream_even_if_inbox_save_fails() -> Result<()>
{
    let f = Fixture::new().await?;
    let message = f.input("UALICE", "1.0");
    let (mut inbox, thread, turn) = f.pending_turn(&message).await?;
    f.service
        .store
        .put(
            &AdapterStore::route(&f.deployment.id, &message),
            &Route {
                thread: Some(thread.record().id),
            },
        )
        .await?;
    f.service
        .queue_stream(
            &f.deployment,
            &inbox,
            vec![progress::Progress::status("Queued")],
            false,
        )
        .await?;
    f.service.deliver(&f.deployment).await?;
    let agent = f
        .service
        .root
        .exoharness_handle()
        .get_agent(&f.deployment.agent)
        .await?
        .unwrap();
    let definition = exo_managed_agents::load_definition(agent.as_ref())
        .await?
        .unwrap();
    agent
        .write_artifact(exoharness::WriteArtifactRequest {
            path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
            contents: definition
                .source()
                .replace(
                    "use_sender_vault: true",
                    "use_sender_vault: true\n    allowed_users: []",
                )
                .into_bytes(),
        })
        .await?;
    *f.storage.fail.lock().unwrap() = Some(AdapterStore::inbox(&f.deployment.id, &message.id));
    assert!(
        f.service
            .process_input(&f.deployment, &mut inbox)
            .await
            .is_err()
    );
    // Recovery uses the stream receipt even though the inbox write was lost.
    inbox = f.inbox(&message).await?;
    assert!(!inbox.complete);
    f.service.process_input(&f.deployment, &mut inbox).await?;
    assert!(inbox.complete);
    assert_eq!(inbox.error, Some(InboxError::AccessRemoved));
    f.service.deliver(&f.deployment).await?;
    let requests = f.slack.received_requests().await.unwrap();
    let stops: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/chat.stopStream")
        .collect();
    assert_eq!(stops.len(), 1);
    #[derive(serde::Deserialize)]
    struct Stop {
        ts: String,
        chunks: Vec<slack_morphism::prelude::SlackStreamChunk>,
    }
    let stopped: Stop = serde_json::from_slice(&stops[0].body)?;
    assert_eq!(stopped.ts, "2.0");
    assert!(stopped.chunks.iter().any(|chunk| matches!(chunk, slack_morphism::prelude::SlackStreamChunk::TaskUpdate { title, status, .. } if title == "Access removed" && *status == slack_morphism::prelude::SlackTaskCardStatus::Error)));
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path() == "/api/chat.postMessage")
    );
    assert!(
        f.service
            .store
            .pending::<Outbox>(&format!("adapters/v1/{}/outbox", f.deployment.id))
            .await?
            .iter()
            .all(|delivery| !matches!(delivery.kind, DeliveryKind::Stream { .. }))
    );
    turn.finish().await?;
    f.service.root.shutdown().await
}

#[tokio::test]
async fn grid_members_in_other_workspaces_are_internal_but_slack_connect_is_not() -> Result<()> {
    use wiremock::matchers::query_param;
    let f = Fixture::with_enterprise(Some("E1")).await?;
    for (sender, enterprise, stranger) in [("UGRID", "E1", true), ("UFOREIGN", "E2", false)] {
        Mock::given(path("/api/users.info")).and(query_param("user", sender))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true,"user":{"id":sender,"team_id":"T2","is_stranger":stranger,"enterprise_user":{"id":sender,"enterprise_id":enterprise}}})))
            .with_priority(1).mount(&f.slack).await;
    }
    Mock::given(path("/api/conversations.info"))
        .and(query_param("channel", "GEXTERNAL"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"ok":true,"channel":{"is_mpim":true,"is_ext_shared":true}}),
        ))
        .mount(&f.slack)
        .await;
    for (sender, channel, ts) in [
        ("UGRID", "D1", "1.0"),
        ("UFOREIGN", "D1", "2.0"),
        ("UGRID", "GEXTERNAL", "3.0"),
    ] {
        f.deployment.connection.push(&f.service, &f.deployment, serde_json::from_value(serde_json::json!({"team_id":"T1","api_app_id":"A1","event_id":format!("Ev{ts}"),"event_time":1700000000,"event":{"type":"message","user":sender,"channel":channel,"ts":ts,"text":"hello"}}))?).await?;
    }
    let inbox = f
        .service
        .store
        .pending::<Inbox>(&format!("adapters/v1/{}/inbox", f.deployment.id))
        .await?;
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].message.sender, "UGRID");
    assert!(!inbox[0].message.external);
    f.service.root.shutdown().await
}

#[tokio::test]
async fn external_dms_and_group_dms_require_explicit_slack_connect_admission() -> Result<()> {
    use wiremock::matchers::query_param;
    let f = Fixture::new().await?;
    for (sender, team, stranger) in [
        ("UFOREIGN", "T2", false),
        ("USTRANGER", "T1", true),
        ("UALICE", "T1", false),
    ] {
        Mock::given(path("/api/users.info")).and(query_param("user", sender)).respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"ok":true,"user":{"id":sender,"team_id":team,"is_stranger":stranger}})
        )).with_priority(1).mount(&f.slack).await;
    }
    Mock::given(path("/api/conversations.info"))
        .and(query_param("channel", "GEXTERNAL"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"ok":true,"channel":{"is_mpim":true,"is_ext_shared":true}}),
        ))
        .mount(&f.slack)
        .await;
    for (sender, channel, ts) in [
        ("UFOREIGN", "D1", "1.0"),
        ("USTRANGER", "D1", "2.0"),
        ("UALICE", "GEXTERNAL", "3.0"),
        ("UALICE", "D1", "4.0"),
    ] {
        let event = serde_json::from_value(
            serde_json::json!({"team_id":"T1","api_app_id":"A1","event_id":format!("Ev{ts}"),"event_time":1700000000,"event":{"type":"message","user":sender,"channel":channel,"ts":ts,"text":"hello"}}),
        )?;
        f.deployment
            .connection
            .push(&f.service, &f.deployment, event)
            .await?;
    }
    let inbox = f
        .service
        .store
        .pending::<Inbox>(&format!("adapters/v1/{}/inbox", f.deployment.id))
        .await?;
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].message.message_ts, "4.0");
    assert!(!inbox[0].message.external);
    f.service.root.shutdown().await
}

#[tokio::test]
async fn email_binding_requires_trust_and_pins_take_precedence_without_revoking_other_users()
-> Result<()> {
    let f = Fixture::new().await?;
    let alice = f.private("alice", "alice@example.com").await?;
    assert!(
        f.service
            .identities
            .resolve_slack("T1", "UALICE", Some("alice@example.com"))
            .await?
            .is_none()
    );
    f.service
        .identities
        .execute(IdentityCommand::TrustSlack {
            workspace: "T1".into(),
            trusted: true,
        })
        .await?;
    let grant = f
        .service
        .identities
        .resolve_slack("T1", "UALICE", Some("alice@example.com"))
        .await?
        .unwrap();
    assert_eq!(grant.vault, alice);
    let bob = f.private("bob", "bob@example.com").await?;
    f.service.identities.validate(&grant).await?;
    f.service
        .identities
        .execute(IdentityCommand::BindSlack {
            workspace: "T1".into(),
            user: "UALICE".into(),
            account: "bob@example.com".into(),
        })
        .await?;
    let pinned = f
        .service
        .identities
        .resolve_slack("T1", "UALICE", Some("alice@example.com"))
        .await?
        .unwrap();
    assert_eq!(pinned.vault, bob);
    assert!(f.service.identities.validate(&grant).await.is_err());
    f.service
        .identities
        .execute(IdentityCommand::UnbindSlack {
            workspace: "T1".into(),
            user: "UALICE".into(),
        })
        .await?;
    assert!(f.service.identities.validate(&pinned).await.is_err());
    assert_eq!(
        f.service
            .identities
            .resolve_slack("T1", "UALICE", Some("alice@example.com"))
            .await?
            .unwrap()
            .vault,
        alice
    );
    assert!(
        f.service
            .identities
            .resolve_slack("T2", "UALICE", Some("alice@example.com"))
            .await?
            .is_none()
    );
    f.service.root.shutdown().await
}

#[tokio::test]
async fn configured_execution_vaults_are_available_and_revocable() -> Result<()> {
    let mut f = Fixture::new().await?;
    let root = f.service.root.exoharness_handle();
    let shared = root.create_vault("sre").await?;
    let hidden = root.create_vault("unselected").await?;
    let agent = root.get_agent(&f.deployment.agent).await?.unwrap();
    let original = exo_managed_agents::load_definition(agent.as_ref())
        .await?
        .unwrap();
    let source = original
        .source()
        .replace("use_sender_vault: true", "turn_vaults: [sre]");
    agent
        .write_artifact(exoharness::WriteArtifactRequest {
            path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
            contents: source.as_bytes().to_vec(),
        })
        .await?;
    f.deployment.definition = AgentDefinition::parse(source)?
        .frontmatter
        .adapters
        .remove(0);
    let message = f.input("UUNKNOWN", "1.0");
    f.service
        .accept(&f.deployment, message.clone(), None)
        .await?;
    let mut inbox = f.inbox(&message).await?;
    f.service.process_input(&f.deployment, &mut inbox).await?;
    let authority = Authority {
        deployment: f.deployment.id.clone(),
        agent: f.deployment.agent,
        thread: inbox.thread.unwrap(),
        adapter: "slack".into(),
        source: (&message).into(),
        grant: None,
    };
    let runtime = f.service.caller_runtime(authority).await?;
    let state = runtime.exoharness_handle();
    let scoped_agent = state.get_agent(&f.deployment.agent).await?.unwrap();
    let thread = scoped_agent
        .get_thread(&inbox.thread.unwrap())
        .await?
        .unwrap();
    assert!(
        thread
            .list_vaults()
            .await?
            .iter()
            .any(|v| v.record().id == shared.record().id)
    );
    assert!(state.get_vault(&hidden.record().id).await?.is_none());
    let bot = exo_managed_agents::vaults::find_vault(root.as_ref(), "support-slack").await?;
    assert!(state.get_vault(&bot.record().id).await?.is_none());
    agent
        .write_artifact(exoharness::WriteArtifactRequest {
            path: exo_managed_agents::AGENT_DEFINITION_PATH.into(),
            contents: original.source().as_bytes().to_vec(),
        })
        .await?;
    assert!(state.get_vault(&shared.record().id).await?.is_none());
    assert!(
        !thread
            .list_vaults()
            .await?
            .iter()
            .any(|v| v.record().id == shared.record().id)
    );
    f.service.root.shutdown().await
}

#[tokio::test]
async fn sender_capabilities_share_history_without_accumulating_vaults() -> Result<()> {
    let f = Fixture::new().await?;
    let alice = f.private("alice", "alice@example.com").await?;
    let bob = f.private("bob", "bob@example.com").await?;
    let root = f.service.root.exoharness_handle();
    let agent = root.get_agent(&f.deployment.agent).await?.unwrap();
    let thread = agent.new_thread(NewThreadRequest::default()).await?;
    let global = exoharness::vault::global_vault(root.as_ref())
        .await?
        .record()
        .id;
    f.service
        .identities
        .execute(IdentityCommand::TrustSlack {
            workspace: "T1".into(),
            trusted: true,
        })
        .await?;
    let mut alice_state = None;
    for (sender, email, expected) in [
        ("UALICE", Some("alice@example.com"), vec![global, alice]),
        ("UBOB", Some("bob@example.com"), vec![global, bob]),
        ("UUNKNOWN", None, vec![global]),
    ] {
        let grant = f
            .service
            .identities
            .resolve_slack("T1", sender, email)
            .await?;
        let authority = Authority {
            deployment: f.deployment.id.clone(),
            agent: f.deployment.agent,
            thread: thread.record().id,
            adapter: "slack".into(),
            source: (&f.input(sender, "1.0")).into(),
            grant,
        };
        f.service.caller_runtime(authority.clone()).await?;
        let scoped = f.service.caller_runtime(authority).await?;
        let state = scoped.exoharness_handle();
        if sender == "UALICE" {
            alice_state = Some(state.clone());
        }
        let selected = state
            .get_agent(&f.deployment.agent)
            .await?
            .unwrap()
            .get_thread(&thread.record().id)
            .await?
            .unwrap();
        assert_eq!(
            selected
                .list_vaults()
                .await?
                .iter()
                .map(|v| v.record().id)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(
            state
                .caller()
                .unwrap()
                .policy
                .check_operator(&state.caller().unwrap().principal)
                .await
                .is_err()
        );
        assert!(state.provider_storage().is_err());
        assert_eq!(selected.record().id, thread.record().id);
        assert_eq!(selected.get_events(None).await?.events.len(), 1);
    }
    assert_eq!(f.answer.1.load(Ordering::SeqCst), 3);
    // Reusing a runtime must not retain authority after the binding changes.
    f.service
        .identities
        .execute(IdentityCommand::BindSlack {
            workspace: "T1".into(),
            user: "UALICE".into(),
            account: "bob@example.com".into(),
        })
        .await?;
    assert!(
        alice_state
            .unwrap()
            .get_agent(&f.deployment.agent)
            .await
            .is_err()
    );
    assert!(thread.record().vaults.is_empty());
    f.service.root.shutdown().await
}

#[tokio::test]
async fn recovery_after_route_receipt_and_outbox_write_failures_does_not_repeat_work() -> Result<()>
{
    let f = Fixture::new().await?;
    let message = f.input("UALICE", "1.0");
    *f.storage.fail.lock().unwrap() = Some(AdapterStore::inbox(&f.deployment.id, &message.id));
    assert!(
        f.service
            .accept(&f.deployment, message.clone(), None)
            .await
            .is_err()
    );
    f.service.deliver_receipts(&f.deployment).await?;
    assert!(
        f.slack
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path() != "/api/reactions.add")
    );
    f.service
        .accept(&f.deployment, message.clone(), None)
        .await?;
    f.service
        .accept(&f.deployment, message.clone(), None)
        .await?;
    assert_eq!(
        f.service
            .store
            .list::<Inbox>(&format!("adapters/v1/{}/inbox", f.deployment.id))
            .await?
            .len(),
        1
    );
    f.service.deliver_receipts(&f.deployment).await?;
    f.service.deliver_receipts(&f.deployment).await?;
    assert_eq!(
        f.slack
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/api/reactions.add")
            .count(),
        1
    );
    *f.storage.fail.lock().unwrap() = Some(AdapterStore::route(&f.deployment.id, &message));
    assert!(
        f.service
            .submit(&f.deployment, &mut f.inbox(&message).await?)
            .await
            .is_err()
    );
    *f.storage.fail.lock().unwrap() = Some(AdapterStore::inbox(&f.deployment.id, &message.id));
    assert!(
        f.service
            .submit(&f.deployment, &mut f.inbox(&message).await?)
            .await
            .is_err()
    );
    let mut record = f.inbox(&message).await?;
    f.service.submit(&f.deployment, &mut record).await?;
    let agent = f
        .service
        .root
        .exoharness_handle()
        .get_agent(&f.deployment.agent)
        .await?
        .unwrap();
    assert_eq!(
        exo_managed_agents::list_threads(agent.as_ref())
            .await?
            .len(),
        1
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let thread = agent.get_thread(&record.thread.unwrap()).await?.unwrap();
            if thread
                .get_events(None)
                .await?
                .events
                .iter()
                .any(|event| matches!(event.data, EventData::TurnEnded))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    *f.storage.fail.lock().unwrap() = Some(AdapterStore::outbox(
        &f.deployment.id,
        &format!("{}:stream", message.id),
    ));
    assert!(
        f.service
            .drain_stream(&f.deployment, &mut record)
            .await
            .is_err()
    );
    record = f.inbox(&message).await?;
    assert!(!record.complete);
    f.service.submit(&f.deployment, &mut record).await?;
    f.service.drain_stream(&f.deployment, &mut record).await?;
    assert!(record.complete);
    f.service.finish_stream(&f.deployment, &mut record).await?;
    f.service.save_inbox(&f.deployment, &record).await?;
    assert_eq!(f.answer.0.load(Ordering::SeqCst), 1);
    assert_eq!(f.answer.1.load(Ordering::SeqCst), 1);
    f.service.deliver(&f.deployment).await?;
    f.service.deliver(&f.deployment).await?;
    let requests = f.slack.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/api/chat.startStream")
            .count(),
        1
    );
    f.service.root.shutdown().await
}

#[tokio::test]
async fn live_completion_closes_the_reply_without_waiting_for_saved_events() -> Result<()> {
    let f = Fixture::new().await?;
    let message = f.input("UALICE", "1.0");
    f.service
        .accept(&f.deployment, message.clone(), None)
        .await?;
    let mut inbox = f.inbox(&message).await?;
    let agent = f
        .service
        .root
        .exoharness_handle()
        .get_agent(&f.deployment.agent)
        .await?
        .unwrap();
    let thread = agent.new_thread(NewThreadRequest::default()).await?;
    let turn = thread
        .begin_turn(exoharness::BeginTurnRequest {
            turn: exoharness::test_support::new_test_turn_record(),
            new_session: true,
            input: Vec::new(),
            initial_events: Vec::new(),
        })
        .await?;
    let latest_event_id = Uuid7::now();
    inbox.thread = Some(thread.record().id);
    inbox.turn = Some(turn.record().clone());
    f.service.save_inbox(&f.deployment, &inbox).await?;
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    f.service.streams.lock().unwrap().insert(
        stream_key(&f.deployment, &inbox),
        crate::ExecutionStreamHandle::new(tokio_stream::wrappers::UnboundedReceiverStream::new(
            receiver,
        )),
    );
    sender.send(Ok(ExecutionStreamEvent::Status {
        message: "Preparing sandbox and tools".into(),
    }))?;
    f.service.process(&f.deployment).await?;
    let requests = f.slack.received_requests().await.unwrap();
    let started = requests
        .iter()
        .find(|request| request.url.path() == "/api/chat.startStream")
        .unwrap();
    let started: slack_morphism::prelude::SlackApiChatStartStreamRequest =
        serde_json::from_slice(&started.body)?;
    assert_eq!(
        started.task_display_mode,
        Some(slack_morphism::prelude::SlackStreamTaskDisplayMode::Timeline)
    );
    assert!(
        matches!(started.chunks.as_deref(), Some([slack_morphism::prelude::SlackStreamChunk::TaskUpdate { title, hide_title, status, .. }])
        if title == "Preparing sandbox and tools" && hide_title != &Some(true) && *status == slack_morphism::prelude::SlackTaskCardStatus::InProgress)
    );
    assert!(!f.inbox(&message).await?.complete);
    sender.send(Ok(ExecutionStreamEvent::Chunk(
        lingua::UniversalStreamChunk::text_delta(0, "2"),
    )))?;
    sender.send(Ok(ExecutionStreamEvent::Completed(crate::SendResult {
        session_id: turn.record().session_id,
        turn_id: turn.record().id,
        latest_event_id,
    })))?;
    f.service.process(&f.deployment).await?;
    f.service.deliver(&f.deployment).await?;
    let completed = f.inbox(&message).await?;
    assert!(completed.complete);
    assert_eq!(completed.cursor, Some(latest_event_id));
    assert_eq!(completed.final_text.as_deref(), Some("2"));
    let requests = f.slack.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/api/chat.startStream")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/api/chat.stopStream")
            .count(),
        1
    );
    f.service.root.shutdown().await
}

#[tokio::test]
async fn live_text_and_recovered_tools_share_one_reply_across_delivery_retry() -> Result<()> {
    use slack_morphism::prelude::*;
    let f = Fixture::new().await?;
    let message = f.input("UALICE", "1.0");
    f.service
        .accept(&f.deployment, message.clone(), None)
        .await?;
    let mut inbox = f.inbox(&message).await?;
    let agent = f
        .service
        .root
        .exoharness_handle()
        .get_agent(&f.deployment.agent)
        .await?
        .unwrap();
    let thread = agent.new_thread(NewThreadRequest::default()).await?;
    let turn = thread
        .begin_turn(exoharness::BeginTurnRequest {
            turn: exoharness::test_support::new_test_turn_record(),
            new_session: true,
            input: Vec::new(),
            initial_events: Vec::new(),
        })
        .await?;
    inbox.thread = Some(thread.record().id);
    inbox.turn = Some(turn.record().clone());
    f.service.streams.lock().unwrap().insert(
        stream_key(&f.deployment, &inbox),
        crate::ExecutionStreamHandle::new(futures::stream::iter(vec![Ok(
            crate::ExecutionStreamEvent::Chunk(lingua::UniversalStreamChunk::text_delta(
                0,
                "I’ll check ",
            )),
        )])),
    );
    assert!(!f.service.drain_stream(&f.deployment, &mut inbox).await?);
    f.service.deliver(&f.deployment).await?;
    let stream_key = AdapterStore::outbox(&f.deployment.id, &format!("{}:stream", message.id));
    let opened = f.service.store.get::<Outbox>(&stream_key).await?.unwrap();
    assert_eq!(opened.external_id.as_deref(), Some("2.0"));
    assert!(!opened.sent);
    // Replay committed events after losing the in-memory observer on restart.
    f.service.streams.lock().unwrap().clear();
    inbox = f.inbox(&message).await?;
    turn.add_events(vec![
        EventData::Messages {
            messages: vec![Message::Assistant {
                content: AssistantContent::String("I’ll check recent query volume.".into()),
                id: None,
            }],
            response_id: None,
            usage: None,
        },
        EventData::ToolRequested {
            tool_call_id: "query".into(),
            response_id: None,
            request: exoharness::ToolRequest {
                function_name: "exo_mcp__datadog__get_datadog_metric".into(),
                arguments: Default::default(),
                namespace: None,
            },
        },
        EventData::ToolResult {
            tool_call_id: "query".into(),
            result: Default::default(),
        },
    ])
    .await?;
    let recovered = f
        .service
        .root
        .watch_turn(thread.as_ref(), turn.record().clone(), inbox.cursor)
        .await?;
    f.service
        .streams
        .lock()
        .unwrap()
        .insert(super::stream_key(&f.deployment, &inbox), recovered);
    f.service.drain_stream(&f.deployment, &mut inbox).await?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    Mock::given(path("/api/chat.appendStream"))
        .respond_with(move |_: &wiremock::Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "1")
                    .set_body_json(serde_json::json!({"ok":false,"error":"ratelimited"}))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok":true,"channel":"D1","ts":"2.0"}))
            }
        })
        .with_priority(1)
        .mount(&f.slack)
        .await;
    f.service.deliver(&f.deployment).await?;
    let mut retained = f.service.store.get::<Outbox>(&stream_key).await?.unwrap();
    assert_eq!(retained.external_id, opened.external_id);
    assert!(matches!(&retained.kind, DeliveryKind::Stream { chunks, .. } if !chunks.is_empty()));
    retained.retry_at = 0;
    f.service.store.put(&stream_key, &retained).await?;
    f.service.deliver(&f.deployment).await?;
    turn.add_events(vec![EventData::Messages {
        messages: vec![Message::Assistant {
            content: AssistantContent::String("Done.".into()),
            id: None,
        }],
        response_id: None,
        usage: None,
    }])
    .await?;
    turn.finish().await?;
    f.service.drain_stream(&f.deployment, &mut inbox).await?;
    f.service.finish_stream(&f.deployment, &mut inbox).await?;
    f.service.save_inbox(&f.deployment, &inbox).await?;
    f.service.deliver(&f.deployment).await?;
    let finished = f.service.store.get::<Outbox>(&stream_key).await?.unwrap();
    assert!(finished.sent);
    assert_eq!(finished.external_id, opened.external_id);
    let requests = f.slack.received_requests().await.unwrap();
    let starts = requests
        .iter()
        .filter(|r| r.url.path() == "/api/chat.startStream")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1);
    let start: SlackApiChatStartStreamRequest = serde_json::from_slice(&starts[0].body)?;
    assert_eq!(start.thread_ts.unwrap().0, "1.0");
    let stop = requests
        .iter()
        .find(|r| r.url.path() == "/api/chat.stopStream")
        .unwrap();
    let stop: SlackApiChatStopStreamRequest = serde_json::from_slice(&stop.body)?;
    assert_eq!(stop.ts.0, "2.0");
    let append = requests
        .iter()
        .rfind(|r| r.url.path() == "/api/chat.appendStream")
        .unwrap();
    let append: SlackApiChatAppendStreamRequest = serde_json::from_slice(&append.body)?;
    assert!(append.chunks.as_ref().unwrap().iter().any(|chunk|
        matches!(chunk, SlackStreamChunk::TaskUpdate { id, status: SlackTaskCardStatus::Complete, .. } if id.0 == "query")
    ));
    let chunks = start
        .chunks
        .into_iter()
        .flatten()
        .chain(append.chunks.into_iter().flatten())
        .chain(stop.chunks.into_iter().flatten());
    let text = chunks
        .filter_map(|chunk| {
            if let SlackStreamChunk::MarkdownText { text } = chunk {
                Some(text)
            } else {
                None
            }
        })
        .collect::<String>();
    assert_eq!(text, "I’ll check recent query volume.\n\nDone.");
    f.service.root.shutdown().await
}

#[tokio::test]
async fn deleted_slack_threads_stop_delivery_without_discarding_the_reply() -> Result<()> {
    use slack_morphism::prelude::SlackStreamChunk;
    let f = Fixture::new().await?;
    Mock::given(path("/api/chat.startStream"))
        .and(wiremock::matchers::body_partial_json(
            serde_json::json!({"thread_ts": "1.0"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok":false,"error":"invalid_thread_ts"})),
        )
        .with_priority(1)
        .expect(1)
        .mount(&f.slack)
        .await;
    let message = f.input("UALICE", "1.0");
    f.service
        .accept(&f.deployment, message.clone(), None)
        .await?;
    f.service
        .queue_stream(
            &f.deployment,
            &f.inbox(&message).await?,
            vec![SlackStreamChunk::MarkdownText { text: "2.".into() }],
            true,
        )
        .await?;
    f.service.deliver(&f.deployment).await?;
    let key = AdapterStore::outbox(&f.deployment.id, &format!("{}:stream", message.id));
    let failed = f.service.store.get::<Outbox>(&key).await?.unwrap();
    assert_eq!(failed.failure.as_deref(), Some("invalid_thread_ts"));
    assert!(!failed.sent);
    assert!(failed.external_id.is_none());
    assert!(
        matches!(&failed.kind, DeliveryKind::Stream { chunks, complete: true, .. }
        if matches!(chunks.as_slice(), [SlackStreamChunk::MarkdownText { text }] if text == "2."))
    );

    // A new service reads the failure from storage and leaves it stopped.
    let mut restarted = AdapterService::new(f.service.root.clone(), None, None).await?;
    Arc::get_mut(&mut restarted).unwrap().store = f.service.store.clone();
    restarted.deliver(&f.deployment).await?;

    let next_message = f.input("UALICE", "3.0");
    restarted
        .accept(&f.deployment, next_message.clone(), None)
        .await?;
    restarted
        .queue_stream(
            &f.deployment,
            &restarted
                .store
                .get::<Inbox>(&AdapterStore::inbox(&f.deployment.id, &next_message.id))
                .await?
                .unwrap(),
            vec![SlackStreamChunk::MarkdownText { text: "4.".into() }],
            true,
        )
        .await?;
    restarted.deliver(&f.deployment).await?;
    restarted.deliver(&f.deployment).await?;
    let delivered = restarted
        .store
        .get::<Outbox>(&AdapterStore::outbox(
            &f.deployment.id,
            &format!("{}:stream", next_message.id),
        ))
        .await?
        .unwrap();
    assert!(delivered.sent);
    assert!(delivered.failure.is_none());
    f.service.root.shutdown().await
}

#[tokio::test]
async fn receipts_continue_during_a_stalled_reply_and_shutdown_keeps_pending_work() -> Result<()> {
    use std::time::Duration;
    let f = Fixture::new().await?;
    let vault = exo_managed_agents::vaults::find_vault(
        f.service.root.exoharness_handle().as_ref(),
        "support-slack",
    )
    .await?;
    let mut revisions = Vec::new();
    for name in ["bot-token", "signing-secret"] {
        let secret = vault
            .put_secret(exoharness::PutSecretRequest {
                name: name.into(),
                secret: exoharness::Secret::Key { value: name.into() },
                policy: None,
            })
            .await?;
        let metadata = vault.list_secrets().await?;
        let metadata = metadata.iter().find(|value| value.id == secret).unwrap();
        revisions.push((metadata.id, metadata.revision));
    }
    f.service
        .deployments
        .lock()
        .unwrap()
        .get_mut(&f.deployment.id)
        .unwrap()
        .fingerprint = digest(&serde_json::to_string(&(
        &f.deployment.definition,
        revisions,
    ))?);
    let requested = Arc::new(tokio::sync::Notify::new());
    let request_signal = requested.clone();
    Mock::given(path("/api/chat.startStream"))
        .respond_with(move |_: &wiremock::Request| {
            request_signal.notify_one();
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true}))
                .set_delay(Duration::from_secs(60))
        })
        .with_priority(1)
        .mount(&f.slack)
        .await;
    let message = f.input("UALICE", "1.0");
    f.service
        .accept(&f.deployment, message.clone(), None)
        .await?;
    let task = tokio::spawn(f.service.clone().run());
    tokio::time::timeout(Duration::from_secs(5), requested.notified()).await?;

    // Reply delivery is still blocked. A newly accepted message must receive
    // its reaction without waiting for that request or another turn's setup.
    let acknowledged = Arc::new(tokio::sync::Notify::new());
    let acknowledgment_signal = acknowledged.clone();
    Mock::given(path("/api/reactions.add"))
        .and(wiremock::matchers::body_partial_json(
            serde_json::json!({"timestamp": "3.0"}),
        ))
        .respond_with(move |_: &wiremock::Request| {
            acknowledgment_signal.notify_one();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true}))
        })
        .with_priority(1)
        .expect(1)
        .mount(&f.slack)
        .await;
    let next_message = f.input("UALICE", "3.0");
    f.service
        .accept(&f.deployment, next_message.clone(), None)
        .await?;
    tokio::time::timeout(Duration::from_secs(2), acknowledged.notified()).await?;
    assert!(f.inbox(&next_message).await?.turn.is_none());

    f.service.cancellation().cancel();
    tokio::time::timeout(Duration::from_secs(2), task).await???;
    let retained = f.inbox(&message).await?;
    assert!(retained.turn.is_some());
    let pending = f
        .service
        .store
        .get::<Outbox>(&AdapterStore::outbox(
            &f.deployment.id,
            &format!("{}:stream", message.id),
        ))
        .await?
        .unwrap();
    assert!(!pending.sent);
    assert!(pending.external_id.is_none());
    assert!(f.service.deployment(&f.deployment.id).is_none());
    f.service.root.shutdown().await
}

#[actix_web::test]
async fn unsigned_webhooks_cannot_bypass_api_authentication() -> Result<()> {
    use actix_web::{App, test, web};
    let f = Fixture::new().await?;
    let vault = exo_managed_agents::vaults::find_vault(
        f.service.root.exoharness_handle().as_ref(),
        "support-slack",
    )
    .await?;
    vault
        .put_secret(exoharness::PutSecretRequest {
            name: "signing-secret".into(),
            secret: exoharness::Secret::Key {
                value: "signing-secret".into(),
            },
            policy: None,
        })
        .await?;
    let service = Arc::new(
        crate::http_service::RuntimeHttpService::new(f.service.root.clone(), Some("exo-token"))?
            .for_agent(f.deployment.agent)
            .with_adapters(f.service.clone()),
    );
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(service))
            .configure(crate::http_service::configure),
    )
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/exo/adapter/slack")
            .set_json(serde_json::json!({"type":"url_verification","challenge":"verify"}))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), actix_web::http::StatusCode::UNAUTHORIZED);
    let body = r#"{"type":"url_verification","challenge":"verify"}"#;
    let timestamp = chrono::Utc::now().timestamp().to_string();
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"signing-secret")?;
    mac.update(format!("v0:{timestamp}:{body}").as_bytes());
    let signature = format!("v0={:x}", mac.finalize().into_bytes());
    let request = test::TestRequest::post()
        .uri("/exo/adapter/slack")
        .insert_header(("content-type", "application/json"))
        .insert_header(("x-slack-request-timestamp", timestamp))
        .insert_header(("x-slack-signature", signature.clone()))
        .set_payload(body)
        .to_request();
    assert_eq!(
        test::call_service(&app, request).await.status(),
        actix_web::http::StatusCode::OK
    );
    let request = test::TestRequest::post()
        .uri("/exo/adapter/slack")
        .insert_header((
            "x-slack-request-timestamp",
            chrono::Utc::now().timestamp().to_string(),
        ))
        .insert_header(("x-slack-signature", signature))
        .set_payload(r#"{"type":"url_verification","challenge":"changed"}"#)
        .to_request();
    assert_eq!(
        test::call_service(&app, request).await.status(),
        actix_web::http::StatusCode::UNAUTHORIZED
    );
    let error = test::try_call_service(
        &app,
        test::TestRequest::get().uri("/exo/agent").to_request(),
    )
    .await
    .err()
    .context("API request without Exo authentication must fail")?;
    assert_eq!(
        error.as_response_error().status_code(),
        actix_web::http::StatusCode::UNAUTHORIZED
    );
    f.service.root.shutdown().await
}

#[tokio::test]
async fn mentions_activate_followups_and_slack_connect_is_rejected() -> Result<()> {
    use wiremock::matchers::query_param;
    let f = Fixture::new().await?;
    for (channel, shared) in [("C1", false), ("C2", true)] {
        Mock::given(path("/api/conversations.info"))
            .and(query_param("channel", channel))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"ok":true,"channel":{"is_ext_shared":shared}}),
                ),
            )
            .mount(&f.slack)
            .await;
    }
    let callback = |kind: &str,
                    channel: &str,
                    ts: &str,
                    thread: Option<&str>,
                    text: &str|
     -> Result<slack_morphism::prelude::SlackPushEventCallback> {
        Ok(serde_json::from_value(
            serde_json::json!({"team_id":"T1","api_app_id":"A1","event_id":format!("Ev{ts}"),"event_time":1700000000,"event":{"type":kind,"user":"UALICE","channel":channel,"ts":ts,"thread_ts":thread,"text":text}}),
        )?)
    };
    let connection = &f.deployment.connection;
    connection
        .push(
            &f.service,
            &f.deployment,
            callback("message", "C1", "0.0", None, "Ambient chat")?,
        )
        .await?;
    connection
        .push(
            &f.service,
            &f.deployment,
            callback("app_mention", "C1", "1.0", None, "<@UBOT> investigate")?,
        )
        .await?;
    connection
        .push(
            &f.service,
            &f.deployment,
            callback("message", "C1", "1.0", None, "<@UBOT> investigate")?,
        )
        .await?;
    connection
        .push(
            &f.service,
            &f.deployment,
            callback(
                "message",
                "C1",
                "2.0",
                Some("1.0"),
                "Follow up without a mention",
            )?,
        )
        .await?;
    connection
        .push(
            &f.service,
            &f.deployment,
            callback("app_mention", "C2", "3.0", None, "<@UBOT> external chat")?,
        )
        .await?;
    let inputs = f
        .service
        .store
        .list::<Inbox>(&format!("adapters/v1/{}/inbox", f.deployment.id))
        .await?;
    assert_eq!(inputs.len(), 2);
    assert!(
        inputs
            .iter()
            .all(|input| input.message.conversation == "T1/C1/1.0")
    );
    f.service.root.shutdown().await
}
