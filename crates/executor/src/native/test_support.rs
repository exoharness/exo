use exoharness::SandboxProvider;
pub(crate) use exoharness::test_support::local_test_config;

pub(crate) fn agent_request(
    slug: impl Into<String>,
    harness: crate::AgentHarnessKind,
) -> crate::CreateAgentRequest {
    crate::CreateAgentRequest {
        slug: slug.into(),
        name: None,
        harness,
        typescript: None,
        enable_agent_tool_creation: true,
        sandbox_image: None,
        sandbox_provider: SandboxProvider::LocalProcess,
        sandbox_scope: None,
        enable_networking: false,
        model: "gpt-5.4".into(),
        credential: Some("test-openai".into()),
        base_url: None,
        max_output_tokens: None,
        max_tool_round_trips: None,
        braintrust: None,
    }
}

pub(crate) async fn create_test_credential(exoharness: &dyn exoharness::ExoHarness) {
    exoharness::vault::global_vault(exoharness)
        .await
        .expect("runtime vault")
        .put_secret(exoharness::PutSecretRequest {
            policy: Some(exoharness::CredentialPolicy::destinations(vec![
                exoharness::CredentialDestination::origin("https://api.openai.com").unwrap(),
                exoharness::CredentialDestination::origin("https://api.anthropic.com").unwrap(),
            ])),
            name: "test-openai".to_string(),
            secret: exoharness::Secret::Key {
                value: "test-key".to_string(),
            },
        })
        .await
        .expect("test secret should register");
}

/// Seed a crash after queue admission using the same ordering as the runtime.
pub(crate) async fn begin_queued_turn(
    coordinator: &dyn exoharness::turn_coordinator::TurnQueue<crate::TurnWork>,
    agent_id: exoharness::AgentId,
    thread: &dyn exoharness::ThreadHandle,
    work: &crate::TurnWork,
    input: Vec<lingua::Message>,
    initial_events: Vec<exoharness::EventData>,
) -> exoharness::Result<std::sync::Arc<dyn exoharness::TurnHandle>> {
    use exoharness::turn_coordinator::{TurnSubmission, TurnThread};
    let scope = TurnThread {
        agent_id,
        thread_id: thread.record().id,
    };
    let turn = exoharness::TurnRecord {
        id: exoharness::Uuid7::now(),
        session_id: work
            .request
            .session_id
            .unwrap_or_else(exoharness::Uuid7::now),
    };
    coordinator
        .enqueue_turn(
            scope,
            TurnSubmission {
                turn: turn.clone(),
                work: work.clone(),
                principal: thread.caller().map(|caller| caller.principal.clone()),
                idempotency_key: None,
                attention: Default::default(),
            },
            (),
        )
        .await?;
    let lease = coordinator
        .claim(scope)
        .await?
        .expect("fixture owns the queue");
    coordinator.start(&lease, turn.id).await?;
    drop(lease);
    thread
        .begin_turn(exoharness::BeginTurnRequest {
            turn,
            new_session: work.request.session_id.is_none(),
            input,
            initial_events,
        })
        .await
}
