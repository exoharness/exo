use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use crate::harness_helpers::to_lingua_value;
use crate::{
    ModelClient, ModelRequest, ModelResponse, ModelResponseStream, PendingToolCall, ToolDefinition,
};
use async_trait::async_trait;
use braintrust_llm_router::{
    AuthConfig, ClientHeaders, ModelCatalog, ModelFlavor, ModelSpec, Router, create_provider,
};
use bytes::Bytes;
use exoharness::{Result, Uuid7};
use futures::{Stream, StreamExt};
use lingua::processing::adapter_for_format;
use lingua::serde_json as lingua_json;
use lingua::universal::{
    AssistantContent, AssistantContentPart, TextContentPart, TokenBudget, ToolCallArguments,
    ToolChoiceConfig, ToolChoiceMode, UniversalParams, UniversalRequest, UniversalResponse,
    UniversalStreamChunk, UniversalTool,
};
use lingua::{Message, ProviderFormat};
use reqwest::Url;

type UniversalChunkStream = Pin<Box<dyn Stream<Item = Result<UniversalStreamChunk>> + Send>>;

#[derive(Debug, Default, Clone)]
pub struct RouterModelClient {
    env: Arc<HashMap<String, String>>,
    catalog: Arc<ModelCatalog>,
}

impl RouterModelClient {
    pub fn new(env: HashMap<String, String>) -> Result<Self> {
        let catalog = load_model_catalog(&env)?;
        Ok(Self {
            env: Arc::new(env),
            catalog: Arc::new(catalog),
        })
    }
}

#[async_trait]
impl ModelClient for RouterModelClient {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let (spec, format) = resolve_model(&self.catalog, &request);
        let config = resolve_runtime_config(&request, self.env.as_ref())?;
        let router = build_router(spec, format, &config)?;
        let universal_request = build_universal_request(&request, false)?;
        let payload = serialize_request(format, &universal_request)?;
        let (prepared, _router_metadata) = router
            .create_request(payload, &request.model, format)
            .await?;
        let body = router.complete(prepared, &ClientHeaders::new()).await?;
        let response = lingua::response_to_universal(body)?;
        normalize_model_response(response)
    }

    async fn complete_stream(&self, request: ModelRequest) -> Result<Box<dyn ModelResponseStream>> {
        let (spec, format) = resolve_model(&self.catalog, &request);
        let config = resolve_runtime_config(&request, self.env.as_ref())?;
        let router = build_router(spec, format, &config)?;
        let universal_request = build_universal_request(&request, true)?;
        let payload = serialize_request(format, &universal_request)?;
        let (prepared, _router_metadata) = router
            .create_stream_request(payload, &request.model, format)
            .await?;
        let raw_stream = router
            .complete_stream(prepared, &ClientHeaders::new())
            .await?;
        Ok(Box::new(RouterModelResponseStream {
            stream: map_universal_stream(raw_stream, format),
            accumulator: UniversalResponseAccumulator::default(),
        }))
    }
}

struct RouterModelResponseStream {
    stream: UniversalChunkStream,
    accumulator: UniversalResponseAccumulator,
}

#[async_trait]
impl ModelResponseStream for RouterModelResponseStream {
    async fn next_chunk(&mut self) -> Result<Option<UniversalStreamChunk>> {
        let Some(chunk) = self.stream.next().await.transpose()? else {
            return Ok(None);
        };
        self.accumulator.push(&chunk);
        Ok(Some(chunk))
    }

    async fn finish(self: Box<Self>) -> Result<ModelResponse> {
        normalize_model_response(self.accumulator.finalize())
    }
}

#[derive(Debug, Clone)]
struct ResolvedRuntimeConfig {
    provider_alias: String,
    provider_kind: String,
    endpoint: Option<Url>,
    endpoint_template: Option<String>,
    metadata: HashMap<String, lingua_json::Value>,
    auth: AuthConfig,
}

