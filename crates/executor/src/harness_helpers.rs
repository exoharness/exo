use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use exoharness::{
    AddEventsRequest, AgentHandle, Binding, ConversationHandle, EventData, EventKind, EventQuery,
    EventQueryDirection, ExoHarness, Result, Secret, ToolCallId, Uuid7,
};
use lingua::Message;
use lingua::universal::{
    AssistantContent, AssistantContentPart, ToolContentPart, UserContent, UserContentPart,
};
use serde::{Deserialize, Serialize};

use crate::ConversationModelConfig;

pub(crate) const CONVERSATION_MODEL_CONFIG_EVENT_TYPE: &str = "conversation_model_config";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ConversationModelConfigEvent {
    Set {
        model: String,
        max_output_tokens: Option<i64>,
    },
    Clear,
}

impl ConversationModelConfigEvent {
    fn into_model_config(self) -> Option<ConversationModelConfig> {
        match self {
            Self::Set {
                model,
                max_output_tokens,
            } => Some(ConversationModelConfig {
                model,
                max_output_tokens,
            }),
            Self::Clear => None,
        }
    }
}

pub(crate) async fn resolve_agent_handle(
    exoharness: &dyn ExoHarness,
    agent_ref: &str,
) -> Result<Option<Arc<dyn AgentHandle>>> {
    if let Some(agent_id) = parse_uuid7(agent_ref)
        && let Some(agent) = exoharness.get_agent(&agent_id).await?
    {
        return Ok(Some(agent));
    }

    let agents = exoharness.list_agents().await?;
    Ok(agents
        .into_iter()
        .find(|agent| agent.record().slug == agent_ref))
}

pub(crate) async fn resolve_conversation_handle(
    agent: &dyn AgentHandle,
    conversation_ref: &str,
) -> Result<Option<Arc<dyn ConversationHandle>>> {
    if let Some(conversation_id) = parse_uuid7(conversation_ref)
        && let Some(conversation) = agent.get_conversation(&conversation_id).await?
    {
        return Ok(Some(conversation));
    }

    let conversations = list_conversation_handles(agent).await?;
    Ok(conversations
        .into_iter()
        .find(|conversation| conversation.record().slug == conversation_ref))
}

pub(crate) async fn list_conversation_handles(
    agent: &dyn AgentHandle,
) -> Result<Vec<Arc<dyn ConversationHandle>>> {
    let mut conversations = Vec::new();
    let mut cursor = None;
    loop {
        let page = agent
            .list_conversations(exoharness::ListConversationsRequest {
                cursor,
                limit: None,
            })
            .await?;
        conversations.extend(page.conversations);
        match page.next_cursor {
            Some(next) => {
                anyhow::ensure!(
                    cursor.is_none_or(|cursor| next < cursor),
                    "thread listing cursor did not advance"
                );
                cursor = Some(next);
            }
            None => return Ok(conversations),
        }
    }
}

pub async fn materialize_conversation_messages(
    conversation: &dyn ConversationHandle,
) -> Result<Vec<Message>> {
    let mut events = Vec::new();
    let mut cursor = None;
    loop {
        let page = conversation
            .get_events(Some(EventQuery {
                cursor,
                direction: Some(EventQueryDirection::Asc),
                limit: None,
                ..Default::default()
            }))
            .await?;
        if page.events.is_empty() {
            break;
        }
        let next = page.events.last().map(|event| event.id);
        anyhow::ensure!(next > cursor, "conversation history cursor did not advance");
        cursor = next;
        events.extend(page.events);
    }

    let mut messages = Vec::new();
    let mut tool_call_names = HashMap::<ToolCallId, String>::new();

    crate::basic::extend_message_history(&mut messages, &mut tool_call_names, &events);

    Ok(messages)
}

pub async fn get_conversation_model_override(
    conversation: &dyn ConversationHandle,
) -> Result<Option<ConversationModelConfig>> {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(1),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::custom(
                CONVERSATION_MODEL_CONFIG_EVENT_TYPE,
            )]),
        }))
        .await?
        .events;

    let Some(event) = events.into_iter().next() else {
        return Ok(None);
    };

    let EventData::Custom { payload, .. } = event.data else {
        return Ok(None);
    };
    let config_event: ConversationModelConfigEvent = serde_json::from_value(payload)?;
    Ok(config_event.into_model_config())
}

