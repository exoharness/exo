use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use futures::io::{AsyncReadExt, AsyncWriteExt};
use lingua::Message;
use lingua::universal::{AssistantContent, UserContent};
use tokio::time::timeout;

use crate::{
    BeginTurnRequest, Binding, EventData, EventKind, EventQuery, EventQueryDirection, ExoHarness,
    ForkConversationRequest, ManagedSandboxHandle, NewAgentRequest, NewConversationRequest,
    SandboxCommand, Uuid7, WriteArtifactRequest,
};

pub async fn supports_agent_and_conversation_crud(harness: Arc<dyn ExoHarness>) {
    let agent_slug = unique_slug("agent");
    let conversation_slug = unique_slug("conversation");
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: agent_slug,
            name: "Agent".to_string(),
        })
        .await
        .expect("agent should be created");
    let conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some(conversation_slug),
            name: Some("Conversation".to_string()),
        })
        .await
        .expect("conversation should be created");
    let events = conversation
        .get_events(None)
        .await
        .expect("get conversation events")
        .events;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::ConversationCreated { .. }))
    );

    assert!(
        harness
            .list_agents()
            .await
            .expect("list agents")
            .iter()
            .any(|candidate| candidate.record().id == agent.record().id)
    );
    assert!(
        agent
            .list_conversations()
            .await
            .expect("list conversations")
            .iter()
            .any(|candidate| candidate.record().id == conversation.record().id)
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
            slug: unique_slug("agent"),
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
        usage: None,
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

    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::SessionStarted))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::TurnStarted))
    );
    assert!(
        events
            .iter()
            .filter(|event| matches!(event.data, EventData::Messages { .. }))
            .count()
            >= 2
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.data, EventData::TurnEnded))
    );
    assert_eq!(events.last().expect("turn ended").id, latest_event_id);
}

pub async fn turn_events_continue_after_artifact_writes(harness: Arc<dyn ExoHarness>) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: unique_slug("agent"),
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
        usage: None,
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
            types: Some(vec![EventKind::ARTIFACT_WRITTEN]),
        }))
        .await
        .expect("artifact event")
        .events;
    let artifact_event = events.first().expect("artifact_written event");
    assert_eq!(artifact_event.session_id, Some(turn.record().session_id));
    assert_eq!(artifact_event.turn_id, Some(turn.record().id));
}

pub async fn conversation_scope_overrides_agent_scope_and_fork_copies_bindings(
    harness: Arc<dyn ExoHarness>,
) {
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: unique_slug("agent"),
            name: "Agent".to_string(),
        })
        .await
        .expect("agent");
    let conversation = agent
        .new_conversation(NewConversationRequest {
            slug: Some(unique_slug("base")),
            name: Some("Base".to_string()),
        })
        .await
        .expect("conversation");

    agent
        .put_binding(Binding::Env {
            name: "OPENAI_API_KEY".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            secret_id: Uuid7::now(),
        })
        .await
        .expect("agent binding");

    let conversation_binding_id = conversation
        .put_binding(Binding::Env {
            name: "OPENAI_API_KEY".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            secret_id: Uuid7::now(),
        })
        .await
        .expect("conversation binding");

    let effective_binding = conversation
        .list_bindings()
        .await
        .expect("list bindings")
        .into_iter()
        .find(|binding| binding.name == "OPENAI_API_KEY")
        .expect("effective binding");
    assert_eq!(effective_binding.id, conversation_binding_id);

    let forked = conversation
        .fork(ForkConversationRequest {
            up_to_inclusive: None,
            slug: Some(unique_slug("fork")),
            name: Some("Fork".to_string()),
        })
        .await
        .expect("fork");
    let forked_binding = forked
        .list_bindings()
        .await
        .expect("list forked bindings")
        .into_iter()
        .find(|binding| binding.name == "OPENAI_API_KEY")
        .expect("forked effective binding");
    assert_eq!(forked_binding.name, "OPENAI_API_KEY");
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

pub async fn sandbox_handle_start_process_supports_interactive_stdio_and_env(
    handle: Arc<dyn ManagedSandboxHandle>,
) -> crate::Result<()> {
    let result =
        sandbox_handle_start_process_supports_interactive_stdio_and_env_inner(Arc::clone(&handle))
            .await;
    let stop_result = handle.stop().await.context("stop sandbox after contract");
    match (result, stop_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(stop_error)) => Err(anyhow!(
            "{error:#}; also failed to stop sandbox after contract: {stop_error:#}"
        )),
    }
}

pub async fn sandbox_handle_start_process_supports_long_running_request_response_protocol(
    handle: Arc<dyn ManagedSandboxHandle>,
) -> crate::Result<()> {
    let result =
        sandbox_handle_start_process_supports_long_running_request_response_protocol_inner(
            Arc::clone(&handle),
        )
        .await;
    let stop_result = handle.stop().await.context("stop sandbox after contract");
    match (result, stop_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(stop_error)) => Err(anyhow!(
            "{error:#}; also failed to stop sandbox after contract: {stop_error:#}"
        )),
    }
}