fn resolve_runtime_config(
    request: &ModelRequest,
    env: &HashMap<String, String>,
) -> Result<ResolvedRuntimeConfig> {
    let key = request
        .api_key
        .clone()
        .or_else(|| optional_env(env, "OPENAI_API_KEY"))
        .ok_or_else(|| anyhow::anyhow!("model request is missing an API key"))?;
    let endpoint = request
        .base_url
        .clone()
        .or_else(|| optional_env(env, "OPENAI_BASE_URL"))
        .map(|raw| Url::parse(&raw))
        .transpose()?;
    let mut metadata = HashMap::new();
    if let Some(organization_id) = optional_env(env, "OPENAI_ORG_ID") {
        metadata.insert(
            "organization_id".to_string(),
            lingua_json::Value::String(organization_id),
        );
    }
    if let Some(project) = optional_env(env, "OPENAI_PROJECT") {
        metadata.insert("project".to_string(), lingua_json::Value::String(project));
    }
    Ok(ResolvedRuntimeConfig {
        provider_alias: "openai".to_string(),
        provider_kind: "openai".to_string(),
        endpoint,
        endpoint_template: None,
        metadata,
        auth: AuthConfig::ApiKey {
            key,
            header: Some("authorization".to_string()),
            prefix: Some("Bearer".to_string()),
        },
    })
}

fn optional_env(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).cloned().or_else(|| std::env::var(key).ok())
}

/// Path to a Braintrust-style `model_list.json` registry to resolve model
/// capabilities (wire format, flavor, context window, ...). When unset, models
/// are resolved by name via lingua's built-in model-family detection.
const MODEL_LIST_PATH_ENV: &str = "EXO_MODEL_LIST_PATH";

fn load_model_catalog(env: &HashMap<String, String>) -> Result<ModelCatalog> {
    match optional_env(env, MODEL_LIST_PATH_ENV) {
        Some(path) => Ok(ModelCatalog::from_file(&path)?),
        None => Ok(ModelCatalog::empty()),
    }
}

/// Resolve the model spec and the wire format to use for a request.
///
/// The spec comes from the configured registry when the model is known, and
/// otherwise falls back to a Chat Completions default. Either way the wire
/// format is finalized through [`wire_format`], which upgrades OpenAI models
/// that require the Responses API.
fn resolve_model(catalog: &ModelCatalog, request: &ModelRequest) -> (ModelSpec, ProviderFormat) {
    let mut spec = catalog
        .get(&request.model)
        .map(|spec| (*spec).clone())
        .unwrap_or_else(|| default_model_spec(&request.model));
    // Key the spec by the exact requested id so the single-entry catalog in
    // `build_router` and the `create_request` lookup always agree on the model.
    spec.model = request.model.clone();
    let format = wire_format(&spec);
    spec.format = format;
    if format == ProviderFormat::Responses {
        spec.flavor = ModelFlavor::Responses;
    }
    if spec.max_output_tokens.is_none() {
        spec.max_output_tokens = request.max_output_tokens.map(|tokens| tokens as u32);
    }
    (spec, format)
}

/// Spec used for models that are absent from the configured registry.
///
/// Defaults to OpenAI Chat Completions, the format spoken by OpenAI and the
/// broad set of OpenAI-compatible providers (DeepSeek, Together, Groq, vLLM,
/// Ollama, ...). [`wire_format`] still upgrades it to Responses by name when
/// required.
fn default_model_spec(model: &str) -> ModelSpec {
    ModelSpec {
        model: model.to_string(),
        format: ProviderFormat::ChatCompletions,
        flavor: ModelFlavor::Chat,
        display_name: None,
        parent: None,
        input_cost_per_mil_tokens: None,
        output_cost_per_mil_tokens: None,
        input_cache_read_cost_per_mil_tokens: None,
        multimodal: None,
        reasoning: None,
        max_input_tokens: None,
        max_output_tokens: None,
        supports_streaming: true,
        extra: Default::default(),
        available_providers: Vec::new(),
    }
}