pub async fn put_conversation_model_override(
    conversation: &dyn ConversationHandle,
    config: Option<ConversationModelConfig>,
) -> Result<()> {
    let payload = match config {
        Some(ConversationModelConfig {
            model,
            max_output_tokens,
        }) => serde_json::to_value(ConversationModelConfigEvent::Set {
            model,
            max_output_tokens,
        })?,
        None => serde_json::to_value(ConversationModelConfigEvent::Clear)?,
    };

    conversation
        .add_events(AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![EventData::Custom {
                event_type: CONVERSATION_MODEL_CONFIG_EVENT_TYPE.to_string(),
                payload,
            }],
        })
        .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedModelBinding {
    pub(crate) model: String,
    pub(crate) api_key: Option<String>,
    pub(crate) base_url: Option<String>,
}

pub(crate) async fn resolve_model_binding(
    conversation: &dyn ConversationHandle,
    name: &str,
) -> Result<ResolvedModelBinding> {
    let binding_record = conversation
        .list_bindings()
        .await?
        .into_iter()
        .find(|binding| binding.name == name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "model is not registered: {name}; run `exo model create {name} --secret <secret>`"
            )
        })?;
    let Binding::Llm {
        model,
        mut base_url,
        secret,
        ..
    } = binding_record.binding
    else {
        return Err(anyhow::anyhow!("binding is not a model: {name}"));
    };
    anyhow::ensure!(
        conversation.caller().is_none() || secret.is_some(),
        "authenticated runs require a model credential from a selected vault; register the model with --secret"
    );
    let api_key = match secret {
        Some(reference) if conversation.caller().is_some() => {
            let variable = if crate::harness_runtime::is_anthropic_model(&model) {
                "ANTHROPIC_API_KEY"
            } else {
                "OPENAI_API_KEY"
            };
            let endpoint = exoharness::vault::model_endpoint(base_url.as_deref(), variable)?;
            base_url = Some(endpoint.to_string());
            Some(exoharness::vault::resolve_model_key(conversation, &reference, &endpoint).await?)
        }
        Some(reference) => {
            let secret = exoharness::vault::require_vault(conversation, &reference.vault_id)
                .await?
                .get_secret(&reference.secret_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("model secret does not exist for {name}"))?;
            match secret {
                Secret::Key { value } => Some(value),
                Secret::Oauth { .. } => {
                    return Err(anyhow::anyhow!(
                        "model secret must be a key secret, got oauth for {name}"
                    ));
                }
            }
        }
        None => None,
    };
    Ok(ResolvedModelBinding {
        model,
        api_key,
        base_url,
    })
}

pub(crate) fn to_lingua_value(value: serde_json::Value) -> lingua::serde_json::Value {
    match value {
        serde_json::Value::Null => lingua::serde_json::Value::Null,
        serde_json::Value::Bool(value) => lingua::serde_json::Value::Bool(value),
        serde_json::Value::Number(value) => lingua::serde_json::Value::Number(
            lingua::serde_json::Number::from_string_unchecked(value.to_string()),
        ),
        serde_json::Value::String(value) => lingua::serde_json::Value::String(value),
        serde_json::Value::Array(values) => {
            lingua::serde_json::Value::Array(values.into_iter().map(to_lingua_value).collect())
        }
        serde_json::Value::Object(values) => lingua::serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, to_lingua_value(value)))
                .collect(),
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HistoryMessage {
    pub index: usize,
    pub role: String,
    pub content: String,
}

pub(crate) fn messages_to_history_messages(messages: &[Message]) -> Vec<HistoryMessage> {
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| history_message_from_message(index, message))
        .collect()
}

