use futures::io::{AsyncReadExt, AsyncWriteExt};
use lingua::Message;
use lingua::universal::{AssistantContent, UserContent};
use tempfile::TempDir;
use tokio::fs;

use crate::{
    Artifact, ArtifactVersion, BasicExoHarness, BasicExoHarnessConfig, BeginTurnRequest, Binding,
    CreateSandboxRequest, EventData, EventQuery, EventQueryDirection, ExoHarness,
    ForkConversationRequest, NewAgentRequest, NewConversationRequest, PutSecretRequest,
    RunInSandboxRequest, SandboxBackendChoice, Secret, SecretBackendChoice, WriteArtifactRequest,
};

fn local_test_config(root: impl Into<std::path::PathBuf>) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.into(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_backend: SandboxBackendChoice::LocalProcess,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_supports_agent_and_conversation_crud() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");

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

#[tokio::test(flavor = "current_thread")]
async fn begin_turn_tracks_events_through_finish() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
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

#[tokio::test(flavor = "current_thread")]
async fn turn_events_continue_after_artifact_writes() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
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

#[tokio::test(flavor = "current_thread")]
async fn stale_turn_artifact_write_reports_unresumable_turn() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
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

    conversation
        .write_artifact(WriteArtifactRequest {
            path: "outside-turn.txt".to_string(),
            contents: b"outside".to_vec(),
        })
        .await
        .expect("advance conversation head outside turn");
    let error = turn
        .write_artifact(WriteArtifactRequest {
            path: "tool-results/example.json".to_string(),
            contents: br#"{"ok":true}"#.to_vec(),
        })
        .await
        .expect_err("stale turn should fail");
    let message = error.to_string();
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await
        .expect("events")
        .events;
    let expected_head_event = events
        .iter()
        .filter(|event| event.turn_id == Some(turn.record().id))
        .next_back()
        .expect("expected head event");
    let current_head_event = events.last().expect("current head event");
    let expected_at = expected_head_event
        .id
        .timestamp()
        .expect("expected head timestamp")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let current_at = current_head_event
        .id
        .timestamp()
        .expect("current head timestamp")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    assert!(
        message.contains("turn is stale and cannot be resumed"),
        "{message}"
    );
    assert!(message.contains(&turn.record().id.to_string()), "{message}");
    assert!(
        message.contains(&format!("expected_head_at: {expected_at}")),
        "{message}"
    );
    assert!(
        message.contains(&format!("current_head_at: {current_at}")),
        "{message}"
    );
    assert!(
        !message.contains(&expected_head_event.id.to_string()),
        "{message}"
    );
    assert!(
        !message.contains(&current_head_event.id.to_string()),
        "{message}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn artifacts_are_versioned_by_path() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
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

    let first = conversation
        .write_artifact(crate::WriteArtifactRequest {
            path: "notes.txt".to_string(),
            contents: b"hello".to_vec(),
        })
        .await
        .expect("write first artifact");
    let second = conversation
        .write_artifact(crate::WriteArtifactRequest {
            path: "notes.txt".to_string(),
            contents: b"world".to_vec(),
        })
        .await
        .expect("write second artifact");

    assert_eq!(first.artifact_id, second.artifact_id);
    assert_eq!(first.version, 1);
    assert_eq!(second.version, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn artifacts_store_metadata_and_raw_contents_separately() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");

    let version = agent
        .write_artifact(crate::WriteArtifactRequest {
            path: "config/executor.json".to_string(),
            contents: br#"{"model":"gpt-5.4"}"#.to_vec(),
        })
        .await
        .expect("write artifact");

    let artifact_dir = tempdir
        .path()
        .join("agents")
        .join(agent.record().id.to_string())
        .join("artifacts")
        .join(version.artifact_id.to_string());
    let metadata = fs::read_to_string(artifact_dir.join("1.json"))
        .await
        .expect("metadata file should exist");
    let metadata_json: serde_json::Value =
        serde_json::from_str(&metadata).expect("metadata should be valid json");
    assert!(metadata_json.get("contents").is_none());

    let contents = fs::read(artifact_dir.join("1.bin"))
        .await
        .expect("contents file should exist");
    assert_eq!(contents, br#"{"model":"gpt-5.4"}"#);
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_json_artifacts_are_still_readable() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");

    let artifact_id = crate::Uuid7::now();
    let artifact_dir = tempdir
        .path()
        .join("agents")
        .join(agent.record().id.to_string())
        .join("artifacts")
        .join(artifact_id.to_string());
    fs::create_dir_all(&artifact_dir)
        .await
        .expect("artifact dir should exist");
    let legacy_artifact = Artifact {
        version: ArtifactVersion {
            artifact_id,
            path: "config/executor.json".to_string(),
            version: 1,
            created_at: crate::Uuid7::now().timestamp().expect("uuid7 timestamp"),
            size_bytes: 19,
        },
        contents: br#"{"model":"gpt-5.4"}"#.to_vec(),
    };
    fs::write(
        artifact_dir.join("1.json"),
        serde_json::to_vec_pretty(&legacy_artifact).expect("legacy artifact should serialize"),
    )
    .await
    .expect("legacy artifact should write");

    let loaded = agent
        .read_artifact(crate::ReadArtifactRequest {
            artifact_id,
            version: Some(1),
        })
        .await
        .expect("legacy artifact should read")
        .expect("legacy artifact should exist");
    assert_eq!(loaded.contents, br#"{"model":"gpt-5.4"}"#);
}

#[tokio::test(flavor = "current_thread")]
async fn conversation_scope_overrides_agent_scope_and_fork_copies_local_state() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
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
        .await;
    let events = events.expect("get forked events").events;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::ConversationForked { .. }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn secrets_are_encrypted_at_rest() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".to_string(),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");

    let secret_id = agent
        .put_secret(PutSecretRequest {
            name: "OPENAI_API_KEY".to_string(),
            secret: Secret::Key {
                value: "super-secret-token".to_string(),
            },
        })
        .await
        .expect("secret should be stored");

    let stored_path = tempdir
        .path()
        .join("agents")
        .join(agent.record().id.to_string())
        .join("secrets")
        .join(format!("{secret_id}.json"));
    let stored_bytes = fs::read(stored_path)
        .await
        .expect("stored secret should exist");
    let stored_text = String::from_utf8_lossy(&stored_bytes);

    assert!(!stored_text.contains("super-secret-token"));
    assert!(stored_text.contains("\"ciphertext\""));
    assert!(stored_text.contains("\"algorithm\""));
}

#[tokio::test(flavor = "current_thread")]
async fn basic_backend_runs_commands_in_created_sandbox() {
    let tempdir = TempDir::new().expect("tempdir");
    let harness = BasicExoHarness::new(local_test_config(tempdir.path()))
        .await
        .expect("harness should initialize");
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

    let sandbox_id = conversation
        .create_sandbox(CreateSandboxRequest {
            image: "basic-local-process".to_string(),
            default_workdir: Some(tempdir.path().display().to_string()),
            file_system_mounts: None,
            enable_networking: Some(true),
            idle_seconds: Some(60),
        })
        .await
        .expect("sandbox should be created");

    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec!["/bin/sh".to_string(), "-lc".to_string(), "cat".to_string()],
            env: Default::default(),
        })
        .await
        .expect("sandbox command should run");
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut stderr = parts.stderr;
    let mut stdin = parts.stdin;
    stdin.write_all(b"hello").await.expect("stdin should write");
    drop(stdin);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let (stdout_result, stderr_result, wait_result) = tokio::join!(
        stdout.read_to_end(&mut stdout_bytes),
        stderr.read_to_end(&mut stderr_bytes),
        parts.wait,
    );

    stdout_result.expect("stdout should read");
    stderr_result.expect("stderr should read");
    assert_eq!(String::from_utf8_lossy(&stdout_bytes), "hello");
    assert_eq!(String::from_utf8_lossy(&stderr_bytes), "");
    assert_eq!(wait_result.expect("process should exit"), 0);
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