/// Finalize the wire format for a spec.
///
/// The public model registry encodes OpenAI's Responses-API models (o1-pro,
/// o3-pro, gpt-5-pro, gpt-5-codex, gpt-5.3+, ...) as Chat Completions and
/// distinguishes them by name. Upgrade those to the Responses format so
/// requests reach the correct OpenAI API surface; all other formats pass
/// through unchanged.
fn wire_format(spec: &ModelSpec) -> ProviderFormat {
    if spec.format == ProviderFormat::ChatCompletions && spec.requires_responses_api() {
        ProviderFormat::Responses
    } else {
        spec.format
    }
}

fn build_router(
    mut spec: ModelSpec,
    format: ProviderFormat,
    config: &ResolvedRuntimeConfig,
) -> Result<Router> {
    let provider = create_provider(
        &config.provider_kind,
        config.endpoint.as_ref(),
        config.endpoint_template.as_deref(),
        None,
        &config.metadata,
        None,
    )?;

    // exo dispatches through a single provider resolved from the binding, so the
    // catalog points the model at that provider regardless of the registry's own
    // provider list.
    spec.available_providers = vec![config.provider_alias.clone()];
    let model = spec.model.clone();
    let mut catalog = ModelCatalog::empty();
    catalog.insert(model, spec);

    Router::builder()
        .with_catalog(Arc::new(catalog))
        .add_provider_arc(
            config.provider_alias.clone(),
            provider,
            config.auth.clone(),
            vec![format],
        )
        .build()
        .map_err(Into::into)
}

fn build_universal_request(request: &ModelRequest, stream: bool) -> Result<UniversalRequest> {
    let tools = build_universal_tools(&request.tools)?;
    let has_tools = !tools.is_empty();
    let params = UniversalParams {
        token_budget: request.max_output_tokens.map(TokenBudget::OutputTokens),
        tools: if tools.is_empty() { None } else { Some(tools) },
        tool_choice: has_tools.then_some(ToolChoiceConfig {
            mode: Some(ToolChoiceMode::Auto),
            tool_name: None,
        }),
        parallel_tool_calls: has_tools.then_some(true),
        stream: Some(stream),
        ..Default::default()
    };

    Ok(UniversalRequest {
        model: Some(request.model.clone()),
        messages: request.messages.clone(),
        params,
    })
}

fn build_universal_tools(tools: &[ToolDefinition]) -> Result<Vec<UniversalTool>> {
    tools.iter().map(tool_definition_to_universal).collect()
}

