use std::sync::Arc;

use lingua::Message;
use lingua::universal::{AssistantContent, UserContent};

use crate::{
    BeginTurnRequest, Binding, EventData, EventQuery, EventQueryDirection, ExoHarness,
    ForkConversationRequest, NewAgentRequest, NewConversationRequest, PutSecretRequest, Secret,
    WriteArtifactRequest,
};

pub async fn supports_agent_and_conversation_crud(harness: Arc<dyn ExoHarness>) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some("conversation".to_string()),
            name: Some("Conversation".to_string()),
        })
        .await
        .expect("conversation should be created");

    assert_eq!(harness.list_agents().await.expect("list agents").len(), 1);
    assert_eq!(
        agent
            .list_conversations()
            .await
            .expect("list conversations")
            .len(),
        1
    );

    assert!(
        agent
            .delete_conversation(&conversation.record().id)
            .await
            .expect("delete conversation")
    );
    assert!(
        harness
            .delete_agent(&agent.record().id)
            .await
            .expect("delete agent")
    );
}

pub async fn begin_turn_tracks_events_through_finish(harness: Arc<dyn ExoHarness>) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");

    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: vec![user_message("ping")],
        })
        .await
        .expect("turn");
    turn.add_events(vec![EventData::Messages {
        messages: vec![assistant_message("pong")],
        response_id: None,
    }])
    .await
    .expect("append assistant message");
    let latest_event_id = turn.finish().await.expect("finish turn");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: Some(turn.record().id),
            types: None,
        }))
        .await
        .expect("get events")
        .events;

    assert!(matches!(events[0].data, EventData::SessionStarted));
    assert!(matches!(events[1].data, EventData::TurnStarted));
    assert!(matches!(events[2].data, EventData::Messages { .. }));
    assert!(matches!(events[3].data, EventData::Messages { .. }));
    assert!(matches!(events[4].data, EventData::TurnEnded));
    assert_eq!(events.last().expect("turn ended").id, latest_event_id);
}

pub async fn turn_events_continue_after_artifact_writes(harness: Arc<dyn ExoHarness>) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest::default())
        .await
        .expect("conversation");

    let turn = conversation
        .begin_turn(BeginTurnRequest {
            session_id: None,
            input: vec![user_message("ping")],
        })
        .await
        .expect("turn");
    turn.write_artifact(WriteArtifactRequest {
        path: "tool-results/example.json".to_string(),
        contents: br#"{"ok":true}"#.to_vec(),
    })
    .await
    .expect("write artifact");
    turn.add_events(vec![EventData::Messages {
        messages: vec![assistant_message("pong")],
        response_id: None,
    }])
    .await
    .expect("append after artifact write");
    turn.finish().await.expect("finish after artifact write");

    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: Some(vec!["artifact_written".to_string()]),
        }))
        .await
        .expect("artifact event")
        .events;
    let artifact_event = events.first().expect("artifact_written event");
    assert_eq!(artifact_event.session_id, Some(turn.record().session_id));
    assert_eq!(artifact_event.turn_id, Some(turn.record().id));
}

pub async fn conversation_scope_overrides_agent_scope_and_fork_copies_local_state(
    harness: Arc<dyn ExoHarness>,
) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some("base".to_string()),
            name: Some("Base".to_string()),
        })
        .await
        .expect("conversation");

    let agent_secret_id = agent
        .put_secret(PutSecretRequest {
            name: "OPENAI_API_KEY".to_string(),
            secret: Secret::Key {
                value: "agent".to_string(),
            },
        })
        .await
        .expect("agent secret");
    agent
        .put_binding(Binding::Env {
            name: "OPENAI_API_KEY".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            secret_id: agent_secret_id,
        })
        .await
        .expect("agent binding");

    let conversation_secret_id = conversation
        .put_secret(PutSecretRequest {
            name: "OPENAI_API_KEY".to_string(),
            secret: Secret::Key {
                value: "conversation".to_string(),
            },
        })
        .await
        .expect("conversation secret");
    conversation
        .put_binding(Binding::Env {
            name: "OPENAI_API_KEY".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            secret_id: conversation_secret_id,
        })
        .await
        .expect("conversation binding");

    let effective_secret = conversation
        .list_secrets()
        .await
        .expect("list secrets")
        .into_iter()
        .find(|secret| secret.name == "OPENAI_API_KEY")
        .expect("effective secret");
    assert_eq!(effective_secret.id, conversation_secret_id);

    let forked = conversation
        .fork(ForkConversationRequest {
            up_to_inclusive: None,
            slug: Some("fork".to_string()),
            name: Some("Fork".to_string()),
        })
        .await
        .expect("fork");
    let forked_secret = forked
        .list_secrets()
        .await
        .expect("list forked secrets")
        .into_iter()
        .find(|secret| secret.name == "OPENAI_API_KEY")
        .expect("forked effective secret");
    assert_eq!(forked_secret.name, "OPENAI_API_KEY");
    let events = forked
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await
        .expect("get forked events")
        .events;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::ConversationForked { .. }))
    );
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant {
        id: None,
        content: AssistantContent::String(text.to_string()),
    }
}