async fn sandbox_handle_start_process_supports_interactive_stdio_and_env_inner(
    handle: Arc<dyn ManagedSandboxHandle>,
) -> crate::Result<()> {
    let mut env = std::collections::HashMap::new();
    env.insert(
        "EXO_CONTRACT_ENV".to_string(),
        "contract-env-value".to_string(),
    );
    let mut process = handle
        .start_process(&SandboxCommand {
            argv: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                "printf 'ready\\n'; IFS= read -r line; printf 'env=%s input=%s\\n' \"$EXO_CONTRACT_ENV\" \"$line\"".to_string(),
            ],
            env,
            display_argv: None,
            cwd: None,
            timeout: Some(Duration::from_secs(30)),
        })
        .await
        .context("start_process should start before the command exits")?;

    let mut ready = [0u8; 6];
    timeout(
        Duration::from_secs(10),
        process.stdout.read_exact(&mut ready),
    )
    .await
    .context("process should stream initial stdout before stdin is written")?
    .context("read ready marker")?;
    if &ready != b"ready\n" {
        bail!(
            "unexpected ready marker: {:?}",
            String::from_utf8_lossy(&ready)
        );
    }

    process
        .stdin
        .write_all(b"contract-stdin-value\n")
        .await
        .context("write process stdin")?;
    process.stdin.close().await.context("close process stdin")?;

    let expected_stdout = "env=contract-env-value input=contract-stdin-value\n";
    let mut final_stdout = vec![0u8; expected_stdout.len()];
    timeout(
        Duration::from_secs(10),
        process.stdout.read_exact(&mut final_stdout),
    )
    .await
    .context("process should stream stdout after stdin is written")?
    .context("read final stdout")?;
    let final_stdout = String::from_utf8(final_stdout).context("final stdout should be UTF-8")?;
    if final_stdout != expected_stdout {
        bail!("unexpected stdout: {final_stdout:?}");
    }

    let exit_code = timeout(Duration::from_secs(30), process.wait)
        .await
        .with_context(|| format!("process wait should finish after final stdout {final_stdout:?}"))?
        .context("process wait should succeed")?;

    let mut stderr = String::new();
    timeout(
        Duration::from_secs(5),
        process.stderr.read_to_string(&mut stderr),
    )
    .await
    .context("stderr should drain after process exit")?
    .context("read stderr")?;
    if exit_code != 0 {
        bail!("unexpected process exit code: {exit_code}; stderr: {stderr:?}");
    }
    if !stderr.is_empty() {
        bail!("unexpected stderr: {stderr:?}");
    }
    Ok(())
}

async fn sandbox_handle_start_process_supports_long_running_request_response_protocol_inner(
    handle: Arc<dyn ManagedSandboxHandle>,
) -> crate::Result<()> {
    let mut process = handle
        .start_process(&SandboxCommand {
            argv: vec![
                "/bin/sh".to_string(),
                "-lc".to_string(),
                [
                    "printf 'protocol-ready\\n'",
                    "while IFS= read -r line; do",
                    "  case \"$line\" in",
                    "    request-one) printf 'response-one\\n' ;;",
                    "    request-two) printf 'protocol-stderr-two\\n' >&2; printf 'response-two\\n' ;;",
                    "    request-three) printf 'response-three\\n'; exit 0 ;;",
                    "    *) printf 'unexpected:%s\\n' \"$line\"; exit 9 ;;",
                    "  esac",
                    "done",
                    "exit 8",
                ]
                .join("\n"),
            ],
            env: std::collections::HashMap::new(),
            display_argv: None,
            cwd: None,
            timeout: Some(Duration::from_secs(30)),
        })
        .await
        .context("start_process should start a long-running protocol process")?;

    read_exact_text(
        &mut process.stdout,
        "protocol-ready\n",
        "read protocol ready marker",
    )
    .await?;

    process
        .stdin
        .write_all(b"request-one\n")
        .await
        .context("write first protocol request")?;
    read_exact_text(
        &mut process.stdout,
        "response-one\n",
        "read first protocol response",
    )
    .await?;

    process
        .stdin
        .write_all(b"request-two\n")
        .await
        .context("write second protocol request")?;
    read_exact_text(
        &mut process.stdout,
        "response-two\n",
        "read second protocol response",
    )
    .await?;

    process
        .stdin
        .write_all(b"request-three\n")
        .await
        .context("write shutdown protocol request")?;
    process.stdin.close().await.context("close process stdin")?;
    read_exact_text(
        &mut process.stdout,
        "response-three\n",
        "read shutdown protocol response",
    )
    .await?;

    let exit_code = timeout(Duration::from_secs(30), process.wait)
        .await
        .context("protocol process wait should finish after shutdown")?
        .context("protocol process wait should succeed")?;
    let mut stderr = String::new();
    timeout(
        Duration::from_secs(5),
        process.stderr.read_to_string(&mut stderr),
    )
    .await
    .context("stderr should drain after protocol process exit")?
    .context("read protocol stderr")?;
    if exit_code != 0 {
        bail!("unexpected protocol process exit code: {exit_code}; stderr: {stderr:?}");
    }
    if stderr != "protocol-stderr-two\n" {
        bail!("unexpected protocol stderr: {stderr:?}");
    }
    Ok(())
}

async fn read_exact_text(
    reader: &mut (impl futures::io::AsyncRead + Unpin),
    expected: &str,
    context: &str,
) -> crate::Result<()> {
    let mut bytes = vec![0u8; expected.len()];
    timeout(Duration::from_secs(10), reader.read_exact(&mut bytes))
        .await
        .with_context(|| context.to_string())?
        .with_context(|| context.to_string())?;
    let actual = String::from_utf8(bytes).with_context(|| format!("{context}: invalid UTF-8"))?;
    if actual != expected {
        bail!("{context}: expected {expected:?}, got {actual:?}");
    }
    Ok(())
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

fn unique_slug(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid7::now())
}
