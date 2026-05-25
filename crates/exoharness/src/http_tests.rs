use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;

use lingua::Message;
use lingua::universal::{AssistantContent, UserContent};
use tempfile::TempDir;

use crate::{
    BasicExoHarness, BasicExoHarnessConfig, BeginTurnRequest, EventData, EventQuery,
    EventQueryDirection, ExoHarness, HttpExoHarness, NewAgentRequest, NewConversationRequest,
    SandboxBackendChoice, SecretBackendChoice, serve_exoharness_http_listener,
};

fn local_test_config(root: impl Into<std::path::PathBuf>) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.into(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_backend: SandboxBackendChoice::LocalProcess,
    }
}

#[actix_web::test]
async fn http_exoharness_runs_basic_backend_requests() {
    let tempdir = TempDir::new().expect("tempdir");
    let basic = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("basic harness");
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("listener");
    let addr = listener.local_addr().expect("local addr");
    let server = actix_web::rt::spawn(serve_exoharness_http_listener(listener, Arc::new(basic)));

    let harness = HttpExoHarness::new(format!("http://{addr}")).expect("http harness");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some("conversation".to_string()),
            name: Some("Conversation".to_string()),
        })
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
    .expect("add events");
    let latest_event_id = turn.finish().await.expect("finish");
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
        .expect("events")
        .events;

    assert_eq!(events.last().expect("last event").id, latest_event_id);
    assert_eq!(harness.list_agents().await.expect("agents").len(), 1);

    server.abort();
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant {
        content: AssistantContent::String(text.to_string()),
        id: None,
    }
}