fn tool_definition_to_universal(tool: &ToolDefinition) -> Result<UniversalTool> {
    Ok(UniversalTool::function(
        tool.name.clone(),
        Some(tool.description.clone()),
        Some(to_lingua_value(tool.parameters.clone())),
        Some(true),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct SerializedChatStreamRequest {
        stream: Option<bool>,
        stream_options: Option<SerializedChatStreamOptions>,
    }

    #[derive(Debug, Deserialize)]
    struct SerializedChatStreamOptions {
        include_usage: Option<bool>,
    }

    #[derive(Debug, Deserialize)]
    struct SerializedResponsesStreamRequest {
        stream: Option<bool>,
        stream_options: Option<SerializedResponsesStreamOptions>,
    }

    #[derive(Debug, Deserialize)]
    struct SerializedResponsesStreamOptions {}

    fn model_request() -> ModelRequest {
        ModelRequest {
            model: "gpt-5.4".to_string(),
            api_key: None,
            base_url: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: None,
        }
    }

    fn request_for(model: &str) -> ModelRequest {
        ModelRequest {
            model: model.to_string(),
            ..model_request()
        }
    }

    #[test]
    fn wire_format_upgrades_openai_responses_models() {
        for model in ["o1-pro", "o3-pro", "gpt-5-pro", "gpt-5-codex", "gpt-5.4"] {
            assert_eq!(
                wire_format(&default_model_spec(model)),
                ProviderFormat::Responses,
                "{model} should use the Responses API",
            );
        }
    }

    #[test]
    fn wire_format_keeps_chat_completions_for_compatible_models() {
        for model in ["gpt-4o", "gpt-5-mini", "deepseek-chat", "deepseek-reasoner"] {
            assert_eq!(
                wire_format(&default_model_spec(model)),
                ProviderFormat::ChatCompletions,
                "{model} should use Chat Completions",
            );
        }
    }

    #[test]
    fn wire_format_preserves_non_openai_formats() {
        let mut spec = default_model_spec("claude-sonnet-4");
        spec.format = ProviderFormat::Anthropic;
        assert_eq!(wire_format(&spec), ProviderFormat::Anthropic);
    }

    #[test]
    fn resolve_model_defaults_unknown_models_to_chat_completions() {
        let (spec, format) = resolve_model(&ModelCatalog::empty(), &request_for("deepseek-chat"));
        assert_eq!(format, ProviderFormat::ChatCompletions);
        assert_eq!(spec.flavor, ModelFlavor::Chat);
        assert_eq!(spec.model, "deepseek-chat");
    }

    #[test]
    fn resolve_model_resolves_responses_models_by_name() {
        let (spec, format) = resolve_model(&ModelCatalog::empty(), &request_for("gpt-5.4"));
        assert_eq!(format, ProviderFormat::Responses);
        assert_eq!(spec.flavor, ModelFlavor::Responses);
    }

    #[test]
    fn resolve_model_uses_registry_format_when_known() {
        let mut catalog = ModelCatalog::empty();
        let mut spec = default_model_spec("some-anthropic-model");
        spec.format = ProviderFormat::Anthropic;
        catalog.insert("some-anthropic-model".to_string(), spec);

        let (_, format) = resolve_model(&catalog, &request_for("some-anthropic-model"));
        assert_eq!(format, ProviderFormat::Anthropic);
    }

    #[test]
    fn resolve_model_defaults_max_output_tokens_from_request() {
        let request = ModelRequest {
            max_output_tokens: Some(4096),
            ..request_for("deepseek-chat")
        };
        let (spec, _) = resolve_model(&ModelCatalog::empty(), &request);
        assert_eq!(spec.max_output_tokens, Some(4096));
    }

    #[test]
    fn load_model_catalog_defaults_to_empty_without_env() {
        let catalog = load_model_catalog(&HashMap::new()).expect("empty env yields empty catalog");
        assert!(catalog.is_empty());
    }

    #[test]
    fn responses_stream_request_does_not_include_chat_usage_stream_option() {
        let request = build_universal_request(&model_request(), true).unwrap();
        let serialized = serialize_request(ProviderFormat::Responses, &request).unwrap();
        let payload: SerializedResponsesStreamRequest =
            lingua_json::from_slice(&serialized).unwrap();

        assert_eq!(payload.stream, Some(true));
        assert!(payload.stream_options.is_none());
    }

    #[test]
    fn chat_completions_stream_request_includes_lingua_usage_stream_option() {
        let request = build_universal_request(&model_request(), true).unwrap();
        let serialized = serialize_request(ProviderFormat::ChatCompletions, &request).unwrap();
        let payload: SerializedChatStreamRequest = lingua_json::from_slice(&serialized).unwrap();

        assert_eq!(payload.stream, Some(true));
        assert_eq!(
            payload
                .stream_options
                .and_then(|options| options.include_usage),
            Some(true)
        );
    }

    #[test]
    fn stream_accumulator_does_not_treat_chunk_id_as_assistant_message_id() {
        let mut accumulator = UniversalResponseAccumulator::default();
        accumulator.push(&UniversalStreamChunk::new(
            Some("resp_123".to_string()),
            Some("gpt-5.4".to_string()),
            vec![lingua::UniversalStreamChoice::text_delta(0, "hello")],
            None,
            None,
        ));

        let response = accumulator.finalize();

        assert!(matches!(
            response.messages.as_slice(),
            [Message::Assistant { id: None, .. }]
        ));
    }

    #[test]
    fn stream_accumulator_drops_tool_call_slots_without_id_and_name() {
        let mut accumulator = UniversalResponseAccumulator::default();
        accumulator.push(&UniversalStreamChunk::new(
            Some("resp_123".to_string()),
            Some("gpt-5.4".to_string()),
            vec![lingua::UniversalStreamChoice {
                index: 0,
                delta: Some(lingua_json::json!({
                    "role": "assistant",
                    "tool_calls": [
                        { "index": 0 },
                        {
                            "index": 1,
                            "id": "call_real",
                            "function": { "name": "shell", "arguments": "{}" }
                        }
                    ]
                })),
                finish_reason: None,
            }],
            None,
            None,
        ));

        let response = accumulator.finalize();

        let Some(Message::Assistant {
            content: AssistantContent::Array(parts),
            ..
        }) = response.messages.first()
        else {
            panic!("expected a single Assistant message with array content");
        };
        let tool_calls: Vec<&AssistantContentPart> = parts
            .iter()
            .filter(|part| matches!(part, AssistantContentPart::ToolCall { .. }))
            .collect();
        assert_eq!(tool_calls.len(), 1);
        let AssistantContentPart::ToolCall {
            tool_call_id,
            tool_name,
            ..
        } = tool_calls[0]
        else {
            unreachable!();
        };
        assert_eq!(tool_call_id, "call_real");
        assert_eq!(tool_name, "shell");
    }
}

fn serialize_request(format: ProviderFormat, request: &UniversalRequest) -> Result<Bytes> {
    let adapter = adapter_for_format(format)
        .ok_or_else(|| anyhow::anyhow!("unsupported provider format for request: {format}"))?;
    let payload = adapter.request_from_universal(request)?;
    Ok(Bytes::from(lingua_json::to_vec(&payload)?))
}

fn map_universal_stream<S>(raw_stream: S, format: ProviderFormat) -> UniversalChunkStream
where
    S: Stream<
            Item = std::result::Result<
                braintrust_llm_router::StreamChunk,
                braintrust_llm_router::Error,
            >,
        > + Send
        + 'static,
{
    Box::pin(raw_stream.filter_map(move |item| async move {
        match item {
            Ok(chunk) => match lingua::parse_stream_event(chunk.data, format, format) {
                Ok(parsed) => parsed.universal.map(Ok),
                Err(error) => Some(Err(error.into())),
            },
            Err(error) => Some(Err(error.into())),
        }
    }))
}

fn normalize_model_response(response: UniversalResponse) -> Result<ModelResponse> {
    let response_id = Some(Uuid7::now());
    let tool_calls = extract_tool_calls(&response.messages)?;

    Ok(ModelResponse {
        response_id,
        messages: response.messages,
        tool_calls,
        usage: response.usage,
    })
}

fn extract_tool_calls(messages: &[Message]) -> Result<Vec<PendingToolCall>> {
    let mut tool_calls = Vec::new();

    for message in messages {
        let Message::Assistant { content, .. } = message else {
            continue;
        };
        let AssistantContent::Array(parts) = content else {
            continue;
        };

        for part in parts {
            if let AssistantContentPart::ToolCall {
                tool_call_id,
                tool_name,
                arguments,
                ..
            } = part
            {
                let ToolCallArguments::Valid(arguments) = arguments else {
                    continue;
                };
                tool_calls.push(PendingToolCall {
                    tool_call_id: tool_call_id.clone(),
                    request: exoharness::ToolRequest {
                        function_name: tool_name.clone(),
                        arguments: to_exoharness_arguments(arguments)?,
                    },
                });
            }
        }
    }

    Ok(tool_calls)
}

fn to_exoharness_arguments(
    arguments: &lingua::serde_json::Map<String, lingua::serde_json::Value>,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let serialized = lingua_json::to_string(arguments)?;
    Ok(serde_json::from_str(&serialized)?)
}

#[derive(Default)]
struct UniversalResponseAccumulator {
    model: Option<String>,
    usage: Option<lingua::UniversalUsage>,
    text: String,
    reasoning: Vec<String>,
    tool_calls: Vec<lingua::UniversalToolCallDelta>,
}

impl UniversalResponseAccumulator {
    fn push(&mut self, chunk: &UniversalStreamChunk) {
        if chunk.is_keep_alive() {
            return;
        }

        if self.model.is_none() {
            self.model = chunk.model.clone();
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(usage.clone());
        }

        for choice in &chunk.choices {
            let Some(delta) = choice.delta_view() else {
                continue;
            };

            if let Some(content) = delta.content {
                self.text.push_str(&content);
            }

            for reasoning in delta.reasoning {
                if let Some(content) = reasoning.content {
                    self.reasoning.push(content);
                }
            }

            for (fallback_index, tool_call) in delta.tool_calls.into_iter().enumerate() {
                let index = tool_call
                    .index
                    .map(|value| value as usize)
                    .unwrap_or(fallback_index);
                if self.tool_calls.len() <= index {
                    self.tool_calls
                        .resize_with(index + 1, lingua::UniversalToolCallDelta::default);
                }
                let accumulator = &mut self.tool_calls[index];
                if accumulator.id.is_none() {
                    accumulator.id = tool_call.id;
                }
                if accumulator.call_type.is_none() {
                    accumulator.call_type = tool_call.call_type;
                }
                let accumulator_function = accumulator
                    .function
                    .get_or_insert_with(lingua::UniversalToolFunctionDelta::default);
                if let Some(function) = tool_call.function {
                    if accumulator_function.name.is_none() {
                        accumulator_function.name = function.name;
                    }
                    if let Some(arguments_delta) = function.arguments {
                        let current = accumulator_function.arguments.take().unwrap_or_default();
                        accumulator_function.arguments = Some(current + &arguments_delta);
                    }
                }
            }
        }
    }

    fn finalize(&self) -> UniversalResponse {
        let mut content_parts = Vec::new();

        if !self.text.is_empty() {
            content_parts.push(AssistantContentPart::Text(TextContentPart {
                text: self.text.clone(),
                encrypted_content: None,
                provider_options: None,
            }));
        }

        for reasoning in &self.reasoning {
            content_parts.push(AssistantContentPart::Reasoning {
                text: reasoning.clone(),
                encrypted_content: None,
            });
        }

        for tool_call in &self.tool_calls {
            let Some(id) = tool_call.id.clone() else {
                continue;
            };
            let function = tool_call.function.clone().unwrap_or_default();
            let Some(name) = function.name else {
                continue;
            };
            content_parts.push(AssistantContentPart::ToolCall {
                tool_call_id: id,
                tool_name: name,
                arguments: ToolCallArguments::from(function.arguments.unwrap_or_default()),
                encrypted_content: None,
                provider_options: None,
                provider_executed: None,
            });
        }

        let assistant_message = if content_parts.is_empty() {
            None
        } else if content_parts.len() == 1 {
            match &content_parts[0] {
                AssistantContentPart::Text(text) => Some(Message::Assistant {
                    content: AssistantContent::String(text.text.clone()),
                    id: None,
                }),
                _ => Some(Message::Assistant {
                    content: AssistantContent::Array(content_parts),
                    id: None,
                }),
            }
        } else {
            Some(Message::Assistant {
                content: AssistantContent::Array(content_parts),
                id: None,
            })
        };

        UniversalResponse {
            id: None,
            id_format: None,
            model: self.model.clone(),
            messages: assistant_message.into_iter().collect(),
            usage: self.usage.clone(),
            finish_reason: None,
        }
    }
}
