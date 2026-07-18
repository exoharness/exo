use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use exoharness::{
    AddEventsRequest, AgentHandle, Binding, ConversationHandle, EventData, EventKind, EventQuery,
    EventQueryDirection, ExoHarness, ResolvedSecret, Result, ToolCallId, Uuid7,
};
use lingua::Message;
use lingua::universal::{
    AssistantContent, AssistantContentPart, ToolContentPart, ToolResultContentPart, UserContent,
    UserContentPart,
};
use serde::{Deserialize, Serialize};

use crate::{ConversationModelConfig, ModelRequestAuth};

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

    let conversations = agent
        .list_conversations(exoharness::ListConversationsRequest::default())
        .await?
        .conversations;
    Ok(conversations
        .into_iter()
        .find(|conversation| conversation.record().slug == conversation_ref))
}

pub(crate) async fn materialize_conversation_messages(
    conversation: &dyn ConversationHandle,
) -> Result<Vec<Message>> {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Asc),
            limit: None,
            session_id: None,
            turn_id: None,
            types: None,
        }))
        .await?
        .events;

    let mut messages = Vec::new();
    let mut tool_call_names = HashMap::<ToolCallId, String>::new();

    for event in events {
        match event.data {
            EventData::Messages {
                messages: event_messages,
                ..
            } => messages.extend(event_messages),
            EventData::ToolRequested {
                tool_call_id,
                request,
                ..
            } => {
                tool_call_names.insert(tool_call_id, request.function_name);
            }
            EventData::ToolResult {
                tool_call_id,
                result,
            } => {
                let Some(tool_name) = tool_call_names.get(&tool_call_id) else {
                    continue;
                };
                messages.push(Message::Tool {
                    content: vec![ToolContentPart::ToolResult(ToolResultContentPart {
                        tool_call_id,
                        tool_name: tool_name.clone(),
                        output: to_lingua_value(result),
                        provider_options: None,
                    })],
                });
            }
            _ => {}
        }
    }

    Ok(messages)
}

pub(crate) async fn get_conversation_model_override(
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

pub(crate) async fn put_conversation_model_override(
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
    pub(crate) provider: String,
    pub(crate) auth: Option<ModelRequestAuth>,
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
                "model is not registered: {name}; run `exo model register {name} --secret <secret>`"
            )
        })?;
    let Binding::Llm {
        model,
        provider,
        base_url,
        secret_id,
        ..
    } = binding_record.binding
    else {
        return Err(anyhow::anyhow!("binding is not a model: {name}"));
    };
    let provider = provider.unwrap_or_else(|| infer_model_provider(&model, base_url.as_deref()));
    let auth = match secret_id {
        Some(secret_id) => {
            let secret = conversation
                .resolve_secret(&secret_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("model secret does not exist for {name}"))?;
            Some(request_auth_for_secret(&provider, secret)?)
        }
        None => None,
    };
    Ok(ResolvedModelBinding {
        model,
        provider,
        auth,
        base_url,
    })
}

fn infer_model_provider(model: &str, base_url: Option<&str>) -> String {
    if base_url.is_some_and(|url| url.contains("openrouter.ai")) {
        "openrouter".to_string()
    } else if model.to_ascii_lowercase().starts_with("claude") {
        "anthropic".to_string()
    } else {
        "openai".to_string()
    }
}

fn request_auth_for_secret(provider: &str, secret: ResolvedSecret) -> Result<ModelRequestAuth> {
    match secret {
        ResolvedSecret::Key { value } if provider == "anthropic" => Ok(ModelRequestAuth {
            authorization: None,
            headers: HashMap::from([("x-api-key".to_string(), value)]),
        }),
        ResolvedSecret::Key { value } => Ok(ModelRequestAuth {
            authorization: Some(format!("Bearer {value}")),
            headers: HashMap::new(),
        }),
        ResolvedSecret::AccessToken {
            provider: credential_provider,
            access_token,
        } => {
            if credential_provider != provider {
                return Err(anyhow::anyhow!(
                    "model provider `{provider}` cannot use an OAuth credential for `{credential_provider}`"
                ));
            }
            if provider != exoharness::OPENAI_CHATGPT_PROVIDER_ID {
                return Err(anyhow::anyhow!(
                    "OAuth request authentication is not implemented for provider `{provider}`"
                ));
            }
            let account_id = chatgpt_account_id(&access_token)?;
            Ok(ModelRequestAuth {
                authorization: Some(format!("Bearer {access_token}")),
                headers: HashMap::from([
                    ("chatgpt-account-id".to_string(), account_id),
                    ("originator".to_string(), "exo".to_string()),
                    (
                        "user-agent".to_string(),
                        format!("exo/{}", env!("CARGO_PKG_VERSION")),
                    ),
                    (
                        "OpenAI-Beta".to_string(),
                        "responses=experimental".to_string(),
                    ),
                ]),
            })
        }
    }
}

#[derive(Deserialize)]
struct ChatGptAccessTokenClaims {
    #[serde(rename = "https://api.openai.com/auth")]
    auth: ChatGptAuthClaims,
}

#[derive(Deserialize)]
struct ChatGptAuthClaims {
    chatgpt_account_id: String,
}

fn chatgpt_account_id(access_token: &str) -> Result<String> {
    let payload = access_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("OpenAI ChatGPT access token is not a JWT"))?;
    let bytes = URL_SAFE_NO_PAD.decode(payload)?;
    let claims: ChatGptAccessTokenClaims = serde_json::from_slice(&bytes)?;
    Ok(claims.auth.chatgpt_account_id)
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
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use exoharness::ResolvedSecret;
    use serde_json::json;

    use super::{request_auth_for_secret, to_lingua_value};

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

    #[test]
    fn generic_request_auth_maps_keys_by_provider() {
        let openai = request_auth_for_secret(
            "openai",
            ResolvedSecret::Key {
                value: "openai-key".to_string(),
            },
        )
        .unwrap();
        assert_eq!(openai.authorization.as_deref(), Some("Bearer openai-key"));
        assert!(openai.headers.is_empty());

        let anthropic = request_auth_for_secret(
            "anthropic",
            ResolvedSecret::Key {
                value: "anthropic-key".to_string(),
            },
        )
        .unwrap();
        assert_eq!(anthropic.authorization, None);
        assert_eq!(anthropic.headers["x-api-key"], "anthropic-key");
    }

    #[test]
    fn chatgpt_access_token_adds_typed_account_and_protocol_headers() {
        let claims = serde_json::to_vec(&json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "account-id"
            }
        }))
        .unwrap();
        let access_token = format!("header.{}.signature", URL_SAFE_NO_PAD.encode(claims));
        let auth = request_auth_for_secret(
            exoharness::OPENAI_CHATGPT_PROVIDER_ID,
            ResolvedSecret::AccessToken {
                provider: exoharness::OPENAI_CHATGPT_PROVIDER_ID.to_string(),
                access_token: access_token.clone(),
            },
        )
        .unwrap();

        assert_eq!(auth.authorization, Some(format!("Bearer {access_token}")));
        assert_eq!(auth.headers["chatgpt-account-id"], "account-id");
        assert_eq!(auth.headers["originator"], "exo");
        assert_eq!(auth.headers["OpenAI-Beta"], "responses=experimental");
        assert!(auth.headers["user-agent"].starts_with("exo/"));
    }
}