pub(crate) fn messages_to_transcript(messages: &[Message]) -> String {
    messages_to_history_messages(messages)
        .into_iter()
        .map(|message| format!("{}:\n{}", message.role.to_uppercase(), message.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) fn assistant_messages_text(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { content, .. } => Some(render_assistant_content(content)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn system_message(text: &str) -> Message {
    Message::System {
        content: UserContent::String(text.to_string()),
    }
}

pub(crate) fn user_message(text: &str) -> Message {
    Message::User {
        content: UserContent::String(text.to_string()),
    }
}

pub(crate) fn assistant_message(text: &str) -> Message {
    Message::Assistant {
        content: AssistantContent::String(text.to_string()),
        id: None,
    }
}

fn history_message_from_message(index: usize, message: &Message) -> HistoryMessage {
    match message {
        Message::User { content } => HistoryMessage {
            index,
            role: "user".to_string(),
            content: render_user_content(content),
        },
        Message::Assistant { content, .. } => HistoryMessage {
            index,
            role: "assistant".to_string(),
            content: render_assistant_content(content),
        },
        Message::Tool { content } => HistoryMessage {
            index,
            role: "tool".to_string(),
            content: content
                .iter()
                .map(|part| {
                    let ToolContentPart::ToolResult(result) = part;
                    format!("{} => {}", result.tool_name, result.output)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        },
        Message::System { content } => HistoryMessage {
            index,
            role: "system".to_string(),
            content: render_user_content(content),
        },
        Message::Developer { content } => HistoryMessage {
            index,
            role: "developer".to_string(),
            content: render_user_content(content),
        },
    }
}

fn parse_uuid7(raw: &str) -> Option<Uuid7> {
    Uuid7::from_str(raw).ok()
}

fn render_user_content(content: &UserContent) -> String {
    match content {
        UserContent::String(text) => text.clone(),
        UserContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                UserContentPart::Text(text) => text.text.clone(),
                _ => "[non-text user content]".to_string(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn render_assistant_content(content: &AssistantContent) -> String {
    match content {
        AssistantContent::String(text) => text.clone(),
        AssistantContent::Array(parts) => parts
            .iter()
            .map(|part| match part {
                AssistantContentPart::Text(text) => text.text.clone(),
                AssistantContentPart::Reasoning { text, .. } => format!("[reasoning] {text}"),
                AssistantContentPart::ToolCall {
                    tool_name,
                    arguments,
                    ..
                } => format!("[tool_call {tool_name}] {arguments}"),
                AssistantContentPart::ToolResult {
                    tool_name, output, ..
                } => format!("[tool_result {tool_name}] {output}"),
                AssistantContentPart::File { .. } => "[file]".to_string(),
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::to_lingua_value;

    #[test]
    fn materializes_embedded_and_standalone_calls_without_duplicates() {
        use super::*;
        let call = json!({"role": "assistant", "content": [{
            "type": "tool_call", "tool_call_id": "call", "tool_name": "shell",
            "arguments": {"type": "valid", "value": {}}
        }]});
        for (embedded, requested) in [(false, true), (true, true), (true, false)] {
            for result in [false, true] {
                let mut data = Vec::new();
                if embedded {
                    data.push(json!({"type": "messages", "messages": [call.clone()]}));
                }
                if requested {
                    data.push(json!({"type": "tool_requested", "tool_call_id": "call",
                        "request": {"function_name": "shell", "arguments": {}}}));
                }
                data.push(json!({"type": "messages", "messages": []}));
                if result {
                    data.push(
                        json!({"type": "tool_result", "tool_call_id": "call", "result": "done"}),
                    );
                }
                data.push(
                    json!({"type": "messages", "messages": [{"role": "user", "content": "next"}]}),
                );
                let events: Vec<_> = data
                    .into_iter()
                    .map(|data| {
                        let id = Uuid7::now();
                        exoharness::Event {
                            id,
                            thread_id: id,
                            session_id: None,
                            turn_id: None,
                            created_at: id.timestamp().unwrap(),
                            data: serde_json::from_value(data).unwrap(),
                        }
                    })
                    .collect();
                let mut messages = Vec::new();
                crate::basic::extend_message_history(&mut messages, &mut HashMap::new(), &events);
                assert_eq!(messages.len(), 3);
                assert!(matches!(messages[0], Message::Assistant { .. }));
                let Message::Tool { content } = &messages[1] else {
                    panic!("missing result")
                };
                let ToolContentPart::ToolResult(output) = &content[0];
                assert_eq!(output.tool_call_id, "call");
                assert_eq!(output.tool_name, "shell");
                assert_eq!(
                    output.output,
                    to_lingua_value(if result {
                        json!("done")
                    } else {
                        json!({"ok": false, "error": "tool execution did not complete before the previous turn ended"})
                    })
                );
                assert!(matches!(messages[2], Message::User { .. }));
            }
        }
    }

    #[test]
    fn converts_std_json_to_lingua_json_structurally() {
        let value = json!({
            "null": null,
            "bool": true,
            "number": 123.5,
            "string": "hello",
            "array": [1, false, {"nested": "value"}],
        });

        let encoded = serde_json::to_string(&value).expect("test json should serialize");
        let expected: lingua::serde_json::Value =
            lingua::serde_json::from_str(&encoded).expect("test json should parse as lingua json");

        assert_eq!(to_lingua_value(value), expected);
    }
}

#[cfg(test)]
mod vault_tests {
    use super::*;
    use exoharness::vault::SecretReference;
    use exoharness::{
        BasicExoHarness, BasicExoHarnessConfig, NewAgentRequest, NewThreadRequest,
        PutSecretRequest, SandboxBackendRegistration, SandboxProvider, SecretBackendChoice,
    };

    #[tokio::test]
    async fn model_auth_uses_the_runtime_vault_even_when_user_secret_names_match() -> Result<()> {
        let harness = BasicExoHarness::in_memory(BasicExoHarnessConfig {
            root: Default::default(),
            secret_backend: SecretBackendChoice::Static([1; 32]),
            sandbox_default: SandboxProvider::LocalProcess,
            sandbox_policy: None,
            sandbox_backends: vec![SandboxBackendRegistration::local_process()],
        })
        .await?;
        let runtime = exoharness::vault::global_vault(&harness).await?;
        let user = harness.create_vault("alice").await?;
        let model_secret = runtime
            .put_secret(PutSecretRequest {
                name: "provider".into(),
                target: None,
                secret: Secret::Key {
                    value: "runtime-key".into(),
                },
            })
            .await?;
        let user_secret = user
            .put_secret(PutSecretRequest {
                name: "provider".into(),
                target: None,
                secret: Secret::Key {
                    value: "user-key".into(),
                },
            })
            .await?;
        harness
            .put_binding(Binding::Llm {
                name: "model".into(),
                model: "gpt-5.6-sol".into(),
                base_url: None,
                secret: Some(SecretReference {
                    vault_id: runtime.record().id,
                    secret_id: model_secret,
                }),
            })
            .await?;
        let agent = harness
            .new_agent(NewAgentRequest {
                vaults: vec![],
                name: "agent".into(),
                slug: "agent".into(),
            })
            .await?;
        let thread = agent
            .new_thread(NewThreadRequest {
                vaults: vec![user.record().id],
                ..Default::default()
            })
            .await?;
        let model = resolve_model_binding(thread.as_ref(), "model").await?;
        assert_eq!(model.api_key.as_deref(), Some("runtime-key"));
        user.update_secret(
            &user_secret,
            Secret::Key {
                value: "rotated-user-key".into(),
            },
        )
        .await?;
        assert_eq!(
            resolve_model_binding(thread.as_ref(), "model")
                .await?
                .api_key
                .as_deref(),
            Some("runtime-key")
        );
        harness
            .put_binding(Binding::Llm {
                name: "user-secret-model".into(),
                model: "gpt-5.6-sol".into(),
                base_url: None,
                secret: Some(SecretReference {
                    vault_id: runtime.record().id,
                    secret_id: user_secret,
                }),
            })
            .await?;
        assert!(
            resolve_model_binding(thread.as_ref(), "user-secret-model")
                .await
                .is_err()
        );
        Ok(())
    }
}
